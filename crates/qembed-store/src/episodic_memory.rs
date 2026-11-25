//! Caller-isolated episodic memory reads through the signed application context.

use std::{error::Error, fmt};

use qembed_domain::{
    ids::{CallerId, ObjectId},
    scope::Scope,
};
use sqlx::types::Uuid;

use crate::{PgContext, caller::ResolvedCaller};

/// A persisted episodic memory visible to the currently resolved Caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EpisodicMemory {
    id: Uuid,
    claim: String,
    source_kind: String,
    verification: String,
}

impl EpisodicMemory {
    #[must_use]
    pub const fn id(&self) -> Uuid {
        self.id
    }

    #[must_use]
    pub fn claim(&self) -> &str {
        &self.claim
    }

    #[must_use]
    pub fn source_kind(&self) -> &str {
        &self.source_kind
    }

    #[must_use]
    pub fn verification(&self) -> &str {
        &self.verification
    }
}

/// PostgreSQL adapter for Caller-scoped episodic memory reads.
#[derive(Clone)]
pub struct PgEpisodicMemoryRepository {
    context: PgContext,
}

impl PgEpisodicMemoryRepository {
    #[must_use]
    pub fn new(context: PgContext) -> Self {
        Self { context }
    }

    /// Lists only unexpired episodic memories belonging to the resolved Caller.
    ///
    /// The selected Caller is validated against the authenticated Principal's
    /// active delegation before SQL executes. The explicit Caller predicate is
    /// defense in depth; PostgreSQL RLS independently applies the signed
    /// transaction's Caller context to memories and dependent rows.
    pub async fn list_episodic(
        &self,
        scope: &Scope,
        requested_caller_id: CallerId,
        object_id: ObjectId,
    ) -> Result<Vec<EpisodicMemory>, EpisodicMemoryError> {
        let (mut transaction, caller) = self
            .context
            .begin_caller(scope, requested_caller_id)
            .await
            .map_err(EpisodicMemoryError::Context)?;
        ensure_caller_binding(&transaction, &caller)?;

        let rows: Vec<(Uuid, String, String, String)> = sqlx::query_as(
            "SELECT id, claim, source_kind::text, verification::text \
               FROM memories \
              WHERE tenant_id = $1::uuid AND object_id = $2::uuid \
                AND caller_id = $3::uuid AND memory_kind = 'episodic' \
                AND expires_at > clock_timestamp() \
              ORDER BY id",
        )
        .bind(scope.tenant_id().to_string())
        .bind(object_id.to_string())
        .bind(caller.caller_id().to_string())
        .fetch_all(transaction.connection())
        .await
        .map_err(EpisodicMemoryError::Database)?;

        transaction
            .commit()
            .await
            .map_err(EpisodicMemoryError::Database)?;

        Ok(rows
            .into_iter()
            .map(|(id, claim, source_kind, verification)| EpisodicMemory {
                id,
                claim,
                source_kind,
                verification,
            })
            .collect())
    }
}

fn ensure_caller_binding(
    transaction: &crate::ContextTransaction,
    caller: &ResolvedCaller,
) -> Result<(), EpisodicMemoryError> {
    if caller.is_bound_to_transaction(transaction) {
        Ok(())
    } else {
        Err(EpisodicMemoryError::CallerContextMismatch)
    }
}

/// Failure while reading episodic memory.
#[derive(Debug)]
pub enum EpisodicMemoryError {
    Context(crate::ContextTransactionError),
    Database(sqlx::Error),
    CallerContextMismatch,
}

impl fmt::Display for EpisodicMemoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Context(error) => write!(formatter, "episodic memory context failed: {error}"),
            Self::Database(error) => write!(formatter, "episodic memory database error: {error}"),
            Self::CallerContextMismatch => {
                formatter.write_str("resolved Caller is not bound to this transaction")
            }
        }
    }
}

impl Error for EpisodicMemoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Context(error) => Some(error),
            Self::Database(error) => Some(error),
            Self::CallerContextMismatch => None,
        }
    }
}
