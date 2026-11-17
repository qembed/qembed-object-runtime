//! Immutable, content-addressed runtime snapshot entity.

use sha2::{Digest, Sha256};

use crate::ids::{ObjectId, ObjectVersionId, SnapshotId, TenantId};
use crate::jcs::{JcsError, validate_canonical_json};

/// Immutable configuration snapshot used to reproduce an invocation's behavior.
///
/// `canonical_json` is the source of truth. The hash is SHA-256 over its exact
/// bytes. The bytes must pass the RFC 8785 validation boundary before this
/// value can be constructed; the hash is then computed over those exact bytes.
#[derive(Debug, PartialEq, Eq)]
pub struct Snapshot {
    id: SnapshotId,
    tenant_id: TenantId,
    object_id: ObjectId,
    object_version_id: ObjectVersionId,
    schema_version: u32,
    canonical_json: Box<[u8]>,
    content_hash: [u8; 32],
}

impl Snapshot {
    /// Creates an immutable snapshot and computes SHA-256 over exact JSON bytes.
    pub fn new(
        id: SnapshotId,
        tenant_id: TenantId,
        object_id: ObjectId,
        object_version_id: ObjectVersionId,
        schema_version: u32,
        canonical_json: impl Into<Box<[u8]>>,
    ) -> Result<Self, SnapshotError> {
        if schema_version == 0 {
            return Err(SnapshotError::InvalidSchemaVersion);
        }

        let canonical_json = canonical_json.into();
        validate_canonical_json(&canonical_json).map_err(SnapshotError::Jcs)?;
        let content_hash = Sha256::digest(&canonical_json).into();

        Ok(Self {
            id,
            tenant_id,
            object_id,
            object_version_id,
            schema_version,
            canonical_json,
            content_hash,
        })
    }

    /// Reconstitutes a persisted snapshot, rejecting a hash mismatch.
    pub fn from_persisted(
        id: SnapshotId,
        tenant_id: TenantId,
        object_id: ObjectId,
        object_version_id: ObjectVersionId,
        schema_version: u32,
        canonical_json: impl Into<Box<[u8]>>,
        expected_hash: [u8; 32],
    ) -> Result<Self, SnapshotError> {
        let snapshot = Self::new(
            id,
            tenant_id,
            object_id,
            object_version_id,
            schema_version,
            canonical_json,
        )?;

        if snapshot.content_hash != expected_hash {
            return Err(SnapshotError::HashMismatch);
        }

        Ok(snapshot)
    }

    #[must_use]
    pub const fn id(&self) -> SnapshotId {
        self.id
    }

    #[must_use]
    pub const fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }

    #[must_use]
    pub const fn object_id(&self) -> ObjectId {
        self.object_id
    }

    #[must_use]
    pub const fn object_version_id(&self) -> ObjectVersionId {
        self.object_version_id
    }

    #[must_use]
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    #[must_use]
    pub fn canonical_json(&self) -> &[u8] {
        &self.canonical_json
    }

    #[must_use]
    pub const fn content_hash(&self) -> &[u8; 32] {
        &self.content_hash
    }
}

/// Builds an immutable snapshot from RFC 8785 canonical JSON bytes.
///
/// The builder preserves the supplied bytes exactly and rejects malformed or
/// noncanonical input. Use the JCS canonicalize_json function when starting
/// from ordinary JSON that needs canonicalization.
#[derive(Debug)]
pub struct SnapshotBuilder {
    id: SnapshotId,
    tenant_id: TenantId,
    object_id: ObjectId,
    object_version_id: ObjectVersionId,
    schema_version: u32,
    canonical_json: Box<[u8]>,
}

impl SnapshotBuilder {
    /// Creates a builder with the identity and exact canonical byte sequence.
    #[must_use]
    pub fn new(
        id: SnapshotId,
        tenant_id: TenantId,
        object_id: ObjectId,
        object_version_id: ObjectVersionId,
        schema_version: u32,
        canonical_json: impl Into<Box<[u8]>>,
    ) -> Self {
        Self {
            id,
            tenant_id,
            object_id,
            object_version_id,
            schema_version,
            canonical_json: canonical_json.into(),
        }
    }

    /// Builds a snapshot and computes SHA-256 over the exact supplied bytes.
    pub fn build(self) -> Result<Snapshot, SnapshotError> {
        Snapshot::new(
            self.id,
            self.tenant_id,
            self.object_id,
            self.object_version_id,
            self.schema_version,
            self.canonical_json,
        )
    }
}

/// Snapshot invariant error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotError {
    InvalidSchemaVersion,
    HashMismatch,
    Jcs(JcsError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_hashes_exact_canonical_json_bytes() {
        let bytes = br#"{"a":1,"b":2}"#.to_vec();
        let snapshot = Snapshot::new(
            SnapshotId::new(),
            TenantId::new(),
            ObjectId::new(),
            ObjectVersionId::new(),
            1,
            bytes.clone(),
        )
        .expect("positive schema version is valid");

        assert_eq!(snapshot.canonical_json(), bytes);
        let expected_hash: [u8; 32] = Sha256::digest(&bytes).into();
        assert_eq!(snapshot.content_hash(), &expected_hash);
    }

    #[test]
    fn persisted_snapshot_rejects_hash_mismatch() {
        let result = Snapshot::from_persisted(
            SnapshotId::new(),
            TenantId::new(),
            ObjectId::new(),
            ObjectVersionId::new(),
            1,
            br#"{"a":1}"#.to_vec(),
            [0; 32],
        );

        assert_eq!(result, Err(SnapshotError::HashMismatch));
    }

    #[test]
    fn snapshot_requires_positive_schema_version() {
        assert_eq!(
            Snapshot::new(
                SnapshotId::new(),
                TenantId::new(),
                ObjectId::new(),
                ObjectVersionId::new(),
                0,
                br#"{}"#.to_vec(),
            ),
            Err(SnapshotError::InvalidSchemaVersion)
        );
    }

    #[test]
    fn snapshot_builder_is_deterministic_for_identical_inputs() {
        let id = SnapshotId::new();
        let tenant_id = TenantId::new();
        let object_id = ObjectId::new();
        let object_version_id = ObjectVersionId::new();
        let bytes = br#"{"config":{"enabled":true},"name":"demo"}"#;

        let first = SnapshotBuilder::new(
            id,
            tenant_id,
            object_id,
            object_version_id,
            1,
            bytes.to_vec(),
        )
        .build()
        .expect("valid snapshot input");

        let second = SnapshotBuilder::new(
            id,
            tenant_id,
            object_id,
            object_version_id,
            1,
            bytes.to_vec(),
        )
        .build()
        .expect("same input must build");

        assert_eq!(first, second);
        assert_eq!(first.canonical_json(), bytes);
    }

    #[test]
    fn snapshot_exposes_bytes_only_as_immutable_slice() {
        let snapshot = Snapshot::new(
            SnapshotId::new(),
            TenantId::new(),
            ObjectId::new(),
            ObjectVersionId::new(),
            1,
            br#"{}"#.to_vec(),
        )
        .expect("valid snapshot");

        assert_eq!(snapshot.canonical_json(), b"{}");
    }
}
