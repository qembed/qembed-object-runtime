use std::{future::Future, pin::Pin, time::Duration};

use async_nats::jetstream::{
    self,
    consumer::{self, AckPolicy},
    stream::Config as StreamConfig,
};
use futures_util::StreamExt;
use qembed_api::adapters::nats_jetstream_consumer::{
    NatsJetStreamConsumer, NatsJetStreamConsumerConfig,
};
use qembed_api::adapters::nats_jetstream_publisher::NatsJetStreamPublisher;
use qembed_domain::ports::{
    InboundMessage, InboundMessageHandler, MessageConsumer, MessageHandlerError, MessagePublisher,
    OutboundMessage,
};
use uuid::Uuid;

struct RetryableFailure;

impl InboundMessageHandler for RetryableFailure {
    fn handle(
        &self,
        _message: InboundMessage,
    ) -> Pin<Box<dyn Future<Output = Result<(), MessageHandlerError>> + Send + '_>> {
        Box::pin(async {
            Err(MessageHandlerError::retryable(
                "dependency_unavailable",
                "downstream dependency is temporarily unavailable",
            ))
        })
    }
}

struct PermanentFailure;

impl InboundMessageHandler for PermanentFailure {
    fn handle(
        &self,
        _message: InboundMessage,
    ) -> Pin<Box<dyn Future<Output = Result<(), MessageHandlerError>> + Send + '_>> {
        Box::pin(async {
            Err(MessageHandlerError::permanent(
                "unsupported_event_version",
                "event version is not supported",
            ))
        })
    }
}

#[tokio::test]
async fn retry_backoff_and_permanent_failures_are_parked_before_source_ack() {
    let nats_url = std::env::var("NATS_URL").expect("NATS_URL is set by test-postgres.sh");
    let client = async_nats::connect(&nats_url)
        .await
        .expect("NATS is available");
    let context = jetstream::new(client);
    let suffix = Uuid::new_v4().simple().to_string();
    let source_subject = format!("qembed.rust052b.source.{suffix}");
    let dlq_subject = format!("qembed.rust052b.dlq.{suffix}");
    let source_stream_name = format!("QEMBED_RUST052B_SRC_{suffix}");
    let dlq_stream_name = format!("QEMBED_RUST052B_DLQ_{suffix}");
    let source_durable = format!("qembed-rust052b-src-{suffix}");
    let dlq_durable = format!("qembed-rust052b-dlq-{suffix}");

    let source_stream = context
        .get_or_create_stream(StreamConfig {
            name: source_stream_name,
            subjects: vec![source_subject.clone()],
            max_messages: 100,
            ..Default::default()
        })
        .await
        .expect("source stream is provisioned");
    let dlq_stream = context
        .get_or_create_stream(StreamConfig {
            name: dlq_stream_name,
            subjects: vec![dlq_subject.clone()],
            max_messages: 100,
            ..Default::default()
        })
        .await
        .expect("DLQ stream is provisioned");

    let source_consumer = source_stream
        .get_or_create_consumer(
            &source_durable,
            consumer::pull::Config {
                durable_name: Some(source_durable.clone()),
                ack_policy: AckPolicy::Explicit,
                ack_wait: Duration::from_secs(2),
                max_deliver: 10,
                ..Default::default()
            },
        )
        .await
        .expect("source durable consumer is provisioned");
    let dlq_consumer = dlq_stream
        .get_or_create_consumer(
            &dlq_durable,
            consumer::pull::Config {
                durable_name: Some(dlq_durable.clone()),
                ack_policy: AckPolicy::Explicit,
                ack_wait: Duration::from_secs(2),
                ..Default::default()
            },
        )
        .await
        .expect("DLQ durable consumer is provisioned");

    let publisher = NatsJetStreamPublisher::new(context.clone());
    let consumer = NatsJetStreamConsumer::new(
        source_consumer,
        context.clone(),
        NatsJetStreamConsumerConfig::new(
            dlq_subject,
            2,
            Duration::from_millis(150),
            Duration::from_millis(500),
            2,
        )
        .expect("retry policy is valid"),
    );

    let retry_delivery_id = Uuid::new_v4();
    publisher
        .publish(OutboundMessage::new(
            retry_delivery_id.to_string(),
            source_subject.clone(),
            serde_json::json!({"type": "temporarily_unavailable"}),
        ))
        .await
        .expect("retryable source message is durably published");

    let first = consumer
        .process_batch(&RetryableFailure, 1, Duration::from_secs(2))
        .await
        .expect("first retry attempt is scheduled");
    assert_eq!(first.received, 1);
    assert_eq!(first.acknowledged, 0);
    assert_eq!(first.retry_scheduled, 1);
    assert_eq!(first.dead_lettered, 0);

    let early = consumer
        .process_batch(&RetryableFailure, 1, Duration::from_millis(40))
        .await
        .expect("short fetch window completes");
    assert_eq!(early.received, 0, "NAK delay prevents immediate redelivery");

    tokio::time::sleep(Duration::from_millis(180)).await;
    let exhausted = consumer
        .process_batch(&RetryableFailure, 1, Duration::from_secs(2))
        .await
        .expect("exhausted message is parked");
    assert_eq!(exhausted.received, 1);
    assert_eq!(exhausted.acknowledged, 1);
    assert_eq!(exhausted.dead_lettered, 1);
    assert_eq!(exhausted.dead_letter_failed, 0);

    let permanent_delivery_id = Uuid::new_v4();
    publisher
        .publish(OutboundMessage::new(
            permanent_delivery_id.to_string(),
            source_subject,
            serde_json::json!({"type": "unsupported", "version": 999}),
        ))
        .await
        .expect("permanent source message is durably published");

    let permanent = consumer
        .process_batch(&PermanentFailure, 1, Duration::from_secs(2))
        .await
        .expect("permanent message is parked immediately");
    assert_eq!(permanent.received, 1);
    assert_eq!(permanent.acknowledged, 1);
    assert_eq!(permanent.dead_lettered, 1);

    let mut messages = dlq_consumer
        .fetch()
        .max_messages(2)
        .expires(Duration::from_secs(2))
        .messages()
        .await
        .expect("DLQ fetch starts");
    let first_parked = messages
        .next()
        .await
        .expect("first DLQ message is present")
        .expect("first DLQ delivery is valid");
    let first_envelope: serde_json::Value =
        serde_json::from_slice(&first_parked.payload).expect("first DLQ envelope is valid JSON");
    assert_eq!(
        first_envelope["original_delivery_id"],
        retry_delivery_id.to_string()
    );
    assert_eq!(first_envelope["delivery_attempt"], 2);
    assert_eq!(first_envelope["failure_kind"], "retryable_exhausted");
    assert_eq!(first_envelope["failure_code"], "dependency_unavailable");
    assert!(
        !first_envelope["payload_bytes"]
            .as_array()
            .expect("raw payload bytes are retained")
            .is_empty()
    );
    assert!(
        first_envelope["dead_letter_id"]
            .as_str()
            .expect("stable DLQ identity is present")
            .starts_with("qembed-dlq-")
    );
    first_parked
        .double_ack()
        .await
        .expect("first DLQ test message is acknowledged");

    let second_parked = messages
        .next()
        .await
        .expect("second DLQ message is present")
        .expect("second DLQ delivery is valid");
    let second_envelope: serde_json::Value =
        serde_json::from_slice(&second_parked.payload).expect("second DLQ envelope is valid JSON");
    assert_eq!(
        second_envelope["original_delivery_id"],
        permanent_delivery_id.to_string()
    );
    assert_eq!(second_envelope["delivery_attempt"], 1);
    assert_eq!(second_envelope["failure_kind"], "permanent");
    assert_eq!(second_envelope["failure_code"], "unsupported_event_version");
    second_parked
        .double_ack()
        .await
        .expect("second DLQ test message is acknowledged");
}

#[tokio::test]
async fn unavailable_dlq_keeps_source_delivery_unacknowledged() {
    let nats_url = std::env::var("NATS_URL").expect("NATS_URL is set by test-postgres.sh");
    let client = async_nats::connect(&nats_url)
        .await
        .expect("NATS is available");
    let context = jetstream::new(client);
    let suffix = Uuid::new_v4().simple().to_string();
    let source_subject = format!("qembed.rust052b.dlq-unavailable.source.{suffix}");
    let missing_dlq_subject = format!("qembed.rust052b.dlq-unavailable.missing.{suffix}");
    let source_stream_name = format!("QEMBED_RUST052B_FAIL_SRC_{suffix}");
    let source_durable = format!("qembed-rust052b-fail-src-{suffix}");

    let source_stream = context
        .get_or_create_stream(StreamConfig {
            name: source_stream_name,
            subjects: vec![source_subject.clone()],
            max_messages: 100,
            ..Default::default()
        })
        .await
        .expect("source stream is provisioned");
    let source_consumer = source_stream
        .get_or_create_consumer(
            &source_durable,
            consumer::pull::Config {
                durable_name: Some(source_durable.clone()),
                ack_policy: AckPolicy::Explicit,
                ack_wait: Duration::from_secs(2),
                max_deliver: 10,
                ..Default::default()
            },
        )
        .await
        .expect("source durable consumer is provisioned");

    let consumer = NatsJetStreamConsumer::new(
        source_consumer,
        context.clone(),
        NatsJetStreamConsumerConfig::new(
            missing_dlq_subject,
            1,
            Duration::from_millis(100),
            Duration::from_millis(250),
            2,
        )
        .expect("retry policy is valid"),
    );
    let publisher = NatsJetStreamPublisher::new(context);
    publisher
        .publish(OutboundMessage::new(
            Uuid::new_v4().to_string(),
            source_subject,
            serde_json::json!({"type": "poison"}),
        ))
        .await
        .expect("source message is durably published");

    let outcome = consumer
        .process_batch(&PermanentFailure, 1, Duration::from_secs(2))
        .await
        .expect("failed DLQ publication is converted to delayed redelivery");
    assert_eq!(outcome.received, 1);
    assert_eq!(outcome.acknowledged, 0);
    assert_eq!(outcome.dead_lettered, 0);
    assert_eq!(outcome.dead_letter_failed, 1);
    assert_eq!(outcome.retry_scheduled, 1);
}
