//! Atomic PostgreSQL commit of a complete Knowledge revision.

use std::{collections::HashSet, error::Error, fmt};

use qembed_domain::{
    ids::{KnowledgeDocumentId, ObjectId, TenantId},
    knowledge::RevisionNumber,
    scope::Scope,
};
use sqlx::types::Uuid;

use crate::{PgContext, execute, fetch_all};

/// Result of committing the indexed Knowledge documents for one Object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnowledgeRevisionCommit {
    tenant_id: TenantId,
    object_id: ObjectId,
    previous_revision: RevisionNumber,
    revision: RevisionNumber,
    published_documents: Vec<KnowledgeDocumentId>,
    retired_documents: Vec<KnowledgeDocumentId>,
}

impl KnowledgeRevisionCommit {
    #[must_use]
    pub const fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }

    #[must_use]
    pub const fn object_id(&self) -> ObjectId {
        self.object_id
    }

    #[must_use]
    pub const fn previous_revision(&self) -> RevisionNumber {
        self.previous_revision
    }

    #[must_use]
    pub const fn revision(&self) -> RevisionNumber {
        self.revision
    }

    #[must_use]
    pub fn published_documents(&self) -> &[KnowledgeDocumentId] {
        &self.published_documents
    }

    #[must_use]
    pub fn retired_documents(&self) -> &[KnowledgeDocumentId] {
        &self.retired_documents
    }
}

/// Failure while committing an Object's next Knowledge revision.
#[derive(Debug)]
pub enum KnowledgeRevisionCommitError {
    Context(crate::ContextTransactionError),
    Database(sqlx::Error),
    NoActiveEmbeddingModels,
    NoIndexedDocuments,
    DuplicateDocumentKey,
    IncompleteIndexing,
    RevisionOutOfRange,
    RevisionOverflow,
    RevisionConflict,
}

impl fmt::Display for KnowledgeRevisionCommitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Context(error) => write!(formatter, "knowledge commit context failed: {error}"),
            Self::Database(error) => write!(formatter, "knowledge commit database error: {error}"),
            Self::NoActiveEmbeddingModels => {
                formatter.write_str("knowledge commit requires at least one active embedding model")
            }
            Self::NoIndexedDocuments => {
                formatter.write_str("knowledge commit has no indexed documents to publish")
            }
            Self::DuplicateDocumentKey => {
                formatter.write_str("multiple indexed documents have the same document key")
            }
            Self::IncompleteIndexing => formatter
                .write_str("one or more indexed documents lack chunks or required embeddings"),
            Self::RevisionOutOfRange => {
                formatter.write_str("persisted knowledge revision is outside the domain range")
            }
            Self::RevisionOverflow => formatter.write_str("knowledge revision number overflow"),
            Self::RevisionConflict => {
                formatter.write_str("knowledge revision changed while the commit was being applied")
            }
        }
    }
}

impl Error for KnowledgeRevisionCommitError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Context(error) => Some(error),
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

impl From<crate::ContextTransactionError> for KnowledgeRevisionCommitError {
    fn from(error: crate::ContextTransactionError) -> Self {
        Self::Context(error)
    }
}

impl From<sqlx::Error> for KnowledgeRevisionCommitError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

/// Tenant-scoped Knowledge persistence adapter.
#[derive(Clone)]
pub struct PgKnowledgeRepository {
    context: PgContext,
}

impl PgKnowledgeRepository {
    #[must_use]
    pub fn new(context: PgContext) -> Self {
        Self { context }
    }

    /// Publishes every fully indexed document for this Object as one revision.
    ///
    /// Documents with a matching live `doc_key` are retired first. A document
    /// is eligible only when it has at least one chunk and every chunk has an
    /// embedding for every model that is active at commit time. The model table
    /// is locked in SHARE mode so its active set cannot change mid-commit.
    ///
    /// The revision row is the serialization point. The interval updates,
    /// revision log, event and outbox delivery all commit or roll back together.
    pub async fn commit_knowledge_revision(
        &self,
        scope: &Scope,
        object_id: ObjectId,
    ) -> Result<KnowledgeRevisionCommit, KnowledgeRevisionCommitError> {
        let mut transaction = self.context.begin_principal(scope).await?;

        let current_revision: i64 = sqlx::query_scalar(
            "SELECT current_revision FROM knowledge_revisions \
             WHERE tenant_id = $1::uuid AND object_id = $2::uuid FOR UPDATE",
        )
        .bind(scope.tenant_id().to_string())
        .bind(object_id.to_string())
        .fetch_one(transaction.connection())
        .await?;

        let current_value = u64::try_from(current_revision)
            .map_err(|_| KnowledgeRevisionCommitError::RevisionOutOfRange)?;
        let previous_revision = RevisionNumber::new(current_value);
        let revision = previous_revision
            .next()
            .map_err(|_| KnowledgeRevisionCommitError::RevisionOverflow)?;
        let revision_sql = i64::try_from(revision.value())
            .map_err(|_| KnowledgeRevisionCommitError::RevisionOverflow)?;

        // RUST-068 owns model lifecycle operations. SHARE prevents concurrent
        // INSERT/UPDATE/DELETE from changing the required active set mid-commit.
        sqlx::query("LOCK TABLE embedding_models IN SHARE MODE")
            .execute(transaction.connection())
            .await?;

        let active_models: Vec<String> =
            sqlx::query_scalar("SELECT id FROM embedding_models WHERE active ORDER BY id")
                .fetch_all(transaction.connection())
                .await?;
        if active_models.is_empty() {
            return Err(KnowledgeRevisionCommitError::NoActiveEmbeddingModels);
        }

        let document_rows: Vec<(Uuid, String, String)> = fetch_all(
            &mut transaction,
            sqlx::query_as(
                "SELECT id, doc_key, status FROM knowledge_documents \
                 WHERE tenant_id = $1::uuid AND object_id = $2::uuid \
                   AND status IN ('indexed', 'live') \
                 ORDER BY id FOR UPDATE",
            )
            .bind(scope.tenant_id().to_string())
            .bind(object_id.to_string()),
        )
        .await?;

        let mut indexed_documents = Vec::new();
        let mut indexed_ids = Vec::new();
        let mut document_keys = Vec::new();
        let mut unique_keys = HashSet::new();
        for (id, doc_key, status) in &document_rows {
            if status == "indexed" {
                if !unique_keys.insert(doc_key.clone()) {
                    return Err(KnowledgeRevisionCommitError::DuplicateDocumentKey);
                }
                let id = KnowledgeDocumentId::from_uuid(*id);
                indexed_ids.push(id.into_uuid());
                indexed_documents.push(id);
                document_keys.push(doc_key.clone());
            }
        }
        if indexed_documents.is_empty() {
            return Err(KnowledgeRevisionCommitError::NoIndexedDocuments);
        }

        // Lock all chunks before checking embeddings. Their FK key-share locks
        // prevent a concurrent embedding insert from racing past this check.
        let _locked_chunks: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM knowledge_chunks \
             WHERE tenant_id = $1::uuid AND object_id = $2::uuid \
               AND document_id = ANY($3::uuid[]) \
             ORDER BY id FOR UPDATE",
        )
        .bind(scope.tenant_id().to_string())
        .bind(object_id.to_string())
        .bind(&indexed_ids)
        .fetch_all(transaction.connection())
        .await?;

        // Lock existing embedding rows as well: a concurrent model adapter must
        // not remove or replace evidence after completeness has been checked.
        let _locked_embeddings: Vec<Uuid> = sqlx::query_scalar(
            "SELECT chunk_id FROM knowledge_chunk_embeddings \
             WHERE tenant_id = $1::uuid AND object_id = $2::uuid \
               AND chunk_id = ANY($3::uuid[]) \
             ORDER BY chunk_id, model_id FOR SHARE",
        )
        .bind(scope.tenant_id().to_string())
        .bind(object_id.to_string())
        .bind(&indexed_ids)
        .fetch_all(transaction.connection())
        .await?;

        let incomplete: bool = sqlx::query_scalar(
            "SELECT EXISTS ( \
                SELECT 1 FROM knowledge_documents d \
                 WHERE d.tenant_id = $1::uuid AND d.object_id = $2::uuid \
                   AND d.status = 'indexed' AND d.id = ANY($3::uuid[]) \
                   AND ( \
                     NOT EXISTS ( \
                       SELECT 1 FROM knowledge_chunks c \
                        WHERE c.tenant_id = d.tenant_id AND c.object_id = d.object_id \
                          AND c.document_id = d.id \
                     ) \
                     OR EXISTS ( \
                       SELECT 1 FROM knowledge_chunks c \
                        WHERE c.tenant_id = d.tenant_id AND c.object_id = d.object_id \
                          AND c.document_id = d.id \
                          AND ( \
                            SELECT count(DISTINCT e.model_id) \
                              FROM knowledge_chunk_embeddings e \
                             WHERE e.tenant_id = c.tenant_id AND e.object_id = c.object_id \
                               AND e.chunk_id = c.id AND e.model_id = ANY($4::text[]) \
                          ) <> cardinality($4::text[]) \
                     ) \
                   ) \
             )",
        )
        .bind(scope.tenant_id().to_string())
        .bind(object_id.to_string())
        .bind(&indexed_ids)
        .bind(&active_models)
        .fetch_one(transaction.connection())
        .await?;
        if incomplete {
            return Err(KnowledgeRevisionCommitError::IncompleteIndexing);
        }

        // Close prior live intervals before opening replacements: the partial
        // unique index permits only one open interval for a document key.
        let retired_rows: Vec<Uuid> = sqlx::query_scalar(
            "UPDATE knowledge_documents SET status = 'retired', valid_to_revision = $4 \
             WHERE tenant_id = $1::uuid AND object_id = $2::uuid \
               AND status = 'live' AND doc_key = ANY($3::text[]) \
             RETURNING id",
        )
        .bind(scope.tenant_id().to_string())
        .bind(object_id.to_string())
        .bind(&document_keys)
        .bind(revision_sql)
        .fetch_all(transaction.connection())
        .await?;
        let retired_documents = retired_rows
            .into_iter()
            .map(KnowledgeDocumentId::from_uuid)
            .collect::<Vec<_>>();

        let published_rows: Vec<Uuid> = sqlx::query_scalar(
            "UPDATE knowledge_documents SET status = 'live', valid_from_revision = $4 \
             WHERE tenant_id = $1::uuid AND object_id = $2::uuid \
               AND status = 'indexed' AND id = ANY($3::uuid[]) \
             RETURNING id",
        )
        .bind(scope.tenant_id().to_string())
        .bind(object_id.to_string())
        .bind(&indexed_ids)
        .bind(revision_sql)
        .fetch_all(transaction.connection())
        .await?;
        if published_rows.len() != indexed_documents.len() {
            return Err(KnowledgeRevisionCommitError::RevisionConflict);
        }

        let update = execute(
            &mut transaction,
            sqlx::query(
                "UPDATE knowledge_revisions SET current_revision = $3 \
                 WHERE tenant_id = $1::uuid AND object_id = $2::uuid AND current_revision = $4",
            )
            .bind(scope.tenant_id().to_string())
            .bind(object_id.to_string())
            .bind(revision_sql)
            .bind(current_revision),
        )
        .await?;
        if update.rows_affected() != 1 {
            return Err(KnowledgeRevisionCommitError::RevisionConflict);
        }

        execute(
            &mut transaction,
            sqlx::query(
                "INSERT INTO knowledge_revision_log (tenant_id, object_id, revision) \
                 VALUES ($1::uuid, $2::uuid, $3)",
            )
            .bind(scope.tenant_id().to_string())
            .bind(object_id.to_string())
            .bind(revision_sql),
        )
        .await?;

        let event_id = qembed_domain::ids::EventId::new();
        let payload = serde_json::json!({
            "type": "revision_committed",
            "object_id": object_id,
            "previous_revision": previous_revision.value(),
            "revision": revision.value(),
            "published_document_count": indexed_documents.len(),
            "retired_document_count": retired_documents.len()
        });
        let payload = serde_json::to_string(&payload)
            .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;

        execute(
            &mut transaction,
            sqlx::query(
                "INSERT INTO events (tenant_id, event_id, aggregate_type, aggregate_id, event_type, payload) \
                 VALUES ($1::uuid, $2::uuid, 'knowledge', $3::uuid, 'knowledge.revision_committed', $4::jsonb)",
            )
            .bind(scope.tenant_id().to_string())
            .bind(event_id.to_string())
            .bind(object_id.to_string())
            .bind(payload),
        )
        .await?;

        execute(
            &mut transaction,
            sqlx::query(
                "INSERT INTO outbox_messages (tenant_id, delivery_id, event_id, topic, payload) \
                 SELECT tenant_id, event_id, event_id, event_type, payload \
                   FROM events WHERE tenant_id = $1::uuid AND event_id = $2::uuid",
            )
            .bind(scope.tenant_id().to_string())
            .bind(event_id.to_string()),
        )
        .await?;

        transaction.commit().await?;

        Ok(KnowledgeRevisionCommit {
            tenant_id: scope.tenant_id(),
            object_id,
            previous_revision,
            revision,
            published_documents: indexed_documents,
            retired_documents,
        })
    }
}
