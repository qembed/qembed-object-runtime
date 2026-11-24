use std::{future::Future, pin::Pin, process::Command, time::Duration};

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
        Box::pin(async {
            Err(MessageHandlerError::retryable(
                "temporary_failure",
                "simulated pre-commit failure",
            ))
        })
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
            let delivery_id = Uuid::parse_str(message.delivery_id()).map_err(|error| {
                MessageHandlerError::permanent("invalid_delivery_id", error.to_string())
            })?;
            let mut transaction = self.pool.begin().await.map_err(|error| {
                MessageHandlerError::retryable("postgres_begin_failed", error.to_string())
            })?;
            sqlx::query(
                "INSERT INTO rust_052a_ack_probe (delivery_id) VALUES ($1) ON CONFLICT (delivery_id) DO NOTHING",
            )
            .bind(delivery_id)
            .execute(&mut *transaction)
            .await
            .map_err(|error| {
                MessageHandlerError::retryable("postgres_write_failed", error.to_string())
            })?;
            transaction.commit().await.map_err(|error| {
                MessageHandlerError::retryable("postgres_commit_failed", error.to_string())
            })?;
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
            name: stream_name.clone(),
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
    let consumer = NatsJetStreamConsumer::new(
        consumer,
        context.clone(),
        NatsJetStreamConsumerConfig::new(
            "qembed.dlq.rust052a",
            5,
            Duration::from_millis(100),
            Duration::from_secs(2),
            2,
        )
        .expect("valid retry policy"),
    );
    let publisher = NatsJetStreamPublisher::connect(&nats_url)
        .await
        .expect("publisher connects");

    let delivery_id = Uuid::new_v4();
    publisher
        .publish(OutboundMessage::new(
            delivery_id.to_string(),
            subject.clone(),
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

struct CommitThenRetryOnce {
    pool: PgPool,
    fail_once: std::sync::Arc<std::sync::atomic::AtomicBool>,
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl InboundMessageHandler for CommitThenRetryOnce {
    fn handle(
        &self,
        message: InboundMessage,
    ) -> Pin<Box<dyn Future<Output = Result<(), MessageHandlerError>> + Send + '_>> {
        let pool = self.pool.clone();
        let fail_once = self.fail_once.clone();
        let calls = self.calls.clone();
        Box::pin(async move {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let delivery_id = Uuid::parse_str(message.delivery_id()).map_err(|error| {
                MessageHandlerError::permanent("invalid_delivery_id", error.to_string())
            })?;
            let mut transaction = pool.begin().await.map_err(|error| {
                MessageHandlerError::retryable("postgres_begin_failed", error.to_string())
            })?;
            sqlx::query(
                "INSERT INTO rust_052a_ack_probe (delivery_id) VALUES ($1) ON CONFLICT (delivery_id) DO NOTHING",
            )
            .bind(delivery_id)
            .execute(&mut *transaction)
            .await
            .map_err(|error| {
                MessageHandlerError::retryable("postgres_write_failed", error.to_string())
            })?;
            transaction.commit().await.map_err(|error| {
                MessageHandlerError::retryable("postgres_commit_failed", error.to_string())
            })?;

            // Simulate losing the ACK path after the business transaction committed.
            // JetStream must redeliver; the second handler run must be idempotent.
            if fail_once.swap(false, std::sync::atomic::Ordering::SeqCst) {
                return Err(MessageHandlerError::retryable(
                    "ack_path_interrupted",
                    "simulated interruption after PostgreSQL commit",
                ));
            }
            Ok(())
        })
    }
}

#[tokio::test]
async fn duplicate_jetstream_delivery_after_commit_has_one_postgres_effect() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

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
    let subject = format!("qembed.rust063.duplicate.{suffix}");
    let stream_name = format!("QEMBED_RUST063_{suffix}");
    let durable_name = format!("qembed-rust063-{suffix}");
    let stream = context
        .get_or_create_stream(StreamConfig {
            name: stream_name,
            subjects: vec![subject.clone()],
            max_messages: 100,
            ..Default::default()
        })
        .await
        .expect("isolated JetStream stream is provisioned");
    let durable = stream
        .get_or_create_consumer(
            &durable_name,
            consumer::pull::Config {
                durable_name: Some(durable_name.clone()),
                ack_policy: AckPolicy::Explicit,
                ack_wait: Duration::from_secs(2),
                max_deliver: 5,
                ..Default::default()
            },
        )
        .await
        .expect("durable pull consumer is provisioned");
    let consumer = NatsJetStreamConsumer::new(
        durable,
        context.clone(),
        NatsJetStreamConsumerConfig::new(
            "qembed.dlq.rust063",
            5,
            Duration::from_millis(100),
            Duration::from_secs(2),
            2,
        )
        .expect("valid retry policy"),
    );
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

    let handler = CommitThenRetryOnce {
        pool: pool.clone(),
        fail_once: Arc::new(AtomicBool::new(true)),
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let first = consumer
        .process_batch(&handler, 1, Duration::from_secs(2))
        .await
        .expect("first delivery is handled with a retryable interruption");
    assert_eq!(first.received, 1);
    assert_eq!(first.acknowledged, 0);
    assert_eq!(first.retry_scheduled, 1);

    // The first worker committed PostgreSQL but exited before ACK. A separate
    // test process now reconnects to the same durable consumer and handles redelivery.
    drop(consumer);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let child = Command::new(std::env::current_exe().expect("test executable path is available"))
        .args([
            "--exact",
            "consumer_worker_process_entrypoint",
            "--nocapture",
        ])
        .env("RUST_064_CONSUMER_SUBJECT", &subject)
        .env("RUST_064_CONSUMER_STREAM", &stream_name)
        .env("RUST_064_CONSUMER_DURABLE", &durable_name)
        .output()
        .expect("replacement consumer worker process starts");
    assert!(
        child.status.success(),
        "replacement consumer worker failed: {}{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    assert_eq!(handler.calls.load(Ordering::SeqCst), 1);

    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM rust_052a_ack_probe WHERE delivery_id = $1")
            .bind(delivery_id)
            .fetch_one(&pool)
            .await
            .expect("business marker count is readable");
    assert_eq!(
        count, 1,
        "duplicate delivery after the first commit must not duplicate the business effect"
    );

    sqlx::query("DELETE FROM rust_052a_ack_probe WHERE delivery_id = $1")
        .bind(delivery_id)
        .execute(&pool)
        .await
        .expect("test marker is cleaned up");
}

#[tokio::test]
async fn consumer_worker_process_entrypoint() {
    let Ok(subject) = std::env::var("RUST_064_CONSUMER_SUBJECT") else {
        return;
    };
    let stream_name =
        std::env::var("RUST_064_CONSUMER_STREAM").expect("child stream name is provided");
    let durable_name =
        std::env::var("RUST_064_CONSUMER_DURABLE").expect("child durable name is provided");
    let nats_url = std::env::var("NATS_URL").expect("NATS_URL is set by test-postgres.sh");
    let database_url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL is set by test-postgres.sh");
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("replacement worker connects to PostgreSQL");
    let context = jetstream::new(
        async_nats::connect(&nats_url)
            .await
            .expect("replacement worker reconnects to NATS"),
    );
    let stream = context
        .get_or_create_stream(StreamConfig {
            name: stream_name,
            subjects: vec![subject],
            max_messages: 100,
            ..Default::default()
        })
        .await
        .expect("replacement worker resolves the existing stream");
    let durable = stream
        .get_or_create_consumer(
            &durable_name,
            consumer::pull::Config {
                durable_name: Some(durable_name.clone()),
                ack_policy: AckPolicy::Explicit,
                ack_wait: Duration::from_secs(2),
                max_deliver: 5,
                ..Default::default()
            },
        )
        .await
        .expect("replacement worker resumes the existing durable consumer");
    let consumer = NatsJetStreamConsumer::new(
        durable,
        context,
        NatsJetStreamConsumerConfig::new(
            "qembed.dlq.rust063",
            5,
            Duration::from_millis(100),
            Duration::from_secs(2),
            2,
        )
        .expect("valid retry policy"),
    );
    let outcome = consumer
        .process_batch(
            &CommitHandler { pool: pool.clone() },
            1,
            Duration::from_secs(2),
        )
        .await
        .expect("replacement worker processes the unacknowledged delivery");
    assert_eq!(outcome.received, 1);
    assert_eq!(outcome.acknowledged, 1);
    assert_eq!(outcome.handler_failed, 0);
    pool.close().await;
}
