//! Revision-pinned candidate filtering for Knowledge retrieval.
//!
//! This adapter deliberately returns the complete visible candidate set. The
//! next retrieval layer must materialize the visible candidates together with
//! their model embeddings, then rank and apply top-k over that materialized set.
//! This prevents a vector index scan from spending top-k slots on invisible rows.

use std::{error::Error, fmt};

use qembed_domain::{
    ids::{KnowledgeChunkId, KnowledgeDocumentId},
    knowledge::PinnedKnowledgeRevision,
    scope::Scope,
};
use sqlx::{Row, types::Uuid};

use crate::PgContext;

/// A chunk that is visible at one pinned Knowledge revision.
#[derive(Clone, Debug, PartialEq)]
pub struct VisibleKnowledgeChunk {
    id: KnowledgeChunkId,
    document_id: KnowledgeDocumentId,
    content: String,
    position: u32,
    metadata: serde_json::Value,
}

impl VisibleKnowledgeChunk {
    #[must_use]
    pub const fn id(&self) -> KnowledgeChunkId {
        self.id
    }

    #[must_use]
    pub const fn document_id(&self) -> KnowledgeDocumentId {
        self.document_id
    }

    #[must_use]
    pub fn content(&self) -> &str {
        &self.content
    }

    #[must_use]
    pub const fn position(&self) -> u32 {
        self.position
    }

    #[must_use]
    pub fn metadata(&self) -> &serde_json::Value {
        &self.metadata
    }
}

/// Failure while loading revision-visible retrieval candidates.
#[derive(Debug)]
pub enum PgKnowledgeRetrievalError {
    Context(crate::ContextTransactionError),
    Database(sqlx::Error),
    PinnedTenantMismatch,
    ObjectNotFound,
    FutureRevision,
    RevisionOutOfRange,
    InvalidPersistedChunkPosition,
}

impl fmt::Display for PgKnowledgeRetrievalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Context(error) => {
                write!(formatter, "knowledge retrieval context failed: {error}")
            }
            Self::Database(error) => {
                write!(formatter, "knowledge retrieval database error: {error}")
            }
            Self::PinnedTenantMismatch => {
                formatter.write_str("pinned Knowledge revision belongs to another tenant")
            }
            Self::ObjectNotFound => formatter.write_str("Knowledge revision object was not found"),
            Self::FutureRevision => formatter
                .write_str("pinned Knowledge revision is newer than the committed revision"),
            Self::RevisionOutOfRange => {
                formatter.write_str("pinned Knowledge revision is outside the database range")
            }
            Self::InvalidPersistedChunkPosition => {
                formatter.write_str("persisted chunk position is negative")
            }
        }
    }
}

impl Error for PgKnowledgeRetrievalError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Context(error) => Some(error),
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

impl From<crate::ContextTransactionError> for PgKnowledgeRetrievalError {
    fn from(error: crate::ContextTransactionError) -> Self {
        Self::Context(error)
    }
}

impl From<sqlx::Error> for PgKnowledgeRetrievalError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

/// Tenant-scoped adapter for loading the candidate set of a pinned revision.
#[derive(Clone)]
pub struct PgKnowledgeRetrievalStore {
    context: PgContext,
}

impl PgKnowledgeRetrievalStore {
    #[must_use]
    pub fn new(context: PgContext) -> Self {
        Self { context }
    }

    /// Loads all non-erased chunks visible at the supplied committed revision.
    ///
    /// The pinned tenant must match the trusted Scope. A future revision is
    /// rejected rather than silently treated as the current revision. Revision
    /// zero is valid and returns an empty candidate set.
    ///
    /// This method intentionally has no top-k argument: callers must apply
    /// ranking and LIMIT only to the filtered candidate set.
    pub async fn visible_chunks(
        &self,
        scope: &Scope,
        pinned: PinnedKnowledgeRevision,
    ) -> Result<Vec<VisibleKnowledgeChunk>, PgKnowledgeRetrievalError> {
        if pinned.tenant_id() != scope.tenant_id() {
            return Err(PgKnowledgeRetrievalError::PinnedTenantMismatch);
        }

        let revision = i64::try_from(pinned.revision().value())
            .map_err(|_| PgKnowledgeRetrievalError::RevisionOutOfRange)?;

        let mut transaction = self.context.begin_principal(scope).await?;
        let current_revision: Option<i64> = sqlx::query_scalar(
            "SELECT current_revision FROM knowledge_revisions \
             WHERE tenant_id = $1::uuid AND object_id = $2::uuid",
        )
        .bind(scope.tenant_id().to_string())
        .bind(pinned.object_id().to_string())
        .fetch_optional(transaction.connection())
        .await?;

        let Some(current_revision) = current_revision else {
            return Err(PgKnowledgeRetrievalError::ObjectNotFound);
        };
        if revision > current_revision {
            return Err(PgKnowledgeRetrievalError::FutureRevision);
        }

        let rows = sqlx::query(
            "SELECT id, document_id, content, position, metadata \
               FROM knowledge_visible_chunks_at($1::uuid, $2::uuid, $3) \
              ORDER BY document_id, position, id",
        )
        .bind(scope.tenant_id().to_string())
        .bind(pinned.object_id().to_string())
        .bind(revision)
        .fetch_all(transaction.connection())
        .await?;

        let mut chunks = Vec::with_capacity(rows.len());
        for row in rows {
            let position: i32 = row.try_get("chunk_position")?;
            let position = u32::try_from(position)
                .map_err(|_| PgKnowledgeRetrievalError::InvalidPersistedChunkPosition)?;
            chunks.push(VisibleKnowledgeChunk {
                id: KnowledgeChunkId::from_uuid(row.try_get::<Uuid, _>("id")?),
                document_id: KnowledgeDocumentId::from_uuid(row.try_get::<Uuid, _>("document_id")?),
                content: row.try_get("content")?,
                position,
                metadata: row.try_get("metadata")?,
            });
        }

        transaction.commit().await?;
        Ok(chunks)
    }
}
