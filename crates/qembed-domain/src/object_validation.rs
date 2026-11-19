//! Bounded semantic validation for immutable object snapshots.
//!
//! This validator implements only semantic rules confirmed by the surviving
//! M0 r3 artifacts. It is not a complete object/configuration schema validator.
//!
//! Successful validation produces an opaque, non-serializable evidence value.
//! Its identity and content hash bind it to the exact immutable snapshot that
//! was checked. The evidence is intended to cross the trusted runtime-to-Store
//! boundary; it is not an API DTO or a client-provided assertion.
//!
//! The validator version is an immutable implementation identifier. Any
//! change to the rules implemented under this identifier requires a new value.

use serde_json::Value;

use crate::ids::{ObjectId, ObjectVersionId, SnapshotId, TenantId};
use crate::snapshot::Snapshot;

/// Immutable identifier for this bounded set of confirmed M0 r3 checks.
///
/// Coverage is intentionally limited to the positive integer budget invariant.
pub const OBJECT_VALIDATOR_VERSION: &str = "m0-r3-bounded-budget-v1";

/// Trusted semantic checks for the currently confirmed snapshot invariants.
#[derive(Clone, Copy, Debug, Default)]
pub struct ObjectValidator;

impl ObjectValidator {
    /// Validates the semantic rules currently confirmed by surviving M0 r3
    /// artifacts and returns evidence bound to this exact snapshot.
    ///
    /// This is deliberately not a claim that tools, providers, permissions,
    /// security policies, or unknown extensions have been fully validated.
    pub fn validate(snapshot: &Snapshot) -> Result<ValidationEvidence, ObjectValidationError> {
        let document: Value = serde_json::from_slice(snapshot.canonical_json())
            .map_err(|_| ObjectValidationError::InvalidSnapshotJson)?;

        let root = document
            .as_object()
            .ok_or(ObjectValidationError::RootMustBeObject)?;
        let limits = root
            .get("limits")
            .ok_or(ObjectValidationError::MissingLimits)?
            .as_object()
            .ok_or(ObjectValidationError::LimitsMustBeObject)?;
        let value = limits
            .get("max_tokens_per_invocation")
            .ok_or(ObjectValidationError::MissingMaxTokensPerInvocation)?;

        // The runtime's token-budget arithmetic is u64-based. as_u64 accepts
        // JSON integer numbers in that representable range and rejects strings,
        // floats (including 1.0), negatives, and out-of-range values.
        let max_tokens_per_invocation = value
            .as_u64()
            .filter(|limit| *limit > 0)
            .ok_or(ObjectValidationError::MaxTokensPerInvocationMustBePositiveInteger)?;

        Ok(ValidationEvidence {
            tenant_id: snapshot.tenant_id(),
            object_id: snapshot.object_id(),
            object_version_id: snapshot.object_version_id(),
            snapshot_id: snapshot.id(),
            content_hash: *snapshot.content_hash(),
            max_tokens_per_invocation,
            validator_version: OBJECT_VALIDATOR_VERSION,
        })
    }
}

/// Opaque proof that the bounded validator accepted one exact snapshot.
///
/// Fields are private and this type intentionally does not implement
/// Serialize/Deserialize. Store adapters must still verify the identity tuple
/// and content hash against persisted rows in the publication transaction.
#[derive(Debug, PartialEq, Eq)]
pub struct ValidationEvidence {
    tenant_id: TenantId,
    object_id: ObjectId,
    object_version_id: ObjectVersionId,
    snapshot_id: SnapshotId,
    content_hash: [u8; 32],
    max_tokens_per_invocation: u64,
    validator_version: &'static str,
}

impl ValidationEvidence {
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
    pub const fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }

    #[must_use]
    pub const fn content_hash(&self) -> &[u8; 32] {
        &self.content_hash
    }

    #[must_use]
    pub const fn max_tokens_per_invocation(&self) -> u64 {
        self.max_tokens_per_invocation
    }

    #[must_use]
    pub const fn validator_version(&self) -> &'static str {
        self.validator_version
    }

    /// Returns true only for the exact tenant/object/version/snapshot/hash
    /// tuple that this evidence was issued for.
    #[must_use]
    pub fn matches_snapshot(&self, snapshot: &Snapshot) -> bool {
        self.tenant_id == snapshot.tenant_id()
            && self.object_id == snapshot.object_id()
            && self.object_version_id == snapshot.object_version_id()
            && self.snapshot_id == snapshot.id()
            && self.content_hash == *snapshot.content_hash()
    }
}

/// A confirmed semantic invariant rejected a candidate snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectValidationError {
    InvalidSnapshotJson,
    RootMustBeObject,
    MissingLimits,
    LimitsMustBeObject,
    MissingMaxTokensPerInvocation,
    MaxTokensPerInvocationMustBePositiveInteger,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{ObjectId, ObjectVersionId, SnapshotId, TenantId};

    fn snapshot(json: &[u8]) -> Snapshot {
        Snapshot::new(
            SnapshotId::new(),
            TenantId::new(),
            ObjectId::new(),
            ObjectVersionId::new(),
            1,
            json.to_vec(),
        )
        .expect("fixture is valid canonical JSON")
    }

    #[test]
    fn accepts_a_positive_json_integer_and_binds_evidence_to_snapshot() {
        let snapshot = snapshot(br#"{"limits":{"max_tokens_per_invocation":4096}}"#);
        let evidence = ObjectValidator::validate(&snapshot).expect("valid budget is accepted");

        assert_eq!(evidence.tenant_id(), snapshot.tenant_id());
        assert_eq!(evidence.object_id(), snapshot.object_id());
        assert_eq!(evidence.object_version_id(), snapshot.object_version_id());
        assert_eq!(evidence.snapshot_id(), snapshot.id());
        assert_eq!(evidence.content_hash(), snapshot.content_hash());
        assert_eq!(evidence.max_tokens_per_invocation(), 4096);
        assert_eq!(evidence.validator_version(), OBJECT_VALIDATOR_VERSION);
        assert!(evidence.matches_snapshot(&snapshot));
    }

    #[test]
    fn rejects_missing_limits_and_missing_budget() {
        assert_eq!(
            ObjectValidator::validate(&snapshot(br#"{"config":{}}"#)),
            Err(ObjectValidationError::MissingLimits)
        );
        assert_eq!(
            ObjectValidator::validate(&snapshot(br#"{"limits":{}}"#)),
            Err(ObjectValidationError::MissingMaxTokensPerInvocation)
        );
    }

    #[test]
    fn rejects_non_object_root_and_non_object_limits() {
        assert_eq!(
            ObjectValidator::validate(&snapshot(br#"[]"#)),
            Err(ObjectValidationError::RootMustBeObject)
        );
        assert_eq!(
            ObjectValidator::validate(&snapshot(br#"{"limits":[]}"#)),
            Err(ObjectValidationError::LimitsMustBeObject)
        );
    }

    #[test]
    fn rejects_zero_negative_fractional_and_string_budgets() {
        for value in ["0", "-1", "1.0", "1.5", "\"10\""] {
            let json = format!(r#"{{"limits":{{"max_tokens_per_invocation":{value}}}}}"#);
            let candidate = snapshot(json.as_bytes());
            assert_eq!(
                ObjectValidator::validate(&candidate),
                Err(ObjectValidationError::MaxTokensPerInvocationMustBePositiveInteger),
                "value {value} must be rejected"
            );
        }
    }

    #[test]
    fn evidence_does_not_match_another_version_or_snapshot() {
        let first = snapshot(br#"{"limits":{"max_tokens_per_invocation":100}}"#);
        let evidence = ObjectValidator::validate(&first).expect("first snapshot is valid");

        let other_version = Snapshot::new(
            SnapshotId::new(),
            first.tenant_id(),
            first.object_id(),
            ObjectVersionId::new(),
            1,
            first.canonical_json().to_vec(),
        )
        .expect("second version snapshot is valid");
        assert!(!evidence.matches_snapshot(&other_version));

        let other_tenant = Snapshot::new(
            SnapshotId::new(),
            TenantId::new(),
            first.object_id(),
            first.object_version_id(),
            1,
            first.canonical_json().to_vec(),
        )
        .expect("other tenant snapshot is valid");
        assert!(!evidence.matches_snapshot(&other_tenant));
    }

    #[test]
    fn validator_coverage_is_versioned_and_explicit() {
        assert_eq!(OBJECT_VALIDATOR_VERSION, "m0-r3-bounded-budget-v1");
    }
}
