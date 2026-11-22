//! Tenant-scoped, read-only invocation status projection.

use std::fmt;

use qembed_domain::ids::InvocationId;
use qembed_domain::invocation::InvocationStatus;
use qembed_domain::invocation_read::InvocationReadModel;
use qembed_domain::ports::InvocationReadStore;
use qembed_domain::scope::Scope;
use crate::{PgContext, fetch_optional};

#[derive(Clone)]
pub struct PgInvocationQueryStore {
    context: PgContext,
}

impl PgInvocationQueryStore {
    #[must_use]
    pub fn new(context: PgContext) -> Self {
        Self { context }
    }
}

#[derive(Debug)]
pub enum InvocationQueryError {
    Context(crate::ContextTransactionError),
    Database(sqlx::Error),
    InvalidPersistedValue(&'static str),
}

impl fmt::Display for InvocationQueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Context(error) => write!(formatter, "invocation query signed-context initialization failed: {error}"),
            Self::Database(error) => write!(formatter, "invocation query failed: {error}"),
            Self::InvalidPersistedValue(field) => {
                write!(formatter, "persisted invocation has invalid {field}")
            }
        }
    }
}

impl std::error::Error for InvocationQueryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Context(error) => Some(error),
            Self::Database(error) => Some(error),
            Self::InvalidPersistedValue(_) => None,
        }
    }
}

impl From<sqlx::Error> for InvocationQueryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl InvocationReadStore for PgInvocationQueryStore {
    type Error = InvocationQueryError;

    async fn get_invocation(
        &self,
        scope: &Scope,
        invocation_id: InvocationId,
    ) -> Result<Option<InvocationReadModel>, Self::Error> {
        let mut transaction = self.context.begin_principal(scope).await.map_err(InvocationQueryError::Context)?;
        let row = fetch_optional(
            &mut transaction,
            sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
                "SELECT invocation.status, invocation.error_code, CASE WHEN invocation.status IN ('Completed','CompletedDegraded') THEN (SELECT message.content FROM interaction_messages AS message WHERE message.tenant_id=invocation.tenant_id AND message.object_id=invocation.object_id AND message.invocation_id=invocation.id AND message.role='assistant' ORDER BY message.created_at, message.id LIMIT 1) ELSE NULL END FROM object_invocations AS invocation WHERE invocation.tenant_id=$1::uuid AND invocation.id=$2::uuid",
            )
            .bind(scope.tenant_id().to_string())
            .bind(invocation_id.to_string()),
        )
        .await?;
        transaction.commit().await?;

        row.map(|(status, error_code, result)| {
            let status = parse_status(&status)?;
            match status {
                InvocationStatus::Completed | InvocationStatus::CompletedDegraded => {
                    if result.is_none() || error_code.is_some() {
                        return Err(InvocationQueryError::InvalidPersistedValue(
                            "completed result",
                        ));
                    }
                }
                InvocationStatus::Failed => {
                    if result.is_some()
                        || !matches!(
                            error_code.as_deref(),
                            Some(
                                "provider_timeout"
                                    | "provider_rate_limited"
                                    | "provider_unavailable"
                                    | "provider_malformed_output"
                                    | "provider_other"
                            )
                        )
                    {
                        return Err(InvocationQueryError::InvalidPersistedValue(
                            "failure error code",
                        ));
                    }
                }
                _ => {
                    if result.is_some() || error_code.is_some() {
                        return Err(InvocationQueryError::InvalidPersistedValue(
                            "nonterminal result",
                        ));
                    }
                }
            }
            Ok(InvocationReadModel::new(
                invocation_id,
                status,
                result,
                error_code,
            ))
        })
        .transpose()
    }
}

fn parse_status(value: &str) -> Result<InvocationStatus, InvocationQueryError> {
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
        _ => Err(InvocationQueryError::InvalidPersistedValue("status")),
    }
}
