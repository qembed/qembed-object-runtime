//! Atomic PostgreSQL persistence for staged Knowledge ingestion.
use std::{error::Error, fmt};
use qembed_domain::{
    ids::KnowledgeDocumentId,
    knowledge_ingestion::{
        IngestionChunk, IngestionEmbeddingModel, IngestionPersistenceOutcome,
        KnowledgeIngestionDocumentState, StagedKnowledgeDocument,
    },
    ports::KnowledgeIngestionStore,
    scope::Scope,
};
use crate::PgContext;
#[derive(Debug)]
pub enum PgKnowledgeIngestionError {
    Context(crate::ContextTransactionError),
    Database(sqlx::Error),
    MissingSourceUri,
    InvalidDocumentState,
    ExistingChunksOnStagedDocument,
    ActiveModelSetChanged,
    IncompleteChunkSet,
}
impl fmt::Display for PgKnowledgeIngestionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Context(e) => write!(f, "signed ingestion transaction failed: {e}"),
            Self::Database(e) => write!(f, "knowledge ingestion database error: {e}"),
            Self::MissingSourceUri => f.write_str("staged knowledge document has no source_uri"),
            Self::InvalidDocumentState => {
                f.write_str("knowledge document is not staged or indexed")
            }
            Self::ExistingChunksOnStagedDocument => {
                f.write_str("staged document already has persisted chunks")
            }
            Self::ActiveModelSetChanged => {
                f.write_str("active embedding model set changed during ingestion")
            }
            Self::IncompleteChunkSet => f.write_str(
                "chunk payload does not contain exactly one valid embedding per active model",
            ),
        }
    }
}
impl Error for PgKnowledgeIngestionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Context(e) => Some(e),
            Self::Database(e) => Some(e),
            _ => None,
        }
    }
}
impl From<crate::ContextTransactionError> for PgKnowledgeIngestionError {
    fn from(e: crate::ContextTransactionError) -> Self {
        Self::Context(e)
    }
}
impl From<sqlx::Error> for PgKnowledgeIngestionError {
    fn from(e: sqlx::Error) -> Self {
        Self::Database(e)
    }
}
#[derive(Clone)]
pub struct PgKnowledgeIngestionStore {
    context: PgContext,
}
impl PgKnowledgeIngestionStore {
    #[must_use]
    pub fn new(context: PgContext) -> Self {
        Self { context }
    }
}
impl KnowledgeIngestionStore for PgKnowledgeIngestionStore {
    type Error = PgKnowledgeIngestionError;
    async fn load_knowledge_ingestion_document(
        &self,
        scope: &Scope,
        document_id: KnowledgeDocumentId,
    ) -> Result<Option<KnowledgeIngestionDocumentState>, Self::Error> {
        let mut tx = self.context.begin_principal(scope).await?;
        let row:Option<(String,String,Option<String>)>=sqlx::query_as(
            "SELECT object_id::text,status,source_uri FROM knowledge_documents WHERE tenant_id=$1::uuid AND id=$2::uuid",
        ).bind(scope.tenant_id().to_string()).bind(document_id.to_string()).fetch_optional(tx.connection()).await?;
        let state = match row {
            None => None,
            Some((_, status, _)) if status == "indexed" => {
                Some(KnowledgeIngestionDocumentState::AlreadyIndexed)
            }
            Some((_, status, Some(uri))) if status == "staged" => {
                Some(KnowledgeIngestionDocumentState::Staged(
                    StagedKnowledgeDocument::new(document_id, uri)
                        .map_err(|_| PgKnowledgeIngestionError::MissingSourceUri)?,
                ))
            }
            Some((_, status, None)) if status == "staged" => {
                return Err(PgKnowledgeIngestionError::MissingSourceUri);
            }
            Some(_) => return Err(PgKnowledgeIngestionError::InvalidDocumentState),
        };
        tx.commit().await?;
        Ok(state)
    }
    async fn active_ingestion_embedding_models(
        &self,
        scope: &Scope,
    ) -> Result<Vec<IngestionEmbeddingModel>, Self::Error> {
        let mut tx = self.context.begin_principal(scope).await?;
        let rows: Vec<(String, i32)> =
            sqlx::query_as("SELECT id,dimension FROM embedding_models WHERE active ORDER BY id")
                .fetch_all(tx.connection())
                .await?;
        let models = rows
            .into_iter()
            .map(|(id, dimension)| {
                IngestionEmbeddingModel::new(id, usize::try_from(dimension).unwrap_or(0))
                    .map_err(|_| PgKnowledgeIngestionError::IncompleteChunkSet)
            })
            .collect::<Result<Vec<_>, _>>()?;
        tx.commit().await?;
        Ok(models)
    }
    async fn persist_indexed_knowledge_document(
        &self,
        scope: &Scope,
        document_id: KnowledgeDocumentId,
        expected_models: &[IngestionEmbeddingModel],
        chunks: &[IngestionChunk],
    ) -> Result<IngestionPersistenceOutcome, Self::Error> {
        let mut tx = self.context.begin_principal(scope).await?;
        sqlx::query("LOCK TABLE embedding_models IN SHARE MODE")
            .execute(tx.connection())
            .await?;
        let rows: Vec<(String, i32)> =
            sqlx::query_as("SELECT id,dimension FROM embedding_models WHERE active ORDER BY id")
                .fetch_all(tx.connection())
                .await?;
        let current = rows
            .into_iter()
            .map(|(id, dim)| (id, dim as usize))
            .collect::<Vec<_>>();
        let expected = expected_models
            .iter()
            .map(|m| (m.id().to_owned(), m.dimension()))
            .collect::<Vec<_>>();
        if current != expected {
            return Err(PgKnowledgeIngestionError::ActiveModelSetChanged);
        }
        let row:Option<(String,String)>=sqlx::query_as("SELECT object_id::text,status FROM knowledge_documents WHERE tenant_id=$1::uuid AND id=$2::uuid FOR UPDATE").bind(scope.tenant_id().to_string()).bind(document_id.to_string()).fetch_optional(tx.connection()).await?;
        let Some((object_id, status)) = row else {
            return Err(PgKnowledgeIngestionError::InvalidDocumentState);
        };
        if status == "indexed" {
            tx.commit().await?;
            return Ok(IngestionPersistenceOutcome::AlreadyIndexed);
        }
        if status != "staged" {
            return Err(PgKnowledgeIngestionError::InvalidDocumentState);
        }
        let existing:i64=sqlx::query_scalar("SELECT count(*) FROM knowledge_chunks WHERE tenant_id=$1::uuid AND document_id=$2::uuid").bind(scope.tenant_id().to_string()).bind(document_id.to_string()).fetch_one(tx.connection()).await?;
        if existing != 0 {
            return Err(PgKnowledgeIngestionError::ExistingChunksOnStagedDocument);
        }
        if chunks.is_empty() {
            return Err(PgKnowledgeIngestionError::IncompleteChunkSet);
        }
        for chunk in chunks {
            if chunk.embeddings().len() != expected_models.len() {
                return Err(PgKnowledgeIngestionError::IncompleteChunkSet);
            }
            for model in expected_models {
                let matches = chunk
                    .embeddings()
                    .iter()
                    .filter(|e| e.model_id() == model.id())
                    .collect::<Vec<_>>();
                if matches.len() != 1 || matches[0].values().len() != model.dimension() {
                    return Err(PgKnowledgeIngestionError::IncompleteChunkSet);
                }
            }
            sqlx::query("INSERT INTO knowledge_chunks(id,tenant_id,object_id,document_id,content,position,metadata) VALUES($1::uuid,$2::uuid,$3::uuid,$4::uuid,$5,$6,'{}'::jsonb)").bind(chunk.id().to_string()).bind(scope.tenant_id().to_string()).bind(&object_id).bind(document_id.to_string()).bind(chunk.content()).bind(i32::try_from(chunk.position()).map_err(|_|PgKnowledgeIngestionError::IncompleteChunkSet)?).execute(tx.connection()).await?;
            for embedding in chunk.embeddings() {
                let vector = format!(
                    "[{}]",
                    embedding
                        .values()
                        .iter()
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                        .join(",")
                );
                sqlx::query("INSERT INTO knowledge_chunk_embeddings(tenant_id,object_id,chunk_id,model_id,embedding) VALUES($1::uuid,$2::uuid,$3::uuid,$4,$5::vector)").bind(scope.tenant_id().to_string()).bind(&object_id).bind(chunk.id().to_string()).bind(embedding.model_id()).bind(vector).execute(tx.connection()).await?;
            }
        }
        let updated=sqlx::query("UPDATE knowledge_documents SET status='indexed' WHERE tenant_id=$1::uuid AND id=$2::uuid AND status='staged'").bind(scope.tenant_id().to_string()).bind(document_id.to_string()).execute(tx.connection()).await?;
        if updated.rows_affected() != 1 {
            return Err(PgKnowledgeIngestionError::InvalidDocumentState);
        }
        tx.commit().await?;
        Ok(IngestionPersistenceOutcome::Indexed)
    }
}
