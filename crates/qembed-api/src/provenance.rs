//! Application-level provenance verification against database-retrieved evidence.
//!
//! Callers submit claims and citation IDs, never evidence text. This service
//! loads the exact chunks visible at the pinned Knowledge revision and passes
//! only those database results to the deterministic domain verifier.

use std::{error::Error, fmt};

use qembed_domain::{
    citation_verifier::{
        CitationDocument, CitationVerifier, RetrievedEvidenceChunk, VerificationReport,
    },
    knowledge::PinnedKnowledgeRevision,
    scope::Scope,
};
use qembed_store::{
    PgContext,
    knowledge_retrieval::{PgKnowledgeRetrievalError, PgKnowledgeRetrievalStore},
};

/// Application boundary that binds citation verification to persisted retrieval.
#[derive(Clone)]
pub struct ApplicationProvenanceVerifier {
    retrieval: PgKnowledgeRetrievalStore,
}

impl ApplicationProvenanceVerifier {
    #[must_use]
    pub fn new(context: PgContext) -> Self {
        Self {
            retrieval: PgKnowledgeRetrievalStore::new(context),
        }
    }

    /// Verifies a caller-supplied claim document against the exact persisted
    /// chunks visible to this trusted Scope at the supplied pinned revision.
    ///
    /// There is intentionally no evidence parameter: untrusted request data
    /// cannot add, replace, or rewrite the chunks against which citations resolve.
    pub async fn verify(
        &self,
        scope: &Scope,
        pinned: PinnedKnowledgeRevision,
        document: &CitationDocument,
    ) -> Result<VerificationReport, ApplicationProvenanceError> {
        let chunks = self
            .retrieval
            .visible_chunks(scope, pinned)
            .await
            .map_err(ApplicationProvenanceError::Retrieval)?;
        let evidence = chunks
            .iter()
            .map(|chunk| RetrievedEvidenceChunk::new(chunk.id(), chunk.content()))
            .collect::<Vec<_>>();
        Ok(CitationVerifier::new().verify(document, &evidence))
    }
}

/// Failure to retrieve trusted evidence for application-level verification.
#[derive(Debug)]
pub enum ApplicationProvenanceError {
    Retrieval(PgKnowledgeRetrievalError),
}

impl fmt::Display for ApplicationProvenanceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Retrieval(error) => {
                write!(
                    formatter,
                    "application provenance evidence retrieval failed: {error}"
                )
            }
        }
    }
}

impl Error for ApplicationProvenanceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Retrieval(error) => Some(error),
        }
    }
}
