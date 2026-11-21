//! Reconciliation workflow for ambiguous ToolExecution outcomes.

use qembed_domain::{
    scope::Scope,
    tool_execution::{ToolExecution, ToolExecutionError, ToolExecutionStatus},
    tool_execution_lease::{ToolExecutionLeaseStore, ToolExecutionReconciliationStore},
    tool_execution_reconciliation::{
        ToolExecutionReconciler, ToolExecutionReconciliationCompletion,
        ToolExecutionReconciliationOutcome, ToolExecutionReconciliationRequest,
    },
};

/// Result of one reconciliation attempt.
#[derive(Debug, PartialEq)]
pub enum ToolExecutionReconciliationResult {
    /// Provider evidence is inconclusive; the row remains Unknown and pending.
    Unresolved,
    /// The provider result was committed under the exact checkpoint reconciled.
    Reconciled(ToolExecution),
    /// Another actor changed the row while the provider lookup was in flight.
    Stale,
}

/// Failure while loading, querying the provider, or applying reconciliation.
#[derive(Debug)]
pub enum ToolExecutionReconciliationWorkflowError<StoreError, ReconcilerError> {
    Store(StoreError),
    Reconciler(ReconcilerError),
    Domain(ToolExecutionError),
    NotFound,
    NotUnknown(ToolExecutionStatus),
}

/// Reconciles one Unknown ToolExecution without holding a database transaction
/// across the external provider call.
pub async fn reconcile_tool_execution<S, R>(
    store: &S,
    reconciler: &R,
    scope: &Scope,
    execution_id: qembed_domain::ids::ToolExecutionId,
) -> Result<
    ToolExecutionReconciliationResult,
    ToolExecutionReconciliationWorkflowError<S::Error, R::Error>,
>
where
    S: ToolExecutionReconciliationStore,
    R: ToolExecutionReconciler,
{
    let execution = store
        .load_tool_execution(scope, execution_id)
        .await
        .map_err(ToolExecutionReconciliationWorkflowError::Store)?
        .ok_or(ToolExecutionReconciliationWorkflowError::NotFound)?;

    if execution.status() != ToolExecutionStatus::Unknown {
        return Err(ToolExecutionReconciliationWorkflowError::NotUnknown(
            execution.status(),
        ));
    }

    let request = ToolExecutionReconciliationRequest::new(
        execution.id(),
        execution.invocation_id(),
        execution.command_id(),
        execution.version(),
        execution.fencing_token(),
    );

    // Deliberately outside any SQL transaction: provider I/O must never hold DB locks.
    let outcome = reconciler
        .reconcile_tool_execution(request.clone())
        .await
        .map_err(ToolExecutionReconciliationWorkflowError::Reconciler)?;

    if outcome == ToolExecutionReconciliationOutcome::Unresolved {
        return Ok(ToolExecutionReconciliationResult::Unresolved);
    }

    let mut next = execution;
    next.reconcile(outcome.clone())
        .map_err(ToolExecutionReconciliationWorkflowError::Domain)?;

    let committed = store
        .apply_tool_execution_reconciliation(
            scope,
            ToolExecutionReconciliationCompletion {
                request,
                next: next.clone(),
                outcome,
            },
        )
        .await
        .map_err(ToolExecutionReconciliationWorkflowError::Store)?;

    if committed {
        Ok(ToolExecutionReconciliationResult::Reconciled(next))
    } else {
        Ok(ToolExecutionReconciliationResult::Stale)
    }
}


#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use qembed_domain::{
        ids::{CommandId, InvocationId, ObjectId, TenantId, ToolExecutionId},
        scope::Scope,
        test_support::scope_for_tenant,
        tool_execution::{
            ToolExecution, ToolExecutionCheckpoint, ToolExecutionFencingToken,
            ToolExecutionStatus,
        },
        tool_execution_lease::{
            ToolExecutionCompletion, ToolExecutionLeaseStore, ToolExecutionReconciliationStore,
        },
        tool_execution_reconciliation::{
            ToolExecutionReconciler, ToolExecutionReconciliationCompletion,
            ToolExecutionReconciliationOutcome, ToolExecutionReconciliationRequest,
        },
    };

    use super::{
        reconcile_tool_execution, ToolExecutionReconciliationResult,
    };

    #[derive(Clone)]
    struct MockStore {
        execution: Arc<Mutex<ToolExecution>>,
    }

    impl MockStore {
        fn new() -> Self {
            let mut execution = ToolExecution::new(
                ToolExecutionId::new(),
                TenantId::new(),
                ObjectId::new(),
                InvocationId::new(),
                CommandId::new(),
            );
            execution.claim("worker").expect("claim succeeds");
            execution.mark_unknown_after_expiry().expect("Unknown transition succeeds");
            Self { execution: Arc::new(Mutex::new(execution)) }
        }

        fn replace_fence(&self) {
            let mut current = self.execution.lock().expect("mock mutex is not poisoned");
            *current = ToolExecution::from_persisted(
                current.id(),
                current.tenant_id(),
                current.object_id(),
                current.invocation_id(),
                current.command_id(),
                ToolExecutionCheckpoint::new(
                    ToolExecutionStatus::Unknown,
                    current.version(),
                    ToolExecutionFencingToken::from_value(current.fencing_token().value() + 1),
                    None,
                ),
            );
        }
    }

    impl ToolExecutionLeaseStore for MockStore {
        type Error = std::io::Error;

        async fn load_tool_execution(
            &self,
            _scope: &Scope,
            execution_id: ToolExecutionId,
        ) -> Result<Option<ToolExecution>, Self::Error> {
            let execution = self.execution.lock().expect("mock mutex is not poisoned");
            Ok((execution.id() == execution_id).then(|| execution.clone()))
        }

        async fn claim_tool_execution(
            &self, _: &Scope, _: ToolExecutionId, _: &str, _: u32,
        ) -> Result<Option<ToolExecution>, Self::Error> { Ok(None) }

        async fn heartbeat_tool_execution(
            &self, _: &Scope, _: ToolExecutionId, _: &str, _: u64,
            _: ToolExecutionFencingToken, _: u32,
        ) -> Result<bool, Self::Error> { Ok(false) }

        async fn mark_tool_execution_unknown(
            &self, _: &Scope, _: ToolExecutionId, _: u64, _: ToolExecutionFencingToken,
        ) -> Result<bool, Self::Error> { Ok(false) }

        async fn complete_tool_execution(
            &self, _: &Scope, _: ToolExecutionCompletion,
        ) -> Result<bool, Self::Error> { Ok(false) }
    }

    impl ToolExecutionReconciliationStore for MockStore {
        async fn apply_tool_execution_reconciliation(
            &self,
            _: &Scope,
            completion: ToolExecutionReconciliationCompletion,
        ) -> Result<bool, Self::Error> {
            let mut current = self.execution.lock().expect("mock mutex is not poisoned");
            if current.status() != ToolExecutionStatus::Unknown
                || current.id() != completion.request.execution_id()
                || current.version() != completion.request.expected_version()
                || current.fencing_token() != completion.request.expected_fencing_token()
            {
                return Ok(false);
            }
            *current = completion.next;
            Ok(true)
        }
    }

    struct MockReconciler {
        outcome: ToolExecutionReconciliationOutcome,
        change_fence_before_return: bool,
        store: Option<MockStore>,
    }

    impl ToolExecutionReconciler for MockReconciler {
        type Error = std::io::Error;

        async fn reconcile_tool_execution(
            &self,
            _request: ToolExecutionReconciliationRequest,
        ) -> Result<ToolExecutionReconciliationOutcome, Self::Error> {
            if self.change_fence_before_return {
                self.store.as_ref().expect("store supplied").replace_fence();
            }
            Ok(self.outcome.clone())
        }
    }

    fn scope() -> Scope { scope_for_tenant(TenantId::new()) }

    #[tokio::test]
    async fn confirmed_success_is_committed_as_terminal() {
        let store = MockStore::new();
        let reconciler = MockReconciler {
            outcome: ToolExecutionReconciliationOutcome::ConfirmedSucceeded {
                output: Some(serde_json::json!({"provider":"confirmed"})),
                output_ttl_seconds: Some(60),
            },
            change_fence_before_return: false,
            store: None,
        };
        let result = reconcile_tool_execution(
            &store, &reconciler, &scope(),
            store.execution.lock().expect("mutex").id(),
        ).await.expect("workflow succeeds");
        match result {
            ToolExecutionReconciliationResult::Reconciled(execution) => {
                assert_eq!(execution.status(), ToolExecutionStatus::Succeeded);
                assert_eq!(execution.version(), 3);
            }
            other => panic!("unexpected workflow result: {other:?}"),
        }
    }

    #[tokio::test]
    async fn confirmed_failure_is_committed_as_terminal() {
        let store = MockStore::new();
        let reconciler = MockReconciler {
            outcome: ToolExecutionReconciliationOutcome::ConfirmedFailed {
                evidence: Some(serde_json::json!({"provider":"confirmed_absent"})),
            },
            change_fence_before_return: false,
            store: None,
        };
        let id = store.execution.lock().expect("mutex").id();
        let result = reconcile_tool_execution(&store, &reconciler, &scope(), id)
            .await.expect("workflow succeeds");
        match result {
            ToolExecutionReconciliationResult::Reconciled(execution) => {
                assert_eq!(execution.status(), ToolExecutionStatus::Failed);
                assert_eq!(execution.version(), 3);
            }
            other => panic!("unexpected workflow result: {other:?}"),
        }
    }

    #[tokio::test]
    async fn unresolved_outcome_leaves_execution_pending_and_unchanged() {
        let store = MockStore::new();
        let reconciler = MockReconciler {
            outcome: ToolExecutionReconciliationOutcome::Unresolved,
            change_fence_before_return: false,
            store: None,
        };
        let id = store.execution.lock().expect("mutex").id();
        let result = reconcile_tool_execution(&store, &reconciler, &scope(), id)
            .await.expect("unresolved is not an error");
        assert_eq!(result, ToolExecutionReconciliationResult::Unresolved);
        let persisted = store.execution.lock().expect("mutex");
        assert_eq!(persisted.status(), ToolExecutionStatus::Unknown);
        assert_eq!(persisted.version(), 2);
    }

    #[tokio::test]
    async fn stale_fencing_token_cannot_commit_a_provider_result() {
        let store = MockStore::new();
        let reconciler = MockReconciler {
            outcome: ToolExecutionReconciliationOutcome::ConfirmedSucceeded {
                output: None,
                output_ttl_seconds: None,
            },
            change_fence_before_return: true,
            store: Some(store.clone()),
        };
        let id = store.execution.lock().expect("mutex").id();
        let result = reconcile_tool_execution(&store, &reconciler, &scope(), id)
            .await.expect("stale CAS is not a storage error");
        assert_eq!(result, ToolExecutionReconciliationResult::Stale);
        let persisted = store.execution.lock().expect("mutex");
        assert_eq!(persisted.status(), ToolExecutionStatus::Unknown);
        assert_eq!(persisted.fencing_token().value(), 2);
    }
}
