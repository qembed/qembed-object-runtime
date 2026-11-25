//! TenantOwner-only promotion of an unverified memory into a new Verified row.
use std::{error::Error, fmt};

use qembed_domain::{
    ids::ObjectId,
    memory::{MemoryKind, MemoryPromotionValidationError, MemorySourceKind, MemoryVerification, validate_owner_promotion},
    principal::PrincipalKind,
    scope::Scope,
};
use sqlx::types::Uuid;

use crate::PgContext;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromotedMemory { id: Uuid, candidate_id: Uuid, claim: String }
impl PromotedMemory {
    #[must_use] pub const fn id(&self) -> Uuid { self.id }
    #[must_use] pub const fn candidate_id(&self) -> Uuid { self.candidate_id }
    #[must_use] pub fn claim(&self) -> &str { &self.claim }
}

#[derive(Clone)]
pub struct PgMemoryPromotionRepository { context: PgContext }
impl PgMemoryPromotionRepository {
    #[must_use]
    pub fn new(context: PgContext) -> Self { Self { context } }

    /// Inserts a new Owner-authored Verified row; the candidate is never updated.
    /// FOR UPDATE on the candidate serializes competing promotion requests.
    pub async fn promote(
        &self, scope: &Scope, object_id: ObjectId, candidate_id: Uuid,
    ) -> Result<PromotedMemory, MemoryPromotionError> {
        if scope.principal().kind() != PrincipalKind::TenantOwner {
            return Err(MemoryPromotionError::NotTenantOwner);
        }
        let mut transaction = self.context.begin_principal(scope).await
            .map_err(MemoryPromotionError::Context)?;
        let candidate = sqlx::query_as::<_, (String, String, String, String)>(
            "SELECT claim, memory_kind, source_kind, verification \
               FROM memories \
              WHERE tenant_id = $1::uuid AND object_id = $2::uuid AND id = $3 \
              FOR UPDATE",
        )
        .bind(scope.tenant_id().to_string()).bind(object_id.to_string()).bind(candidate_id)
        .fetch_optional(transaction.connection()).await
        .map_err(MemoryPromotionError::Database)?
        .ok_or(MemoryPromotionError::CandidateNotFound)?;
        let (claim, raw_kind, raw_source, raw_verification) = candidate;
        let kind = MemoryKind::parse(&raw_kind).ok_or(MemoryPromotionError::InvalidStoredState)?;
        let source = MemorySourceKind::parse(&raw_source).ok_or(MemoryPromotionError::InvalidStoredState)?;
        let verification = MemoryVerification::parse(&raw_verification).ok_or(MemoryPromotionError::InvalidStoredState)?;
        validate_owner_promotion(scope.principal().kind(), kind, source, verification)
            .map_err(MemoryPromotionError::CandidateRejected)?;

        let existing: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM memories WHERE tenant_id = $1::uuid AND object_id = $2::uuid \
               AND supersedes_id = $3 AND source_kind = 'Owner' AND verification = 'Verified' LIMIT 1",
        )
        .bind(scope.tenant_id().to_string()).bind(object_id.to_string()).bind(candidate_id)
        .fetch_optional(transaction.connection()).await
        .map_err(MemoryPromotionError::Database)?;
        if existing.is_some() { return Err(MemoryPromotionError::AlreadyPromoted); }

        let (id, persisted_claim): (Uuid, String) = sqlx::query_as(
            "INSERT INTO memories \
             (id, tenant_id, object_id, memory_kind, claim, source_kind, verification, confidence, supersedes_id, verified_by, verified_at) \
             VALUES ($1, $2::uuid, $3::uuid, 'semantic', $4, 'Owner', 'Verified', 1.0, $5, $6::uuid, clock_timestamp()) \
             RETURNING id, claim",
        )
        .bind(Uuid::new_v4()).bind(scope.tenant_id().to_string()).bind(object_id.to_string())
        .bind(&claim).bind(candidate_id).bind(scope.principal().principal_id().to_string())
        .fetch_one(transaction.connection()).await
        .map_err(MemoryPromotionError::Database)?;
        transaction.commit().await.map_err(MemoryPromotionError::Database)?;
        Ok(PromotedMemory { id, candidate_id, claim: persisted_claim })
    }
}

#[derive(Debug)]
pub enum MemoryPromotionError {
    NotTenantOwner, CandidateNotFound, CandidateRejected(MemoryPromotionValidationError),
    AlreadyPromoted, InvalidStoredState, Context(crate::ContextTransactionError), Database(sqlx::Error),
}
impl fmt::Display for MemoryPromotionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotTenantOwner => f.write_str("only a TenantOwner may promote memory"),
            Self::CandidateNotFound => f.write_str("memory candidate was not found"),
            Self::CandidateRejected(e) => write!(f, "memory candidate cannot be promoted: {e:?}"),
            Self::AlreadyPromoted => f.write_str("memory candidate has already been promoted"),
            Self::InvalidStoredState => f.write_str("memory candidate has an unknown persisted state"),
            Self::Context(e) => write!(f, "memory promotion context failed: {e}"),
            Self::Database(e) => write!(f, "memory promotion database error: {e}"),
        }
    }
}
impl Error for MemoryPromotionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CandidateRejected(e) => Some(e), Self::Context(e) => Some(e),
            Self::Database(e) => Some(e), _ => None,
        }
    }
}
