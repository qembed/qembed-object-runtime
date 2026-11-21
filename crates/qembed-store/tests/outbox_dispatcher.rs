mod common;

use std::collections::HashSet;

use qembed_domain::ports::PublicationReceipt;
use qembed_store::outbox_dispatcher::PgOutboxDispatcher;
use qembed_store::run_migrations;
use sqlx::PgPool;
use uuid::Uuid;

fn receipt() -> PublicationReceipt {
    PublicationReceipt::new("QEMBED_EVENTS", 1, false)
}

async fn setup() -> PgPool {
    let pool = common::connect().await;
    run_migrations(&pool)
        .await
        .expect("migrations should apply");
    pool
}

async fn enqueue(pool: &PgPool, topic: &str) -> (String, String) {
    let tenant_id = Uuid::new_v4();
    let event_id = Uuid::new_v4();
    let delivery_id = Uuid::new_v4();
    let aggregate_id = Uuid::new_v4();

    sqlx::query(
        "INSERT INTO events (tenant_id, event_id, aggregate_type, aggregate_id, event_type, payload) VALUES ($1, $2, 'test', $3, $4, $5)",
    )
    .bind(tenant_id)
    .bind(event_id)
    .bind(aggregate_id)
    .bind(topic)
    .bind(serde_json::json!({"type":"test.delivery","safe":true}))
    .execute(pool)
    .await
    .expect("event should be persisted");

    sqlx::query(
        "INSERT INTO outbox_messages (tenant_id, delivery_id, event_id, topic, payload) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(tenant_id)
    .bind(delivery_id)
    .bind(event_id)
    .bind(topic)
    .bind(serde_json::json!({"type":"test.delivery","safe":true}))
    .execute(pool)
    .await
    .expect("delivery should be persisted");

    (tenant_id.to_string(), delivery_id.to_string())
}

#[tokio::test]
async fn release_and_reclaim_fence_stale_acknowledgements() {
    let pool = setup().await;
    let topic = format!("test.rust051.{}", Uuid::new_v4());
    let (tenant_id, delivery_id) = enqueue(&pool, &topic).await;
    let dispatcher = PgOutboxDispatcher::new(pool.clone());

    let first = dispatcher
        .claim_batch("worker-a", &topic, 10, 30)
        .await
        .expect("first worker should claim");
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].tenant_id(), tenant_id);
    assert_eq!(first[0].delivery_id(), delivery_id);
    assert_eq!(first[0].attempts(), 1);
    assert_eq!(first[0].claim_token(), 1);

    assert!(
        dispatcher
            .release(&first[0], "worker-a")
            .await
            .expect("release")
    );
    let second = dispatcher
        .claim_batch("worker-b", &topic, 10, 30)
        .await
        .expect("second worker should reclaim");
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].attempts(), 2);
    assert_eq!(second[0].claim_token(), 2);
    assert_eq!(first[0].delivery_id(), second[0].delivery_id());

    // At-least-once consumers use the stable delivery ID as their idempotency key.
    let mut processed = HashSet::new();
    let mut side_effects = 0;
    for delivery in [&first[0], &second[0]] {
        if processed.insert(delivery.delivery_id().to_owned()) {
            side_effects += 1;
        }
    }
    assert_eq!(side_effects, 1);

    assert!(
        !dispatcher
            .acknowledge(&first[0], "worker-a", receipt())
            .await
            .expect("stale ack is rejected")
    );
    assert!(
        dispatcher
            .acknowledge(&second[0], "worker-b", receipt())
            .await
            .expect("current ack succeeds")
    );
    assert!(
        dispatcher
            .claim_batch("worker-c", &topic, 10, 30)
            .await
            .expect("queue is empty")
            .is_empty()
    );

    let (delivered, stream, sequence, duplicate): (bool, Option<String>, Option<i64>, Option<bool>) = sqlx::query_as(
        "SELECT delivered_at IS NOT NULL, publication_stream, publication_sequence, publication_duplicate FROM outbox_messages WHERE tenant_id = $1::uuid AND delivery_id = $2::uuid",
    )
    .bind(&tenant_id)
    .bind(&delivery_id)
    .fetch_one(&pool)
    .await
    .expect("delivery status should be readable by test owner");
    assert!(delivered);
    assert_eq!(stream.as_deref(), Some("QEMBED_EVENTS"));
    assert_eq!(sequence, Some(1));
    assert_eq!(duplicate, Some(false));
    let receipt_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox_publication_receipts WHERE tenant_id = $1::uuid AND delivery_id = $2::uuid",
    )
    .bind(&tenant_id)
    .bind(&delivery_id)
    .fetch_one(&pool)
    .await
    .expect("publication history should be readable by the owner");
    assert_eq!(receipt_count, 1);

    let immutable_error = sqlx::query(
        "UPDATE outbox_publication_receipts SET publication_duplicate = true WHERE tenant_id = $1::uuid AND delivery_id = $2::uuid",
    )
    .bind(&tenant_id)
    .bind(&delivery_id)
    .execute(&pool)
    .await
    .expect_err("publication history must reject UPDATE");
    assert_eq!(
        immutable_error
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("55000")
    );
    pool.close().await;
}

#[tokio::test]
async fn expired_lease_can_be_reclaimed_with_a_new_fencing_token() {
    let pool = setup().await;
    let topic = format!("test.rust051.expired.{}", Uuid::new_v4());
    let (tenant_id, delivery_id) = enqueue(&pool, &topic).await;
    let dispatcher = PgOutboxDispatcher::new(pool.clone());

    let first = dispatcher
        .claim_batch("worker-a", &topic, 10, 30)
        .await
        .expect("first worker should claim");
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].attempts(), 1);
    assert_eq!(first[0].claim_token(), 1);

    sqlx::query(
        "UPDATE outbox_messages SET lease_expires_at = now() - interval '1 second' WHERE tenant_id = $1::uuid AND delivery_id = $2::uuid",
    )
    .bind(&tenant_id)
    .bind(&delivery_id)
    .execute(&pool)
    .await
    .expect("test should expire the first lease");

    let second = dispatcher
        .claim_batch("worker-b", &topic, 10, 30)
        .await
        .expect("second worker should reclaim the expired lease");
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].attempts(), 2);
    assert_eq!(second[0].claim_token(), 2);
    assert_eq!(first[0].delivery_id(), second[0].delivery_id());

    assert!(
        !dispatcher
            .acknowledge(&first[0], "worker-a", receipt())
            .await
            .expect("stale acknowledgement must be rejected")
    );
    assert!(
        dispatcher
            .acknowledge(&second[0], "worker-b", receipt())
            .await
            .expect("current owner should acknowledge")
    );

    pool.close().await;
}

#[tokio::test]
async fn independent_dispatchers_claim_disjoint_rows() {
    let pool = setup().await;
    let topic = format!("test.rust051.concurrent.{}", Uuid::new_v4());
    enqueue(&pool, &topic).await;
    enqueue(&pool, &topic).await;
    let first = PgOutboxDispatcher::new(pool.clone());
    let second = PgOutboxDispatcher::new(pool.clone());

    let (a, b) = tokio::join!(
        first.claim_batch("worker-a", &topic, 1, 30),
        second.claim_batch("worker-b", &topic, 1, 30),
    );
    let a = a.expect("worker A claim");
    let b = b.expect("worker B claim");
    assert_eq!(a.len(), 1);
    assert_eq!(b.len(), 1);
    assert_ne!(a[0].delivery_id(), b[0].delivery_id());

    assert!(
        first
            .acknowledge(&a[0], "worker-a", receipt())
            .await
            .expect("ack A")
    );
    assert!(
        second
            .acknowledge(&b[0], "worker-b", receipt())
            .await
            .expect("ack B")
    );
    pool.close().await;
}

#[tokio::test]
async fn dispatcher_role_is_limited_to_delivery_projection() {
    let pool = setup().await;
    let topic = format!("test.rust051.projection.{}", Uuid::new_v4());
    enqueue(&pool, &topic).await;

    let mut transaction = pool.begin().await.expect("transaction");
    sqlx::query("SET LOCAL ROLE qembed_dispatcher")
        .execute(&mut *transaction)
        .await
        .expect("dispatcher role should be available");

    let projection: String =
        sqlx::query_scalar("SELECT topic FROM outbox_messages WHERE topic = $1 LIMIT 1")
            .bind(&topic)
            .fetch_one(&mut *transaction)
            .await
            .expect("delivery projection columns should be visible");
    assert_eq!(projection, topic);
    let publication_stream: Option<String> = sqlx::query_scalar(
        "SELECT publication_stream FROM outbox_messages WHERE topic = $1 LIMIT 1",
    )
    .bind(&topic)
    .fetch_one(&mut *transaction)
    .await
    .expect("dispatcher may read the publication receipt projection");
    assert_eq!(publication_stream, None);
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT has_table_privilege('qembed_dispatcher', 'public.outbox_publication_receipts', 'INSERT')",
        )
        .fetch_one(&mut *transaction)
        .await
        .expect("receipt insert privilege should be inspectable")
    );
    assert!(
        !sqlx::query_scalar::<_, bool>(
            "SELECT has_table_privilege('qembed_dispatcher', 'public.outbox_publication_receipts', 'SELECT')",
        )
        .fetch_one(&mut *transaction)
        .await
        .expect("receipt select privilege should be inspectable")
    );
    transaction
        .rollback()
        .await
        .expect("rollback role transaction");

    let mut event_transaction = pool.begin().await.expect("event transaction");
    sqlx::query("SET LOCAL ROLE qembed_dispatcher")
        .execute(&mut *event_transaction)
        .await
        .expect("dispatcher role should be available");
    let denied = sqlx::query("SELECT event_id FROM outbox_messages LIMIT 1")
        .execute(&mut *event_transaction)
        .await
        .expect_err("event identity is outside the delivery projection");
    assert!(matches!(denied, sqlx::Error::Database(_)));
    event_transaction
        .rollback()
        .await
        .expect("rollback failed transaction");

    let mut domain_transaction = pool.begin().await.expect("domain transaction");
    sqlx::query("SET LOCAL ROLE qembed_dispatcher")
        .execute(&mut *domain_transaction)
        .await
        .expect("dispatcher role should be available");
    let denied = sqlx::query("SELECT event_id FROM events LIMIT 1")
        .execute(&mut *domain_transaction)
        .await
        .expect_err("dispatcher must not read the domain event log");
    assert!(matches!(denied, sqlx::Error::Database(_)));
    domain_transaction
        .rollback()
        .await
        .expect("rollback failed transaction");
    pool.close().await;
}
