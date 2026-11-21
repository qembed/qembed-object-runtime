//! Durable pull-consumer adapter for NATS JetStream.
//!
//! Consumer creation and stream provisioning remain deployment responsibilities.
//! A delivery is explicitly ACKed only after the broker-neutral handler returns
//! success, which is the handler's guarantee that its PostgreSQL transaction committed.

use std::{error::Error, fmt, time::Duration};

use async_nats::jetstream::consumer::PullConsumer;
use futures_util::StreamExt;
use qembed_domain::ports::{
    InboundMessage, InboundMessageHandler, MessageConsumer, MessageConsumerOutcome,
};

const DELIVERY_ID_HEADER: &str = "Qembed-Delivery-Id";

#[derive(Debug)]
pub enum NatsJetStreamConsumerError {
    InvalidBatchSize,
    InvalidExpiry,
    Transport(Box<dyn Error + Send + Sync>),
}

impl fmt::Display for NatsJetStreamConsumerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBatchSize => formatter.write_str("consumer batch size must be 1..=500"),
            Self::InvalidExpiry => formatter.write_str("consumer fetch expiry must be positive"),
            Self::Transport(error) => write!(formatter, "NATS JetStream consumer failed: {error}"),
        }
    }
}

impl Error for NatsJetStreamConsumerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Transport(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

impl NatsJetStreamConsumerError {
    fn transport(error: impl Error + Send + Sync + 'static) -> Self {
        Self::Transport(Box::new(error))
    }
}

/// A handle to an already provisioned durable pull consumer.
#[derive(Clone)]
pub struct NatsJetStreamConsumer {
    consumer: PullConsumer,
}

impl NatsJetStreamConsumer {
    #[must_use]
    pub fn new(consumer: PullConsumer) -> Self {
        Self { consumer }
    }
}

impl MessageConsumer for NatsJetStreamConsumer {
    type Error = NatsJetStreamConsumerError;

    async fn process_batch(
        &self,
        handler: &dyn InboundMessageHandler,
        batch_size: u16,
        expires_after: Duration,
    ) -> Result<MessageConsumerOutcome, Self::Error> {
        if !(1..=500).contains(&batch_size) {
            return Err(NatsJetStreamConsumerError::InvalidBatchSize);
        }
        if expires_after.is_zero() {
            return Err(NatsJetStreamConsumerError::InvalidExpiry);
        }

        let mut messages = self
            .consumer
            .fetch()
            .max_messages(usize::from(batch_size))
            .expires_after(expires_after)
            .messages()
            .await
            .map_err(NatsJetStreamConsumerError::transport)?;

        let mut outcome = MessageConsumerOutcome::default();
        while let Some(result) = messages.next().await {
            let message = result.map_err(NatsJetStreamConsumerError::transport)?;
            outcome.received += 1;

            let delivery_id = message
                .headers
                .as_ref()
                .and_then(|headers| headers.get(DELIVERY_ID_HEADER))
                .map(ToString::to_string);
            let payload = serde_json::from_slice(&message.payload);

            let (Some(delivery_id), Ok(payload)) = (delivery_id, payload) else {
                // Malformed messages remain unacknowledged. RUST-052b adds
                // durable poison-message classification and parking/DLQ policy.
                outcome.handler_failed += 1;
                continue;
            };

            let inbound = InboundMessage::new(delivery_id, message.subject.to_string(), payload);
            if handler.handle(inbound).await.is_err() {
                // Let JetStream redeliver after AckWait. Do not ACK a failed
                // handler: it may not have committed its PostgreSQL work.
                outcome.handler_failed += 1;
                continue;
            }

            // Confirm the ACK with JetStream. If this confirmation is lost,
            // redelivery is safe because the PostgreSQL handler is idempotent.
            message
                .double_ack()
                .await
                .map_err(NatsJetStreamConsumerError::transport)?;
            outcome.acknowledged += 1;
        }

        Ok(outcome)
    }
}
