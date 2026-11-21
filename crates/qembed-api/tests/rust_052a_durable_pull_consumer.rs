use std::{future::Future, pin::Pin, time::Duration};

use async_nats::jetstream::{
    self,
    consumer::{self, AckPolicy},
    stream::Config as StreamConfig,
};
use qembed_api::adapters::{
    nats_jetstream_consumer::{NatsJetStreamConsumer, NatsJetStreamConsumerConfig},
    nats_jetstream_publisher::NatsJetStreamPublisher,
};
use qembed_domain::ports::{
    InboundMessage, InboundMessageHandler, MessageConsumer, MessageHandlerError, MessagePublisher,
    OutboundMessage,
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

struct FailingHandler;

impl InboundMessageHandler for FailingHandler {
    fn handle(
        &self,
        _message: InboundMessage,
    ) -> Pin<Box<dyn Future<Output = Result<(), MessageHandlerError>> + Send + '_>> {
        Box::pin(async { Err(MessageHandlerError::retryable("temporary_failure", "simulated pre-commit failure")) })
    }
}

struct CommitHandler {
    pool: PgPool,
}

impl InboundMessageHandler for CommitHandler {
    fn handle(
        &self,
        message: InboundMessage,
    ) -> Pin<Box<dyn Future<Output = Result<(), MessageHandlerError>> + Send + '_>> {
        Box::pin(async move {
            let delivery_id = Uuid::parse_str(message.delivery_id())?;
            let mut transaction = self.pool.begin().await?;
            sqlx::query(
                "INSERT INTO rust_052a_ack_probe (delivery_id) VALUES ($1) ON CONFLICT (delivery_id) DO NOTHING",
            )
            .bind(delivery_id)
            .execute(&mut *transaction)
            .await?;
            transaction.commit().await?;
            Ok(())
        })
    }
}

#[tokio::test]
async fn durable_pull_consumer_acks_only_after_handler_commit() {
    let nats_url = std::env::var("NATS_URL").expect("NATS_URL is set by test-postgres.sh");
    let database_url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL is set by test-postgres.sh");
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("PostgreSQL is available");
    sqlx::query("CREATE TABLE IF NOT EXISTS rust_052a_ack_probe (delivery_id UUID PRIMARY KEY)")
        .execute(&pool)
        .await
        .expect("ack probe table exists");

    let client = async_nats::connect(&nats_url)
        .await
        .expect("NATS is available");
    let context = jetstream::new(client);
    let suffix = Uuid::new_v4().simple().to_string();
    let subject = format!("qembed.rust052a.consumer.{suffix}");
    let stream_name = format!("QEMBED_RUST052A_{suffix}");
    let durable_name = format!("qembed-rust052a-{suffix}");
    let stream = context
        .get_or_create_stream(StreamConfig {
            name: stream_name,
            subjects: vec![subject.clone()],
            max_messages: 100,
            ..Default::default()
        })
        .await
        .expect("isolated stream is provisioned");
    let consumer = stream
        .get_or_create_consumer(
            &durable_name,
            consumer::pull::Config {
                durable_name: Some(durable_name.clone()),
                ack_policy: AckPolicy::Explicit,
                ack_wait: Duration::from_millis(300),
                max_deliver: 5,
                ..Default::default()
            },
        )
        .await
        .expect("durable pull consumer is provisioned");
    let consumer = NatsJetStreamConsumer::new(consumer, context.clone(), NatsJetStreamConsumerConfig::new("qembed.dlq.rust052a", 5, Duration::from_millis(100), Duration::from_secs(2), 2).expect("valid retry policy"));
    let publisher = NatsJetStreamPublisher::connect(&nats_url)
        .await
        .expect("publisher connects");

    let delivery_id = Uuid::new_v4();
    publisher
        .publish(OutboundMessage::new(
            delivery_id.to_string(),
            subject,
            serde_json::json!({
                "type": "invocation_received",
                "invocation_id": Uuid::new_v4()
            }),
        ))
        .await
        .expect("JetStream accepts the delivery");

    let failed = consumer
        .process_batch(&FailingHandler, 1, Duration::from_secs(2))
        .await
        .expect("failed handler does not fail the consumer batch");
    assert_eq!(failed.received, 1);
    assert_eq!(failed.acknowledged, 0);
    assert_eq!(failed.handler_failed, 1);

    tokio::time::sleep(Duration::from_millis(450)).await;
    let committed = consumer
        .process_batch(
            &CommitHandler { pool: pool.clone() },
            1,
            Duration::from_secs(2),
        )
        .await
        .expect("redelivered message is processed");
    assert_eq!(committed.received, 1);
    assert_eq!(committed.acknowledged, 1);
    assert_eq!(committed.handler_failed, 0);

    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM rust_052a_ack_probe WHERE delivery_id = $1)",
    )
    .bind(delivery_id)
    .fetch_one(&pool)
    .await
    .expect("committed marker is readable");
    assert!(exists, "PostgreSQL commit precedes the JetStream ACK");

    sqlx::query("DELETE FROM rust_052a_ack_probe WHERE delivery_id = $1")
        .bind(delivery_id)
        .execute(&pool)
        .await
        .expect("test marker is cleaned up");
}
