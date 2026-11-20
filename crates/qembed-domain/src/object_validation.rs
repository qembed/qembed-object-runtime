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
//! The coverage report is informational: it makes implemented and unimplemented
//! areas explicit, but it is neither validation evidence nor authorization.
//!
//! The validator version is an immutable implementation identifier. Any
//! change to the semantic rules implemented under this identifier requires a
//! new value. Reporting-only changes do not change the rule-set version.
//!
//! Evidence cannot be serialized or reconstructed from request data:
//!
//! ```compile_fail
//! use qembed_domain::object_validation::ValidationEvidence;
//! fn require_serialize<T: serde::Serialize>() {}
//! require_serialize::<ValidationEvidence>();
//! ```
//!
//! ```compile_fail
//! use qembed_domain::object_validation::ValidationEvidence;
//! fn require_deserialize<T: serde::de::DeserializeOwned>() {}
//! require_deserialize::<ValidationEvidence>();
//! ```

use serde_json::Value;

use crate::ids::{ObjectId, ObjectVersionId, SnapshotId, TenantId};
use crate::snapshot::Snapshot;

/// Immutable identifier for this bounded set of confirmed M0 r3 checks.
///
/// Coverage is intentionally limited to the positive integer budget invariant.
pub const OBJECT_VALIDATOR_VERSION: &str = "m0-r3-bounded-budget-v1";

/// Semantic areas whose current validation status is reported explicitly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationArea {
    InvocationBudget,
    ObjectSchema,
    ToolConfiguration,
    ProviderConfiguration,
    PermissionConfiguration,
    SecurityPolicyConfiguration,
    UnknownFieldCompatibility,
}

/// Whether this validator implements a verified rule for an area.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageStatus {
    /// At least the identified rule is implemented and backed by a verified source.
    Validated,
    /// No verified rule set is currently implemented for this area.
    ///
    /// This is not a blanket rejection of fields in that area.
    NotValidatedSpecificationGap,
}

/// One stable, documented statement about validator coverage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ValidationCoverage {
    area: ValidationArea,
    status: CoverageStatus,
    rule_or_gap_id: &'static str,
    explanation: &'static str,
}

impl ValidationCoverage {
    #[must_use]
    pub const fn area(&self) -> ValidationArea {
        self.area
    }

    #[must_use]
    pub const fn status(&self) -> CoverageStatus {
        self.status
    }

    #[must_use]
    pub const fn rule_or_gap_id(&self) -> &'static str {
        self.rule_or_gap_id
    }

    #[must_use]
    pub const fn explanation(&self) -> &'static str {
        self.explanation
    }
}

const VALIDATION_COVERAGE: &[ValidationCoverage] = &[
    ValidationCoverage {
        area: ValidationArea::InvocationBudget,
        status: CoverageStatus::Validated,
        rule_or_gap_id: "M0-R3-ADDENDUM-5",
        explanation: "limits.max_tokens_per_invocation must be a positive JSON integer",
    },
    ValidationCoverage {
        area: ValidationArea::ObjectSchema,
        status: CoverageStatus::NotValidatedSpecificationGap,
        rule_or_gap_id: "GAP-044-01",
        explanation: "the complete object/configuration semantic schema is unavailable",
    },
    ValidationCoverage {
        area: ValidationArea::ToolConfiguration,
        status: CoverageStatus::NotValidatedSpecificationGap,
        rule_or_gap_id: "GAP-044-02",
        explanation: "tool declaration semantics and supported configuration are not established",
    },
    ValidationCoverage {
        area: ValidationArea::ProviderConfiguration,
        status: CoverageStatus::NotValidatedSpecificationGap,
        rule_or_gap_id: "GAP-044-03",
        explanation: "provider configuration and compatibility semantics are not established",
    },
    ValidationCoverage {
        area: ValidationArea::PermissionConfiguration,
        status: CoverageStatus::NotValidatedSpecificationGap,
        rule_or_gap_id: "GAP-044-04",
        explanation: "permission configuration semantics are not established",
    },
    ValidationCoverage {
        area: ValidationArea::SecurityPolicyConfiguration,
        status: CoverageStatus::NotValidatedSpecificationGap,
        rule_or_gap_id: "GAP-044-04",
        explanation: "security-policy configuration semantics are not established",
    },
    ValidationCoverage {
        area: ValidationArea::UnknownFieldCompatibility,
        status: CoverageStatus::NotValidatedSpecificationGap,
        rule_or_gap_id: "GAP-044-05",
        explanation: "unknown-field and extension compatibility policy is not established",
    },
];

/// Informational, immutable description of the validator's actual rule coverage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ValidationReport {
    validator_version: &'static str,
    coverage: &'static [ValidationCoverage],
}

impl ValidationReport {
    #[must_use]
    pub const fn validator_version(&self) -> &'static str {
        self.validator_version
    }

    #[must_use]
    pub const fn coverage(&self) -> &'static [ValidationCoverage] {
        self.coverage
    }

    /// True only when every reported area has verified validation coverage.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.coverage
            .iter()
            .all(|entry| entry.status() == CoverageStatus::Validated)
    }
}

/// Result of bounded validation: opaque publication evidence plus coverage metadata.
#[must_use = "inspect the validation report and pass the evidence to the publication Store"]
#[derive(Debug, PartialEq, Eq)]
pub struct ValidationOutcome {
    evidence: ValidationEvidence,
    report: ValidationReport,
}

impl ValidationOutcome {
    #[must_use]
    pub const fn evidence(&self) -> &ValidationEvidence {
        &self.evidence
    }

    #[must_use]
    pub const fn report(&self) -> &ValidationReport {
        &self.report
    }

    /// Splits the outcome for trusted callers that need both values separately.
    #[must_use = "retain the validation evidence and its coverage report"]
    pub fn into_parts(self) -> (ValidationEvidence, ValidationReport) {
        (self.evidence, self.report)
    }

    /// Consumes the report and returns only the opaque evidence.
    ///
    /// Prefer retaining the report when presenting validation status to an
    /// operator or API caller; this method exists for existing trusted flows.
    #[must_use = "publication evidence must be checked by the Store"]
    pub fn into_evidence(self) -> ValidationEvidence {
        self.evidence
    }
}

/// Trusted semantic checks for the currently confirmed snapshot invariants.
#[derive(Clone, Copy, Debug, Default)]
pub struct ObjectValidator;

impl ObjectValidator {
    /// Backwards-compatible evidence-only entry point.
    ///
    /// New callers that need to present truthful coverage should use
    /// `validate_with_report()`.
    pub fn validate(snapshot: &Snapshot) -> Result<ValidationEvidence, ObjectValidationError> {
        Self::validate_with_report(snapshot).map(ValidationOutcome::into_evidence)
    }

    /// Validates confirmed rules and returns evidence plus explicit coverage.
    ///
    /// This does not claim that tools, providers, permissions, security policies,
    /// or unknown extensions have been fully validated.
    pub fn validate_with_report(
        snapshot: &Snapshot,
    ) -> Result<ValidationOutcome, ObjectValidationError> {
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
        // fractional numbers, negatives, and out-of-range values.
        let max_tokens_per_invocation = value
            .as_u64()
            .filter(|limit| *limit > 0)
            .ok_or(ObjectValidationError::MaxTokensPerInvocationMustBePositiveInteger)?;

        let evidence = ValidationEvidence {
            tenant_id: snapshot.tenant_id(),
            object_id: snapshot.object_id(),
            object_version_id: snapshot.object_version_id(),
            snapshot_id: snapshot.id(),
            content_hash: *snapshot.content_hash(),
            max_tokens_per_invocation,
            validator_version: OBJECT_VALIDATOR_VERSION,
        };
        let report = ValidationReport {
            validator_version: OBJECT_VALIDATOR_VERSION,
            coverage: VALIDATION_COVERAGE,
        };

        Ok(ValidationOutcome { evidence, report })
    }
}

/// Opaque proof that the bounded validator accepted one exact snapshot.
///
/// Fields are private and this type intentionally does not implement
/// Serialize/Deserialize. Store adapters must still verify the identity tuple
/// and content hash against persisted rows in the publication transaction.
#[must_use = "validation evidence must be checked by the publication Store"]
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
    fn report_lists_the_confirmed_rule_and_every_open_semantic_gap() {
        let snapshot = snapshot(br#"{"limits":{"max_tokens_per_invocation":4096}}"#);
        let outcome = ObjectValidator::validate_with_report(&snapshot)
            .expect("valid budget is accepted with a coverage report");
        let report = outcome.report();

        assert_eq!(report.validator_version(), OBJECT_VALIDATOR_VERSION);
        assert!(!report.is_complete());
        assert_eq!(report.coverage().len(), 7);
        assert_eq!(outcome.evidence().max_tokens_per_invocation(), 4096);
        assert!(outcome.evidence().matches_snapshot(&snapshot));

        assert_eq!(
            report.coverage()[0].area(),
            ValidationArea::InvocationBudget
        );
        assert_eq!(report.coverage()[0].status(), CoverageStatus::Validated);
        assert_eq!(report.coverage()[0].rule_or_gap_id(), "M0-R3-ADDENDUM-5");

        let gap_ids: Vec<_> = report.coverage()[1..]
            .iter()
            .map(ValidationCoverage::rule_or_gap_id)
            .collect();
        assert_eq!(
            gap_ids,
            vec![
                "GAP-044-01",
                "GAP-044-02",
                "GAP-044-03",
                "GAP-044-04",
                "GAP-044-04",
                "GAP-044-05",
            ]
        );
        assert!(
            report.coverage()[1..]
                .iter()
                .all(|entry| entry.status() == CoverageStatus::NotValidatedSpecificationGap)
        );
    }

    #[test]
    fn evidence_only_entry_point_remains_compatible() {
        let snapshot = snapshot(br#"{"limits":{"max_tokens_per_invocation":4096}}"#);
        let evidence = ObjectValidator::validate(&snapshot).expect("valid budget is accepted");
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
        for value in ["0", "-1", "1.5", r#""10""#] {
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
