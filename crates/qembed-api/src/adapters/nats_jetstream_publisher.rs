//! Concrete NATS JetStream adapter for the broker-neutral MessagePublisher port.

use std::error::Error;
use std::fmt;

use qembed_domain::ports::{MessagePublisher, OutboundMessage, PublicationReceipt};

/// Error wrapper that keeps async-nats SDK types out of the domain port.
#[derive(Debug)]
pub struct NatsJetStreamPublisherError {
    source: Box<dyn Error + Send + Sync>,
}

impl NatsJetStreamPublisherError {
    fn new(source: impl Error + Send + Sync + 'static) -> Self {
        Self { source: Box::new(source) }
    }
}

impl fmt::Display for NatsJetStreamPublisherError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "NATS JetStream publication failed: {}", self.source)
    }
}

impl Error for NatsJetStreamPublisherError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Publishes durable Outbox messages to an already configured JetStream context.
#[derive(Clone)]
pub struct NatsJetStreamPublisher {
    context: async_nats::jetstream::Context,
}

impl NatsJetStreamPublisher {
    #[must_use]
    pub fn new(context: async_nats::jetstream::Context) -> Self {
        Self { context }
    }

    /// Connects to NATS and creates a JetStream context.
    ///
    /// Stream provisioning and credentials remain deployment responsibilities;
    /// this adapter never creates or mutates stream configuration.
    pub async fn connect(server_url: &str) -> Result<Self, NatsJetStreamPublisherError> {
        let client = async_nats::connect(server_url)
            .await
            .map_err(NatsJetStreamPublisherError::new)?;
        Ok(Self::new(async_nats::jetstream::new(client)))
    }
}

impl MessagePublisher for NatsJetStreamPublisher {
    type Error = NatsJetStreamPublisherError;

    async fn publish(
        &self,
        message: OutboundMessage,
    ) -> Result<PublicationReceipt, Self::Error> {
        let payload = serde_json::to_vec(message.payload())
            .map_err(NatsJetStreamPublisherError::new)?;
        let mut headers = async_nats::HeaderMap::new();
        headers.append("Nats-Msg-Id", message.delivery_id());

        // The first await sends the publish; awaiting the returned future waits
        // for JetStream PubAck. The Outbox Relay persists that receipt afterward.
        let acknowledgement = self.context
            .publish_with_headers(message.subject().to_owned(), headers, payload.into())
            .await
            .map_err(NatsJetStreamPublisherError::new)?
            .await
            .map_err(NatsJetStreamPublisherError::new)?;

        Ok(PublicationReceipt::new(
            acknowledgement.stream,
            acknowledgement.sequence,
            acknowledgement.duplicate,
        ))
    }
}
