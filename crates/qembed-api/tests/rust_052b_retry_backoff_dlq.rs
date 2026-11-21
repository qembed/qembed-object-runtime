use std::{future::Future, pin::Pin, time::Duration};

use async_nats::jetstream::{self, consumer::{self, AckPolicy}, stream::Config as StreamConfig};
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

#[tokio::test]
async fn retry_backoff_exhaustion_parks_message_before_source_ack() {
    let nats_url = std::env::var("NATS_URL").expect("NATS_URL is set by test-postgres.sh");
    let client = async_nats::connect(&nats_url).await.expect("NATS is available");
    let context = jetstream::new(client);
    let suffix = Uuid::new_v4().simple().to_string();
    let source_subject = format!("qembed.rust052b.source.{suffix}");
    let dlq_subject = format!("qembed.rust052b.dlq.{suffix}");
    let source_stream_name = format!("QEMBED_RUST052B_SRC_{suffix}");
    let dlq_stream_name = format!("QEMBED_RUST052B_DLQ_{suffix}");
    let source_durable = format!("qembed-rust052b-src-{suffix}");
    let dlq_durable = format!("qembed-rust052b-dlq-{suffix}");

    let source_stream = context.get_or_create_stream(StreamConfig {
        name: source_stream_name,
        subjects: vec![source_subject.clone()],
        max_messages: 100,
        ..Default::default()
    }).await.expect("source stream is provisioned");
    let dlq_stream = context.get_or_create_stream(StreamConfig {
        name: dlq_stream_name,
        subjects: vec![dlq_subject.clone()],
        max_messages: 100,
        ..Default::default()
    }).await.expect("DLQ stream is provisioned");

    let source_consumer = source_stream.get_or_create_consumer(
        &source_durable,
        consumer::pull::Config {
            durable_name: Some(source_durable),
            ack_policy: AckPolicy::Explicit,
            ack_wait: Duration::from_secs(2),
            max_deliver: 10,
            ..Default::default()
        },
    ).await.expect("source durable consumer is provisioned");
    let dlq_consumer = dlq_stream.get_or_create_consumer(
        &dlq_durable,
        consumer::pull::Config {
            durable_name: Some(dlq_durable),
            ack_policy: AckPolicy::Explicit,
            ack_wait: Duration::from_secs(2),
            ..Default::default()
        },
    ).await.expect("DLQ durable consumer is provisioned");

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
        ).expect("retry policy is valid"),
    );
    let delivery_id = Uuid::new_v4();
    publisher.publish(OutboundMessage::new(
        delivery_id.to_string(),
        source_subject,
        serde_json::json!({"type": "unsupported", "version": 999}),
    )).await.expect("source message is durably published");

    let first = consumer.process_batch(&RetryableFailure, 1, Duration::from_secs(2))
        .await.expect("first retry attempt is scheduled");
    assert_eq!(first.received, 1);
    assert_eq!(first.acknowledged, 0);
    assert_eq!(first.retry_scheduled, 1);
    assert_eq!(first.dead_lettered, 0);

    let early = consumer.process_batch(&RetryableFailure, 1, Duration::from_millis(40))
        .await.expect("short fetch window completes");
    assert_eq!(early.received, 0, "NAK delay prevents immediate redelivery");

    tokio::time::sleep(Duration::from_millis(180)).await;
    let exhausted = consumer.process_batch(&RetryableFailure, 1, Duration::from_secs(2))
        .await.expect("exhausted message is parked");
    assert_eq!(exhausted.received, 1);
    assert_eq!(exhausted.acknowledged, 1);
    assert_eq!(exhausted.dead_lettered, 1);
    assert_eq!(exhausted.dead_letter_failed, 0);

    let mut messages = dlq_consumer.fetch()
        .max_messages(1)
        .expires(Duration::from_secs(2))
        .messages()
        .await
        .expect("DLQ fetch starts");
    let parked = messages.next().await.expect("DLQ message is present")
        .expect("DLQ delivery is valid");
    let envelope: serde_json::Value = serde_json::from_slice(&parked.payload)
        .expect("DLQ envelope is valid JSON");
    assert_eq!(envelope["schema_version"], 1);
    assert_eq!(envelope["original_delivery_id"], delivery_id.to_string());
    assert_eq!(envelope["delivery_attempt"], 2);
    assert_eq!(envelope["failure_kind"], "retryable_exhausted");
    assert_eq!(envelope["failure_code"], "dependency_unavailable");
    assert!(!envelope["payload_bytes"].as_array().expect("raw payload bytes are retained").is_empty());
    assert!(envelope["dead_letter_id"].as_str().unwrap().starts_with("qembed-dlq-"));
    parked.double_ack().await.expect("DLQ test message is acknowledged");
}
