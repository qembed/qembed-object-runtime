mod common;

use std::sync::{Arc, Barrier};
use std::time::{Duration, SystemTime};

use qembed_domain::ids::{InvocationId, ObjectId, SnapshotId, TenantId};
use qembed_domain::scope::Scope;
use qembed_domain::test_support::scope_for_tenant;
use qembed_store::idempotency::{
    IdempotencyReservation, IdempotencyReservationError, reserve_invocation_idempotency,
};
use qembed_store::{TenantTransaction, begin_tenant_transaction, execute};
use sqlx::PgPool;
use uuid::Uuid;

const PAYLOAD_HASH: &[u8] = b"stable-payload-digest";

/// Creates an isolated schema containing the exact RUST-035 table columns,
/// uniqueness key and deferred FK from qembed/qembed-sql's canonical schema.
/// This is a test fixture only, not a second production migration line.
async fn setup() -> (PgPool, String) {
    let pool = common::connect().await;
    let schema = format!("invocation_idem_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&pool)
        .await
        .expect("isolated test schema should be created");

    let invocations = format!(
        r#"
        CREATE TABLE "{schema}".object_invocations (
            id UUID PRIMARY KEY,
            tenant_id UUID NOT NULL,
            object_id UUID NOT NULL,
            caller_id UUID NOT NULL,
            snapshot_id UUID NOT NULL,
            knowledge_revision BIGINT NOT NULL CHECK (knowledge_revision >= 0),
            pool TEXT NOT NULL CHECK (pool IN ('anonymous','authenticated')),
            status TEXT NOT NULL,
            trace_id UUID NOT NULL,
            fencing_token BIGINT NOT NULL DEFAULT 0 CHECK (fencing_token >= 0),
            started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            finished_at TIMESTAMPTZ,
            version BIGINT NOT NULL DEFAULT 0 CHECK (version >= 0),
            UNIQUE (tenant_id, object_id, id),
            UNIQUE (tenant_id, object_id, caller_id, id),
            UNIQUE (tenant_id, object_id, id, pool)
        )
        "#
    );
    sqlx::query(&invocations)
        .execute(&pool)
        .await
        .expect("invocation fixture table should be created");

    let idempotency = format!(
        r#"
        CREATE TABLE "{schema}".invocation_idempotency (
            tenant_id UUID NOT NULL,
            object_id UUID NOT NULL,
            caller_id UUID NOT NULL,
            idempotency_key TEXT NOT NULL,
            payload_hash BYTEA NOT NULL,
            invocation_id UUID NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            expires_at TIMESTAMPTZ NOT NULL,
            PRIMARY KEY (tenant_id, object_id, caller_id, idempotency_key),
            UNIQUE (tenant_id, object_id, invocation_id),
            FOREIGN KEY (tenant_id, object_id, caller_id, invocation_id)
                REFERENCES "{schema}".object_invocations(tenant_id, object_id, caller_id, id)
                DEFERRABLE INITIALLY DEFERRED
        )
        "#
    );
    sqlx::query(&idempotency)
        .execute(&pool)
        .await
        .expect("idempotency fixture table should be created");

    (pool, schema)
}

async fn set_schema(transaction: &mut TenantTransaction, schema: &str) {
    execute(
        transaction,
        sqlx::query(&format!("SET LOCAL search_path TO \"{schema}\"")),
    )
    .await
    .expect("transaction should use the isolated test schema");
}

async fn insert_invocation(
    transaction: &mut TenantTransaction,
    scope: &Scope,
    object_id: ObjectId,
    invocation_id: InvocationId,
) {
    execute(
        transaction,
        sqlx::query(
            "INSERT INTO object_invocations (id, tenant_id, object_id, caller_id, snapshot_id, knowledge_revision, pool, status, trace_id) VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, $5::uuid, 0, 'authenticated', 'Received', $6::uuid)",
        )
        .bind(invocation_id.to_string())
        .bind(scope.tenant_id().to_string())
        .bind(object_id.to_string())
        .bind(scope.principal().caller_id().to_string())
        .bind(SnapshotId::new().to_string())
        .bind(Uuid::new_v4().to_string()),
    )
    .await
    .expect("the deferred invocation reference should be satisfied before commit");
}

async fn reserve_and_finish(
    pool: &PgPool,
    schema: &str,
    scope: &Scope,
    object_id: ObjectId,
    key: &str,
    hash: &[u8],
    candidate: InvocationId,
) -> Result<IdempotencyReservation, IdempotencyReservationError> {
    let mut transaction = begin_tenant_transaction(pool, scope)
        .await
        .expect("tenant transaction should begin");
    set_schema(&mut transaction, schema).await;

    let reservation = reserve_invocation_idempotency(
        &mut transaction,
        scope,
        object_id,
        key,
        hash,
        candidate,
        SystemTime::now() + Duration::from_secs(3_600),
    )
    .await?;

    if let IdempotencyReservation::Reserved { invocation_id } = reservation {
        insert_invocation(&mut transaction, scope, object_id, invocation_id).await;
    }

    transaction
        .commit()
        .await
        .expect("reservation transaction should commit");
    Ok(reservation)
}

async fn invocation_count(pool: &PgPool, schema: &str) -> i64 {
    sqlx::query_scalar(&format!(
        "SELECT count(*) FROM \"{schema}\".object_invocations"
    ))
    .fetch_one(pool)
    .await
    .expect("invocation count should be available")
}

#[tokio::test]
async fn repeated_key_returns_the_original_invocation_without_creating_a_duplicate() {
    let (pool, schema) = setup().await;
    let scope = scope_for_tenant(TenantId::new());
    let object_id = ObjectId::new();
    let first_id = InvocationId::new();

    let first = reserve_and_finish(
        &pool,
        &schema,
        &scope,
        object_id,
        "request-123",
        PAYLOAD_HASH,
        first_id,
    )
    .await
    .expect("first request should reserve the key");
    assert_eq!(
        first,
        IdempotencyReservation::Reserved {
            invocation_id: first_id
        }
    );

    let replay = reserve_and_finish(
        &pool,
        &schema,
        &scope,
        object_id,
        "request-123",
        PAYLOAD_HASH,
        InvocationId::new(),
    )
    .await
    .expect("same key and payload should replay");
    assert_eq!(
        replay,
        IdempotencyReservation::Replay {
            invocation_id: first_id
        }
    );
    assert_eq!(invocation_count(&pool, &schema).await, 1);

    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&pool)
        .await
        .expect("test schema should be removed");
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_same_key_requests_converge_on_one_invocation() {
    let (pool, schema) = setup().await;
    let scope = scope_for_tenant(TenantId::new());
    let object_id = ObjectId::new();
    let barrier = Arc::new(Barrier::new(2));

    let first_pool = pool.clone();
    let first_schema = schema.clone();
    let first_scope = scope;
    let first_barrier = Arc::clone(&barrier);
    let first = tokio::spawn(async move {
        first_barrier.wait();
        reserve_and_finish(
            &first_pool,
            &first_schema,
            &first_scope,
            object_id,
            "concurrent-key",
            PAYLOAD_HASH,
            InvocationId::new(),
        )
        .await
        .expect("first concurrent request should resolve")
    });

    let second_pool = pool.clone();
    let second_schema = schema.clone();
    let second_scope = scope;
    let second_barrier = Arc::clone(&barrier);
    let second = tokio::spawn(async move {
        second_barrier.wait();
        reserve_and_finish(
            &second_pool,
            &second_schema,
            &second_scope,
            object_id,
            "concurrent-key",
            PAYLOAD_HASH,
            InvocationId::new(),
        )
        .await
        .expect("second concurrent request should resolve")
    });

    let first = first.await.expect("first task should finish");
    let second = second.await.expect("second task should finish");
    let first_id = match first {
        IdempotencyReservation::Reserved { invocation_id }
        | IdempotencyReservation::Replay { invocation_id } => invocation_id,
    };
    let second_id = match second {
        IdempotencyReservation::Reserved { invocation_id }
        | IdempotencyReservation::Replay { invocation_id } => invocation_id,
    };

    assert_eq!(first_id, second_id);
    assert_eq!(invocation_count(&pool, &schema).await, 1);

    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&pool)
        .await
        .expect("test schema should be removed");
    pool.close().await;
}

#[tokio::test]
async fn same_key_with_different_payload_is_rejected() {
    let (pool, schema) = setup().await;
    let scope = scope_for_tenant(TenantId::new());
    let object_id = ObjectId::new();
    let original_id = InvocationId::new();

    reserve_and_finish(
        &pool,
        &schema,
        &scope,
        object_id,
        "payload-bound-key",
        b"payload-a",
        original_id,
    )
    .await
    .expect("first request should reserve the key");

    let mut transaction = begin_tenant_transaction(&pool, &scope)
        .await
        .expect("tenant transaction should begin");
    set_schema(&mut transaction, &schema).await;
    let mismatch = reserve_invocation_idempotency(
        &mut transaction,
        &scope,
        object_id,
        "payload-bound-key",
        b"payload-b",
        InvocationId::new(),
        SystemTime::now() + Duration::from_secs(3_600),
    )
    .await
    .expect_err("same key with a different payload must conflict");
    assert!(matches!(
        mismatch,
        IdempotencyReservationError::PayloadMismatch
    ));
    transaction
        .rollback()
        .await
        .expect("mismatched request transaction should roll back");
    assert_eq!(invocation_count(&pool, &schema).await, 1);

    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&pool)
        .await
        .expect("test schema should be removed");
    pool.close().await;
}

#[tokio::test]
async fn rolling_back_a_new_claim_releases_the_key() {
    let (pool, schema) = setup().await;
    let scope = scope_for_tenant(TenantId::new());
    let object_id = ObjectId::new();

    let mut abandoned = begin_tenant_transaction(&pool, &scope)
        .await
        .expect("tenant transaction should begin");
    set_schema(&mut abandoned, &schema).await;
    let abandoned_id = InvocationId::new();
    let abandoned_result = reserve_invocation_idempotency(
        &mut abandoned,
        &scope,
        object_id,
        "rollback-key",
        PAYLOAD_HASH,
        abandoned_id,
        SystemTime::now() + Duration::from_secs(3_600),
    )
    .await
    .expect("first transaction should acquire the key");
    assert_eq!(
        abandoned_result,
        IdempotencyReservation::Reserved {
            invocation_id: abandoned_id
        }
    );
    abandoned
        .rollback()
        .await
        .expect("abandoned reservation should roll back");

    let committed_id = InvocationId::new();
    let retry = reserve_and_finish(
        &pool,
        &schema,
        &scope,
        object_id,
        "rollback-key",
        PAYLOAD_HASH,
        committed_id,
    )
    .await
    .expect("a rolled-back key should be available again");
    assert_eq!(
        retry,
        IdempotencyReservation::Reserved {
            invocation_id: committed_id
        }
    );
    assert_eq!(invocation_count(&pool, &schema).await, 1);

    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&pool)
        .await
        .expect("test schema should be removed");
    pool.close().await;
}

#[tokio::test]
async fn idempotency_key_is_scoped_by_tenant_object_and_authenticated_caller() {
    let (pool, schema) = setup().await;
    let tenant_id = TenantId::new();
    let caller_a = scope_for_tenant(tenant_id);
    let caller_b = scope_for_tenant(tenant_id);
    let tenant_b = scope_for_tenant(TenantId::new());
    let object_a = ObjectId::new();
    let object_b = ObjectId::new();

    let first_id = InvocationId::new();
    assert!(matches!(
        reserve_and_finish(
            &pool,
            &schema,
            &caller_a,
            object_a,
            "same-key",
            PAYLOAD_HASH,
            first_id
        )
        .await
        .expect("first scope should acquire key"),
        IdempotencyReservation::Reserved { .. }
    ));

    for (scope, object_id) in [
        (&caller_b, object_a),
        (&caller_a, object_b),
        (&tenant_b, object_a),
    ] {
        assert!(matches!(
            reserve_and_finish(
                &pool,
                &schema,
                scope,
                object_id,
                "same-key",
                PAYLOAD_HASH,
                InvocationId::new(),
            )
            .await
            .expect("different idempotency scope should acquire key"),
            IdempotencyReservation::Reserved { .. }
        ));
    }

    assert_eq!(invocation_count(&pool, &schema).await, 4);

    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&pool)
        .await
        .expect("test schema should be removed");
    pool.close().await;
}
