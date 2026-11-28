//! PostgreSQL adapter for the independent ToolExecution lease lifecycle.

use std::fmt;
use std::str::FromStr;

use crate::{PgContext, execute, fetch_optional};
use qembed_domain::ids::{CommandId, InvocationId, ObjectId, TenantId, ToolExecutionId};
use qembed_domain::scope::Scope;
use qembed_domain::tool_execution::{
    ToolExecution, ToolExecutionCheckpoint, ToolExecutionFencingToken, ToolExecutionStatus,
};
use qembed_domain::tool_execution_lease::{
    ToolExecutionCompletion, ToolExecutionLeaseStore, ToolExecutionReconciliationStore,
};
use qembed_domain::tool_execution_reconciliation::{
    ToolExecutionReconciliationCompletion, ToolExecutionReconciliationOutcome,
};

type ToolExecutionRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    i64,
    i64,
    Vec<u8>,
    Option<String>,
);

/// PostgreSQL implementation of the ToolExecution lease store port.
#[derive(Clone)]
pub struct PgToolExecutionStore {
    context: PgContext,
}

impl PgToolExecutionStore {
    /// Creates a store over the application's signed PostgreSQL context.
    #[must_use]
    pub fn new(context: PgContext) -> Self {
        Self { context }
    }
}

/// Failure to load or mutate a persisted ToolExecution.
#[derive(Debug)]
pub enum ToolExecutionStoreError {
    Context(crate::ContextTransactionError),
    Database(sqlx::Error),
    InvalidPersistedValue(&'static str),
    InvalidRequestedValue(&'static str),
}

impl fmt::Display for ToolExecutionStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Context(error) => write!(
                formatter,
                "ToolExecution signed-context initialization failed: {error}"
            ),
            Self::Database(error) => write!(formatter, "ToolExecution persistence failed: {error}"),
            Self::InvalidPersistedValue(field) => {
                write!(formatter, "persisted ToolExecution has invalid {field}")
            }
            Self::InvalidRequestedValue(field) => {
                write!(formatter, "invalid ToolExecution write request: {field}")
            }
        }
    }
}

impl std::error::Error for ToolExecutionStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Context(error) => Some(error),
            Self::Database(error) => Some(error),
            Self::InvalidPersistedValue(_) | Self::InvalidRequestedValue(_) => None,
        }
    }
}

impl From<crate::ContextTransactionError> for ToolExecutionStoreError {
    fn from(error: crate::ContextTransactionError) -> Self {
        Self::Context(error)
    }
}

impl From<sqlx::Error> for ToolExecutionStoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl ToolExecutionLeaseStore for PgToolExecutionStore {
    type Error = ToolExecutionStoreError;

    async fn load_tool_execution(
        &self,
        scope: &Scope,
        execution_id: ToolExecutionId,
    ) -> Result<Option<ToolExecution>, Self::Error> {
        let mut transaction = self.context.begin_principal(scope).await?;
        let row = fetch_optional(
            &mut transaction,
            sqlx::query_as::<_, ToolExecutionRow>(
                "SELECT id::text, tenant_id::text, object_id::text, invocation_id::text, command_id::text, status, version, fencing_token, arguments_hash, lease_owner FROM tool_executions WHERE tenant_id=$1::uuid AND id=$2::uuid",
            )
            .bind(scope.tenant_id().to_string())
            .bind(execution_id.to_string()),
        )
        .await?;
        let execution = row.map(reconstitute_tool_execution).transpose()?;
        transaction.commit().await?;
        Ok(execution)
    }

    async fn claim_tool_execution(
        &self,
        scope: &Scope,
        execution_id: ToolExecutionId,
        owner: &str,
        lease_seconds: u32,
    ) -> Result<Option<ToolExecution>, Self::Error> {
        validate_lease_request(owner, lease_seconds)?;
        let mut transaction = self.context.begin_principal(scope).await?;
        let row = fetch_optional(
            &mut transaction,
            sqlx::query_as::<_, ToolExecutionRow>(
                "UPDATE tool_executions
                    SET status='Executing',
                        lease_owner=$3,
                        lease_until=clock_timestamp()+make_interval(secs => $4::double precision),
                        heartbeat_at=clock_timestamp(),
                        fencing_token=fencing_token+1,
                        version=version+1
                  WHERE tenant_id=$1::uuid AND id=$2::uuid
                    AND status='Authorized' AND version=0 AND fencing_token=0
                    AND EXISTS (
                        SELECT 1 FROM object_tools binding
                         WHERE binding.tenant_id=tool_executions.tenant_id
                           AND binding.object_id=tool_executions.object_id
                           AND binding.tool_id=tool_executions.tool_id
                           AND binding.tool_version_id=tool_executions.tool_version_id
                           AND binding.binding_version=tool_executions.binding_version
                           AND binding.enabled
                    )
                RETURNING id::text, tenant_id::text, object_id::text, invocation_id::text,
                          command_id::text, status, version, fencing_token, arguments_hash, lease_owner",
            )
            .bind(scope.tenant_id().to_string())
            .bind(execution_id.to_string())
            .bind(owner)
            .bind(f64::from(lease_seconds)),
        )
        .await?;
        let execution = row.map(reconstitute_tool_execution).transpose()?;
        transaction.commit().await?;
        Ok(execution)
    }

    async fn heartbeat_tool_execution(
        &self,
        scope: &Scope,
        execution_id: ToolExecutionId,
        owner: &str,
        expected_version: u64,
        expected_fencing_token: ToolExecutionFencingToken,
        lease_seconds: u32,
    ) -> Result<bool, Self::Error> {
        validate_lease_request(owner, lease_seconds)?;
        let mut transaction = self.context.begin_principal(scope).await?;
        let result = execute(
            &mut transaction,
            sqlx::query(
                "UPDATE tool_executions
                    SET lease_until=GREATEST(
                            lease_until,
                            clock_timestamp() + make_interval(secs => $1::double precision)
                        ),
                        heartbeat_at=clock_timestamp()
                  WHERE tenant_id=$2::uuid AND id=$3::uuid AND status='Executing'
                    AND lease_owner=$4 AND lease_until > clock_timestamp()
                    AND version=$5 AND fencing_token=$6",
            )
            .bind(f64::from(lease_seconds))
            .bind(scope.tenant_id().to_string())
            .bind(execution_id.to_string())
            .bind(owner)
            .bind(to_i64(expected_version, "expected version")?)
            .bind(to_i64(
                expected_fencing_token.value(),
                "expected fencing token",
            )?),
        )
        .await?;
        transaction.commit().await?;
        Ok(result.rows_affected() == 1)
    }

    async fn mark_tool_execution_unknown(
        &self,
        scope: &Scope,
        execution_id: ToolExecutionId,
        expected_version: u64,
        expected_fencing_token: ToolExecutionFencingToken,
    ) -> Result<bool, Self::Error> {
        let mut transaction = self.context.begin_principal(scope).await?;
        let result = execute(
            &mut transaction,
            sqlx::query(
                "UPDATE tool_executions
                    SET status='Unknown',
                        version=version+1,
                        lease_owner=NULL,
                        lease_until=NULL,
                        heartbeat_at=NULL,
                        reconciliation_state='pending'
                  WHERE tenant_id=$1::uuid AND id=$2::uuid AND status='Executing'
                    AND lease_until <= clock_timestamp()
                    AND version=$3 AND fencing_token=$4",
            )
            .bind(scope.tenant_id().to_string())
            .bind(execution_id.to_string())
            .bind(to_i64(expected_version, "expected version")?)
            .bind(to_i64(
                expected_fencing_token.value(),
                "expected fencing token",
            )?),
        )
        .await?;
        transaction.commit().await?;
        Ok(result.rows_affected() == 1)
    }

    async fn complete_tool_execution(
        &self,
        scope: &Scope,
        completion: ToolExecutionCompletion,
    ) -> Result<bool, Self::Error> {
        let ToolExecutionCompletion {
            execution_id,
            owner,
            expected_version,
            expected_fencing_token,
            next,
            output,
            output_ttl_seconds,
        } = completion;
        validate_owner(&owner)?;
        let next_status = validate_next_execution(
            scope,
            execution_id,
            expected_version,
            expected_fencing_token,
            &next,
        )?;
        let ttl_seconds = match (&output, output_ttl_seconds) {
            (Some(_), Some(ttl)) if ttl > 0 => f64::from(ttl),
            (None, None) => 0.0,
            (Some(_), _) => {
                return Err(ToolExecutionStoreError::InvalidRequestedValue(
                    "output requires a positive output TTL",
                ));
            }
            (None, Some(_)) => {
                return Err(ToolExecutionStoreError::InvalidRequestedValue(
                    "output TTL cannot be set without output",
                ));
            }
        };
        let mut transaction = self.context.begin_principal(scope).await?;
        let result = execute(
            &mut transaction,
            sqlx::query(
                "UPDATE tool_executions
                    SET status=$1,
                        version=version+1,
                        lease_owner=NULL,
                        lease_until=NULL,
                        heartbeat_at=NULL,
                        finished_at=clock_timestamp(),
                        output=$2,
                        output_expires_at=CASE
                            WHEN $2::jsonb IS NULL THEN NULL
                            ELSE clock_timestamp()+make_interval(secs => $3::double precision)
                        END
                  WHERE tenant_id=$4::uuid AND id=$5::uuid AND status='Executing'
                    AND lease_owner=$6 AND lease_until > clock_timestamp()
                    AND version=$7 AND fencing_token=$8",
            )
            .bind(next_status)
            .bind(output)
            .bind(ttl_seconds)
            .bind(scope.tenant_id().to_string())
            .bind(execution_id.to_string())
            .bind(owner)
            .bind(to_i64(expected_version, "expected version")?)
            .bind(to_i64(
                expected_fencing_token.value(),
                "expected fencing token",
            )?),
        )
        .await?;
        transaction.commit().await?;
        Ok(result.rows_affected() == 1)
    }
}

impl ToolExecutionReconciliationStore for PgToolExecutionStore {
    async fn apply_tool_execution_reconciliation(
        &self,
        scope: &Scope,
        completion: ToolExecutionReconciliationCompletion,
    ) -> Result<bool, Self::Error> {
        let ToolExecutionReconciliationCompletion {
            request,
            next,
            outcome,
        } = completion;
        let execution_id = request.execution_id();
        let expected_version = request.expected_version();
        let expected_fencing_token = request.expected_fencing_token();
        if next.invocation_id() != request.invocation_id()
            || next.command_id() != request.command_id()
        {
            return Err(ToolExecutionStoreError::InvalidRequestedValue(
                "reconciliation request does not identify the same command",
            ));
        }
        let next_status = validate_next_execution(
            scope,
            execution_id,
            expected_version,
            expected_fencing_token,
            &next,
        )?;

        let (reconciliation_state, output, ttl_seconds) = match outcome {
            ToolExecutionReconciliationOutcome::ConfirmedSucceeded {
                output,
                output_ttl_seconds,
            } => {
                let ttl = match (&output, output_ttl_seconds) {
                    (Some(_), Some(ttl)) if ttl > 0 => f64::from(ttl),
                    (None, None) => 0.0,
                    (Some(_), _) => {
                        return Err(ToolExecutionStoreError::InvalidRequestedValue(
                            "confirmed success output requires a positive retention TTL",
                        ));
                    }
                    (None, Some(_)) => {
                        return Err(ToolExecutionStoreError::InvalidRequestedValue(
                            "output TTL cannot be set without output",
                        ));
                    }
                };
                ("confirmed_succeeded", output, ttl)
            }
            ToolExecutionReconciliationOutcome::ConfirmedFailed { evidence } => {
                ("confirmed_failed", evidence, 0.0)
            }
            ToolExecutionReconciliationOutcome::Unresolved => {
                return Err(ToolExecutionStoreError::InvalidRequestedValue(
                    "unresolved reconciliation cannot be persisted as terminal",
                ));
            }
        };

        let expected_status = if reconciliation_state == "confirmed_succeeded" {
            "Succeeded"
        } else {
            "Failed"
        };
        if next_status != expected_status {
            return Err(ToolExecutionStoreError::InvalidRequestedValue(
                "reconciliation evidence does not match the requested terminal state",
            ));
        }

        let mut transaction = self.context.begin_principal(scope).await?;
        let result = execute(
            &mut transaction,
            sqlx::query(
                "UPDATE tool_executions
                    SET status=$1,
                        version=version+1,
                        reconciliation_state=$2,
                        finished_at=clock_timestamp(),
                        output=$3,
                        output_expires_at=CASE
                            WHEN $3::jsonb IS NULL OR $4::double precision = 0 THEN NULL
                            ELSE clock_timestamp()+make_interval(secs => $4::double precision)
                        END
                  WHERE tenant_id=$5::uuid AND id=$6::uuid
                    AND status='Unknown' AND reconciliation_state='pending'
                    AND version=$7 AND fencing_token=$8
                    AND invocation_id=$9::uuid AND command_id=$10::uuid
                ",
            )
            .bind(next_status)
            .bind(reconciliation_state)
            .bind(output)
            .bind(ttl_seconds)
            .bind(scope.tenant_id().to_string())
            .bind(execution_id.to_string())
            .bind(to_i64(expected_version, "expected version")?)
            .bind(to_i64(
                expected_fencing_token.value(),
                "expected fencing token",
            )?)
            .bind(request.invocation_id().to_string())
            .bind(request.command_id().to_string()),
        )
        .await?;
        transaction.commit().await?;
        Ok(result.rows_affected() == 1)
    }
}

fn validate_next_execution(
    scope: &Scope,
    execution_id: ToolExecutionId,
    expected_version: u64,
    expected_fencing_token: ToolExecutionFencingToken,
    next: &ToolExecution,
) -> Result<&'static str, ToolExecutionStoreError> {
    if next.id() != execution_id || next.tenant_id() != scope.tenant_id() {
        return Err(ToolExecutionStoreError::InvalidRequestedValue(
            "next execution identity differs from the requested tenant execution",
        ));
    }
    if next.version()
        != expected_version
            .checked_add(1)
            .ok_or(ToolExecutionStoreError::InvalidRequestedValue(
                "expected version overflow",
            ))?
    {
        return Err(ToolExecutionStoreError::InvalidRequestedValue(
            "next ToolExecution version must advance exactly once",
        ));
    }
    if next.fencing_token() != expected_fencing_token {
        return Err(ToolExecutionStoreError::InvalidRequestedValue(
            "ToolExecution fencing token cannot change when committing a result",
        ));
    }
    if next.lease_owner().is_some() || !next.status().is_terminal() {
        return Err(ToolExecutionStoreError::InvalidRequestedValue(
            "next ToolExecution must be terminal and release its lease",
        ));
    }

    match next.status() {
        ToolExecutionStatus::Succeeded => Ok("Succeeded"),
        ToolExecutionStatus::Failed => Ok("Failed"),
        _ => Err(ToolExecutionStoreError::InvalidRequestedValue(
            "unsupported ToolExecution result state",
        )),
    }
}

fn validate_lease_request(owner: &str, lease_seconds: u32) -> Result<(), ToolExecutionStoreError> {
    validate_owner(owner)?;
    if lease_seconds == 0 {
        return Err(ToolExecutionStoreError::InvalidRequestedValue(
            "lease duration must be positive",
        ));
    }
    Ok(())
}

fn validate_owner(owner: &str) -> Result<(), ToolExecutionStoreError> {
    if owner.trim().is_empty() {
        Err(ToolExecutionStoreError::InvalidRequestedValue(
            "lease owner must be non-empty",
        ))
    } else {
        Ok(())
    }
}

fn to_i64(value: u64, field: &'static str) -> Result<i64, ToolExecutionStoreError> {
    i64::try_from(value).map_err(|_| ToolExecutionStoreError::InvalidRequestedValue(field))
}

fn reconstitute_tool_execution(
    row: ToolExecutionRow,
) -> Result<ToolExecution, ToolExecutionStoreError> {
    let id = parse_id::<ToolExecutionId>(&row.0, "id")?;
    let tenant_id = parse_id::<TenantId>(&row.1, "tenant_id")?;
    let object_id = parse_id::<ObjectId>(&row.2, "object_id")?;
    let invocation_id = parse_id::<InvocationId>(&row.3, "invocation_id")?;
    let command_id = parse_id::<CommandId>(&row.4, "command_id")?;
    let status = parse_status(&row.5)?;
    let version = u64::try_from(row.6)
        .map_err(|_| ToolExecutionStoreError::InvalidPersistedValue("version"))?;
    let token = u64::try_from(row.7)
        .map_err(|_| ToolExecutionStoreError::InvalidPersistedValue("fencing_token"))?;
    let arguments_hash: [u8; 32] = row.8.try_into().map_err(|_| {
        ToolExecutionStoreError::InvalidPersistedValue("arguments_hash")
    })?;

    Ok(ToolExecution::from_persisted(
        id,
        tenant_id,
        object_id,
        invocation_id,
        command_id,
        arguments_hash,
        ToolExecutionCheckpoint::new(
            status,
            version,
            ToolExecutionFencingToken::from_value(token),
            row.9,
        ),
    ))
}

fn parse_id<T: FromStr>(value: &str, field: &'static str) -> Result<T, ToolExecutionStoreError> {
    value
        .parse()
        .map_err(|_| ToolExecutionStoreError::InvalidPersistedValue(field))
}

fn parse_status(value: &str) -> Result<ToolExecutionStatus, ToolExecutionStoreError> {
    match value {
        "Authorized" => Ok(ToolExecutionStatus::Authorized),
        "Executing" => Ok(ToolExecutionStatus::Executing),
        "Succeeded" => Ok(ToolExecutionStatus::Succeeded),
        "Failed" => Ok(ToolExecutionStatus::Failed),
        "Unknown" => Ok(ToolExecutionStatus::Unknown),
        _ => Err(ToolExecutionStoreError::InvalidPersistedValue("status")),
    }
}
