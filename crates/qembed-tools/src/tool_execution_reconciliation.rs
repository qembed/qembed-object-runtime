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
