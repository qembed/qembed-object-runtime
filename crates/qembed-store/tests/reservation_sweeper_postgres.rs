mod common;

use std::{
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};

use qembed_store::reservation_sweeper::PgReservationSweeper;
use ring::digest;
use sqlx::PgPool;
use uuid::Uuid;

async fn admin_pool() -> PgPool {
    common::connect_with_max_connections(4).await
}

async fn worker_pool() -> PgPool {
    // The integration harness connects as its database owner. Assume the exact
    // worker role on the sole pooled session to exercise production grants.
    let pool = common::connect_with_max_connections(1).await;
    let mut connection = pool.acquire().await.expect("worker connection");
    sqlx::query("SET ROLE qembed_worker")
        .execute(&mut *connection)
        .await
        .expect("test database owner should assume qembed_worker");
    drop(connection);
    pool
}

#[tokio::test]
#[ignore = "requires canonical qembed-sql schema; run from scripts/test-postgres.sh"]
async fn parallel_sweepers_release_one_safe_reservation_once_and_preserve_unknown() {
    let admin = admin_pool().await;
    let tenant_id = Uuid::new_v4();
    let object_id = Uuid::new_v4();
    let caller_id = Uuid::new_v4();
    let snapshot_id = Uuid::new_v4();
    let version_id = Uuid::new_v4();
    let eligible_invocation_id = Uuid::new_v4();
    let unknown_invocation_id = Uuid::new_v4();

    sqlx::query("INSERT INTO tenants (id) VALUES ($1)")
        .bind(tenant_id)
        .execute(&admin)
        .await
        .expect("tenant fixture");
    sqlx::query(
        "INSERT INTO callers (id, tenant_id, token_hash, caller_kind) \
         VALUES ($1, $2, $3, 'Authenticated')",
    )
    .bind(caller_id)
    .bind(tenant_id)
    .bind(digest::digest(&digest::SHA256, caller_id.as_bytes()).as_ref())
    .execute(&admin)
    .await
    .expect("Caller fixture");
    sqlx::query("INSERT INTO objects (tenant_id, object_id) VALUES ($1, $2)")
        .bind(tenant_id)
        .bind(object_id)
        .execute(&admin)
        .await
        .expect("Object fixture");
    sqlx::query(
        "INSERT INTO object_versions \
         (tenant_id, object_id, object_version_id, version_number, status) \
         VALUES ($1, $2, $3, 1, 'draft')",
    )
    .bind(tenant_id)
    .bind(object_id)
    .bind(version_id)
    .execute(&admin)
    .await
    .expect("ObjectVersion fixture");

    let canonical = b"{}";
    let content_hash = digest::digest(&digest::SHA256, canonical);
    sqlx::query(
        "INSERT INTO snapshots \
         (tenant_id, snapshot_id, object_id, object_version_id, schema_version, \
          canonical_json, content_hash, json_projection) \
         VALUES ($1, $2, $3, $4, 1, $5, $6, '{}'::jsonb)",
    )
    .bind(tenant_id)
    .bind(snapshot_id)
    .bind(object_id)
    .bind(version_id)
    .bind(canonical.as_slice())
    .bind(content_hash.as_ref())
    .execute(&admin)
    .await
    .expect("Snapshot fixture");

    for invocation_id in [eligible_invocation_id, unknown_invocation_id] {
        sqlx::query(
            "INSERT INTO object_invocations \
             (id, tenant_id, object_id, caller_id, snapshot_id, knowledge_revision, \
              pool, status, trace_id) \
             VALUES ($1, $2, $3, $4, $5, 0, 'authenticated', 'Received', $6)",
        )
        .bind(invocation_id)
        .bind(tenant_id)
        .bind(object_id)
        .bind(caller_id)
        .bind(snapshot_id)
        .bind(Uuid::new_v4())
        .execute(&admin)
        .await
        .expect("Invocation fixture");
    }

    sqlx::query(
        "INSERT INTO usage_counters \
         (tenant_id, object_id, usage_date, pool, reserved_tokens, reserved_cost_micros) \
         VALUES ($1, $2, CURRENT_DATE, 'authenticated', 30, 12)",
    )
    .bind(tenant_id)
    .bind(object_id)
    .execute(&admin)
    .await
    .expect("object usage counter fixture");
    sqlx::query(
        "INSERT INTO caller_usage_counters \
         (tenant_id, object_id, caller_id, usage_date, reserved_tokens) \
         VALUES ($1, $2, $3, CURRENT_DATE, 30)",
    )
    .bind(tenant_id)
    .bind(object_id)
    .bind(caller_id)
    .execute(&admin)
    .await
    .expect("Caller usage counter fixture");

    // Both reservations are older than the fixed 2026-01-03 cutoff.
    for (invocation_id, reservation_id, reserved_tokens, reserved_cost) in [
        (eligible_invocation_id, Uuid::new_v4(), 10_i64, 4_i64),
        (unknown_invocation_id, Uuid::new_v4(), 20_i64, 8_i64),
    ] {
        sqlx::query(
            "INSERT INTO budget_reservations \
             (id, tenant_id, object_id, invocation_id, pool, usage_date, \
              reserved_tokens, reserved_cost_micros, status, expires_at) \
             VALUES ($1, $2, $3, $4, 'authenticated', CURRENT_DATE, $5, $6, \
                     'Reserved', '2026-01-02 00:00:00+00'::timestamptz)",
        )
        .bind(reservation_id)
        .bind(tenant_id)
        .bind(object_id)
        .bind(invocation_id)
        .bind(reserved_tokens)
        .bind(reserved_cost)
        .execute(&admin)
        .await
        .expect("reservation fixture");
    }

    // A possibly-sent provider call is not safe to expire automatically.
    sqlx::query(
        "UPDATE provider_call_attempts SET state='MayHaveBeenSent' \
         WHERE tenant_id=$1 AND object_id=$2 AND invocation_id=$3",
    )
    .bind(tenant_id)
    .bind(object_id)
    .bind(unknown_invocation_id)
    .execute(&admin)
    .await
    .expect("mark possible send");
    sqlx::query(
        "UPDATE provider_call_attempts SET state='OutcomeUnknown' \
         WHERE tenant_id=$1 AND object_id=$2 AND invocation_id=$3",
    )
    .bind(tenant_id)
    .bind(object_id)
    .bind(unknown_invocation_id)
    .execute(&admin)
    .await
    .expect("mark ambiguous outcome");

    let first_worker = worker_pool().await;
    let second_worker = worker_pool().await;
    let first = PgReservationSweeper::new(first_worker.clone());
    let second = PgReservationSweeper::new(second_worker.clone());

    let before_expiry = UNIX_EPOCH + Duration::from_secs(1_767_225_600);
    assert_eq!(
        first
            .sweep_before(before_expiry, 20)
            .await
            .expect("pre-expiry sweep should succeed"),
        0
    );

    let cutoff = UNIX_EPOCH + Duration::from_secs(1_767_398_400);
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let first_barrier = barrier.clone();
    let first_task = tokio::spawn(async move {
        first_barrier.wait().await;
        first.sweep_before(cutoff, 20).await
    });
    let second_barrier = barrier.clone();
    let second_task = tokio::spawn(async move {
        second_barrier.wait().await;
        second.sweep_before(cutoff, 20).await
    });
    let first_count = first_task
        .await
        .expect("first sweeper task should join")
        .expect("first sweep should succeed");
    let second_count = second_task
        .await
        .expect("second sweeper task should join")
        .expect("second sweep should succeed");
    assert_eq!(first_count + second_count, 1);

    let eligible_status: String = sqlx::query_scalar(
        "SELECT status FROM budget_reservations WHERE tenant_id=$1 AND object_id=$2 AND invocation_id=$3",
    )
    .bind(tenant_id)
    .bind(object_id)
    .bind(eligible_invocation_id)
    .fetch_one(&admin)
    .await
    .expect("eligible reservation status");
    let unknown_status: String = sqlx::query_scalar(
        "SELECT status FROM budget_reservations WHERE tenant_id=$1 AND object_id=$2 AND invocation_id=$3",
    )
    .bind(tenant_id)
    .bind(object_id)
    .bind(unknown_invocation_id)
    .fetch_one(&admin)
    .await
    .expect("unknown reservation status");
    assert_eq!(eligible_status, "Expired");
    assert_eq!(unknown_status, "Reserved");

    let object_counters: (i64, i64) = sqlx::query_as(
        "SELECT reserved_tokens, reserved_cost_micros FROM usage_counters \
         WHERE tenant_id=$1 AND object_id=$2 AND usage_date=CURRENT_DATE AND pool='authenticated'",
    )
    .bind(tenant_id)
    .bind(object_id)
    .fetch_one(&admin)
    .await
    .expect("object counters");
    let caller_reserved: i64 = sqlx::query_scalar(
        "SELECT reserved_tokens FROM caller_usage_counters \
         WHERE tenant_id=$1 AND object_id=$2 AND caller_id=$3 AND usage_date=CURRENT_DATE",
    )
    .bind(tenant_id)
    .bind(object_id)
    .bind(caller_id)
    .fetch_one(&admin)
    .await
    .expect("Caller counter");
    assert_eq!(object_counters, (20, 8));
    assert_eq!(caller_reserved, 20);

    let repeated = PgReservationSweeper::new(first_worker.clone())
        .sweep_before(cutoff, 20)
        .await
        .expect("repeated sweep should succeed");
    assert_eq!(repeated, 0);

    let counters_after_replay: (i64, i64) = sqlx::query_as(
        "SELECT reserved_tokens, reserved_cost_micros FROM usage_counters \
         WHERE tenant_id=$1 AND object_id=$2 AND usage_date=CURRENT_DATE AND pool='authenticated'",
    )
    .bind(tenant_id)
    .bind(object_id)
    .fetch_one(&admin)
    .await
    .expect("object counters after repeated sweep");
    assert_eq!(counters_after_replay, (20, 8));

    first_worker.close().await;
    second_worker.close().await;
    admin.close().await;
}
