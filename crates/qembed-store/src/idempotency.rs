//! Tenant-scoped idempotency reservation for begin-invocation.
//!
//! The canonical SQL contract lives in qembed-sql:
//! `invocation_idempotency(tenant_id, object_id, caller_id, idempotency_key,
//! payload_hash, invocation_id, created_at, expires_at)`. The composite
//! foreign key to `object_invocations` is DEFERRABLE INITIALLY DEFERRED so a
//! begin-invocation transaction can claim the key first and insert the
//! invocation before commit.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::caller::ResolvedCaller;
use qembed_domain::ids::{InvocationId, ObjectId};
use qembed_domain::scope::Scope;

use crate::{TenantTransaction, fetch_optional};

/// Inputs for one caller-scoped idempotency reservation.
#[derive(Clone, Copy, Debug)]
pub struct IdempotencyClaim<'a> {
    object_id: ObjectId,
    idempotency_key: &'a str,
    payload_hash: &'a [u8],
    proposed_invocation_id: InvocationId,
    expires_at: SystemTime,
}

impl<'a> IdempotencyClaim<'a> {
    /// Returns the object whose idempotency namespace is being reserved.
    #[must_use]
    pub const fn object_id(&self) -> ObjectId {
        self.object_id
    }

    /// Creates a reservation claim without choosing hash or expiry policy.
    #[must_use]
    pub const fn new(
        object_id: ObjectId,
        idempotency_key: &'a str,
        payload_hash: &'a [u8],
        proposed_invocation_id: InvocationId,
        expires_at: SystemTime,
    ) -> Self {
        Self {
            object_id,
            idempotency_key,
            payload_hash,
            proposed_invocation_id,
            expires_at,
        }
    }
}

/// Outcome of reserving an idempotency key inside a begin-invocation transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdempotencyReservation {
    /// This transaction acquired the key and must insert this invocation before
    /// committing, satisfying the deferred foreign key.
    Reserved { invocation_id: InvocationId },
    /// The same key and payload were already committed; reuse this invocation.
    Replay { invocation_id: InvocationId },
}

/// Failure while reserving an invocation idempotency key.
#[derive(Debug)]
pub enum IdempotencyReservationError {
    /// The key already exists but belongs to a different request payload.
    PayloadMismatch,
    /// The resolved Caller does not belong to this tenant/Principal transaction.
    CallerContextMismatch,
    /// The supplied expiration cannot be represented by PostgreSQL's timestamp.
    ExpiryOutOfRange,
    /// A persisted invocation identifier could not be parsed.
    InvalidPersistedInvocationId,
    /// PostgreSQL rejected or could not complete the reservation operation.
    Database(sqlx::Error),
}

impl fmt::Display for IdempotencyReservationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PayloadMismatch => {
                formatter.write_str("idempotency key was already used with a different payload")
            }
            Self::CallerContextMismatch => {
                formatter.write_str("resolved Caller belongs to a different transaction context")
            }
            Self::ExpiryOutOfRange => {
                formatter.write_str("idempotency expiration is outside the supported range")
            }
            Self::InvalidPersistedInvocationId => {
                formatter.write_str("persisted idempotency record has an invalid invocation ID")
            }
            Self::Database(error) => {
                write!(formatter, "idempotency reservation database error: {error}")
            }
        }
    }
}

impl std::error::Error for IdempotencyReservationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::PayloadMismatch
            | Self::CallerContextMismatch
            | Self::ExpiryOutOfRange
            | Self::InvalidPersistedInvocationId => None,
        }
    }
}

// Converts the supplied absolute expiry to PostgreSQL epoch microseconds.
fn expiry_unix_micros(expires_at: SystemTime) -> Result<i64, IdempotencyReservationError> {
    let micros = expires_at
        .duration_since(UNIX_EPOCH)
        .map_err(|_| IdempotencyReservationError::ExpiryOutOfRange)?
        .as_micros();
    i64::try_from(micros).map_err(|_| IdempotencyReservationError::ExpiryOutOfRange)
}

/// Reserves an invocation key before the invocation row is inserted.
///
/// The caller must pass a transaction opened with `begin_tenant_transaction`
/// for the same `scope`. The caller also owns transaction commit/rollback:
///
/// - `Reserved`: insert the returned invocation ID into `object_invocations`
///   in this transaction before committing. Its deferred FK is checked at
///   commit, so a failed begin-invocation rolls back the reservation too.
/// - `Replay`: do not create another invocation; return the existing ID.
/// - `PayloadMismatch`: roll back and return an idempotency conflict.
///
/// The key is scoped by the canonical SQL primary key
/// (tenant, object, effective Caller, key). The payload hash is treated as
/// opaque bytes here; this function does not choose request canonicalization or
/// a hash algorithm. Expiration is supplied by the caller because retention
/// policy is not part of this adapter contract. An expired row remains reserved
/// until the configured retention process removes it.
///
/// Concurrent inserts serialize on the database primary key. The losing
/// transaction reads and returns the winning invocation ID after it commits.
pub async fn reserve_invocation_idempotency(
    transaction: &mut TenantTransaction,
    scope: &Scope,
    caller: &ResolvedCaller,
    claim: &IdempotencyClaim<'_>,
) -> Result<IdempotencyReservation, IdempotencyReservationError> {
    if !caller.is_bound_to(transaction, scope) {
        return Err(IdempotencyReservationError::CallerContextMismatch);
    }
    let expiry_micros = expiry_unix_micros(claim.expires_at)?;
    let caller_id = caller.caller_id();

    loop {
        let inserted = fetch_optional(
            transaction,
            sqlx::query_as::<_, (String,)>(
                "INSERT INTO invocation_idempotency (tenant_id, object_id, caller_id, idempotency_key, payload_hash, invocation_id, expires_at) VALUES ($1::uuid, $2::uuid, $3::uuid, $4, $5, $6::uuid, to_timestamp($7::double precision / 1000000.0)) ON CONFLICT (tenant_id, object_id, caller_id, idempotency_key) DO NOTHING RETURNING invocation_id::text",
            )
            .bind(scope.tenant_id().to_string())
            .bind(claim.object_id.to_string())
            .bind(caller_id.to_string())
            .bind(claim.idempotency_key)
            .bind(claim.payload_hash)
            .bind(claim.proposed_invocation_id.to_string())
            .bind(expiry_micros as f64),
        )
        .await
        .map_err(IdempotencyReservationError::Database)?;

        if inserted.is_some() {
            return Ok(IdempotencyReservation::Reserved {
                invocation_id: claim.proposed_invocation_id,
            });
        }

        let existing = fetch_optional(
            transaction,
            sqlx::query_as::<_, (Vec<u8>, String)>(
                "SELECT payload_hash, invocation_id::text FROM invocation_idempotency WHERE tenant_id = $1::uuid AND object_id = $2::uuid AND caller_id = $3::uuid AND idempotency_key = $4 FOR UPDATE",
            )
            .bind(scope.tenant_id().to_string())
            .bind(claim.object_id.to_string())
            .bind(caller_id.to_string())
            .bind(claim.idempotency_key),
        )
        .await
        .map_err(IdempotencyReservationError::Database)?;

        let Some((existing_hash, existing_invocation_id)) = existing else {
            // A retention purge may have deleted an expired conflicting row
            // between the unique-index check and this SELECT. Retry the claim.
            continue;
        };

        if existing_hash != claim.payload_hash {
            return Err(IdempotencyReservationError::PayloadMismatch);
        }

        let invocation_id = existing_invocation_id
            .parse::<InvocationId>()
            .map_err(|_| IdempotencyReservationError::InvalidPersistedInvocationId)?;

        return Ok(IdempotencyReservation::Replay { invocation_id });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_is_converted_to_unix_microseconds() {
        let expiry = UNIX_EPOCH + std::time::Duration::from_micros(1_234_567);
        assert_eq!(
            expiry_unix_micros(expiry).expect("expiry should fit"),
            1_234_567
        );
    }

    #[test]
    fn expiry_before_unix_epoch_is_rejected() {
        let expiry = UNIX_EPOCH
            .checked_sub(std::time::Duration::from_secs(1))
            .expect("pre-epoch time should be representable");
        assert!(matches!(
            expiry_unix_micros(expiry),
            Err(IdempotencyReservationError::ExpiryOutOfRange)
        ));
    }
}
