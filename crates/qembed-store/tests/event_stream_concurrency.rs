mod common;

use qembed_domain::ids::{EventId, ObjectId, TenantId};
use qembed_store::run_migrations;
use sqlx::PgPool;
use tokio::task::JoinSet;

async fn append_event(
    pool: PgPool,
    tenant_id: String,
    aggregate_id: String,
    event_id: String,
) -> i64 {
    let mut transaction = pool.begin().await.expect("event transaction should begin");
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(&tenant_id)
        .execute(&mut *transaction)
        .await
        .expect("tenant scope should be set for the transaction");
    let stream_version: i64 = sqlx::query_scalar(
        "INSERT INTO events (tenant_id, event_id, aggregate_type, aggregate_id, event_type, payload) VALUES ($1::uuid, $2::uuid, 'invocation', $3::uuid, 'accepted', '{}'::jsonb) RETURNING stream_version",
    )
    .bind(&tenant_id)
    .bind(&event_id)
    .bind(&aggregate_id)
    .fetch_one(&mut *transaction)
    .await
    .expect("database should allocate a stream version");
    transaction
        .commit()
        .await
        .expect("event append should commit");
    stream_version
}

#[tokio::test]
async fn concurrent_independent_invocation_streams_get_gapless_versions_without_conflicts() {
    const EVENTS_PER_STREAM: usize = 32;

    let pool = common::connect().await;
    run_migrations(&pool)
        .await
        .expect("migrations should apply");

    let tenant_id = TenantId::new().to_string();
    let stream_a = ObjectId::new().to_string();
    let stream_b = ObjectId::new().to_string();
    let mut tasks = JoinSet::new();

    for _ in 0..EVENTS_PER_STREAM {
        for aggregate_id in [&stream_a, &stream_b] {
            tasks.spawn(append_event(
                pool.clone(),
                tenant_id.clone(),
                aggregate_id.clone(),
                EventId::new().to_string(),
            ));
        }
    }

    let mut versions_a = Vec::with_capacity(EVENTS_PER_STREAM);
    let mut versions_b = Vec::with_capacity(EVENTS_PER_STREAM);
    while let Some(result) = tasks.join_next().await {
        let version = result.expect("event append task should not panic");
        // The stream identity is intentionally recovered from the committed rows
        // below; task completion order is not stream order.
        assert!(version > 0);
    }

    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT aggregate_id::text, stream_version FROM events WHERE tenant_id = $1::uuid AND aggregate_type = 'invocation' AND aggregate_id IN ($2::uuid, $3::uuid) ORDER BY aggregate_id, stream_version",
    )
    .bind(&tenant_id)
    .bind(&stream_a)
    .bind(&stream_b)
    .fetch_all(&pool)
    .await
    .expect("both event streams should be queryable");

    for (aggregate_id, version) in rows {
        if aggregate_id == stream_a {
            versions_a.push(version);
        } else if aggregate_id == stream_b {
            versions_b.push(version);
        }
    }

    let expected: Vec<i64> = (1..=EVENTS_PER_STREAM as i64).collect();
    assert_eq!(versions_a, expected);
    assert_eq!(versions_b, expected);

    pool.close().await;
}
