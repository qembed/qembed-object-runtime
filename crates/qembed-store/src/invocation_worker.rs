//! PostgreSQL persistence adapter for the invocation worker lifecycle.

use std::fmt;
use std::str::FromStr;

use qembed_domain::ids::{CallerId, InvocationId, ObjectId, SnapshotId, TenantId};
use qembed_domain::invocation::{FencingToken, Invocation, InvocationCheckpoint, InvocationStatus};
use qembed_domain::ports::{ExecutionStore, InvocationWorkStore};
use qembed_domain::scope::Scope;
use sqlx::PgPool;

use crate::{begin_tenant_transaction, execute, fetch_all, fetch_optional};

#[derive(Clone)]
pub struct PgExecutionStore {
    pub(crate) pool: PgPool,
}

impl PgExecutionStore {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[derive(Debug)]
pub enum InvocationStoreError {
    Database(sqlx::Error),
    InvalidPersistedValue(&'static str),
    InvalidRequestedState(&'static str),
    AmbiguousMessage(&'static str),
}

impl fmt::Display for InvocationStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "invocation persistence failed: {error}"),
            Self::InvalidPersistedValue(field) => {
                write!(formatter, "persisted invocation has invalid {field}")
            }
            Self::InvalidRequestedState(reason) => {
                write!(formatter, "invalid invocation write request: {reason}")
            }
            Self::AmbiguousMessage(role) => {
                write!(formatter, "invocation has multiple {role} messages")
            }
        }
    }
}

impl std::error::Error for InvocationStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

impl From<sqlx::Error> for InvocationStoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl ExecutionStore for PgExecutionStore {
    type Error = InvocationStoreError;

    async fn load_invocation(
        &self,
        scope: &Scope,
        invocation_id: InvocationId,
    ) -> Result<Option<Invocation>, Self::Error> {
        let mut transaction = begin_tenant_transaction(&self.pool, scope).await?;
        let row = fetch_optional(
            &mut transaction,
            sqlx::query_as::<_, (String, String, String, String, String, String, i64, i64)>(
                "SELECT id::text, tenant_id::text, object_id::text, caller_id::text, snapshot_id::text, status, version, fencing_token FROM object_invocations WHERE tenant_id=$1::uuid AND id=$2::uuid",
            )
            .bind(scope.tenant_id().to_string())
            .bind(invocation_id.to_string()),
        )
        .await?;
        let invocation = row.map(reconstitute_invocation).transpose()?;
        transaction.commit().await?;
        Ok(invocation)
    }

    async fn compare_and_set_invocation(
        &self,
        scope: &Scope,
        expected_version: u64,
        expected_fencing_token: FencingToken,
        next: Invocation,
    ) -> Result<bool, Self::Error> {
        validate_next_state(scope, expected_version, expected_fencing_token, &next)?;
        if next.status().is_terminal() {
            return Err(InvocationStoreError::InvalidRequestedState(
                "terminal transitions require a dedicated atomic result operation",
            ));
        }
        let mut transaction = begin_tenant_transaction(&self.pool, scope).await?;
        let result = execute(
            &mut transaction,
            sqlx::query(
                "UPDATE object_invocations SET status=$1, version=$2 WHERE tenant_id=$3::uuid AND object_id=$4::uuid AND id=$5::uuid AND version=$6 AND fencing_token=$7 AND lease_owner IS NULL AND status NOT IN ('Completed','CompletedDegraded','Failed','Cancelled')",
            )
            .bind(status_name(next.status()))
            .bind(to_i64(next.version(), "next version")?)
            .bind(scope.tenant_id().to_string())
            .bind(next.object_id().to_string())
            .bind(next.id().to_string())
            .bind(to_i64(expected_version, "expected version")?)
            .bind(to_i64(expected_fencing_token.value(), "expected fencing token")?),
        )
        .await?;
        transaction.commit().await?;
        Ok(result.rows_affected() == 1)
    }
}

impl InvocationWorkStore for PgExecutionStore {
    async fn load_user_input(
        &self,
        scope: &Scope,
        invocation_id: InvocationId,
    ) -> Result<Option<String>, Self::Error> {
        load_single_message(&self.pool, scope, invocation_id, "user").await
    }

    async fn load_assistant_output(
        &self,
        scope: &Scope,
        invocation_id: InvocationId,
    ) -> Result<Option<String>, Self::Error> {
        load_single_message(&self.pool, scope, invocation_id, "assistant").await
    }

    async fn persist_assistant_output(
        &self,
        _scope: &Scope,
        _expected_version: u64,
        _expected_fencing_token: FencingToken,
        _next: Invocation,
        _content: String,
    ) -> Result<bool, Self::Error> {
        Err(InvocationStoreError::InvalidRequestedState(
            "assistant output requires the lease-fenced provider attempt protocol",
        ))
    }

    async fn complete_invocation(
        &self,
        scope: &Scope,
        expected_version: u64,
        expected_fencing_token: FencingToken,
        next: Invocation,
    ) -> Result<bool, Self::Error> {
        validate_next_state(scope, expected_version, expected_fencing_token, &next)?;
        if next.status() != InvocationStatus::Completed {
            return Err(InvocationStoreError::InvalidRequestedState(
                "completion must transition to Completed",
            ));
        }
        let mut ownership = begin_tenant_transaction(&self.pool, scope).await?;
        let live = fetch_optional(
            &mut ownership,
            sqlx::query_as::<_, (String,)>(
                "SELECT id::text FROM object_invocations WHERE tenant_id=$1::uuid AND object_id=$2::uuid AND id=$3::uuid AND version=$4 AND fencing_token=$5 AND status='Verifying' AND lease_owner IS NULL",
            )
            .bind(scope.tenant_id().to_string())
            .bind(next.object_id().to_string())
            .bind(next.id().to_string())
            .bind(to_i64(expected_version, "expected version")?)
            .bind(to_i64(expected_fencing_token.value(), "expected fencing token")?),
        )
        .await?;
        if live.is_none() {
            ownership.rollback().await?;
            return Ok(false);
        }
        ownership.commit().await?;

        let attempt = {
            let mut transaction = begin_tenant_transaction(&self.pool, scope).await?;
            let row = fetch_optional(
                &mut transaction,
                sqlx::query_as::<_, (String, Option<i64>, Option<i64>)>(
                    "SELECT state, input_tokens, output_tokens FROM provider_call_attempts WHERE tenant_id=$1::uuid AND object_id=$2::uuid AND invocation_id=$3::uuid",
                )
                .bind(scope.tenant_id().to_string())
                .bind(next.object_id().to_string())
                .bind(next.id().to_string()),
            )
            .await?;
            transaction.commit().await?;
            row
        }
        .ok_or(InvocationStoreError::InvalidRequestedState(
            "provider call attempt is missing",
        ))?;
        if attempt.0 != "ResponseReceived" {
            return Err(InvocationStoreError::InvalidRequestedState(
                "provider response is not durably recorded",
            ));
        }
        let usage = match (attempt.1, attempt.2) {
            (None, None) => None,
            (Some(input), Some(output)) => Some(qembed_domain::chat::TokenUsage::new(
                u32::try_from(input).map_err(|_| InvocationStoreError::InvalidPersistedValue("provider input_tokens"))?,
                u32::try_from(output).map_err(|_| InvocationStoreError::InvalidPersistedValue("provider output_tokens"))?,
            )),
            _ => return Err(InvocationStoreError::InvalidPersistedValue("provider usage pair")),
        };
        crate::budget_settlement::settle_invocation_usage(
            &self.pool,
            scope,
            next.id(),
            usage,
        )
        .await
        .map_err(|_| InvocationStoreError::InvalidRequestedState("provider usage settlement failed"))?;
        let mut transaction = begin_tenant_transaction(&self.pool, scope).await?;
        let result = execute(
            &mut transaction,
            sqlx::query(
                "UPDATE object_invocations SET status=CASE WHEN degraded_reason='provider_usage_missing' THEN 'CompletedDegraded' ELSE 'Completed' END, finished_at=now(), version=version+1, lease_owner=NULL, lease_until=NULL WHERE tenant_id=$1::uuid AND object_id=$2::uuid AND id=$3::uuid AND version=$4 AND fencing_token=$5 AND status='Verifying' AND lease_owner IS NULL AND EXISTS (SELECT 1 FROM interaction_messages WHERE tenant_id=$1::uuid AND object_id=$2::uuid AND invocation_id=$3::uuid AND role='assistant')",
            )
            .bind(scope.tenant_id().to_string())
            .bind(next.object_id().to_string())
            .bind(next.id().to_string())
            .bind(to_i64(expected_version, "expected version")?)
            .bind(to_i64(expected_fencing_token.value(), "expected fencing token")?),
        )
        .await?;
        transaction.commit().await?;
        Ok(result.rows_affected() == 1)
    }

    async fn fail_invocation(
        &self,
        scope: &Scope,
        expected_version: u64,
        expected_fencing_token: FencingToken,
        next: Invocation,
        error_code: &'static str,
    ) -> Result<bool, Self::Error> {
        validate_next_state(scope, expected_version, expected_fencing_token, &next)?;
        if next.status() != InvocationStatus::Failed
            || !matches!(
                error_code,
                "provider_timeout"
                    | "provider_rate_limited"
                    | "provider_unavailable"
                    | "provider_malformed_output"
                    | "provider_other"
            )
        {
            return Err(InvocationStoreError::InvalidRequestedState(
                "failure must use a stable allowlisted provider error code",
            ));
        }
        let mut transaction = begin_tenant_transaction(&self.pool, scope).await?;
        let attempt = fetch_optional(
            &mut transaction,
            sqlx::query_as::<_, (String,)>(
                "SELECT state FROM provider_call_attempts WHERE tenant_id=$1::uuid AND object_id=$2::uuid AND invocation_id=$3::uuid FOR UPDATE",
            )
            .bind(scope.tenant_id().to_string())
            .bind(next.object_id().to_string())
            .bind(next.id().to_string()),
        )
        .await?;
        let Some((attempt_state,)) = attempt else {
            transaction.rollback().await?;
            return Err(InvocationStoreError::InvalidRequestedState(
                "provider call attempt is missing; failure fails closed",
            ));
        };
        if !matches!(
            attempt_state.as_str(),
            "NotStarted" | "ConfirmedNotAccepted" | "CancelledBeforeSend"
        ) {
            transaction.rollback().await?;
            return Err(InvocationStoreError::InvalidRequestedState(
                "cannot fail invocation after provider may have accepted the call",
            ));
        }
        let result = execute(
            &mut transaction,
            sqlx::query(
                "UPDATE object_invocations SET status='Failed', error_code=$1, finished_at=now(), version=version+1, lease_owner=NULL, lease_until=NULL WHERE tenant_id=$2::uuid AND object_id=$3::uuid AND id=$4::uuid AND version=$5 AND fencing_token=$6 AND lease_owner IS NULL AND status NOT IN ('Completed','CompletedDegraded','Failed','Cancelled')",
            )
            .bind(error_code)
            .bind(scope.tenant_id().to_string())
            .bind(next.object_id().to_string())
            .bind(next.id().to_string())
            .bind(to_i64(expected_version, "expected version")?)
            .bind(to_i64(expected_fencing_token.value(), "expected fencing token")?),
        )
        .await?;
        transaction.commit().await?;
        Ok(result.rows_affected() == 1)
    }
}

async fn load_single_message(
    pool: &PgPool,
    scope: &Scope,
    invocation_id: InvocationId,
    role: &'static str,
) -> Result<Option<String>, InvocationStoreError> {
    let mut transaction = begin_tenant_transaction(pool, scope).await?;
    let messages = fetch_all(
        &mut transaction,
        sqlx::query_as::<_, (String,)>(
            "SELECT content FROM interaction_messages WHERE tenant_id=$1::uuid AND invocation_id=$2::uuid AND role=$3 ORDER BY created_at, id",
        )
        .bind(scope.tenant_id().to_string())
        .bind(invocation_id.to_string())
        .bind(role),
    )
    .await?;
    transaction.commit().await?;
    match messages.as_slice() {
        [] => Ok(None),
        [(content,)] => Ok(Some(content.clone())),
        _ => Err(InvocationStoreError::AmbiguousMessage(role)),
    }
}

fn validate_next_state(
    scope: &Scope,
    expected_version: u64,
    expected_fencing_token: FencingToken,
    next: &Invocation,
) -> Result<(), InvocationStoreError> {
    if next.tenant_id() != scope.tenant_id() {
        return Err(InvocationStoreError::InvalidRequestedState(
            "invocation tenant differs from trusted Scope",
        ));
    }
    if next.version()
        != expected_version
            .checked_add(1)
            .ok_or(InvocationStoreError::InvalidRequestedState(
                "expected version overflow",
            ))?
    {
        return Err(InvocationStoreError::InvalidRequestedState(
            "next invocation version must advance exactly once",
        ));
    }
    if next.fencing_token() != expected_fencing_token {
        return Err(InvocationStoreError::InvalidRequestedState(
            "fencing token cannot change during a lifecycle transition",
        ));
    }
    Ok(())
}

fn reconstitute_invocation(
    row: (String, String, String, String, String, String, i64, i64),
) -> Result<Invocation, InvocationStoreError> {
    let id = parse_id::<InvocationId>(&row.0, "id")?;
    let tenant_id = parse_id::<TenantId>(&row.1, "tenant_id")?;
    let object_id = parse_id::<ObjectId>(&row.2, "object_id")?;
    let caller_id = parse_id::<CallerId>(&row.3, "caller_id")?;
    let snapshot_id = parse_id::<SnapshotId>(&row.4, "snapshot_id")?;
    let status = parse_status(&row.5)?;
    let version =
        u64::try_from(row.6).map_err(|_| InvocationStoreError::InvalidPersistedValue("version"))?;
    let token = u64::try_from(row.7)
        .map_err(|_| InvocationStoreError::InvalidPersistedValue("fencing_token"))?;
    Ok(Invocation::from_persisted(
        id,
        tenant_id,
        object_id,
        caller_id,
        snapshot_id,
        InvocationCheckpoint::new(status, version, FencingToken::from_value(token)),
    ))
}

fn parse_id<T: FromStr>(value: &str, field: &'static str) -> Result<T, InvocationStoreError> {
    value
        .parse()
        .map_err(|_| InvocationStoreError::InvalidPersistedValue(field))
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

const fn status_name(status: InvocationStatus) -> &'static str {
    match status {
        InvocationStatus::Received => "Received",
        InvocationStatus::ContextLoading => "ContextLoading",
        InvocationStatus::Processing => "Processing",
        InvocationStatus::ExecutingTool => "ExecutingTool",
        InvocationStatus::Verifying => "Verifying",
        InvocationStatus::WaitingForInput => "WaitingForInput",
        InvocationStatus::WaitingForApproval => "WaitingForApproval",
        InvocationStatus::Completed => "Completed",
        InvocationStatus::CompletedDegraded => "CompletedDegraded",
        InvocationStatus::Failed => "Failed",
        InvocationStatus::Cancelled => "Cancelled",
    }
}

fn to_i64(value: u64, field: &'static str) -> Result<i64, InvocationStoreError> {
    i64::try_from(value).map_err(|_| InvocationStoreError::InvalidRequestedState(field))
}
