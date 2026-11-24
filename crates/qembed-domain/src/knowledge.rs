//! Tenant- and object-scoped knowledge revisions, documents and chunks.
//!
//! Revision intervals are half-open: [valid_from, valid_to). A document is
//! staged until indexing is complete, becomes live at a committed revision,
//! and is retired at a later revision. Erasure is terminal and may be applied
//! to any lifecycle state. Chunk content is immutable except for erasure.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::ids::{KnowledgeChunkId, KnowledgeDocumentId, ObjectId, TenantId};

/// A monotonically increasing knowledge revision number.
///
/// Zero represents an object with no committed knowledge revision yet.
/// Documents can only become visible at a positive revision.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct RevisionNumber(u64);

impl RevisionNumber {
    /// The initial revision before any knowledge commit.
    pub const INITIAL: Self = Self(0);

    /// Creates a revision number, including the initial zero value.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }

    /// Returns the next revision, rejecting integer overflow.
    pub fn next(self) -> Result<Self, KnowledgeError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(KnowledgeError::RevisionOverflow)
    }
}

/// A committed revision pointer for one tenant-owned object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnowledgeRevision {
    tenant_id: TenantId,
    object_id: ObjectId,
    current: RevisionNumber,
}

impl KnowledgeRevision {
    #[must_use]
    pub const fn new(tenant_id: TenantId, object_id: ObjectId, current: RevisionNumber) -> Self {
        Self {
            tenant_id,
            object_id,
            current,
        }
    }

    #[must_use]
    pub const fn tenant_id(self) -> TenantId {
        self.tenant_id
    }

    #[must_use]
    pub const fn object_id(self) -> ObjectId {
        self.object_id
    }

    #[must_use]
    pub const fn current(self) -> RevisionNumber {
        self.current
    }

    /// Pins the current revision as an immutable reference for a read or invocation.
    #[must_use]
    pub const fn pin(self) -> PinnedKnowledgeRevision {
        PinnedKnowledgeRevision::new(self.tenant_id, self.object_id, self.current)
    }

    /// Advances exactly one revision after a successful repository commit.
    pub fn advance(&mut self) -> Result<RevisionNumber, KnowledgeError> {
        self.current = self.current.next()?;
        Ok(self.current)
    }
}

/// Immutable knowledge revision selected by an invocation or retrieval request.
///
/// The tenant and object are part of the identity: a numeric revision alone is
/// not globally meaningful and must never be used as a cross-object reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PinnedKnowledgeRevision {
    tenant_id: TenantId,
    object_id: ObjectId,
    revision: RevisionNumber,
}

impl PinnedKnowledgeRevision {
    #[must_use]
    pub const fn new(tenant_id: TenantId, object_id: ObjectId, revision: RevisionNumber) -> Self {
        Self {
            tenant_id,
            object_id,
            revision,
        }
    }

    #[must_use]
    pub const fn tenant_id(self) -> TenantId {
        self.tenant_id
    }

    #[must_use]
    pub const fn object_id(self) -> ObjectId {
        self.object_id
    }

    #[must_use]
    pub const fn revision(self) -> RevisionNumber {
        self.revision
    }
}

/// Validity interval for a document or chunk, using [from, to) semantics.
///
/// An absent end means the interval is still open. Revision zero is not a
/// publishable revision; an end must be strictly greater than the start.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(try_from = "RevisionIntervalWire")]
pub struct RevisionInterval {
    from: RevisionNumber,
    to: Option<RevisionNumber>,
}

#[derive(Deserialize)]
struct RevisionIntervalWire {
    from: RevisionNumber,
    to: Option<RevisionNumber>,
}

impl TryFrom<RevisionIntervalWire> for RevisionInterval {
    type Error = &'static str;

    fn try_from(value: RevisionIntervalWire) -> Result<Self, Self::Error> {
        Self::new(value.from, value.to).map_err(|_| "invalid knowledge revision interval")
    }
}

impl<'de> Deserialize<'de> for RevisionInterval {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = RevisionIntervalWire::deserialize(deserializer)?;
        Self::try_from(wire).map_err(serde::de::Error::custom)
    }
}

impl RevisionInterval {
    pub fn new(from: RevisionNumber, to: Option<RevisionNumber>) -> Result<Self, KnowledgeError> {
        if from == RevisionNumber::INITIAL || to.is_some_and(|end| end <= from) {
            return Err(KnowledgeError::InvalidRevisionInterval);
        }
        Ok(Self { from, to })
    }

    #[must_use]
    pub const fn from(self) -> RevisionNumber {
        self.from
    }

    #[must_use]
    pub const fn to(self) -> Option<RevisionNumber> {
        self.to
    }

    #[must_use]
    pub const fn contains(self, revision: RevisionNumber) -> bool {
        revision.0 >= self.from.0
            && match self.to {
                Some(to) => revision.0 < to.0,
                None => true,
            }
    }

    pub fn close_at(&mut self, revision: RevisionNumber) -> Result<(), KnowledgeError> {
        if self.to.is_some() || revision <= self.from {
            return Err(KnowledgeError::InvalidRevisionInterval);
        }
        self.to = Some(revision);
        Ok(())
    }
}

/// Knowledge document lifecycle matching the SQL schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KnowledgeDocumentStatus {
    Staged,
    Indexed,
    Live,
    Retired,
    Erased,
}

impl KnowledgeDocumentStatus {
    /// Allowed lifecycle transitions. Erasure is an explicit terminal override.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Staged, Self::Indexed)
                | (Self::Indexed, Self::Live)
                | (Self::Live, Self::Retired)
        ) || (!matches!(self, Self::Erased) && matches!(next, Self::Erased))
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Retired | Self::Erased)
    }
}

/// A tenant-owned logical knowledge source with revision-bounded visibility.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(try_from = "KnowledgeDocumentWire")]
pub struct KnowledgeDocument {
    id: KnowledgeDocumentId,
    tenant_id: TenantId,
    object_id: ObjectId,
    doc_key: String,
    title: String,
    source_uri: Option<String>,
    trusted: bool,
    status: KnowledgeDocumentStatus,
    interval: Option<RevisionInterval>,
}

#[derive(Deserialize)]
struct KnowledgeDocumentWire {
    id: KnowledgeDocumentId,
    tenant_id: TenantId,
    object_id: ObjectId,
    doc_key: String,
    title: String,
    source_uri: Option<String>,
    trusted: bool,
    status: KnowledgeDocumentStatus,
    interval: Option<RevisionInterval>,
}

impl TryFrom<KnowledgeDocumentWire> for KnowledgeDocument {
    type Error = &'static str;

    fn try_from(value: KnowledgeDocumentWire) -> Result<Self, Self::Error> {
        let document = Self {
            id: value.id,
            tenant_id: value.tenant_id,
            object_id: value.object_id,
            doc_key: value.doc_key,
            title: value.title,
            source_uri: value.source_uri,
            trusted: value.trusted,
            status: value.status,
            interval: value.interval,
        };
        document
            .validate()
            .map_err(|_| "invalid knowledge document state")?;
        Ok(document)
    }
}

impl<'de> Deserialize<'de> for KnowledgeDocument {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = KnowledgeDocumentWire::deserialize(deserializer)?;
        Self::try_from(wire).map_err(serde::de::Error::custom)
    }
}

impl KnowledgeDocument {
    #[allow(clippy::too_many_arguments)]
    pub fn staged(
        id: KnowledgeDocumentId,
        tenant_id: TenantId,
        object_id: ObjectId,
        doc_key: impl Into<String>,
        title: impl Into<String>,
        source_uri: Option<String>,
        trusted: bool,
    ) -> Result<Self, KnowledgeError> {
        let document = Self {
            id,
            tenant_id,
            object_id,
            doc_key: doc_key.into(),
            title: title.into(),
            source_uri,
            trusted,
            status: KnowledgeDocumentStatus::Staged,
            interval: None,
        };
        document.validate()?;
        Ok(document)
    }

    #[must_use]
    pub const fn id(&self) -> KnowledgeDocumentId { self.id }
    #[must_use]
    pub const fn tenant_id(&self) -> TenantId { self.tenant_id }
    #[must_use]
    pub const fn object_id(&self) -> ObjectId { self.object_id }
    #[must_use]
    pub fn doc_key(&self) -> &str { &self.doc_key }
    #[must_use]
    pub fn title(&self) -> &str { &self.title }
    #[must_use]
    pub fn source_uri(&self) -> Option<&str> { self.source_uri.as_deref() }
    #[must_use]
    pub const fn trusted(&self) -> bool { self.trusted }
    #[must_use]
    pub const fn status(&self) -> KnowledgeDocumentStatus { self.status }
    #[must_use]
    pub const fn interval(&self) -> Option<RevisionInterval> { self.interval }

    pub fn mark_indexed(&mut self) -> Result<(), KnowledgeError> {
        self.transition(KnowledgeDocumentStatus::Indexed)
    }

    pub fn publish_at(&mut self, revision: RevisionNumber) -> Result<(), KnowledgeError> {
        if self.status != KnowledgeDocumentStatus::Indexed {
            return Err(KnowledgeError::InvalidTransition {
                from: self.status,
                to: KnowledgeDocumentStatus::Live,
            });
        }
        self.interval = Some(RevisionInterval::new(revision, None)?);
        self.status = KnowledgeDocumentStatus::Live;
        Ok(())
    }

    pub fn retire_at(&mut self, revision: RevisionNumber) -> Result<(), KnowledgeError> {
        if self.status != KnowledgeDocumentStatus::Live {
            return Err(KnowledgeError::InvalidTransition {
                from: self.status,
                to: KnowledgeDocumentStatus::Retired,
            });
        }
        self.interval
            .as_mut()
            .ok_or(KnowledgeError::InvalidRevisionInterval)?
            .close_at(revision)?;
        self.status = KnowledgeDocumentStatus::Retired;
        Ok(())
    }

    /// Marks a document erased. Chunk payload erasure is performed by the Store.
    pub fn erase(&mut self) -> Result<(), KnowledgeError> {
        self.transition(KnowledgeDocumentStatus::Erased)
    }

    fn transition(&mut self, next: KnowledgeDocumentStatus) -> Result<(), KnowledgeError> {
        if !self.status.can_transition_to(next) {
            return Err(KnowledgeError::InvalidTransition {
                from: self.status,
                to: next,
            });
        }
        self.status = next;
        Ok(())
    }

    /// Returns whether this document is visible at the pinned tenant/object revision.
    #[must_use]
    pub fn is_visible_at(&self, pinned: PinnedKnowledgeRevision) -> bool {
        self.tenant_id == pinned.tenant_id
            && self.object_id == pinned.object_id
            && self.status == KnowledgeDocumentStatus::Live
            && self
                .interval
                .is_some_and(|interval| interval.contains(pinned.revision))
    }

    fn validate(&self) -> Result<(), KnowledgeError> {
        if self.doc_key.trim().is_empty() || self.title.trim().is_empty() {
            return Err(KnowledgeError::EmptyDocumentMetadata);
        }
        let valid = match self.status {
            KnowledgeDocumentStatus::Staged | KnowledgeDocumentStatus::Indexed => {
                self.interval.is_none()
            }
            KnowledgeDocumentStatus::Live => self
                .interval
                .is_some_and(|interval| interval.to().is_none()),
            KnowledgeDocumentStatus::Retired => self
                .interval
                .is_some_and(|interval| interval.to().is_some()),
            KnowledgeDocumentStatus::Erased => true,
        };
        if !valid {
            return Err(KnowledgeError::InvalidDocumentRevisionReference);
        }
        Ok(())
    }
}

/// A document chunk with immutable identity/content and revision visibility.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(try_from = "KnowledgeChunkWire")]
pub struct KnowledgeChunk {
    id: KnowledgeChunkId,
    tenant_id: TenantId,
    object_id: ObjectId,
    document_id: KnowledgeDocumentId,
    content: String,
    erased: bool,
    position: u32,
    metadata: Value,
    interval: Option<RevisionInterval>,
}

#[derive(Deserialize)]
struct KnowledgeChunkWire {
    id: KnowledgeChunkId,
    tenant_id: TenantId,
    object_id: ObjectId,
    document_id: KnowledgeDocumentId,
    content: String,
    erased: bool,
    position: u32,
    metadata: Value,
    interval: Option<RevisionInterval>,
}

impl TryFrom<KnowledgeChunkWire> for KnowledgeChunk {
    type Error = &'static str;

    fn try_from(value: KnowledgeChunkWire) -> Result<Self, Self::Error> {
        let chunk = Self {
            id: value.id,
            tenant_id: value.tenant_id,
            object_id: value.object_id,
            document_id: value.document_id,
            content: value.content,
            erased: value.erased,
            position: value.position,
            metadata: value.metadata,
            interval: value.interval,
        };
        chunk
            .validate()
            .map_err(|_| "invalid knowledge chunk state")?;
        Ok(chunk)
    }
}

impl<'de> Deserialize<'de> for KnowledgeChunk {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = KnowledgeChunkWire::deserialize(deserializer)?;
        Self::try_from(wire).map_err(serde::de::Error::custom)
    }
}

impl KnowledgeChunk {
    pub fn staged(
        id: KnowledgeChunkId,
        tenant_id: TenantId,
        object_id: ObjectId,
        document_id: KnowledgeDocumentId,
        content: impl Into<String>,
        position: u32,
        metadata: Value,
    ) -> Result<Self, KnowledgeError> {
        let chunk = Self {
            id,
            tenant_id,
            object_id,
            document_id,
            content: content.into(),
            erased: false,
            position,
            metadata,
            interval: None,
        };
        chunk.validate()?;
        if chunk.content.trim().is_empty() {
            return Err(KnowledgeError::EmptyChunkContent);
        }
        Ok(chunk)
    }

    #[must_use]
    pub const fn id(&self) -> KnowledgeChunkId { self.id }
    #[must_use]
    pub const fn tenant_id(&self) -> TenantId { self.tenant_id }
    #[must_use]
    pub const fn object_id(&self) -> ObjectId { self.object_id }
    #[must_use]
    pub const fn document_id(&self) -> KnowledgeDocumentId { self.document_id }
    #[must_use]
    pub fn content(&self) -> &str { &self.content }
    #[must_use]
    pub const fn is_erased(&self) -> bool { self.erased }
    #[must_use]
    pub const fn position(&self) -> u32 { self.position }
    #[must_use]
    pub fn metadata(&self) -> &Value { &self.metadata }
    #[must_use]
    pub const fn interval(&self) -> Option<RevisionInterval> { self.interval }

    /// Applies the document's initial committed visibility interval to this chunk.
    pub fn set_visibility_interval(
        &mut self,
        interval: RevisionInterval,
    ) -> Result<(), KnowledgeError> {
        if self.erased || self.interval.is_some() || interval.to().is_some() {
            return Err(KnowledgeError::ChunkContentIsImmutable);
        }
        self.interval = Some(interval);
        Ok(())
    }

    /// Closes the chunk's visibility interval when its document is retired.
    pub fn close_visibility_at(&mut self, revision: RevisionNumber) -> Result<(), KnowledgeError> {
        if self.erased {
            return Err(KnowledgeError::ChunkContentIsImmutable);
        }
        self.interval
            .as_mut()
            .ok_or(KnowledgeError::InvalidRevisionInterval)?
            .close_at(revision)
    }

    /// Returns whether this chunk is visible at the pinned tenant/object revision.
    #[must_use]
    pub fn is_visible_at(&self, pinned: PinnedKnowledgeRevision) -> bool {
        self.tenant_id == pinned.tenant_id
            && self.object_id == pinned.object_id
            && !self.erased
            && self
                .interval
                .is_some_and(|interval| interval.contains(pinned.revision))
    }

    /// Erasure is the only operation allowed to change persisted chunk content.
    pub fn erase(&mut self) {
        self.content.clear();
        self.erased = true;
    }

    fn validate(&self) -> Result<(), KnowledgeError> {
        if self.erased && !self.content.is_empty() {
            return Err(KnowledgeError::ErasedChunkMustBeEmpty);
        }
        Ok(())
    }
}

/// Domain invariant violation in the knowledge model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KnowledgeError {
    RevisionOverflow,
    InvalidRevisionInterval,
    EmptyDocumentMetadata,
    EmptyChunkContent,
    ErasedChunkMustBeEmpty,
    InvalidDocumentRevisionReference,
    ChunkContentIsImmutable,
    InvalidTransition {
        from: KnowledgeDocumentStatus,
        to: KnowledgeDocumentStatus,
    }
}

impl std::fmt::Display for KnowledgeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for KnowledgeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn document() -> KnowledgeDocument {
        KnowledgeDocument::staged(
            KnowledgeDocumentId::new(),
            TenantId::new(),
            ObjectId::new(),
            "handbook",
            "Handbook",
            None,
            true,
        )
        .expect("valid staged document")
    }

    #[test]
    fn document_transition_matrix_matches_lifecycle_contract() {
        use KnowledgeDocumentStatus as S;
        let states = [S::Staged, S::Indexed, S::Live, S::Retired, S::Erased];
        for from in states {
            for to in states {
                let expected = matches!(
                    (from, to),
                    (S::Staged, S::Indexed) | (S::Indexed, S::Live) | (S::Live, S::Retired)
                ) || (from != S::Erased && to == S::Erased);
                assert_eq!(from.can_transition_to(to), expected, "{from:?} -> {to:?}");
            }
        }
    }

    #[test]
    fn document_requires_indexing_before_publication_and_monotonic_retirement() {
        let mut doc = document();
        assert!(matches!(
            doc.publish_at(RevisionNumber::new(1)),
            Err(KnowledgeError::InvalidTransition { .. })
        ));
        doc.mark_indexed().expect("staged document can be indexed");
        doc.publish_at(RevisionNumber::new(3))
            .expect("indexed document can go live");
        assert!(
            doc.interval()
                .expect("live interval")
                .contains(RevisionNumber::new(3))
        );
        assert!(
            !doc.interval()
                .expect("live interval")
                .contains(RevisionNumber::new(4))
        );
        let pinned =
            PinnedKnowledgeRevision::new(doc.tenant_id(), doc.object_id(), RevisionNumber::new(3));
        assert!(doc.is_visible_at(pinned));
        assert!(!doc.is_visible_at(PinnedKnowledgeRevision::new(
            TenantId::new(),
            doc.object_id(),
            RevisionNumber::new(3)
        )));
        assert!(matches!(
            doc.retire_at(RevisionNumber::new(3)),
            Err(KnowledgeError::InvalidRevisionInterval)
        ));
        doc.retire_at(RevisionNumber::new(4))
            .expect("retirement closes interval");
        assert_eq!(doc.status(), KnowledgeDocumentStatus::Retired);
        assert!(
            !doc.interval()
                .expect("retired interval")
                .contains(RevisionNumber::new(4))
        );
    }

    #[test]
    fn revision_identity_is_scoped_and_pin_is_immutable() {
        let mut revision =
            KnowledgeRevision::new(TenantId::new(), ObjectId::new(), RevisionNumber::INITIAL);
        let pinned = revision.pin();
        assert_eq!(pinned.revision(), RevisionNumber::INITIAL);
        revision.advance().expect("revision can advance");
        assert_eq!(pinned.revision(), RevisionNumber::INITIAL);
        assert_eq!(revision.current(), RevisionNumber::new(1));
    }

    #[test]
    fn interval_rejects_zero_and_non_monotonic_bounds() {
        assert_eq!(
            RevisionInterval::new(RevisionNumber::INITIAL, None),
            Err(KnowledgeError::InvalidRevisionInterval)
        );
        assert_eq!(
            RevisionInterval::new(RevisionNumber::new(4), Some(RevisionNumber::new(4))),
            Err(KnowledgeError::InvalidRevisionInterval)
        );
        assert_eq!(
            RevisionInterval::new(RevisionNumber::new(5), Some(RevisionNumber::new(4))),
            Err(KnowledgeError::InvalidRevisionInterval)
        );
    }

    #[test]
    fn document_and_chunk_serde_round_trip_preserves_invariants() {
        let mut doc = document();
        doc.mark_indexed().expect("indexed");
        doc.publish_at(RevisionNumber::new(2)).expect("published");
        let encoded = serde_json::to_vec(&doc).expect("serialize document");
        let decoded: KnowledgeDocument =
            serde_json::from_slice(&encoded).expect("validated deserialize");
        assert_eq!(decoded, doc);

        let chunk = KnowledgeChunk::staged(
            KnowledgeChunkId::new(),
            doc.tenant_id(),
            doc.object_id(),
            doc.id(),
            "durable content",
            0,
            json!({"source":"test"}),
        )
        .expect("valid chunk");
        let encoded = serde_json::to_vec(&chunk).expect("serialize chunk");
        let decoded: KnowledgeChunk =
            serde_json::from_slice(&encoded).expect("validated deserialize");
        assert_eq!(decoded, chunk);
        let interval = RevisionInterval::new(RevisionNumber::new(2), None).expect("valid interval");
        let mut visible_chunk = chunk.clone();
        visible_chunk
            .set_visibility_interval(interval)
            .expect("publish chunk");
        assert!(visible_chunk.is_visible_at(PinnedKnowledgeRevision::new(
            doc.tenant_id(),
            doc.object_id(),
            RevisionNumber::new(2),
        )));
        visible_chunk
            .close_visibility_at(RevisionNumber::new(3))
            .expect("retire chunk");
        assert!(!visible_chunk.is_visible_at(PinnedKnowledgeRevision::new(
            doc.tenant_id(),
            doc.object_id(),
            RevisionNumber::new(3),
        )));
    }

    #[test]
    fn deserialization_rejects_invalid_revision_references_and_erasure_state() {
        let doc = document();
        let mut value = serde_json::to_value(&doc).expect("serialize");
        value["status"] = json!("live");
        assert!(serde_json::from_value::<KnowledgeDocument>(value).is_err());

        let chunk = KnowledgeChunk::staged(
            KnowledgeChunkId::new(),
            doc.tenant_id(),
            doc.object_id(),
            doc.id(),
            "payload",
            0,
            json!({}),
        )
        .expect("valid chunk");
        let mut value = serde_json::to_value(&chunk).expect("serialize");
        value["erased"] = json!(true);
        assert!(serde_json::from_value::<KnowledgeChunk>(value).is_err());
    }

    #[test]
    fn erasure_is_terminal_and_clears_chunk_payload() {
        let mut doc = document();
        doc.erase().expect("staged document can be erased");
        assert_eq!(doc.status(), KnowledgeDocumentStatus::Erased);
        assert!(doc.erase().is_err());

        let mut chunk = KnowledgeChunk::staged(
            KnowledgeChunkId::new(),
            doc.tenant_id(),
            doc.object_id(),
            doc.id(),
            "secret payload",
            0,
            json!({}),
        )
        .expect("valid chunk");
        chunk.erase();
        assert!(chunk.is_erased());
        assert!(chunk.content().is_empty());
    }
}
