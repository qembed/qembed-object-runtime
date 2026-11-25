//! Caller-isolated episodic memory access through the signed application context.

use std::{error::Error, fmt};

use qembed_domain::{
    ids::{CallerId, ObjectId},
    scope::Scope,
};
use sqlx::types::Uuid;

use crate::{PgContext, caller::ResolvedCaller};

pub const MAX_EPISODIC_CLAIM_BYTES: usize = 8 * 1024;

/// A persisted episodic memory visible to the currently resolved Caller.
///
/// Provenance fields are deliberately not accepted from callers when writing
/// memories. User-authored input is always stored as User / Unverified.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EpisodicMemory {
    id: Uuid,
    claim: String,
    source_kind: String,
    verification: String,
}

impl EpisodicMemory {
    #[must_use]
    pub const fn id(&self) -> Uuid {
        self.id
    }

    #[must_use]
    pub fn claim(&self) -> &str {
        &self.claim
    }

    #[must_use]
    pub fn source_kind(&self) -> &str {
        &self.source_kind
    }

    #[must_use]
    pub fn verification(&self) -> &str {
        &self.verification
    }
}

/// PostgreSQL adapter for Caller-scoped episodic memory.
#[derive(Clone)]
pub struct PgEpisodicMemoryRepository {
    context: PgContext,
}

impl PgEpisodicMemoryRepository {
    #[must_use]
    pub fn new(context: PgContext) -> Self {
        Self { context }
    }

    /// Persists a user-authored episodic claim for the database-resolved Caller.
    ///
    /// The Caller ID is resolved from the authenticated Principal's active
    /// delegation inside the signed transaction. The request cannot provide
    /// source_kind, verification, or a different effective Caller.
    pub async fn record_user_claim(
        &self,
        scope: &Scope,
        requested_caller_id: CallerId,
        object_id: ObjectId,
        claim: &str,
    ) -> Result<Uuid, EpisodicMemoryError> {
        validate_claim(claim)?;

        let (mut transaction, caller) = self
            .context
            .begin_caller(scope, requested_caller_id)
            .await
            .map_err(EpisodicMemoryError::Context)?;
        ensure_caller_binding(&transaction, &caller)?;

        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO memories \
             (id, tenant_id, object_id, caller_id, memory_kind, claim, source_kind, verification, confidence) \
             VALUES (gen_random_uuid(), $1::uuid, $2::uuid, $3::uuid, 'episodic', $4, 'User', 'Unverified', 0.5) \
             RETURNING id",
        )
        .bind(scope.tenant_id().to_string())
        .bind(object_id.to_string())
        .bind(caller.caller_id().to_string())
        .bind(claim)
        .fetch_one(transaction.connection())
        .await
        .map_err(EpisodicMemoryError::Database)?;

        transaction
            .commit()
            .await
            .map_err(EpisodicMemoryError::Database)?;
        Ok(id)
    }

    /// Lists only unexpired episodic memories belonging to the resolved Caller.
    ///
    /// The selected Caller is validated against the authenticated Principal's
    /// active delegation before SQL executes. The explicit Caller predicate is
    /// defense in depth; PostgreSQL RLS independently applies the signed
    /// transaction's Caller context to memories and dependent rows.
    pub async fn list_episodic(
        &self,
        scope: &Scope,
        requested_caller_id: CallerId,
        object_id: ObjectId,
    ) -> Result<Vec<EpisodicMemory>, EpisodicMemoryError> {
        let (mut transaction, caller) = self
            .context
            .begin_caller(scope, requested_caller_id)
            .await
            .map_err(EpisodicMemoryError::Context)?;
        ensure_caller_binding(&transaction, &caller)?;

        let rows: Vec<(Uuid, String, String, String)> = sqlx::query_as(
            "SELECT id, claim, source_kind, verification \
               FROM memories \
              WHERE tenant_id = $1::uuid AND object_id = $2::uuid \
                AND caller_id = $3::uuid AND memory_kind = 'episodic' \
                AND (expires_at IS NULL OR expires_at > clock_timestamp()) \
              ORDER BY id",
        )
        .bind(scope.tenant_id().to_string())
        .bind(object_id.to_string())
        .bind(caller.caller_id().to_string())
        .fetch_all(transaction.connection())
        .await
        .map_err(EpisodicMemoryError::Database)?;

        transaction
            .commit()
            .await
            .map_err(EpisodicMemoryError::Database)?;

        Ok(rows
            .into_iter()
            .map(|(id, claim, source_kind, verification)| EpisodicMemory {
                id,
                claim,
                source_kind,
                verification,
            })
            .collect())
    }
}

fn ensure_caller_binding(
    transaction: &crate::ContextTransaction,
    caller: &ResolvedCaller,
) -> Result<(), EpisodicMemoryError> {
    if caller.is_bound_to_transaction(transaction) {
        Ok(())
    } else {
        Err(EpisodicMemoryError::CallerContextMismatch)
    }
}

fn validate_claim(claim: &str) -> Result<(), EpisodicMemoryError> {
    if claim.trim().is_empty() {
        return Err(EpisodicMemoryError::EmptyClaim);
    }
    if claim.len() > MAX_EPISODIC_CLAIM_BYTES {
        return Err(EpisodicMemoryError::ClaimTooLarge);
    }
    if claim.chars().any(char::is_control) {
        return Err(EpisodicMemoryError::InvalidClaimCharacters);
    }
    Ok(())
}

/// Failure while recording or reading episodic memory.
#[derive(Debug)]
pub enum EpisodicMemoryError {
    Context(crate::ContextTransactionError),
    Database(sqlx::Error),
    CallerContextMismatch,
    EmptyClaim,
    ClaimTooLarge,
    InvalidClaimCharacters,
}

impl fmt::Display for EpisodicMemoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Context(error) => write!(formatter, "episodic memory context failed: {error}"),
            Self::Database(error) => write!(formatter, "episodic memory database error: {error}"),
            Self::CallerContextMismatch => {
                formatter.write_str("resolved Caller is not bound to this transaction")
            }
            Self::EmptyClaim => formatter.write_str("episodic memory claim must not be empty"),
            Self::ClaimTooLarge => formatter.write_str("episodic memory claim exceeds the byte limit"),
            Self::InvalidClaimCharacters => {
                formatter.write_str("episodic memory claim contains control characters")
            }
        }
    }
}

impl Error for EpisodicMemoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Context(error) => Some(error),
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_claim_validation_is_bounded_and_rejects_controls() {
        assert!(matches!(validate_claim(""), Err(EpisodicMemoryError::EmptyClaim)));
        assert!(matches!(
            validate_claim(&"x".repeat(MAX_EPISODIC_CLAIM_BYTES + 1)),
            Err(EpisodicMemoryError::ClaimTooLarge)
        ));
        assert!(matches!(
            validate_claim("first\u{0000}second"),
            Err(EpisodicMemoryError::InvalidClaimCharacters)
        ));
        assert!(validate_claim("user supplied fact").is_ok());
    }
}
