use std::{
    error::Error,
    fmt, io,
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
};

use async_nats::jetstream::{self, stream::Config as StreamConfig};
use qembed_api::adapters::nats_jetstream_publisher::NatsJetStreamPublisher;
use qembed_domain::ports::{
    MessagePublisher, OutboundMessage, OutboxDeliveryStore, PublicationReceipt,
};
use qembed_runtime::outbox_relay::{OutboxRelay, OutboxRelayConfig};
use qembed_store::{
    outbox_dispatcher::{OutboxDelivery, PgOutboxDispatcher},
    run_migrations,
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

async fn database() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL is set by test-postgres.sh");
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&url)
        .await
        .expect("PostgreSQL is available");
    run_migrations(&pool).await.expect("migrations apply");
    pool
}

async fn jetstream() -> (
    NatsJetStreamPublisher,
    async_nats::jetstream::stream::Stream,
    String,
) {
    let url = std::env::var("NATS_URL").expect("NATS_URL is set by test-postgres.sh");
    let client = async_nats::connect(url)
        .await
        .expect("real NATS server is available");
    let context = jetstream::new(client);
    let suffix = Uuid::new_v4().simple().to_string();
    let subject = format!("qembed.rust051c.{suffix}");
    let stream_name = format!("QEMBED_RUST051C_{}", suffix.to_uppercase());
    let stream = context
        .get_or_create_stream(StreamConfig {
            name: stream_name,
            subjects: vec![subject.clone()],
            duplicate_window: Duration::from_secs(120),
            max_messages: 100,
            ..Default::default()
        })
        .await
        .expect("test stream is provisioned");
    (NatsJetStreamPublisher::new(context), stream, subject)
}

async fn enqueue(pool: &PgPool, topic: &str) -> (String, String) {
    let tenant_id = Uuid::new_v4();
    let event_id = Uuid::new_v4();
    let delivery_id = Uuid::new_v4();
    let aggregate_id = Uuid::new_v4();
    let payload = serde_json::json!({
        "type": "test.outbox.delivery",
        "delivery_id": delivery_id,
        "stable": true
    });

    sqlx::query(
        "INSERT INTO events (tenant_id, event_id, aggregate_type, aggregate_id, event_type, payload) VALUES ($1, $2, 'test', $3, $4, $5)",
    )
    .bind(tenant_id)
    .bind(event_id)
    .bind(aggregate_id)
    .bind(topic)
    .bind(&payload)
    .execute(pool)
    .await
    .expect("event is persisted");

    sqlx::query(
        "INSERT INTO outbox_messages (tenant_id, delivery_id, event_id, topic, payload) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(tenant_id)
    .bind(delivery_id)
    .bind(event_id)
    .bind(topic)
    .bind(payload)
    .execute(pool)
    .await
    .expect("outbox delivery is persisted");

    (tenant_id.to_string(), delivery_id.to_string())
}

fn relay_config(
    owner: &str,
    subject: &str,
    batch_size: u16,
    lease_seconds: u32,
) -> OutboxRelayConfig {
    OutboxRelayConfig::new(owner, subject, batch_size, lease_seconds).expect("valid relay config")
}

async fn expire_lease(pool: &PgPool, tenant_id: &str, delivery_id: &str) {
    sqlx::query(
        "UPDATE outbox_messages SET lease_expires_at = now() - interval '1 second' WHERE tenant_id = $1::uuid AND delivery_id = $2::uuid",
    )
    .bind(tenant_id)
    .bind(delivery_id)
    .execute(pool)
    .await
    .expect("test expires the abandoned lease");
}

async fn stream_message_count(stream: &async_nats::jetstream::stream::Stream) -> u64 {
    stream
        .get_info()
        .await
        .expect("stream info is available")
        .state
        .messages
}

async fn publication_state(
    pool: &PgPool,
    tenant_id: &str,
    delivery_id: &str,
) -> (bool, Option<i64>, i64) {
    let row: (bool, Option<i64>, i64) = sqlx::query_as(
        "SELECT delivered_at IS NOT NULL, publication_sequence, (SELECT count(*) FROM outbox_publication_receipts WHERE tenant_id = $1::uuid AND delivery_id = $2::uuid) FROM outbox_messages WHERE tenant_id = $1::uuid AND delivery_id = $2::uuid",
    )
    .bind(tenant_id)
    .bind(delivery_id)
    .fetch_one(pool)
    .await
    .expect("publication state is readable");
    row
}

async fn publication_duplicate(pool: &PgPool, tenant_id: &str, delivery_id: &str) -> bool {
    sqlx::query_scalar(
        "SELECT publication_duplicate FROM outbox_messages WHERE tenant_id = $1::uuid AND delivery_id = $2::uuid",
    )
    .bind(tenant_id)
    .bind(delivery_id)
    .fetch_one(pool)
    .await
    .expect("latest publication duplicate flag is readable")
}

#[derive(Debug)]
struct InjectedStoreError(&'static str);

impl fmt::Display for InjectedStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for InjectedStoreError {}

struct FailReceiptOnceStore {
    inner: PgOutboxDispatcher,
    fail_once: AtomicBool,
}

impl OutboxDeliveryStore for FailReceiptOnceStore {
    type Delivery = OutboxDelivery;
    type Error = InjectedStoreError;

    async fn claim_batch(
        &self,
        owner: &str,
        topic: &str,
        batch_size: u16,
        lease_seconds: u32,
    ) -> Result<Vec<Self::Delivery>, Self::Error> {
        self.inner
            .claim_batch(
                owner,
                topic,
                i64::from(batch_size),
                i64::from(lease_seconds),
            )
            .await
            .map_err(|_| InjectedStoreError("claim failed"))
    }

    fn outbound_message(&self, delivery: &Self::Delivery) -> OutboundMessage {
        OutboundMessage::new(
            delivery.delivery_id().to_owned(),
            delivery.topic().to_owned(),
            delivery.payload().clone(),
        )
    }

    async fn record_publication(
        &self,
        delivery: &Self::Delivery,
        owner: &str,
        receipt: PublicationReceipt,
    ) -> Result<bool, Self::Error> {
        if self.fail_once.swap(false, Ordering::SeqCst) {
            return Err(InjectedStoreError(
                "injected failure before PostgreSQL receipt write",
            ));
        }
        self.inner
            .acknowledge(delivery, owner, receipt)
            .await
            .map_err(|_| InjectedStoreError("receipt persistence failed"))
    }

    async fn release(&self, delivery: &Self::Delivery, owner: &str) -> Result<bool, Self::Error> {
        self.inner
            .release(delivery, owner)
            .await
            .map_err(|_| InjectedStoreError("lease release failed"))
    }
}

struct PublishThenLoseAckOnce {
    inner: NatsJetStreamPublisher,
    fail_once: AtomicBool,
}

impl MessagePublisher for PublishThenLoseAckOnce {
    type Error = io::Error;

    async fn publish(&self, message: OutboundMessage) -> Result<PublicationReceipt, Self::Error> {
        let receipt = self
            .inner
            .publish(message)
            .await
            .map_err(|error| io::Error::other(error.to_string()))?;
        if self.fail_once.swap(false, Ordering::SeqCst) {
            return Err(io::Error::other(
                "injected lost PubAck after server acceptance",
            ));
        }
        Ok(receipt)
    }
}

#[tokio::test]
async fn crash_after_claim_before_publish_reclaims_same_delivery() {
    let pool = database().await;
    let (publisher, stream, subject) = jetstream().await;
    let (tenant_id, delivery_id) = enqueue(&pool, &subject).await;
    drop(publisher);

    let child = run_child_process(&subject, "relay-crashed", "claim_only");
    assert!(
        child.status.success(),
        "claiming child process failed: {}{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    assert_eq!(stream_message_count(&stream).await, 0);

    expire_lease(&pool, &tenant_id, &delivery_id).await;
    let publisher =
        NatsJetStreamPublisher::connect(&std::env::var("NATS_URL").expect("NATS_URL is set"))
            .await
            .expect("restarted relay connects");
    let relay = OutboxRelay::new(
        PgOutboxDispatcher::new(pool.clone()),
        publisher,
        relay_config("relay-restarted", &subject, 10, 30),
    );
    let outcome = relay.run_once().await.expect("restarted relay publishes");
    assert_eq!(outcome.claimed, 1);
    assert_eq!(outcome.published, 1);
    assert_eq!(stream_message_count(&stream).await, 1);
    assert_eq!(
        publication_state(&pool, &tenant_id, &delivery_id).await,
        (true, Some(1), 1)
    );
    assert!(!publication_duplicate(&pool, &tenant_id, &delivery_id).await);
    pool.close().await;
}

#[tokio::test]
async fn publish_accepted_but_puback_lost_retries_with_same_id_and_jetstream_deduplicates() {
    let pool = database().await;
    let (publisher, stream, subject) = jetstream().await;
    let (tenant_id, delivery_id) = enqueue(&pool, &subject).await;
    let dispatcher = PgOutboxDispatcher::new(pool.clone());
    let ambiguous = PublishThenLoseAckOnce {
        inner: publisher.clone(),
        fail_once: AtomicBool::new(true),
    };

    let first = OutboxRelay::new(
        dispatcher.clone(),
        ambiguous,
        relay_config("relay-a", &subject, 10, 30),
    );
    let first_outcome = first
        .run_once()
        .await
        .expect("transport failure is released");
    assert_eq!(first_outcome.publish_failed, 1);
    assert_eq!(first_outcome.published, 0);

    let retry = OutboxRelay::new(
        dispatcher,
        publisher,
        relay_config("relay-b", &subject, 10, 30),
    );
    let second_outcome = retry.run_once().await.expect("same delivery is retried");
    assert_eq!(second_outcome.published, 1);
    assert_eq!(stream_message_count(&stream).await, 1);
    assert!(publication_duplicate(&pool, &tenant_id, &delivery_id).await);
    assert_eq!(
        publication_state(&pool, &tenant_id, &delivery_id).await,
        (true, Some(1), 1)
    );
    pool.close().await;
}

#[tokio::test]
async fn puback_before_postgres_receipt_failure_replays_same_delivery_id() {
    let pool = database().await;
    let (publisher, stream, subject) = jetstream().await;
    let (tenant_id, delivery_id) = enqueue(&pool, &subject).await;
    let store = FailReceiptOnceStore {
        inner: PgOutboxDispatcher::new(pool.clone()),
        fail_once: AtomicBool::new(true),
    };
    let relay = OutboxRelay::new(
        store,
        publisher.clone(),
        relay_config("relay-a", &subject, 10, 1),
    );

    assert!(
        relay.run_once().await.is_err(),
        "the injected PostgreSQL receipt write fails after JetStream PubAck"
    );
    assert_eq!(stream_message_count(&stream).await, 1);
    assert_eq!(
        publication_state(&pool, &tenant_id, &delivery_id).await,
        (false, None, 0)
    );

    expire_lease(&pool, &tenant_id, &delivery_id).await;
    let retry = OutboxRelay::new(
        PgOutboxDispatcher::new(pool.clone()),
        publisher,
        relay_config("relay-b", &subject, 10, 30),
    );
    let outcome = retry.run_once().await.expect("replay succeeds");
    assert_eq!(outcome.published, 1);
    assert_eq!(stream_message_count(&stream).await, 1);
    assert!(publication_duplicate(&pool, &tenant_id, &delivery_id).await);
    assert_eq!(
        publication_state(&pool, &tenant_id, &delivery_id).await,
        (true, Some(1), 1)
    );
    pool.close().await;
}

fn run_child_process(topic: &str, owner: &str, mode: &str) -> std::process::Output {
    Command::new(std::env::current_exe().expect("test executable path is available"))
        .args(["--exact", "child_process_relay_entrypoint", "--nocapture"])
        .env("RUST_051C_CHILD_TOPIC", topic)
        .env("RUST_051C_CHILD_OWNER", owner)
        .env("RUST_051C_CHILD_MODE", mode)
        .output()
        .expect("child relay process starts")
}

#[tokio::test]
async fn child_process_relay_entrypoint() {
    let Ok(topic) = std::env::var("RUST_051C_CHILD_TOPIC") else {
        return;
    };
    let owner = std::env::var("RUST_051C_CHILD_OWNER").expect("child owner is provided");
    let mode = std::env::var("RUST_051C_CHILD_MODE").expect("child mode is provided");
    let pool = database().await;
    let dispatcher = PgOutboxDispatcher::new(pool.clone());

    if mode == "before_nats_connect" {
        // Simulate a worker process exiting before it has opened a broker connection.
        pool.close().await;
        return;
    }

    if mode == "claim_only" {
        let claimed = dispatcher
            .claim_batch(&owner, &topic, 1, 1)
            .await
            .expect("child claims delivery before exiting");
        assert_eq!(claimed.len(), 1);
        pool.close().await;
        return;
    }

    let url = std::env::var("NATS_URL").expect("NATS_URL is provided");
    let publisher = NatsJetStreamPublisher::connect(&url)
        .await
        .expect("child process connects to JetStream");
    let relay = OutboxRelay::new(dispatcher, publisher, relay_config(&owner, &topic, 1, 30));
    let outcome = relay.run_once().await.expect("child relay batch succeeds");
    assert_eq!(outcome.claimed, 1);
    assert_eq!(outcome.published, 1);
    pool.close().await;
}

#[tokio::test]
async fn independent_relay_processes_claim_disjoint_rows_and_publish_both() {
    let pool = database().await;
    let (publisher, stream, subject) = jetstream().await;
    drop(publisher);
    enqueue(&pool, &subject).await;
    enqueue(&pool, &subject).await;

    let topic_a = subject.clone();
    let topic_b = subject.clone();
    let child_a = thread::spawn(move || run_child_process(&topic_a, "relay-process-a", "publish"));
    let child_b = thread::spawn(move || run_child_process(&topic_b, "relay-process-b", "publish"));
    let output_a = child_a.join().expect("child A joins");
    let output_b = child_b.join().expect("child B joins");
    assert!(
        output_a.status.success(),
        "child A failed: {}{}",
        String::from_utf8_lossy(&output_a.stdout),
        String::from_utf8_lossy(&output_a.stderr)
    );
    assert!(
        output_b.status.success(),
        "child B failed: {}{}",
        String::from_utf8_lossy(&output_b.stdout),
        String::from_utf8_lossy(&output_b.stderr)
    );

    let delivered: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox_messages WHERE topic = $1 AND delivered_at IS NOT NULL",
    )
    .bind(&subject)
    .fetch_one(&pool)
    .await
    .expect("both rows are acknowledged in PostgreSQL");
    assert_eq!(delivered, 2);
    assert_eq!(stream_message_count(&stream).await, 2);
    pool.close().await;
}

#[tokio::test]
async fn nats_connect_failure_before_claim_leaves_outbox_unclaimed() {
    let pool = database().await;
    let (publisher, stream, subject) = jetstream().await;
    let (tenant_id, delivery_id) = enqueue(&pool, &subject).await;

    let unavailable = NatsJetStreamPublisher::connect("nats://127.0.0.1:1").await;
    assert!(
        unavailable.is_err(),
        "an unavailable NATS endpoint must fail before the relay claims PostgreSQL work"
    );

    let state: (bool, Option<String>, bool, i32) = sqlx::query_as(
        "SELECT delivered_at IS NOT NULL, lease_owner, lease_expires_at IS NOT NULL, attempts FROM outbox_messages WHERE tenant_id = $1::uuid AND delivery_id = $2::uuid",
    )
    .bind(&tenant_id)
    .bind(&delivery_id)
    .fetch_one(&pool)
    .await
    .expect("outbox state is readable after failed NATS connection");
    assert_eq!(
        state,
        (false, None, false, 0),
        "connection failure before relay startup must not claim or mutate the Outbox row"
    );
    assert_eq!(stream_message_count(&stream).await, 0);

    let relay = OutboxRelay::new(
        PgOutboxDispatcher::new(pool.clone()),
        publisher,
        relay_config("relay-after-connect-recovery", &subject, 10, 30),
    );
    let outcome = relay
        .run_once()
        .await
        .expect("the restarted relay publishes the still-pending delivery");
    assert_eq!(outcome.claimed, 1);
    assert_eq!(outcome.published, 1);
    assert_eq!(stream_message_count(&stream).await, 1);
    assert_eq!(
        publication_state(&pool, &tenant_id, &delivery_id).await,
        (true, Some(1), 1)
    );
    pool.close().await;
}

#[tokio::test]
async fn worker_exit_before_nats_connect_leaves_outbox_pending() {
    let pool = database().await;
    let (publisher, stream, subject) = jetstream().await;
    let (tenant_id, delivery_id) = enqueue(&pool, &subject).await;

    let child =
        run_child_process(&subject, "relay-before-nats-connect", "before_nats_connect");
    assert!(
        child.status.success(),
        "worker process failed before NATS connect: {}{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );

    let state: (bool, Option<String>, bool, i32) = sqlx::query_as(
        "SELECT delivered_at IS NOT NULL, lease_owner, lease_expires_at IS NOT NULL, attempts FROM outbox_messages WHERE tenant_id = $1::uuid AND delivery_id = $2::uuid",
    )
    .bind(&tenant_id)
    .bind(&delivery_id)
    .fetch_one(&pool)
    .await
    .expect("Outbox state is readable after the worker exits");
    assert_eq!(state, (false, None, false, 0));
    assert_eq!(stream_message_count(&stream).await, 0);

    let relay = OutboxRelay::new(
        PgOutboxDispatcher::new(pool.clone()),
        publisher,
        relay_config("relay-after-preconnect-exit", &subject, 10, 30),
    );
    let outcome = relay
        .run_once()
        .await
        .expect("replacement relay publishes the still-pending delivery");
    assert_eq!(outcome.claimed, 1);
    assert_eq!(outcome.published, 1);
    assert_eq!(stream_message_count(&stream).await, 1);
    assert_eq!(
        publication_state(&pool, &tenant_id, &delivery_id).await,
        (true, Some(1), 1)
    );
    pool.close().await;
}
