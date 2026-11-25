//! TenantOwner-only promotion of an unverified memory into a new Verified row.

use std::{error::Error, fmt};

use qembed_domain::{
    ids::ObjectId,
    memory::{
        MemoryKind, MemoryPromotionValidationError, MemorySourceKind, MemoryVerification,
        validate_owner_promotion,
    },
    principal::PrincipalKind,
    scope::Scope,
};
use sqlx::types::Uuid;

use crate::PgContext;

/// The newly inserted Owner-authored Verified memory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromotedMemory {
    id: Uuid,
    candidate_id: Uuid,
    claim: String,
}

impl PromotedMemory {
    /// Returns the new Verified memory ID.
    #[must_use]
    pub const fn id(&self) -> Uuid {
        self.id
    }

    /// Returns the candidate ID linked through supersedes_id.
    #[must_use]
    pub const fn candidate_id(&self) -> Uuid {
        self.candidate_id
    }

    /// Returns the Owner-authored claim copied from the candidate.
    #[must_use]
    pub fn claim(&self) -> &str {
        &self.claim
    }
}

/// PostgreSQL adapter for Owner-authored memory promotion.
#[derive(Clone)]
pub struct PgMemoryPromotionRepository {
    context: PgContext,
}

impl PgMemoryPromotionRepository {
    /// Creates the repository with the trusted PostgreSQL context.
    #[must_use]
    pub fn new(context: PgContext) -> Self {
        Self { context }
    }

    /// Inserts a new Owner-authored Verified row; the candidate is never updated.
    ///
    /// FOR UPDATE on the candidate serializes competing promotion requests.
    pub async fn promote(
        &self,
        scope: &Scope,
        object_id: ObjectId,
        candidate_id: Uuid,
    ) -> Result<PromotedMemory, MemoryPromotionError> {
        if scope.principal().kind() != PrincipalKind::TenantOwner {
            return Err(MemoryPromotionError::NotTenantOwner);
        }

        let mut transaction = self
            .context
            .begin_principal(scope)
            .await
            .map_err(MemoryPromotionError::Context)?;

        let candidate = sqlx::query_as::<_, (String, String, String, String)>(
            "SELECT claim, memory_kind, source_kind, verification \
               FROM memories \
              WHERE tenant_id = $1::uuid AND object_id = $2::uuid AND id = $3 \
              FOR UPDATE",
        )
        .bind(scope.tenant_id().to_string())
        .bind(object_id.to_string())
        .bind(candidate_id)
        .fetch_optional(transaction.connection())
        .await
        .map_err(MemoryPromotionError::Database)?
        .ok_or(MemoryPromotionError::CandidateNotFound)?;

        let (claim, raw_kind, raw_source, raw_verification) = candidate;
        let kind = MemoryKind::parse(&raw_kind).ok_or(MemoryPromotionError::InvalidStoredState)?;
        let source =
            MemorySourceKind::parse(&raw_source).ok_or(MemoryPromotionError::InvalidStoredState)?;
        let verification = MemoryVerification::parse(&raw_verification)
            .ok_or(MemoryPromotionError::InvalidStoredState)?;

        validate_owner_promotion(scope.principal().kind(), kind, source, verification)
            .map_err(MemoryPromotionError::CandidateRejected)?;

        let existing: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM memories \
              WHERE tenant_id = $1::uuid AND object_id = $2::uuid \
                AND supersedes_id = $3 AND source_kind = 'Owner' AND verification = 'Verified' \
              LIMIT 1",
        )
        .bind(scope.tenant_id().to_string())
        .bind(object_id.to_string())
        .bind(candidate_id)
        .fetch_optional(transaction.connection())
        .await
        .map_err(MemoryPromotionError::Database)?;

        if existing.is_some() {
            return Err(MemoryPromotionError::AlreadyPromoted);
        }

        let (id, persisted_claim): (Uuid, String) = sqlx::query_as(
            "INSERT INTO memories \
             (id, tenant_id, object_id, memory_kind, claim, source_kind, verification, confidence, supersedes_id, verified_by, verified_at) \
             VALUES ($1, $2::uuid, $3::uuid, 'semantic', $4, 'Owner', 'Verified', 1.0, $5, $6::uuid, clock_timestamp()) \
             RETURNING id, claim",
        )
        .bind(Uuid::new_v4())
        .bind(scope.tenant_id().to_string())
        .bind(object_id.to_string())
        .bind(&claim)
        .bind(candidate_id)
        .bind(scope.principal().principal_id().to_string())
        .fetch_one(transaction.connection())
        .await
        .map_err(MemoryPromotionError::Database)?;

        transaction
            .commit()
            .await
            .map_err(MemoryPromotionError::Database)?;

        Ok(PromotedMemory {
            id,
            candidate_id,
            claim: persisted_claim,
        })
    }
}

/// Failure while promoting a memory candidate.
#[derive(Debug)]
pub enum MemoryPromotionError {
    /// The trusted Principal is not a TenantOwner.
    NotTenantOwner,
    /// No candidate with the supplied tenant, Object, and ID was visible.
    CandidateNotFound,
    /// The candidate failed domain eligibility rules.
    CandidateRejected(MemoryPromotionValidationError),
    /// An Owner/Verified row already supersedes this candidate.
    AlreadyPromoted,
    /// The database returned an unknown persisted enum value.
    InvalidStoredState,
    /// The signed Principal context could not be established.
    Context(crate::ContextTransactionError),
    /// PostgreSQL rejected the operation.
    Database(sqlx::Error),
}

impl fmt::Display for MemoryPromotionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotTenantOwner => formatter.write_str("only a TenantOwner may promote memory"),
            Self::CandidateNotFound => formatter.write_str("memory candidate was not found"),
            Self::CandidateRejected(error) => {
                write!(formatter, "memory candidate cannot be promoted: {error}")
            }
            Self::AlreadyPromoted => formatter.write_str("memory candidate has already been promoted"),
            Self::InvalidStoredState => {
                formatter.write_str("memory candidate has an unknown persisted state")
            }
            Self::Context(error) => write!(formatter, "memory promotion context failed: {error}"),
            Self::Database(error) => write!(formatter, "memory promotion database error: {error}"),
        }
    }
}

impl Error for MemoryPromotionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CandidateRejected(error) => Some(error),
            Self::Context(error) => Some(error),
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}
