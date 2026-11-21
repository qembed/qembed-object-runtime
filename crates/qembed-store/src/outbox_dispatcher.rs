//! Lease-fenced PostgreSQL outbox dispatcher adapter.
//!
//! The dispatcher runs with the restricted qembed_dispatcher database role.
//! It can see only the delivery projection and cannot query domain events.

use std::fmt;

use qembed_domain::ports::{OutboundMessage, OutboxDeliveryStore, PublicationReceipt};
use serde_json::Value;
use sqlx::{FromRow, PgPool, Row, postgres::PgRow};

/// A delivery projection; deliberately excludes event and aggregate internals.
#[derive(Clone, Debug, PartialEq)]
pub struct OutboxDelivery {
    tenant_id: String,
    delivery_id: String,
    topic: String,
    payload: Value,
    attempts: i32,
    claim_token: i64,
}

impl<'row> FromRow<'row, PgRow> for OutboxDelivery {
    fn from_row(row: &'row PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            tenant_id: row.try_get("tenant_id")?,
            delivery_id: row.try_get("delivery_id")?,
            topic: row.try_get("topic")?,
            payload: row.try_get("payload")?,
            attempts: row.try_get("attempts")?,
            claim_token: row.try_get("claim_token")?,
        })
    }
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
    InvalidPublicationReceipt,
    Database(sqlx::Error),
}

impl fmt::Display for OutboxDispatcherError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidOwner => f.write_str("outbox dispatcher owner must not be empty"),
            Self::InvalidTopic => f.write_str("outbox topic must not be empty"),
            Self::InvalidBatchSize => f.write_str("outbox batch size must be between 1 and 500"),
            Self::InvalidLeaseDuration => {
                f.write_str("outbox lease must be between 1 and 3600 seconds")
            }
            Self::InvalidPublicationReceipt => {
                f.write_str("publication receipt is incomplete or invalid")
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
            "WITH candidates AS (                SELECT tenant_id, delivery_id FROM outbox_messages                  WHERE topic = $4 AND delivered_at IS NULL                    AND (lease_expires_at IS NULL OR lease_expires_at <= now())                  ORDER BY created_at, delivery_id                  LIMIT $1 FOR UPDATE SKIP LOCKED             )              UPDATE outbox_messages AS message                 SET lease_owner = $2,                     lease_expires_at = now() + make_interval(secs => $3::int),                     claim_token = message.claim_token + 1,                     attempts = message.attempts + 1                FROM candidates               WHERE message.tenant_id = candidates.tenant_id                 AND message.delivery_id = candidates.delivery_id              RETURNING message.tenant_id::text AS tenant_id,                        message.delivery_id::text AS delivery_id,                        message.topic, message.payload, message.attempts, message.claim_token",
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
        receipt: PublicationReceipt,
    ) -> Result<bool, OutboxDispatcherError> {
        if receipt.stream().trim().is_empty() || receipt.sequence() == 0 {
            return Err(OutboxDispatcherError::InvalidPublicationReceipt);
        }

        let publication_sequence = i64::try_from(receipt.sequence())
            .map_err(|_| OutboxDispatcherError::InvalidPublicationReceipt)?;

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
            "UPDATE outbox_messages
                SET delivered_at = now(),
                    publication_stream = $5,
                    publication_sequence = $6,
                    publication_duplicate = $7,
                    lease_owner = NULL,
                    lease_expires_at = NULL
              WHERE tenant_id = $1::uuid AND delivery_id = $2::uuid
                AND delivered_at IS NULL AND lease_owner = $3
                AND claim_token = $4 AND lease_expires_at > now()",
        )
        .bind(&delivery.tenant_id)
        .bind(&delivery.delivery_id)
        .bind(owner)
        .bind(delivery.claim_token)
        .bind(receipt.stream())
        .bind(publication_sequence)
        .bind(receipt.duplicate())
        .execute(&mut *transaction)
        .await
        .map_err(OutboxDispatcherError::Database)?;

        let acknowledged = result.rows_affected() == 1;
        if acknowledged {
            sqlx::query(
                "INSERT INTO outbox_publication_receipts
                    (tenant_id, delivery_id, publication_stream, publication_sequence, publication_duplicate)
                 VALUES ($1::uuid, $2::uuid, $3, $4, $5)",
            )
            .bind(&delivery.tenant_id)
            .bind(&delivery.delivery_id)
            .bind(receipt.stream())
            .bind(publication_sequence)
            .bind(receipt.duplicate())
            .execute(&mut *transaction)
            .await
            .map_err(OutboxDispatcherError::Database)?;
        }

        transaction
            .commit()
            .await
            .map_err(OutboxDispatcherError::Database)?;
        Ok(acknowledged)
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

impl OutboxDeliveryStore for PgOutboxDispatcher {
    type Delivery = OutboxDelivery;
    type Error = OutboxDispatcherError;

    async fn claim_batch(
        &self,
        owner: &str,
        topic: &str,
        batch_size: u16,
        lease_seconds: u32,
    ) -> Result<Vec<Self::Delivery>, Self::Error> {
        PgOutboxDispatcher::claim_batch(
            self,
            owner,
            topic,
            i64::from(batch_size),
            i64::from(lease_seconds),
        )
        .await
    }

    fn outbound_message(&self, delivery: &Self::Delivery) -> OutboundMessage {
        OutboundMessage::new(
            delivery.delivery_id.clone(),
            delivery.topic.clone(),
            delivery.payload.clone(),
        )
    }

    async fn record_publication(
        &self,
        delivery: &Self::Delivery,
        owner: &str,
        receipt: PublicationReceipt,
    ) -> Result<bool, Self::Error> {
        PgOutboxDispatcher::acknowledge(self, delivery, owner, receipt).await
    }

    async fn release(&self, delivery: &Self::Delivery, owner: &str) -> Result<bool, Self::Error> {
        PgOutboxDispatcher::release(self, delivery, owner).await
    }
}
