//! Lease-fenced PostgreSQL outbox dispatcher adapter.
//!
//! The dispatcher runs with the restricted qembed_dispatcher database role.
//! It can see only the delivery projection and cannot query domain events.

use std::fmt;

use serde_json::Value;
use sqlx::{FromRow, PgPool};

/// A delivery projection; deliberately excludes event and aggregate internals.
#[derive(Clone, Debug, PartialEq, FromRow)]
pub struct OutboxDelivery {
    tenant_id: String,
    delivery_id: String,
    topic: String,
    payload: Value,
    attempts: i32,
    claim_token: i64,
}

impl OutboxDelivery {
    #[must_use]
    pub fn tenant_id(&self) -> &str {
        &self.tenant_id
    }

    #[must_use]
    pub fn delivery_id(&self) -> &str {
        &self.delivery_id
    }

    #[must_use]
    pub fn topic(&self) -> &str {
        &self.topic
    }

    #[must_use]
    pub fn payload(&self) -> &Value {
        &self.payload
    }

    #[must_use]
    pub const fn attempts(&self) -> i32 {
        self.attempts
    }

    #[must_use]
    pub const fn claim_token(&self) -> i64 {
        self.claim_token
    }
}

/// Errors from invalid dispatcher settings or PostgreSQL operations.
#[derive(Debug)]
pub enum OutboxDispatcherError {
    InvalidOwner,
    InvalidTopic,
    InvalidBatchSize,
    InvalidLeaseDuration,
    Database(sqlx::Error),
}

impl fmt::Display for OutboxDispatcherError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidOwner => f.write_str("outbox dispatcher owner must not be empty"),
            Self::InvalidTopic => f.write_str("outbox topic must not be empty"),
            Self::InvalidBatchSize => {
                f.write_str("outbox batch size must be between 1 and 500")
            }
            Self::InvalidLeaseDuration => {
                f.write_str("outbox lease must be between 1 and 3600 seconds")
            }
            Self::Database(error) => write!(f, "outbox database operation failed: {error}"),
        }
    }
}

impl std::error::Error for OutboxDispatcherError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

/// PostgreSQL adapter for cross-tenant outbox delivery.
#[derive(Clone)]
pub struct PgOutboxDispatcher {
    pool: PgPool,
}

impl PgOutboxDispatcher {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Atomically claims up to `batch_size` pending messages.
    ///
    /// SKIP LOCKED lets independent dispatcher processes claim disjoint rows.
    /// An expired claim can be reclaimed; each claim increments a fencing token.
    pub async fn claim_batch(
        &self,
        owner: &str,
        topic: &str,
        batch_size: i64,
        lease_seconds: i64,
    ) -> Result<Vec<OutboxDelivery>, OutboxDispatcherError> {
        if owner.trim().is_empty() {
            return Err(OutboxDispatcherError::InvalidOwner);
        }
        if topic.trim().is_empty() {
            return Err(OutboxDispatcherError::InvalidTopic);
        }
        if !(1..=500).contains(&batch_size) {
            return Err(OutboxDispatcherError::InvalidBatchSize);
        }
        if !(1..=3600).contains(&lease_seconds) {
            return Err(OutboxDispatcherError::InvalidLeaseDuration);
        }

        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(OutboxDispatcherError::Database)?;
        sqlx::query("SET LOCAL ROLE qembed_dispatcher")
            .execute(&mut *transaction)
            .await
            .map_err(OutboxDispatcherError::Database)?;

        let deliveries = sqlx::query_as::<_, OutboxDelivery>(
            "WITH candidates AS (                SELECT tenant_id, delivery_id FROM outbox_messages                  WHERE topic = $4 AND delivered_at IS NULL                    AND (lease_expires_at IS NULL OR lease_expires_at <= now())                  ORDER BY created_at, delivery_id                  FOR UPDATE SKIP LOCKED LIMIT $1             )              UPDATE outbox_messages AS message                 SET lease_owner = $2,                     lease_expires_at = now() + make_interval(secs => $3::int),                     claim_token = message.claim_token + 1,                     attempts = message.attempts + 1                FROM candidates               WHERE message.tenant_id = candidates.tenant_id                 AND message.delivery_id = candidates.delivery_id              RETURNING message.tenant_id::text AS tenant_id,                        message.delivery_id::text AS delivery_id,                        message.topic, message.payload, message.attempts, message.claim_token",
        )
        .bind(batch_size)
        .bind(owner)
        .bind(lease_seconds as i32)
        .bind(topic)
        .fetch_all(&mut *transaction)
        .await
        .map_err(OutboxDispatcherError::Database)?;

        transaction
            .commit()
            .await
            .map_err(OutboxDispatcherError::Database)?;
        Ok(deliveries)
    }

    /// Acknowledges only the current, unexpired claim. Stale workers are fenced.
    pub async fn acknowledge(
        &self,
        delivery: &OutboxDelivery,
        owner: &str,
    ) -> Result<bool, OutboxDispatcherError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(OutboxDispatcherError::Database)?;
        sqlx::query("SET LOCAL ROLE qembed_dispatcher")
            .execute(&mut *transaction)
            .await
            .map_err(OutboxDispatcherError::Database)?;

        let result = sqlx::query(
            "UPDATE outbox_messages                 SET delivered_at = now(), lease_owner = NULL, lease_expires_at = NULL               WHERE tenant_id = $1::uuid AND delivery_id = $2::uuid                 AND delivered_at IS NULL AND lease_owner = $3                 AND claim_token = $4 AND lease_expires_at > now()",
        )
        .bind(&delivery.tenant_id)
        .bind(&delivery.delivery_id)
        .bind(owner)
        .bind(delivery.claim_token)
        .execute(&mut *transaction)
        .await
        .map_err(OutboxDispatcherError::Database)?;

        transaction
            .commit()
            .await
            .map_err(OutboxDispatcherError::Database)?;
        Ok(result.rows_affected() == 1)
    }

    /// Releases a failed delivery for later redelivery without changing its token.
    /// The next claim increments the token, fencing this worker permanently.
    pub async fn release(
        &self,
        delivery: &OutboxDelivery,
        owner: &str,
    ) -> Result<bool, OutboxDispatcherError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(OutboxDispatcherError::Database)?;
        sqlx::query("SET LOCAL ROLE qembed_dispatcher")
            .execute(&mut *transaction)
            .await
            .map_err(OutboxDispatcherError::Database)?;

        let result = sqlx::query(
            "UPDATE outbox_messages                 SET lease_owner = NULL, lease_expires_at = NULL               WHERE tenant_id = $1::uuid AND delivery_id = $2::uuid                 AND delivered_at IS NULL AND lease_owner = $3 AND claim_token = $4",
        )
        .bind(&delivery.tenant_id)
        .bind(&delivery.delivery_id)
        .bind(owner)
        .bind(delivery.claim_token)
        .execute(&mut *transaction)
        .await
        .map_err(OutboxDispatcherError::Database)?;

        transaction
            .commit()
            .await
            .map_err(OutboxDispatcherError::Database)?;
        Ok(result.rows_affected() == 1)
    }
}
