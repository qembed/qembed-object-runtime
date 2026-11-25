//! Staged-document ingestion orchestration. Provider I/O is outside DB transactions.
use qembed_domain::{
    ids::{KnowledgeChunkId, KnowledgeDocumentId},
    knowledge_ingestion::{
        IngestionChunk, IngestionEmbeddingModel, IngestionPersistenceOutcome,
        KnowledgeIngestionDocumentState, KnowledgeIngestionError, ModelEmbedding,
    },
    ports::KnowledgeIngestionStore,
    scope::Scope,
};
use std::{error::Error, fmt};

pub trait KnowledgeSourceReader: Send + Sync {
    type Error: Error + Send + Sync + 'static;
    fn read_document(
        &self,
        source_uri: &str,
    ) -> impl std::future::Future<Output = Result<String, Self::Error>> + Send;
}
pub trait EmbeddingGenerator: Send + Sync {
    type Error: Error + Send + Sync + 'static;
    fn embed(
        &self,
        model_id: &str,
        text: &str,
    ) -> impl std::future::Future<Output = Result<Vec<f32>, Self::Error>> + Send;
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkingConfig {
    max_chars: usize,
    overlap_chars: usize,
}
impl ChunkingConfig {
    pub fn new(
        max_chars: usize,
        overlap_chars: usize,
    ) -> Result<
        Self,
        IngestionError<
            std::convert::Infallible,
            std::convert::Infallible,
            std::convert::Infallible,
        >,
    > {
        if max_chars == 0 || overlap_chars >= max_chars {
            return Err(IngestionError::InvalidChunkingConfig);
        }
        Ok(Self {
            max_chars,
            overlap_chars,
        })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KnowledgeIngestionOutcome {
    Indexed { chunks: usize },
    AlreadyIndexed,
}
#[derive(Debug)]
pub enum IngestionError<S, R, E> {
    Store(S),
    Source(R),
    Embedding(E),
    InvalidChunkingConfig,
    DocumentNotFound,
    EmptySourceDocument,
    NoActiveEmbeddingModels,
    EmbeddingDimensionMismatch {
        model_id: String,
        expected: usize,
        actual: usize,
    },
    TooManyChunks,
    InvalidEmbedding,
    Domain(KnowledgeIngestionError),
}
impl<S: fmt::Display, R: fmt::Display, E: fmt::Display> fmt::Display for IngestionError<S, R, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(e) => write!(f, "ingestion store failed: {e}"),
            Self::Source(e) => write!(f, "document source failed: {e}"),
            Self::Embedding(e) => write!(f, "embedding provider failed: {e}"),
            Self::InvalidChunkingConfig => f.write_str("invalid chunking configuration"),
            Self::DocumentNotFound => f.write_str("staged document was not found"),
            Self::EmptySourceDocument => f.write_str("source document is empty"),
            Self::NoActiveEmbeddingModels => f.write_str("no active embedding models configured"),
            Self::EmbeddingDimensionMismatch {
                model_id,
                expected,
                actual,
            } => write!(
                f,
                "embedding model {model_id} returned dimension {actual}, expected {expected}"
            ),
            Self::TooManyChunks => f.write_str("document produced too many chunks"),
            Self::InvalidEmbedding => f.write_str("embedding contains non-finite values"),
            Self::Domain(e) => write!(f, "invalid ingestion data: {e:?}"),
        }
    }
}
impl<S: Error + 'static, R: Error + 'static, E: Error + 'static> Error for IngestionError<S, R, E> {}

pub struct KnowledgeIngestionPipeline<St, Src, Emb> {
    store: St,
    source: Src,
    embeddings: Emb,
    chunking: ChunkingConfig,
}
impl<St, Src, Emb> KnowledgeIngestionPipeline<St, Src, Emb>
where
    St: KnowledgeIngestionStore,
    Src: KnowledgeSourceReader,
    Emb: EmbeddingGenerator,
{
    #[must_use]
    pub const fn new(store: St, source: Src, embeddings: Emb, chunking: ChunkingConfig) -> Self {
        Self {
            store,
            source,
            embeddings,
            chunking,
        }
    }
    pub async fn ingest(
        &self,
        scope: &Scope,
        document_id: KnowledgeDocumentId,
    ) -> Result<KnowledgeIngestionOutcome, IngestionError<St::Error, Src::Error, Emb::Error>> {
        let state = self
            .store
            .load_knowledge_ingestion_document(scope, document_id)
            .await
            .map_err(IngestionError::Store)?
            .ok_or(IngestionError::DocumentNotFound)?;
        let document = match state {
            KnowledgeIngestionDocumentState::AlreadyIndexed => {
                return Ok(KnowledgeIngestionOutcome::AlreadyIndexed);
            }
            KnowledgeIngestionDocumentState::Staged(d) => d,
        };
        let body = self
            .source
            .read_document(document.source_uri())
            .await
            .map_err(IngestionError::Source)?;
        let pieces = chunk_text(&body, self.chunking).map_err(|e| match e {
            IngestionError::EmptySourceDocument => IngestionError::EmptySourceDocument,
            _ => IngestionError::InvalidChunkingConfig,
        })?;
        let models = self
            .store
            .active_ingestion_embedding_models(scope)
            .await
            .map_err(IngestionError::Store)?;
        if models.is_empty() {
            return Err(IngestionError::NoActiveEmbeddingModels);
        }
        let mut chunks = Vec::with_capacity(pieces.len());
        for (index, content) in pieces.into_iter().enumerate() {
            let position = u32::try_from(index).map_err(|_| IngestionError::TooManyChunks)?;
            let mut vectors = Vec::with_capacity(models.len());
            for model in &models {
                let values = self
                    .embeddings
                    .embed(model.id(), &content)
                    .await
                    .map_err(IngestionError::Embedding)?;
                if values.len() != model.dimension() {
                    return Err(IngestionError::EmbeddingDimensionMismatch {
                        model_id: model.id().to_owned(),
                        expected: model.dimension(),
                        actual: values.len(),
                    });
                }
                if values.iter().any(|v| !v.is_finite()) {
                    return Err(IngestionError::InvalidEmbedding);
                }
                vectors
                    .push(ModelEmbedding::new(model.id(), values).map_err(IngestionError::Domain)?);
            }
            chunks.push(
                IngestionChunk::new(KnowledgeChunkId::new(), position, content, vectors)
                    .map_err(IngestionError::Domain)?,
            );
        }
        match self
            .store
            .persist_indexed_knowledge_document(scope, document_id, &models, &chunks)
            .await
            .map_err(IngestionError::Store)?
        {
            IngestionPersistenceOutcome::Indexed => Ok(KnowledgeIngestionOutcome::Indexed {
                chunks: chunks.len(),
            }),
            IngestionPersistenceOutcome::AlreadyIndexed => {
                Ok(KnowledgeIngestionOutcome::AlreadyIndexed)
            }
        }
    }
}
pub fn chunk_text(
    text: &str,
    config: ChunkingConfig,
) -> Result<
    Vec<String>,
    IngestionError<std::convert::Infallible, std::convert::Infallible, std::convert::Infallible>,
> {
    let chars: Vec<char> = text.chars().collect();
    if chars.iter().all(|c| c.is_whitespace()) {
        return Err(IngestionError::EmptySourceDocument);
    }
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
        let end = (start + config.max_chars).min(chars.len());
        let value: String = chars[start..end].iter().collect();
        if !value.trim().is_empty() {
            chunks.push(value)
        }
        if end == chars.len() {
            break;
        }
        start = end - config.overlap_chars;
    }
    Ok(chunks)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_chunks_are_bounded_and_overlap() {
        let c = ChunkingConfig::new(8, 2).unwrap();
        let chunks = chunk_text("Привет мир, hello world", c).unwrap();
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|x| x.chars().count() <= 8));
        for pair in chunks.windows(2) {
            let overlap: String = pair[0]
                .chars()
                .rev()
                .take(2)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            assert!(pair[1].starts_with(&overlap));
        }
    }
    #[test]
    fn blank_text_is_rejected() {
        let c = ChunkingConfig::new(10, 2).unwrap();
        assert!(matches!(
            chunk_text(" \n", c),
            Err(IngestionError::EmptySourceDocument)
        ));
    }
    #[test]
    fn invalid_overlap_is_rejected() {
        assert!(ChunkingConfig::new(4, 4).is_err());
    }
}
