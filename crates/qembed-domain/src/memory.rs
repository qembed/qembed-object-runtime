//! Domain validation for promoting unverified memory into Owner-authored truth.

use std::fmt;

use crate::principal::PrincipalKind;

/// Memory category persisted by the canonical Qembed SQL schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryKind {
    /// Tenant-scoped knowledge shared by the tenant.
    Semantic,
    /// Caller-scoped conversational memory.
    Episodic,
}

impl MemoryKind {
    /// Parses the canonical SQL value.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "semantic" => Some(Self::Semantic),
            "episodic" => Some(Self::Episodic),
            _ => None,
        }
    }
}

/// Provenance category persisted by the canonical Qembed SQL schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemorySourceKind {
    /// A claim authored by the tenant Owner.
    Owner,
    /// A claim extracted from a trusted document.
    TrustedDocument,
    /// A claim supplied by a user.
    User,
    /// A claim supplied by a third party.
    ThirdParty,
    /// A claim observed from a tool.
    Tool,
    /// A claim inferred by the runtime.
    RuntimeInference,
}

impl MemorySourceKind {
    /// Parses the canonical SQL value.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "Owner" => Some(Self::Owner),
            "TrustedDocument" => Some(Self::TrustedDocument),
            "User" => Some(Self::User),
            "ThirdParty" => Some(Self::ThirdParty),
            "Tool" => Some(Self::Tool),
            "RuntimeInference" => Some(Self::RuntimeInference),
            _ => None,
        }
    }
}

/// Verification state persisted by the canonical Qembed SQL schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryVerification {
    /// Not yet confirmed by the tenant Owner.
    Unverified,
    /// Confirmed and authored as a new Owner row.
    Verified,
    /// Observed by a tool.
    Observed,
    /// Explicitly rejected.
    Rejected,
}

impl MemoryVerification {
    /// Parses the canonical SQL value.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "Unverified" => Some(Self::Unverified),
            "Verified" => Some(Self::Verified),
            "Observed" => Some(Self::Observed),
            "Rejected" => Some(Self::Rejected),
            _ => None,
        }
    }
}

/// Validates whether a trusted Principal may promote the given candidate.
///
/// Owner approval of a User/ThirdParty candidate creates a new semantic row.
/// Episodic memories cannot be Verified under the canonical SQL contract.
pub fn validate_owner_promotion(
    principal_kind: PrincipalKind,
    memory_kind: MemoryKind,
    source_kind: MemorySourceKind,
    verification: MemoryVerification,
) -> Result<(), MemoryPromotionValidationError> {
    if principal_kind != PrincipalKind::TenantOwner {
        return Err(MemoryPromotionValidationError::NotTenantOwner);
    }
    if memory_kind != MemoryKind::Semantic {
        return Err(MemoryPromotionValidationError::EpisodicCandidate);
    }
    if !matches!(
        source_kind,
        MemorySourceKind::User | MemorySourceKind::ThirdParty
    ) {
        return Err(MemoryPromotionValidationError::UnsupportedCandidateSource);
    }
    if verification != MemoryVerification::Unverified {
        return Err(MemoryPromotionValidationError::CandidateNotUnverified);
    }
    Ok(())
}

/// Why a candidate cannot be promoted into an Owner-authored Verified row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryPromotionValidationError {
    /// The authenticated Principal is not a TenantOwner.
    NotTenantOwner,
    /// Episodic memory is not eligible for Verified state.
    EpisodicCandidate,
    /// Only User and ThirdParty candidates are eligible.
    UnsupportedCandidateSource,
    /// The candidate is not in Unverified state.
    CandidateNotUnverified,
}

impl fmt::Display for MemoryPromotionValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotTenantOwner => formatter.write_str("principal is not a TenantOwner"),
            Self::EpisodicCandidate => formatter.write_str("episodic memory cannot be Verified"),
            Self::UnsupportedCandidateSource => {
                formatter.write_str("candidate source is not eligible for Owner promotion")
            }
            Self::CandidateNotUnverified => formatter.write_str("candidate is not Unverified"),
        }
    }
}

impl std::error::Error for MemoryPromotionValidationError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_can_promote_user_and_third_party_semantic_candidates() {
        for source in [MemorySourceKind::User, MemorySourceKind::ThirdParty] {
            assert_eq!(
                validate_owner_promotion(
                    PrincipalKind::TenantOwner,
                    MemoryKind::Semantic,
                    source,
                    MemoryVerification::Unverified
                ),
                Ok(())
            );
        }
    }

    #[test]
    fn authenticated_non_owner_cannot_promote() {
        assert_eq!(
            validate_owner_promotion(
                PrincipalKind::Authenticated,
                MemoryKind::Semantic,
                MemorySourceKind::User,
                MemoryVerification::Unverified
            ),
            Err(MemoryPromotionValidationError::NotTenantOwner)
        );
    }

    #[test]
    fn episodic_candidate_is_rejected() {
        assert_eq!(
            validate_owner_promotion(
                PrincipalKind::TenantOwner,
                MemoryKind::Episodic,
                MemorySourceKind::User,
                MemoryVerification::Unverified
            ),
            Err(MemoryPromotionValidationError::EpisodicCandidate)
        );
    }

    #[test]
    fn only_unverified_candidates_can_be_promoted() {
        for verification in [
            MemoryVerification::Verified,
            MemoryVerification::Rejected,
            MemoryVerification::Observed,
        ] {
            assert_eq!(
                validate_owner_promotion(
                    PrincipalKind::TenantOwner,
                    MemoryKind::Semantic,
                    MemorySourceKind::User,
                    verification
                ),
                Err(MemoryPromotionValidationError::CandidateNotUnverified)
            );
        }
    }

    #[test]
    fn owner_and_trusted_document_rows_are_not_candidates() {
        for source in [
            MemorySourceKind::Owner,
            MemorySourceKind::TrustedDocument,
            MemorySourceKind::Tool,
            MemorySourceKind::RuntimeInference,
        ] {
            assert_eq!(
                validate_owner_promotion(
                    PrincipalKind::TenantOwner,
                    MemoryKind::Semantic,
                    source,
                    MemoryVerification::Unverified
                ),
                Err(MemoryPromotionValidationError::UnsupportedCandidateSource)
            );
        }
    }
}
