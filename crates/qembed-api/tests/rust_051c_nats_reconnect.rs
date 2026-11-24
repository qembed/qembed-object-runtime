use std::{
    net::{SocketAddr, TcpStream},
    process::Command,
    thread,
    time::{Duration, Instant, SystemTime},
};

use async_nats::jetstream::{
    self,
    consumer::{self, AckPolicy},
    stream::Config as StreamConfig,
};
use futures_util::StreamExt;
use qembed_api::adapters::nats_jetstream_publisher::NatsJetStreamPublisher;
use qembed_domain::{
    ids::{
        CallerId, EventId, InvocationId, ObjectId, ObjectVersionId, SnapshotId, TenantId, TraceId,
    },
    ports::{MessagePublisher, OutboundMessage},
    test_support::scope_for_tenant,
};
use qembed_runtime::outbox_relay::{OutboxRelay, OutboxRelayConfig};
use qembed_store::{
    invocation_recovery::PgInvocationRecoverySweeper, outbox_dispatcher::PgOutboxDispatcher,
    run_migrations,
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

#[allow(dead_code)]
mod support;

fn compose(args: &[&str]) {
    let status = Command::new("docker")
        .args([
            "compose",
            "--project-name",
            "qembed-rust-test",
            "--file",
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../compose.test.yaml"),
        ])
        .args(args)
        .status()
        .expect("Docker Compose is available");
    assert!(status.success(), "docker compose {args:?} should succeed");
}

fn wait_for_nats(address: SocketAddr) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&address, Duration::from_millis(250)).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("NATS did not become available at {address}");
}

#[tokio::test]
#[ignore = "restarts the shared NATS container; invoked separately by scripts/test-postgres.sh"]
async fn existing_async_nats_client_reconnects_after_server_restart() {
    let url = std::env::var("NATS_URL").expect("NATS_URL is set by test-postgres.sh");
    let publisher = NatsJetStreamPublisher::connect(&url)
        .await
        .expect("publisher connects before outage");

    let admin = async_nats::connect(&url)
        .await
        .expect("admin client connects before outage");
    let context = jetstream::new(admin);
    let suffix = Uuid::new_v4().simple().to_string();
    let subject = format!("qembed.rust051c.reconnect.{suffix}");
    let stream = context
        .get_or_create_stream(StreamConfig {
            name: format!("QEMBED_RUST051C_RECONNECT_{}", suffix.to_uppercase()),
            subjects: vec![subject.clone()],
            duplicate_window: Duration::from_secs(120),
            max_messages: 100,
            ..Default::default()
        })
        .await
        .expect("reconnect test stream is provisioned");

    let address: SocketAddr = "127.0.0.1:4222".parse().expect("valid NATS address");
    compose(&["stop", "nats"]);
    compose(&["start", "nats"]);
    wait_for_nats(address);

    let message = OutboundMessage::new(
        Uuid::new_v4().to_string(),
        subject,
        serde_json::json!({"kind":"reconnect-probe"}),
    );
    let publish = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match publisher.publish(message.clone()).await {
                Ok(receipt) => break receipt,
                Err(_) => tokio::time::sleep(Duration::from_millis(250)).await,
            }
        }
    })
    .await
    .expect("existing client reconnects and receives PubAck");

    assert_eq!(publish.stream(), stream.cached_info().config.name);
    let info = stream
        .get_info()
        .await
        .expect("stream remains available after restart");
    assert_eq!(info.state.messages, 1);
}

async fn recovery_database() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL is set by test-postgres.sh");
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&url)
        .await
        .expect("PostgreSQL is available");
    run_migrations(&pool).await.expect("migrations apply");
    pool
}

async fn seed_stale_invocations(
    pool: &PgPool,
    subject: &str,
) -> (TenantId, Vec<(InvocationId, EventId)>) {
    let tenant_id = TenantId::new();
    let caller_id = CallerId::new();
    let object_id = ObjectId::new();
    let version_id = ObjectVersionId::new();
    let snapshot_id = SnapshotId::new();

    sqlx::query("INSERT INTO tenants (id) VALUES ($1)")
        .bind(tenant_id.into_uuid())
        .execute(pool)
        .await
        .expect("tenant exists");
    sqlx::query(
        "INSERT INTO callers (id, tenant_id, token_hash, caller_kind) VALUES ($1, $2, $3, 'Anonymous')",
    )
    .bind(caller_id.into_uuid())
    .bind(tenant_id.into_uuid())
    .bind(Uuid::new_v4().as_bytes().to_vec())
    .execute(pool)
    .await
    .expect("caller exists");
    sqlx::query("INSERT INTO objects (tenant_id, object_id) VALUES ($1, $2)")
        .bind(tenant_id.into_uuid())
        .bind(object_id.into_uuid())
        .execute(pool)
        .await
        .expect("object exists");
    sqlx::query(
        "INSERT INTO object_versions (tenant_id, object_id, object_version_id, version_number) VALUES ($1, $2, $3, 1)",
    )
    .bind(tenant_id.into_uuid())
    .bind(object_id.into_uuid())
    .bind(version_id.into_uuid())
    .execute(pool)
    .await
    .expect("object version exists");
    sqlx::query(
        "INSERT INTO snapshots (tenant_id, snapshot_id, object_id, object_version_id, schema_version, canonical_json, content_hash, json_projection) VALUES ($1, $2, $3, $4, 1, $5, $6, $7)",
    )
    .bind(tenant_id.into_uuid())
    .bind(snapshot_id.into_uuid())
    .bind(object_id.into_uuid())
    .bind(version_id.into_uuid())
    .bind(br#"{"input":"rust-064-recovery"}"#.to_vec())
    .bind(vec![9_u8; 32])
    .bind(serde_json::json!({"input":"rust-064-recovery"}))
    .execute(pool)
    .await
    .expect("snapshot exists");

    let mut seeded = Vec::new();
    for _ in 0..2 {
        let invocation_id = InvocationId::new();
        let delivery_id = EventId::new();
        let trace_id = TraceId::new();
        let payload = serde_json::json!({
            "type": "invocation_received",
            "invocation_id": invocation_id,
            "object_id": object_id,
        });
        sqlx::query(
            "INSERT INTO object_invocations (id, tenant_id, object_id, caller_id, snapshot_id, knowledge_revision, pool, status, trace_id, started_at) VALUES ($1, $2, $3, $4, $5, 0, 'anonymous', 'Received', $6, now() - interval '2 hours')",
        )
        .bind(invocation_id.into_uuid())
        .bind(tenant_id.into_uuid())
        .bind(object_id.into_uuid())
        .bind(caller_id.into_uuid())
        .bind(snapshot_id.into_uuid())
        .bind(trace_id.into_uuid())
        .execute(pool)
        .await
        .expect("stale Received invocation exists");
        sqlx::query(
            "INSERT INTO provider_call_attempts (tenant_id, object_id, invocation_id, state) VALUES ($1, $2, $3, 'NotStarted')",
        )
        .bind(tenant_id.into_uuid())
        .bind(object_id.into_uuid())
        .bind(invocation_id.into_uuid())
        .execute(pool)
        .await
        .expect("provider attempt is known not to have started");
        sqlx::query(
            "INSERT INTO events (tenant_id, event_id, aggregate_type, aggregate_id, event_type, payload) VALUES ($1, $2, 'invocation', $3, 'invocation.received', $4)",
        )
        .bind(tenant_id.into_uuid())
        .bind(delivery_id.into_uuid())
        .bind(invocation_id.into_uuid())
        .bind(&payload)
        .execute(pool)
        .await
        .expect("durable invocation event exists");
        sqlx::query(
            "INSERT INTO outbox_messages (tenant_id, delivery_id, event_id, topic, payload) VALUES ($1, $2, $2, $3, $4)",
        )
        .bind(tenant_id.into_uuid())
        .bind(delivery_id.into_uuid())
        .bind(subject)
        .bind(payload)
        .execute(pool)
        .await
        .expect("original Outbox delivery exists");
        seeded.push((invocation_id, delivery_id));
    }
    (tenant_id, seeded)
}

struct RestoreNatsContainer;

impl Drop for RestoreNatsContainer {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args([
                "compose",
                "--project-name",
                "qembed-rust-test",
                "--file",
                concat!(env!("CARGO_MANIFEST_DIR"), "/../../compose.test.yaml"),
                "up",
                "--detach",
                "--wait",
                "nats",
            ])
            .status();
    }
}

#[tokio::test]
#[ignore = "removes and recreates the shared NATS container to simulate JetStream storage loss"]
async fn jetstream_storage_loss_before_and_after_delivery_replays_from_postgres() {
    let pool = recovery_database().await;
    let url = std::env::var("NATS_URL").expect("NATS_URL is set by test-postgres.sh");
    let suffix = Uuid::new_v4().simple().to_string();
    let subject = format!("qembed.rust064.loss.{suffix}");
    let stream_name = format!("QEMBED_RUST064_LOSS_{}", suffix.to_uppercase());
    let durable_name = format!("qembed-rust064-loss-{suffix}");
    let client = async_nats::connect(&url)
        .await
        .expect("initial NATS is available");
    let context = jetstream::new(client);
    let stream = context
        .get_or_create_stream(StreamConfig {
            name: stream_name.clone(),
            subjects: vec![subject.clone()],
            duplicate_window: Duration::from_millis(100),
            max_messages: 100,
            ..Default::default()
        })
        .await
        .expect("initial stream is provisioned");
    let (tenant_id, seeded) = seed_stale_invocations(&pool, &subject).await;
    let delivery_ids: Vec<String> = seeded
        .iter()
        .map(|(_, delivery_id)| delivery_id.to_string())
        .collect();

    let initial_publisher = NatsJetStreamPublisher::new(context.clone());
    let initial_relay = OutboxRelay::new(
        PgOutboxDispatcher::new(pool.clone()),
        initial_publisher.clone(),
        OutboxRelayConfig::new("relay-before-loss", &subject, 10, 30)
            .expect("initial relay configuration is valid"),
    );
    let initial_outcome = initial_relay
        .run_once()
        .await
        .expect("both original deliveries receive PubAck");
    assert_eq!(initial_outcome.published, 2);
    assert_eq!(
        stream.get_info().await.expect("stream info").state.messages,
        2
    );

    let initial_consumer = stream
        .get_or_create_consumer(
            &durable_name,
            consumer::pull::Config {
                durable_name: Some(durable_name.clone()),
                ack_policy: AckPolicy::Explicit,
                ack_wait: Duration::from_secs(30),
                max_deliver: 5,
                ..Default::default()
            },
        )
        .await
        .expect("durable consumer is provisioned");
    let mut first_fetch = initial_consumer
        .fetch()
        .max_messages(1)
        .expires(Duration::from_secs(2))
        .messages()
        .await
        .expect("first consumer fetch starts");
    let delivered_before_loss = first_fetch
        .next()
        .await
        .expect("one message was delivered before broker loss")
        .expect("delivered message is valid");
    let delivered_id = delivered_before_loss
        .headers
        .as_ref()
        .and_then(|headers| headers.get("Qembed-Delivery-Id"))
        .map(ToString::to_string)
        .expect("stable Outbox delivery ID is present");
    assert!(delivery_ids.contains(&delivered_id));
    drop(delivered_before_loss);
    drop(first_fetch);
    drop(initial_consumer);
    drop(initial_relay);
    drop(initial_publisher);
    drop(stream);
    drop(context);

    // Removing the container discards its writable-layer JetStream store.
    // PostgreSQL remains untouched and is the recovery source of truth.
    let _restore = RestoreNatsContainer;
    compose(&["rm", "--force", "--stop", "nats"]);
    compose(&["up", "--detach", "--wait", "nats"]);
    wait_for_nats("127.0.0.1:4222".parse().expect("valid NATS address"));

    let context = jetstream::new(
        async_nats::connect(&url)
            .await
            .expect("fresh NATS connects"),
    );
    let stream = context
        .get_or_create_stream(StreamConfig {
            name: stream_name,
            subjects: vec![subject.clone()],
            duplicate_window: Duration::from_millis(100),
            max_messages: 100,
            ..Default::default()
        })
        .await
        .expect("empty replacement stream is provisioned");
    assert_eq!(
        stream
            .get_info()
            .await
            .expect("fresh stream info")
            .state
            .messages,
        0,
        "the broker restart intentionally lost both messages, including one already delivered"
    );

    let scope = scope_for_tenant(tenant_id);
    let sweeper = PgInvocationRecoverySweeper::new(support::signed_context(&pool).await);
    let replayed = sweeper
        .sweep_before(&scope, SystemTime::now() - Duration::from_secs(60), 10)
        .await
        .expect("PostgreSQL recovery marks stale Received invocations for replay");
    assert_eq!(replayed.len(), 2);
    let replayed_ids: Vec<String> = replayed
        .iter()
        .map(|delivery| delivery.delivery_id().to_string())
        .collect();
    assert_eq!(replayed_ids.len(), delivery_ids.len());
    for delivery_id in &delivery_ids {
        assert!(replayed_ids.contains(delivery_id));
    }

    let publisher = NatsJetStreamPublisher::new(context.clone());
    let relay = OutboxRelay::new(
        PgOutboxDispatcher::new(pool.clone()),
        publisher,
        OutboxRelayConfig::new("relay-after-broker-loss", &subject, 10, 30)
            .expect("recovery relay configuration is valid"),
    );
    let outcome = relay
        .run_once()
        .await
        .expect("PostgreSQL-marked Outbox deliveries are republished");
    assert_eq!(outcome.claimed, 2);
    assert_eq!(outcome.published, 2);
    assert_eq!(
        stream
            .get_info()
            .await
            .expect("replayed stream info")
            .state
            .messages,
        2
    );

    let consumer = stream
        .get_or_create_consumer(
            &durable_name,
            consumer::pull::Config {
                durable_name: Some(durable_name.clone()),
                ack_policy: AckPolicy::Explicit,
                ack_wait: Duration::from_secs(30),
                max_deliver: 5,
                ..Default::default()
            },
        )
        .await
        .expect("restarted worker recreates the durable consumer");
    let mut fetch = consumer
        .fetch()
        .max_messages(2)
        .expires(Duration::from_secs(2))
        .messages()
        .await
        .expect("replayed consumer fetch starts");
    let mut observed = Vec::new();
    while let Some(result) = fetch.next().await {
        let message = result.expect("replayed JetStream delivery is valid");
        let delivery_id = message
            .headers
            .as_ref()
            .and_then(|headers| headers.get("Qembed-Delivery-Id"))
            .map(ToString::to_string)
            .expect("replayed delivery retains its original identity");
        observed.push(delivery_id);
        message
            .double_ack()
            .await
            .expect("replayed delivery is acknowledged after inspection");
    }
    observed.sort();
    let mut expected = delivery_ids.clone();
    expected.sort();
    assert_eq!(observed, expected);

    for delivery_id in &delivery_ids {
        let state: (bool, bool, i64) = sqlx::query_as(
            "SELECT delivered_at IS NOT NULL, replay_requested_at IS NULL, (SELECT count(*) FROM outbox_publication_receipts WHERE tenant_id = $1 AND delivery_id = $2::uuid) FROM outbox_messages WHERE tenant_id = $1 AND delivery_id = $2::uuid",
        )
        .bind(tenant_id.into_uuid())
        .bind(delivery_id)
        .fetch_one(&pool)
        .await
        .expect("recovered Outbox state is readable");
        assert_eq!(state, (true, true, 2));
    }
    pool.close().await;
}
