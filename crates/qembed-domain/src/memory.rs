//! Domain validation for promoting unverified memory into Owner-authored truth.
use crate::principal::PrincipalKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryKind { Semantic, Episodic }
impl MemoryKind {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value { "semantic" => Some(Self::Semantic), "episodic" => Some(Self::Episodic), _ => None }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemorySourceKind { Owner, TrustedDocument, User, ThirdParty, Tool, RuntimeInference }
impl MemorySourceKind {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "Owner" => Some(Self::Owner), "TrustedDocument" => Some(Self::TrustedDocument),
            "User" => Some(Self::User), "ThirdParty" => Some(Self::ThirdParty),
            "Tool" => Some(Self::Tool), "RuntimeInference" => Some(Self::RuntimeInference), _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryVerification { Unverified, Verified, Observed, Rejected }
impl MemoryVerification {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "Unverified" => Some(Self::Unverified), "Verified" => Some(Self::Verified),
            "Observed" => Some(Self::Observed), "Rejected" => Some(Self::Rejected), _ => None,
        }
    }
}

/// Owner approval of a User/ThirdParty candidate creates a NEW semantic row.
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
    if !matches!(source_kind, MemorySourceKind::User | MemorySourceKind::ThirdParty) {
        return Err(MemoryPromotionValidationError::UnsupportedCandidateSource);
    }
    if verification != MemoryVerification::Unverified {
        return Err(MemoryPromotionValidationError::CandidateNotUnverified);
    }
    Ok(())
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryPromotionValidationError {
    NotTenantOwner, EpisodicCandidate, UnsupportedCandidateSource, CandidateNotUnverified,
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn owner_can_promote_user_and_third_party_semantic_candidates() {
        for source in [MemorySourceKind::User, MemorySourceKind::ThirdParty] {
            assert_eq!(validate_owner_promotion(PrincipalKind::TenantOwner, MemoryKind::Semantic, source, MemoryVerification::Unverified), Ok(()));
        }
    }
    #[test]
    fn authenticated_non_owner_cannot_promote() {
        assert_eq!(validate_owner_promotion(PrincipalKind::Authenticated, MemoryKind::Semantic, MemorySourceKind::User, MemoryVerification::Unverified), Err(MemoryPromotionValidationError::NotTenantOwner));
    }
    #[test]
    fn episodic_candidate_is_rejected() {
        assert_eq!(validate_owner_promotion(PrincipalKind::TenantOwner, MemoryKind::Episodic, MemorySourceKind::User, MemoryVerification::Unverified), Err(MemoryPromotionValidationError::EpisodicCandidate));
    }
    #[test]
    fn only_unverified_candidates_can_be_promoted() {
        for verification in [MemoryVerification::Verified, MemoryVerification::Rejected, MemoryVerification::Observed] {
            assert_eq!(validate_owner_promotion(PrincipalKind::TenantOwner, MemoryKind::Semantic, MemorySourceKind::User, verification), Err(MemoryPromotionValidationError::CandidateNotUnverified));
        }
    }
    #[test]
    fn owner_and_trusted_document_rows_are_not_candidates() {
        for source in [MemorySourceKind::Owner, MemorySourceKind::TrustedDocument, MemorySourceKind::Tool, MemorySourceKind::RuntimeInference] {
            assert_eq!(validate_owner_promotion(PrincipalKind::TenantOwner, MemoryKind::Semantic, source, MemoryVerification::Unverified), Err(MemoryPromotionValidationError::UnsupportedCandidateSource));
        }
    }
}
