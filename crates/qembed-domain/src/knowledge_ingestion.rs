//! Value types for staged Knowledge ingestion.
use crate::ids::{KnowledgeChunkId, KnowledgeDocumentId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedKnowledgeDocument {
    id: KnowledgeDocumentId,
    source_uri: String,
}
impl StagedKnowledgeDocument {
    pub fn new(
        id: KnowledgeDocumentId,
        source_uri: impl Into<String>,
    ) -> Result<Self, KnowledgeIngestionError> {
        let source_uri = source_uri.into();
        if source_uri.trim().is_empty() {
            return Err(KnowledgeIngestionError::MissingSourceUri);
        }
        Ok(Self { id, source_uri })
    }
    #[must_use]
    pub const fn id(&self) -> KnowledgeDocumentId {
        self.id
    }
    #[must_use]
    pub fn source_uri(&self) -> &str {
        &self.source_uri
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IngestionEmbeddingModel {
    id: String,
    dimension: usize,
}
impl IngestionEmbeddingModel {
    pub fn new(id: impl Into<String>, dimension: usize) -> Result<Self, KnowledgeIngestionError> {
        let id = id.into();
        if id.trim().is_empty() || dimension == 0 {
            return Err(KnowledgeIngestionError::InvalidEmbeddingModel);
        }
        Ok(Self { id, dimension })
    }
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }
    #[must_use]
    pub const fn dimension(&self) -> usize {
        self.dimension
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct ModelEmbedding {
    model_id: String,
    values: Vec<f32>,
}
impl ModelEmbedding {
    pub fn new(
        model_id: impl Into<String>,
        values: Vec<f32>,
    ) -> Result<Self, KnowledgeIngestionError> {
        let model_id = model_id.into();
        if model_id.trim().is_empty() || values.is_empty() || values.iter().any(|v| !v.is_finite())
        {
            return Err(KnowledgeIngestionError::InvalidEmbedding);
        }
        Ok(Self { model_id, values })
    }
    #[must_use]
    pub fn model_id(&self) -> &str {
        &self.model_id
    }
    #[must_use]
    pub fn values(&self) -> &[f32] {
        &self.values
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct IngestionChunk {
    id: KnowledgeChunkId,
    position: u32,
    content: String,
    embeddings: Vec<ModelEmbedding>,
}
impl IngestionChunk {
    pub fn new(
        id: KnowledgeChunkId,
        position: u32,
        content: String,
        embeddings: Vec<ModelEmbedding>,
    ) -> Result<Self, KnowledgeIngestionError> {
        if content.trim().is_empty() || embeddings.is_empty() {
            return Err(KnowledgeIngestionError::IncompleteChunk);
        }
        Ok(Self {
            id,
            position,
            content,
            embeddings,
        })
    }
    #[must_use]
    pub const fn id(&self) -> KnowledgeChunkId {
        self.id
    }
    #[must_use]
    pub const fn position(&self) -> u32 {
        self.position
    }
    #[must_use]
    pub fn content(&self) -> &str {
        &self.content
    }
    #[must_use]
    pub fn embeddings(&self) -> &[ModelEmbedding] {
        &self.embeddings
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IngestionPersistenceOutcome {
    Indexed,
    AlreadyIndexed,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KnowledgeIngestionDocumentState {
    Staged(StagedKnowledgeDocument),
    AlreadyIndexed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KnowledgeIngestionError {
    MissingSourceUri,
    InvalidEmbeddingModel,
    InvalidEmbedding,
    IncompleteChunk,
}
