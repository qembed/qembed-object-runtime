//! Tenant-scoped garbage collection for retired Knowledge documents.

use std::{error::Error, fmt};

use qembed_domain::{ids::ObjectId, scope::Scope};

use crate::PgContext;

/// Errors returned by the PostgreSQL Knowledge garbage collector.
#[derive(Debug)]
pub enum PgKnowledgeGcError {
    Context(crate::ContextTransactionError),
    Database(sqlx::Error),
    InvalidBatchSize,
    InvalidDeletedCount,
}

impl fmt::Display for PgKnowledgeGcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Context(error) => write!(formatter, "Knowledge GC context failed: {error}"),
            Self::Database(error) => write!(formatter, "Knowledge GC database error: {error}"),
            Self::InvalidBatchSize => {
                formatter.write_str("Knowledge GC batch size must be between 1 and 500")
            }
            Self::InvalidDeletedCount => {
                formatter.write_str("Knowledge GC returned an invalid deleted-row count")
            }
        }
    }
}

impl Error for PgKnowledgeGcError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Context(error) => Some(error),
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

impl From<crate::ContextTransactionError> for PgKnowledgeGcError {
    fn from(error: crate::ContextTransactionError) -> Self {
        Self::Context(error)
    }
}

impl From<sqlx::Error> for PgKnowledgeGcError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

/// PostgreSQL-backed, tenant-scoped collector for retired Knowledge documents.
///
/// The database function serializes collection with revision commits and pin
/// creation. A document is eligible only when no active invocation can observe
/// its half-open revision interval.
#[derive(Clone)]
pub struct PgKnowledgeGarbageCollector {
    context: PgContext,
}

impl PgKnowledgeGarbageCollector {
    #[must_use]
    pub fn new(context: PgContext) -> Self {
        Self { context }
    }

    /// Deletes up to batch_size retired documents no active pin can observe.
    ///
    /// The database cascades deletion to chunks and their embeddings. The
    /// immutable Knowledge revision log is intentionally retained.
    pub async fn collect_retired_documents(
        &self,
        scope: &Scope,
        object_id: ObjectId,
        batch_size: u16,
    ) -> Result<u64, PgKnowledgeGcError> {
        if !(1..=500).contains(&batch_size) {
            return Err(PgKnowledgeGcError::InvalidBatchSize);
        }

        let mut transaction = self.context.begin_principal(scope).await?;
        let deleted: i32 = sqlx::query_scalar(
            "SELECT collect_retired_knowledge_documents($1::uuid, $2::uuid, $3)",
        )
        .bind(scope.tenant_id().to_string())
        .bind(object_id.to_string())
        .bind(i32::from(batch_size))
        .fetch_one(transaction.connection())
        .await?;
        transaction.commit().await?;

        u64::try_from(deleted).map_err(|_| PgKnowledgeGcError::InvalidDeletedCount)
    }
}
