//! Provider-neutral orchestration for PostgreSQL Outbox publication.
//!
//! The claim transaction commits before MessagePublisher is called. A PubAck
//! receipt is persisted in a second short transaction. Ambiguous publish
//! failures release the lease for at-least-once retry with the same delivery ID.

use std::error::Error;
use std::fmt;

use qembed_domain::ports::{MessagePublisher, OutboxDeliveryStore};

/// Validated settings for one relay batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboxRelayConfig {
    owner: String,
    topic: String,
    batch_size: u16,
    lease_seconds: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutboxRelayConfigError {
    EmptyOwner,
    EmptyTopic,
    InvalidBatchSize,
    InvalidLeaseDuration,
}

impl fmt::Display for OutboxRelayConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyOwner => formatter.write_str("relay owner must not be empty"),
            Self::EmptyTopic => formatter.write_str("relay topic must not be empty"),
            Self::InvalidBatchSize => formatter.write_str("relay batch size must be 1..=500"),
            Self::InvalidLeaseDuration => formatter.write_str("relay lease must be 1..=3600 seconds"),
        }
    }
}

impl Error for OutboxRelayConfigError {}

impl OutboxRelayConfig {
    pub fn new(
        owner: impl Into<String>,
        topic: impl Into<String>,
        batch_size: u16,
        lease_seconds: u32,
    ) -> Result<Self, OutboxRelayConfigError> {
        let owner = owner.into();
        let topic = topic.into();
        if owner.trim().is_empty() {
            return Err(OutboxRelayConfigError::EmptyOwner);
        }
        if topic.trim().is_empty() {
            return Err(OutboxRelayConfigError::EmptyTopic);
        }
        if !(1..=500).contains(&batch_size) {
            return Err(OutboxRelayConfigError::InvalidBatchSize);
        }
        if !(1..=3600).contains(&lease_seconds) {
            return Err(OutboxRelayConfigError::InvalidLeaseDuration);
        }
        Ok(Self { owner, topic, batch_size, lease_seconds })
    }
}

/// Observable result of one claim/publish/receipt batch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OutboxRelayOutcome {
    pub claimed: usize,
    pub published: usize,
    pub publish_failed: usize,
    pub lease_lost: usize,
}

/// Store failures abort a batch; transport failures are counted and released.
#[derive(Debug)]
pub struct OutboxRelayError<E>(pub E);

impl<E: fmt::Display> fmt::Display for OutboxRelayError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "outbox relay persistence failed: {}", self.0)
    }
}

impl<E: Error + 'static> Error for OutboxRelayError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
    }
}

/// Runs one bounded Outbox batch. It deliberately owns no SQL or broker SDK types.
pub struct OutboxRelay<S, P> {
    store: S,
    publisher: P,
    config: OutboxRelayConfig,
}

impl<S, P> OutboxRelay<S, P>
where
    S: OutboxDeliveryStore,
    P: MessagePublisher,
{
    #[must_use]
    pub fn new(store: S, publisher: P, config: OutboxRelayConfig) -> Self {
        Self { store, publisher, config }
    }

    /// Claims rows, publishes outside PostgreSQL transactions, then persists PubAck.
    ///
    /// If publish returns an ambiguous transport error, the lease is released.
    /// The next attempt reuses the same delivery ID, allowing bounded JetStream
    /// deduplication while retaining consumer-side idempotency as the guarantee.
    pub async fn run_once(&self) -> Result<OutboxRelayOutcome, OutboxRelayError<S::Error>> {
        let deliveries = self.store
            .claim_batch(
                &self.config.owner,
                &self.config.topic,
                self.config.batch_size,
                self.config.lease_seconds,
            )
            .await
            .map_err(OutboxRelayError)?;

        let mut outcome = OutboxRelayOutcome {
            claimed: deliveries.len(),
            ..OutboxRelayOutcome::default()
        };

        for delivery in &deliveries {
            let message = self.store.outbound_message(delivery);
            match self.publisher.publish(message).await {
                Ok(receipt) => {
                    if self.store
                        .record_publication(delivery, &self.config.owner, receipt)
                        .await
                        .map_err(OutboxRelayError)?
                    {
                        outcome.published += 1;
                    } else {
                        // JetStream accepted the message, but this claim was fenced.
                        // PostgreSQL will reclaim and safely republish the same ID.
                        outcome.lease_lost += 1;
                    }
                }
                Err(_) => {
                    outcome.publish_failed += 1;
                    if !self.store
                        .release(delivery, &self.config.owner)
                        .await
                        .map_err(OutboxRelayError)?
                    {
                        outcome.lease_lost += 1;
                    }
                }
            }
        }

        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::future::Future;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use qembed_domain::ports::{OutboundMessage, PublicationReceipt};

    use super::*;

    struct Delivery;

    struct TestStore {
        published: Arc<AtomicUsize>,
        released: Arc<AtomicUsize>,
    }

    impl OutboxDeliveryStore for TestStore {
        type Delivery = Delivery;
        type Error = Infallible;

        fn claim_batch(
            &self,
            _: &str,
            _: &str,
            _: u16,
            _: u32,
        ) -> impl Future<Output = Result<Vec<Self::Delivery>, Self::Error>> + Send {
            async { Ok(vec![Delivery]) }
        }

        fn outbound_message(&self, _: &Self::Delivery) -> OutboundMessage {
            OutboundMessage::new("stable-delivery-id", "qembed.events", serde_json::json!({"v": 1}))
        }

        fn record_publication(
            &self,
            _: &Self::Delivery,
            _: &str,
            receipt: PublicationReceipt,
        ) -> impl Future<Output = Result<bool, Self::Error>> + Send {
            let published = self.published.clone();
            async move {
                assert_eq!(receipt.stream(), "QEMBED_EVENTS");
                assert_eq!(receipt.sequence(), 7);
                published.fetch_add(1, Ordering::SeqCst);
                Ok(true)
            }
        }

        fn release(
            &self,
            _: &Self::Delivery,
            _: &str,
        ) -> impl Future<Output = Result<bool, Self::Error>> + Send {
            let released = self.released.clone();
            async move {
                released.fetch_add(1, Ordering::SeqCst);
                Ok(true)
            }
        }
    }

    struct TestPublisher;

    impl MessagePublisher for TestPublisher {
        type Error = Infallible;

        fn publish(
            &self,
            message: OutboundMessage,
        ) -> impl Future<Output = Result<PublicationReceipt, Self::Error>> + Send {
            async move {
                assert_eq!(message.delivery_id(), "stable-delivery-id");
                assert_eq!(message.subject(), "qembed.events");
                Ok(PublicationReceipt::new("QEMBED_EVENTS", 7, false))
            }
        }
    }

    #[tokio::test]
    async fn relay_persists_puback_after_publish() {
        let published = Arc::new(AtomicUsize::new(0));
        let released = Arc::new(AtomicUsize::new(0));
        let store = TestStore { published: published.clone(), released: released.clone() };
        let config = OutboxRelayConfig::new("relay-a", "qembed.events", 10, 30)
            .expect("valid config");
        let relay = OutboxRelay::new(store, TestPublisher, config);

        let outcome = relay.run_once().await.expect("relay batch should succeed");
        assert_eq!(outcome, OutboxRelayOutcome {
            claimed: 1, published: 1, publish_failed: 0, lease_lost: 0,
        });
        assert_eq!(published.load(Ordering::SeqCst), 1);
        assert_eq!(released.load(Ordering::SeqCst), 0);
    }
}
