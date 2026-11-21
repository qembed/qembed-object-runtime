//! Durable pull-consumer adapter for NATS JetStream.
//!
//! Retryable failures use bounded delayed NAK. Permanent/exhausted failures are
//! first published to a durable DLQ stream and only then ACKed on the source.

use std::{error::Error, fmt, time::Duration};

use async_nats::jetstream::{AckKind, Context, consumer::PullConsumer};
use futures_util::StreamExt;
use qembed_domain::ports::{
    InboundMessage, InboundMessageHandler, MessageConsumer, MessageConsumerOutcome,
    MessageFailureKind, MessageHandlerError,
};

const DELIVERY_ID_HEADER: &str = "Qembed-Delivery-Id";
const DEAD_LETTER_ID_HEADER: &str = "Qembed-Dead-Letter-Id";
const ORIGINAL_DELIVERY_ID_HEADER: &str = "Qembed-Original-Delivery-Id";

#[derive(Debug)]
pub enum NatsJetStreamConsumerError {
    InvalidBatchSize,
    InvalidExpiry,
    InvalidPolicy,
    Transport(String),
}

impl fmt::Display for NatsJetStreamConsumerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBatchSize => formatter.write_str("consumer batch size must be 1..=500"),
            Self::InvalidExpiry => formatter.write_str("consumer fetch expiry must be positive"),
            Self::InvalidPolicy => formatter.write_str("consumer retry/DLQ policy is invalid"),
            Self::Transport(error) => write!(formatter, "NATS JetStream consumer failed: {error}"),
        }
    }
}

impl Error for NatsJetStreamConsumerError {}

impl NatsJetStreamConsumerError {
    fn transport(error: impl fmt::Display) -> Self {
        Self::Transport(error.to_string())
    }
}

/// Bounded retry policy and the deployment-provisioned DLQ subject.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NatsJetStreamConsumerConfig {
    dead_letter_subject: String,
    max_attempts: u32,
    initial_retry_delay: Duration,
    max_retry_delay: Duration,
    retry_multiplier: u32,
}

impl NatsJetStreamConsumerConfig {
    pub fn new(
        dead_letter_subject: impl Into<String>,
        max_attempts: u32,
        initial_retry_delay: Duration,
        max_retry_delay: Duration,
        retry_multiplier: u32,
    ) -> Result<Self, NatsJetStreamConsumerError> {
        let dead_letter_subject = dead_letter_subject.into();
        if dead_letter_subject.trim().is_empty()
            || max_attempts == 0
            || initial_retry_delay.is_zero()
            || max_retry_delay < initial_retry_delay
            || retry_multiplier < 2
        {
            return Err(NatsJetStreamConsumerError::InvalidPolicy);
        }
        Ok(Self {
            dead_letter_subject,
            max_attempts,
            initial_retry_delay,
            max_retry_delay,
            retry_multiplier,
        })
    }

    fn retry_delay(&self, attempt: u32) -> Duration {
        (1..attempt)
            .try_fold(self.initial_retry_delay, |delay, _| {
                let next = delay.saturating_mul(self.retry_multiplier);
                (next < self.max_retry_delay).then_some(next)
            })
            .unwrap_or(self.max_retry_delay)
    }
}

#[derive(Clone, Copy)]
struct DeadLetterSource<'a> {
    delivery_id: Option<&'a str>,
    stream: &'a str,
    consumer: &'a str,
    sequence: u64,
    attempt: u32,
}

/// DLQ envelope preserves exact bytes, including malformed/non-UTF8 payloads.
#[derive(serde::Serialize)]
struct DeadLetterEnvelope<'a> {
    schema_version: u8,
    dead_letter_id: String,
    original_delivery_id: Option<&'a str>,
    source_stream: &'a str,
    source_consumer: &'a str,
    source_sequence: u64,
    delivery_attempt: u32,
    source_subject: &'a str,
    failure_kind: &'static str,
    failure_code: &'a str,
    failure_message: &'a str,
    payload_bytes: &'a [u8],
}

/// Handle to an already provisioned durable pull consumer.
#[derive(Clone)]
pub struct NatsJetStreamConsumer {
    consumer: PullConsumer,
    context: Context,
    config: NatsJetStreamConsumerConfig,
}

impl NatsJetStreamConsumer {
    #[must_use]
    pub fn new(
        consumer: PullConsumer,
        context: Context,
        config: NatsJetStreamConsumerConfig,
    ) -> Self {
        Self {
            consumer,
            context,
            config,
        }
    }

    async fn publish_dead_letter(
        &self,
        message: &async_nats::jetstream::Message,
        failure: &MessageHandlerError,
        source: DeadLetterSource<'_>,
    ) -> Result<(), NatsJetStreamConsumerError> {
        let dead_letter_id = format!(
            "qembed-dlq-{}-{}-{}",
            source.stream, source.consumer, source.sequence
        );
        let source_subject = message.subject.to_string();
        let envelope = DeadLetterEnvelope {
            schema_version: 1,
            dead_letter_id: dead_letter_id.clone(),
            original_delivery_id: source.delivery_id,
            source_stream: source.stream,
            source_consumer: source.consumer,
            source_sequence: source.sequence,
            delivery_attempt: source.attempt,
            source_subject: &source_subject,
            failure_kind: match failure.kind() {
                MessageFailureKind::Retryable => "retryable_exhausted",
                MessageFailureKind::Permanent => "permanent",
            },
            failure_code: failure.code(),
            failure_message: failure.message(),
            payload_bytes: &message.payload,
        };
        let payload =
            serde_json::to_vec(&envelope).map_err(NatsJetStreamConsumerError::transport)?;
        let mut headers = async_nats::HeaderMap::new();
        headers.append("Nats-Msg-Id", dead_letter_id.as_str());
        headers.append(DEAD_LETTER_ID_HEADER, dead_letter_id.as_str());
        if let Some(delivery_id) = source.delivery_id {
            headers.append(ORIGINAL_DELIVERY_ID_HEADER, delivery_id);
        }

        // Awaiting the returned future waits for JetStream PubAck. The source
        // delivery is never ACKed unless this durable publication is confirmed.
        self.context
            .publish_with_headers(
                self.config.dead_letter_subject.clone(),
                headers,
                payload.into(),
            )
            .await
            .map_err(NatsJetStreamConsumerError::transport)?
            .await
            .map_err(NatsJetStreamConsumerError::transport)?;
        Ok(())
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
            .expires(expires_after)
            .messages()
            .await
            .map_err(NatsJetStreamConsumerError::transport)?;

        let mut outcome = MessageConsumerOutcome::default();
        while let Some(result) = messages.next().await {
            let message = result.map_err(NatsJetStreamConsumerError::transport)?;
            outcome.received += 1;

            let info = message
                .info()
                .map_err(NatsJetStreamConsumerError::transport)?;
            let source_stream = info.stream.to_owned();
            let source_consumer = info.consumer.to_owned();
            let source_sequence = info.stream_sequence;
            let attempt = u32::try_from(info.delivered).unwrap_or(u32::MAX).max(1);
            let delivery_id = message
                .headers
                .as_ref()
                .and_then(|headers| headers.get(DELIVERY_ID_HEADER))
                .map(ToString::to_string);

            let handled = match (
                delivery_id.as_deref(),
                serde_json::from_slice(&message.payload),
            ) {
                (Some(delivery_id), Ok(payload)) => {
                    handler
                        .handle(InboundMessage::new(
                            delivery_id,
                            message.subject.to_string(),
                            payload,
                        ))
                        .await
                }
                (None, _) => Err(MessageHandlerError::permanent(
                    "missing_delivery_id",
                    "message has no Qembed-Delivery-Id header",
                )),
                (_, Err(_)) => Err(MessageHandlerError::permanent(
                    "invalid_json",
                    "message payload is not valid JSON",
                )),
            };

            match handled {
                Ok(()) => {
                    message
                        .double_ack()
                        .await
                        .map_err(NatsJetStreamConsumerError::transport)?;
                    outcome.acknowledged += 1;
                }
                Err(failure) => {
                    outcome.handler_failed += 1;
                    let exhausted = attempt >= self.config.max_attempts;
                    if failure.kind() == MessageFailureKind::Permanent || exhausted {
                        match self
                            .publish_dead_letter(
                                &message,
                                &failure,
                                DeadLetterSource {
                                    delivery_id: delivery_id.as_deref(),
                                    stream: &source_stream,
                                    consumer: &source_consumer,
                                    sequence: source_sequence,
                                    attempt,
                                },
                            )
                            .await
                        {
                            Ok(()) => {
                                message
                                    .double_ack()
                                    .await
                                    .map_err(NatsJetStreamConsumerError::transport)?;
                                outcome.acknowledged += 1;
                                outcome.dead_lettered += 1;
                            }
                            Err(_) => {
                                // DLQ unavailable: retain the source and retry parking later.
                                message
                                    .ack_with(AckKind::Nak(Some(self.config.max_retry_delay)))
                                    .await
                                    .map_err(NatsJetStreamConsumerError::transport)?;
                                outcome.dead_letter_failed += 1;
                                outcome.retry_scheduled += 1;
                            }
                        }
                    } else {
                        let delay = self.config.retry_delay(attempt);
                        message
                            .ack_with(AckKind::Nak(Some(delay)))
                            .await
                            .map_err(NatsJetStreamConsumerError::transport)?;
                        outcome.retry_scheduled += 1;
                    }
                }
            }
        }

        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::NatsJetStreamConsumerConfig;
    use std::time::Duration;

    #[test]
    fn retry_delay_grows_exponentially_and_is_capped() {
        let policy = NatsJetStreamConsumerConfig::new(
            "qembed.dead",
            5,
            Duration::from_millis(100),
            Duration::from_millis(350),
            2,
        )
        .expect("policy is valid");

        assert_eq!(policy.retry_delay(0), Duration::from_millis(100));
        assert_eq!(policy.retry_delay(1), Duration::from_millis(100));
        assert_eq!(policy.retry_delay(2), Duration::from_millis(200));
        assert_eq!(policy.retry_delay(3), Duration::from_millis(350));
        assert_eq!(policy.retry_delay(20), Duration::from_millis(350));

        let tiny_initial = NatsJetStreamConsumerConfig::new(
            "qembed.dead",
            u32::MAX,
            Duration::from_nanos(1),
            Duration::from_secs(600),
            2,
        )
        .expect("policy with a large attempt budget is valid");
        assert_eq!(
            tiny_initial.retry_delay(u32::MAX),
            Duration::from_secs(600),
            "the delay must still reach its cap when the exponent exceeds u32"
        );
    }

    #[test]
    fn rejects_invalid_retry_policy() {
        assert!(
            NatsJetStreamConsumerConfig::new(
                "",
                3,
                Duration::from_millis(100),
                Duration::from_secs(1),
                2,
            )
            .is_err()
        );
        assert!(
            NatsJetStreamConsumerConfig::new(
                "qembed.dead",
                0,
                Duration::from_millis(100),
                Duration::from_secs(1),
                2,
            )
            .is_err()
        );
    }
}
