//! PostgreSQL implementation of durable invocation cancellation.

use qembed_domain::ids::InvocationId;
use qembed_domain::invocation::InvocationStatus;
use qembed_domain::ports::{InvocationCancellationOutcome, InvocationCancellationStore};
use qembed_domain::scope::Scope;

use crate::invocation_worker::{InvocationStoreError, PgExecutionStore};
use crate::{begin_tenant_transaction, execute, fetch_optional};

impl InvocationCancellationStore for PgExecutionStore {
    type Error = InvocationStoreError;

    async fn cancel_invocation(
        &self,
        scope: &Scope,
        invocation_id: InvocationId,
    ) -> Result<InvocationCancellationOutcome, Self::Error> {
        let mut transaction = begin_tenant_transaction(&self.pool, scope).await?;

        // Lock the provider attempt before touching the invocation. This is the
        // same serialization point used by the pre-send marker and the canonical
        // reservation sweeper, so cancellation cannot race a send authorization.
        let attempt = fetch_optional(
            &mut transaction,
            sqlx::query_as::<_, (String,)>(
                "SELECT state FROM provider_call_attempts WHERE tenant_id=$1::uuid AND invocation_id=$2::uuid FOR UPDATE",
            )
            .bind(scope.tenant_id().to_string())
            .bind(invocation_id.to_string()),
        )
        .await?;
        let Some((attempt_state,)) = attempt else {
            transaction.rollback().await?;
            return Err(InvocationStoreError::InvalidRequestedState(
                "provider call attempt is missing; cancellation fails closed",
            ));
        };

        match attempt_state.as_str() {
            "NotStarted" => {
                execute(
                    &mut transaction,
                    sqlx::query("UPDATE provider_call_attempts SET state='CancelledBeforeSend' WHERE tenant_id=$1::uuid AND invocation_id=$2::uuid AND state='NotStarted'")
                        .bind(scope.tenant_id().to_string())
                        .bind(invocation_id.to_string()),
                )
                .await?;
            }
            "MayHaveBeenSent" => {
                execute(
                    &mut transaction,
                    sqlx::query("UPDATE provider_call_attempts SET state='OutcomeUnknown' WHERE tenant_id=$1::uuid AND invocation_id=$2::uuid AND state='MayHaveBeenSent'")
                        .bind(scope.tenant_id().to_string())
                        .bind(invocation_id.to_string()),
                )
                .await?;
            }
            "OutcomeUnknown" | "CancelledBeforeSend" | "ConfirmedNotAccepted" => {}
            "ResponseReceived" => {
                transaction.rollback().await?;
                return Err(InvocationStoreError::InvalidRequestedState(
                    "provider response is already durable; invocation must finish settlement",
                ));
            }
            _ => {
                transaction.rollback().await?;
                return Err(InvocationStoreError::InvalidPersistedValue("provider attempt state"));
            }
        }

        // Expire an active lease before releasing its owner; the row lock and
        // transaction hide this intermediate state from competing workers.
        execute(
            &mut transaction,
            sqlx::query(
                "UPDATE object_invocations SET lease_until=now()-interval '1 microsecond' WHERE tenant_id=$1::uuid AND id=$2::uuid AND lease_owner IS NOT NULL AND lease_until > now() AND status NOT IN ('Completed','CompletedDegraded','Failed','Cancelled')",
            )
            .bind(scope.tenant_id().to_string())
            .bind(invocation_id.to_string()),
        )
        .await?;

        let cancelled = fetch_optional(
            &mut transaction,
            sqlx::query_as::<_, (String,)>(
                "UPDATE object_invocations SET status='Cancelled', cancel_requested_at=COALESCE(cancel_requested_at, now()), finished_at=now(), version=version+1, fencing_token=fencing_token+1, lease_owner=NULL, lease_until=NULL WHERE tenant_id=$1::uuid AND id=$2::uuid AND status NOT IN ('Completed','CompletedDegraded','Failed','Cancelled') AND (lease_owner IS NULL OR lease_until <= now()) AND fencing_token < 9223372036854775807 RETURNING status",
            )
            .bind(scope.tenant_id().to_string())
            .bind(invocation_id.to_string()),
        )
        .await?;

        if cancelled.is_some() {
            transaction.commit().await?;
            return Ok(InvocationCancellationOutcome::Cancelled {
                newly_cancelled: true,
            });
        }

        let current = fetch_optional(
            &mut transaction,
            sqlx::query_as::<_, (String, i64)>(
                "SELECT status, fencing_token FROM object_invocations WHERE tenant_id=$1::uuid AND id=$2::uuid",
            )
            .bind(scope.tenant_id().to_string())
            .bind(invocation_id.to_string()),
        )
        .await?;

        let Some((status, fencing_token)) = current else {
            transaction.commit().await?;
            return Ok(InvocationCancellationOutcome::NotFound);
        };
        let status = parse_status(&status)?;

        if status == InvocationStatus::Cancelled {
            transaction.commit().await?;
            return Ok(InvocationCancellationOutcome::Cancelled {
                newly_cancelled: false,
            });
        }
        if status.is_terminal() {
            transaction.commit().await?;
            return Ok(InvocationCancellationOutcome::AlreadyTerminal(status));
        }
        transaction.rollback().await?;
        if fencing_token == i64::MAX {
            return Err(InvocationStoreError::InvalidRequestedState(
                "fencing token exhausted; cancellation cannot safely revoke the worker",
            ));
        }

        Err(InvocationStoreError::InvalidRequestedState(
            "nonterminal invocation could not be cancelled atomically",
        ))
    }
}

fn parse_status(value: &str) -> Result<InvocationStatus, InvocationStoreError> {
    match value {
        "Received" => Ok(InvocationStatus::Received),
        "ContextLoading" => Ok(InvocationStatus::ContextLoading),
        "Processing" => Ok(InvocationStatus::Processing),
        "ExecutingTool" => Ok(InvocationStatus::ExecutingTool),
        "Verifying" => Ok(InvocationStatus::Verifying),
        "WaitingForInput" => Ok(InvocationStatus::WaitingForInput),
        "WaitingForApproval" => Ok(InvocationStatus::WaitingForApproval),
        "Completed" => Ok(InvocationStatus::Completed),
        "CompletedDegraded" => Ok(InvocationStatus::CompletedDegraded),
        "Failed" => Ok(InvocationStatus::Failed),
        "Cancelled" => Ok(InvocationStatus::Cancelled),
        _ => Err(InvocationStoreError::InvalidPersistedValue("status")),
    }
}
