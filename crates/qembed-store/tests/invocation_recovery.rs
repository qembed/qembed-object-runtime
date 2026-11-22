mod common;

use std::time::{Duration, SystemTime};

use qembed_domain::ports::PublicationReceipt;
use qembed_domain::{
    ids::{
        CallerId, EventId, InvocationId, ObjectId, ObjectVersionId, SnapshotId, TenantId, TraceId,
    },
    test_support::scope_for_tenant,
};
use qembed_store::{
    invocation_recovery::PgInvocationRecoverySweeper, outbox_dispatcher::PgOutboxDispatcher,
    run_migrations,
};
use sqlx::PgPool;
use uuid::Uuid;

struct Fixture {
    pool: PgPool,
    tenant_id: TenantId,
    invocation_id: InvocationId,
    delivery_id: EventId,
    scope: qembed_domain::scope::Scope,
}

async fn setup(provider_attempt_state: &str) -> Fixture {
    let pool = common::connect().await;
    run_migrations(&pool)
        .await
        .expect("all store migrations should apply");

    let tenant_id = TenantId::new();
    let object_id = ObjectId::new();
    let caller_id = CallerId::new();
    let version_id = ObjectVersionId::new();
    let snapshot_id = SnapshotId::new();
    let invocation_id = InvocationId::new();
    let delivery_id = EventId::new();
    let trace_id = TraceId::new();
    let canonical_json = br#"{"input":"recovery-test"}"#.to_vec();
    let json_projection = serde_json::json!({"input":"recovery-test"});

    sqlx::query("INSERT INTO tenants (id) VALUES ($1)")
        .bind(tenant_id.into_uuid())
        .execute(&pool)
        .await
        .expect("tenant exists");

    sqlx::query(
        "INSERT INTO callers (id, tenant_id, token_hash, caller_kind) VALUES ($1, $2, $3, 'Anonymous')",
    )
    .bind(caller_id.into_uuid())
    .bind(tenant_id.into_uuid())
    .bind(Uuid::new_v4().as_bytes().to_vec())
    .execute(&pool)
    .await
    .expect("caller exists");

    sqlx::query("INSERT INTO objects (tenant_id, object_id) VALUES ($1, $2)")
        .bind(tenant_id.into_uuid())
        .bind(object_id.into_uuid())
        .execute(&pool)
        .await
        .expect("object exists");

    sqlx::query(
        "INSERT INTO object_versions (tenant_id, object_id, object_version_id, version_number) VALUES ($1, $2, $3, 1)",
    )
    .bind(tenant_id.into_uuid())
    .bind(object_id.into_uuid())
    .bind(version_id.into_uuid())
    .execute(&pool)
    .await
    .expect("object version exists");

    sqlx::query(
        "INSERT INTO snapshots (tenant_id, snapshot_id, object_id, object_version_id, schema_version, canonical_json, content_hash, json_projection) VALUES ($1, $2, $3, $4, 1, $5, $6, $7)",
    )
    .bind(tenant_id.into_uuid())
    .bind(snapshot_id.into_uuid())
    .bind(object_id.into_uuid())
    .bind(version_id.into_uuid())
    .bind(canonical_json)
    .bind(vec![7_u8; 32])
    .bind(json_projection)
    .execute(&pool)
    .await
    .expect("snapshot exists");

    sqlx::query(
        "INSERT INTO object_invocations (id, tenant_id, object_id, caller_id, snapshot_id, knowledge_revision, pool, status, trace_id, started_at) VALUES ($1, $2, $3, $4, $5, 0, 'anonymous', 'Received', $6, now() - interval '2 hours')",
    )
    .bind(invocation_id.into_uuid())
    .bind(tenant_id.into_uuid())
    .bind(object_id.into_uuid())
    .bind(caller_id.into_uuid())
    .bind(snapshot_id.into_uuid())
    .bind(trace_id.into_uuid())
    .execute(&pool)
    .await
    .expect("queued invocation exists");

    sqlx::query(
        "INSERT INTO provider_call_attempts (tenant_id, object_id, invocation_id, state) VALUES ($1, $2, $3, $4)",
    )
    .bind(tenant_id.into_uuid())
    .bind(object_id.into_uuid())
    .bind(invocation_id.into_uuid())
    .bind(provider_attempt_state)
    .execute(&pool)
    .await
    .expect("provider attempt evidence exists");

    let payload = serde_json::json!({
        "type": "invocation_received",
        "invocation_id": invocation_id,
        "object_id": object_id,
    });
    sqlx::query(
        "INSERT INTO events (tenant_id, event_id, aggregate_type, aggregate_id, event_type, payload) VALUES ($1, $2, 'invocation', $3, 'invocation.received', $4)",
    )
    .bind(tenant_id.into_uuid())
    .bind(delivery_id.into_uuid())
    .bind(invocation_id.into_uuid())
    .bind(payload.clone())
    .execute(&pool)
    .await
    .expect("durable invocation event exists");

    sqlx::query(
        "INSERT INTO outbox_messages (tenant_id, delivery_id, event_id, topic, payload, delivered_at, publication_stream, publication_sequence, publication_duplicate) VALUES ($1, $2, $2, 'invocation.received', $3, now(), 'QEMBED_EVENTS', 1, false)",
    )
    .bind(tenant_id.into_uuid())
    .bind(delivery_id.into_uuid())
    .bind(payload)
    .execute(&pool)
    .await
    .expect("original Outbox delivery is already published");

    sqlx::query(
        "INSERT INTO outbox_publication_receipts (tenant_id, delivery_id, publication_stream, publication_sequence, publication_duplicate) VALUES ($1, $2, 'QEMBED_EVENTS', 1, false)",
    )
    .bind(tenant_id.into_uuid())
    .bind(delivery_id.into_uuid())
    .execute(&pool)
    .await
    .expect("first PubAck is retained");

    Fixture {
        pool,
        tenant_id,
        invocation_id,
        delivery_id,
        scope: scope_for_tenant(tenant_id),
    }
}

#[tokio::test]
async fn stale_received_invocation_replays_same_outbox_identity_and_preserves_puback_history() {
    let fixture = setup("NotStarted").await;
    let sweeper = PgInvocationRecoverySweeper::new(fixture.pool.clone());
    let cutoff = SystemTime::now() - Duration::from_secs(60);

    let first = sweeper
        .sweep_before(&fixture.scope, cutoff, 10)
        .await
        .expect("stale queued invocation should be marked");
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].invocation_id(), fixture.invocation_id);
    assert_eq!(first[0].delivery_id(), fixture.delivery_id);

    let second = sweeper
        .sweep_before(&fixture.scope, cutoff, 10)
        .await
        .expect("a repeated sweep should be idempotent");
    assert!(second.is_empty());

    let marker: (Option<String>, Option<String>, i32) = sqlx::query_as(
        "SELECT replay_requested_at::text, replay_reason, replay_count FROM outbox_messages WHERE tenant_id=$1 AND delivery_id=$2",
    )
    .bind(fixture.tenant_id.into_uuid())
    .bind(fixture.delivery_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await
    .expect("replay marker is readable");
    assert!(marker.0.is_some());
    assert_eq!(
        marker.1.as_deref(),
        Some("invocation_progress_deadline_expired")
    );
    assert_eq!(marker.2, 1);

    let relay = PgOutboxDispatcher::new(fixture.pool.clone());
    // The dispatcher is deliberately cross-tenant. Claim a bounded batch,
    // retain the row under test, and release unrelated pending deliveries.
    let mut claimed = relay
        .claim_batch("recovery-relay", "invocation.received", 500, 30)
        .await
        .expect("relay claims marked and pending invocation deliveries");
    let target_index = claimed
        .iter()
        .position(|delivery| delivery.delivery_id() == fixture.delivery_id.to_string())
        .expect("the marked original delivery is in the claimed batch");
    let target = claimed.remove(target_index);
    for unrelated in &claimed {
        assert!(
            relay
                .release(unrelated, "recovery-relay")
                .await
                .expect("unrelated delivery lease is released")
        );
    }
    assert_eq!(
        target.payload()["invocation_id"],
        fixture.invocation_id.to_string()
    );

    assert!(
        relay
            .acknowledge(
                &target,
                "recovery-relay",
                PublicationReceipt::new("QEMBED_EVENTS", 2, false),
            )
            .await
            .expect("successful replay PubAck is recorded")
    );

    let state: (Option<String>, Option<String>, i32, i64) = sqlx::query_as(
        "SELECT replay_requested_at::text, replay_reason, replay_count, (SELECT count(*) FROM outbox_publication_receipts WHERE tenant_id=$1 AND delivery_id=$2) FROM outbox_messages WHERE tenant_id=$1 AND delivery_id=$2",
    )
    .bind(fixture.tenant_id.into_uuid())
    .bind(fixture.delivery_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await
    .expect("replay completion and receipt history are readable");
    assert_eq!(state.0, None);
    assert_eq!(state.1, None);
    assert_eq!(state.2, 1);
    assert_eq!(
        state.3, 2,
        "the first PubAck remains in append-only history"
    );

    fixture.pool.close().await;
}

#[tokio::test]
async fn recovery_fails_closed_for_unknown_provider_outcome() {
    let fixture = setup("OutcomeUnknown").await;
    let sweeper = PgInvocationRecoverySweeper::new(fixture.pool.clone());
    let cutoff = SystemTime::now() - Duration::from_secs(60);

    let replayed = sweeper
        .sweep_before(&fixture.scope, cutoff, 10)
        .await
        .expect("unknown provider outcomes are skipped, not retried");
    assert!(replayed.is_empty());

    let marker: Option<String> = sqlx::query_scalar(
        "SELECT replay_requested_at::text FROM outbox_messages WHERE tenant_id=$1 AND delivery_id=$2",
    )
    .bind(fixture.tenant_id.into_uuid())
    .bind(fixture.delivery_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await
    .expect("marker state is readable");
    assert_eq!(marker, None);

    fixture.pool.close().await;
}

#[tokio::test]
async fn recovery_does_not_replay_received_invocation_with_a_live_lease() {
    let fixture = setup("NotStarted").await;
    sqlx::query(
        "UPDATE object_invocations SET lease_owner='active-worker', lease_until=now() + interval '30 minutes', heartbeat_at=now() - interval '2 hours' WHERE tenant_id=$1 AND id=$2",
    )
    .bind(fixture.tenant_id.into_uuid())
    .bind(fixture.invocation_id.into_uuid())
    .execute(&fixture.pool)
    .await
    .expect("a live invocation lease is established");

    let sweeper = PgInvocationRecoverySweeper::new(fixture.pool.clone());
    let cutoff = SystemTime::now() - Duration::from_secs(60);
    let replayed = sweeper
        .sweep_before(&fixture.scope, cutoff, 10)
        .await
        .expect("live leased invocation is excluded");
    assert!(replayed.is_empty());

    let marker: Option<String> = sqlx::query_scalar(
        "SELECT replay_requested_at::text FROM outbox_messages WHERE tenant_id=$1 AND delivery_id=$2",
    )
    .bind(fixture.tenant_id.into_uuid())
    .bind(fixture.delivery_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await
    .expect("marker state is readable");
    assert_eq!(marker, None);

    fixture.pool.close().await;
}
