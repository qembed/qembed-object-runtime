mod common;

use std::sync::{Arc, Barrier};
use std::time::{Duration, SystemTime};

use qembed_domain::ids::{CallerId, InvocationId, ObjectId, SnapshotId, TenantId};
use qembed_domain::scope::Scope;
use qembed_domain::test_support::scope_for_tenant;
use qembed_store::caller::resolve_delegated_caller;
use qembed_store::idempotency::{
    IdempotencyReservation, IdempotencyReservationError, reserve_invocation_idempotency,
};
use qembed_store::{TenantTransaction, begin_tenant_transaction, execute};
use sqlx::PgPool;
use uuid::Uuid;

const PAYLOAD_HASH: &[u8] = b"stable-payload-digest";

/// Isolated fixture for the canonical Caller/delegation and idempotency contract.
/// Production DDL remains owned by qembed-sql.
async fn setup() -> (PgPool, String) {
    let pool = common::connect().await;
    let schema = format!("invocation_idem_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&pool)
        .await
        .expect("isolated test schema should be created");

    for ddl in [
        format!("CREATE TABLE \"{schema}\".owner_principals (tenant_id UUID NOT NULL, caller_id UUID NOT NULL, disabled_at TIMESTAMPTZ, PRIMARY KEY (tenant_id, caller_id))"),
        format!("CREATE TABLE \"{schema}\".callers (id UUID PRIMARY KEY, tenant_id UUID NOT NULL, caller_kind TEXT NOT NULL, revoked_at TIMESTAMPTZ, UNIQUE (tenant_id, id))"),
        format!("CREATE TABLE \"{schema}\".principal_caller_delegations (delegation_id UUID PRIMARY KEY, tenant_id UUID NOT NULL, principal_id UUID NOT NULL, caller_id UUID NOT NULL, revoked_at TIMESTAMPTZ, FOREIGN KEY (tenant_id, principal_id) REFERENCES \"{schema}\".owner_principals(tenant_id, caller_id), FOREIGN KEY (tenant_id, caller_id) REFERENCES \"{schema}\".callers(tenant_id, id))"),
        format!("CREATE TABLE \"{schema}\".object_invocations (id UUID PRIMARY KEY, tenant_id UUID NOT NULL, object_id UUID NOT NULL, caller_id UUID NOT NULL, snapshot_id UUID NOT NULL, knowledge_revision BIGINT NOT NULL CHECK (knowledge_revision >= 0), pool TEXT NOT NULL CHECK (pool IN ('anonymous','authenticated')), status TEXT NOT NULL, trace_id UUID NOT NULL, fencing_token BIGINT NOT NULL DEFAULT 0 CHECK (fencing_token >= 0), started_at TIMESTAMPTZ NOT NULL DEFAULT now(), finished_at TIMESTAMPTZ, version BIGINT NOT NULL DEFAULT 0 CHECK (version >= 0), UNIQUE (tenant_id, object_id, id), UNIQUE (tenant_id, object_id, caller_id, id), UNIQUE (tenant_id, object_id, id, pool), FOREIGN KEY (tenant_id, caller_id) REFERENCES \"{schema}\".callers(tenant_id, id))"),
        format!("CREATE TABLE \"{schema}\".invocation_idempotency (tenant_id UUID NOT NULL, object_id UUID NOT NULL, caller_id UUID NOT NULL, idempotency_key TEXT NOT NULL, payload_hash BYTEA NOT NULL, invocation_id UUID NOT NULL, created_at TIMESTAMPTZ NOT NULL DEFAULT now(), expires_at TIMESTAMPTZ NOT NULL, PRIMARY KEY (tenant_id, object_id, caller_id, idempotency_key), UNIQUE (tenant_id, object_id, invocation_id), FOREIGN KEY (tenant_id, object_id, caller_id, invocation_id) REFERENCES \"{schema}\".object_invocations(tenant_id, object_id, caller_id, id) DEFERRABLE INITIALLY DEFERRED)")
    ] {
        sqlx::query(&ddl).execute(&pool).await.expect("fixture table should be created");
    }

    let resolver = format!(
        r#"CREATE FUNCTION \"{schema}\".resolve_delegated_caller(p_caller_id UUID)
        RETURNS TABLE (caller_id UUID, caller_kind TEXT)
        LANGUAGE plpgsql AS $$
        DECLARE v_tenant UUID := NULLIF(current_setting('app.tenant_id', true), '')::uuid;
                v_principal UUID := NULLIF(current_setting('app.principal_id', true), '')::uuid;
        BEGIN
          RETURN QUERY
          SELECT c.id, c.caller_kind
            FROM \"{schema}\".principal_caller_delegations d
            JOIN \"{schema}\".owner_principals p
              ON p.tenant_id=d.tenant_id AND p.caller_id=d.principal_id
            JOIN \"{schema}\".callers c
              ON c.tenant_id=d.tenant_id AND c.id=d.caller_id
           WHERE d.tenant_id=v_tenant AND d.principal_id=v_principal
             AND d.caller_id=p_caller_id AND d.revoked_at IS NULL
             AND p.disabled_at IS NULL AND c.revoked_at IS NULL
           FOR SHARE OF d,p,c;
          IF NOT FOUND THEN
            RAISE EXCEPTION 'no active delegation' USING ERRCODE='42501';
          END IF;
        END $$"#
    );
    sqlx::query(&resolver)
        .execute(&pool)
        .await
        .expect("delegation resolver fixture should be created");

    (pool, schema)
}

async fn prepare_delegation(pool: &PgPool, schema: &str, scope: &Scope, caller_id: CallerId) {
    sqlx::query(&format!(
        "INSERT INTO \"{schema}\".owner_principals (tenant_id, caller_id) VALUES ($1::uuid, $2::uuid) ON CONFLICT DO NOTHING"
    ))
    .bind(scope.tenant_id().to_string())
    .bind(scope.principal().principal_id().to_string())
    .execute(pool)
    .await
    .expect("test Principal should be present");

    sqlx::query(&format!(
        "INSERT INTO \"{schema}\".callers (id, tenant_id, caller_kind) VALUES ($1::uuid, $2::uuid, 'Authenticated')"
    ))
    .bind(caller_id.to_string())
    .bind(scope.tenant_id().to_string())
    .execute(pool)
    .await
    .expect("test Caller should be present");

    sqlx::query(&format!(
        "INSERT INTO \"{schema}\".principal_caller_delegations (delegation_id, tenant_id, principal_id, caller_id) VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid)"
    ))
    .bind(Uuid::new_v4().to_string())
    .bind(scope.tenant_id().to_string())
    .bind(scope.principal().principal_id().to_string())
    .bind(caller_id.to_string())
    .execute(pool)
    .await
    .expect("explicit Principal-to-Caller delegation should be present");
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
    caller_id: CallerId,
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
        .bind(caller_id.to_string())
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
    caller_id: CallerId,
    object_id: ObjectId,
    key: &str,
    hash: &[u8],
    candidate: InvocationId,
) -> Result<IdempotencyReservation, IdempotencyReservationError> {
    let mut transaction = begin_tenant_transaction(pool, scope)
        .await
        .expect("tenant transaction should begin");
    set_schema(&mut transaction, schema).await;
    let caller = resolve_delegated_caller(&mut transaction, caller_id)
        .await
        .expect("requested Caller must be actively delegated");

    let reservation = reserve_invocation_idempotency(
        &mut transaction,
        scope,
        &caller,
        object_id,
        key,
        hash,
        candidate,
        SystemTime::now() + Duration::from_secs(3_600),
    )
    .await?;

    if let IdempotencyReservation::Reserved { invocation_id } = reservation {
        insert_invocation(&mut transaction, scope, caller_id, object_id, invocation_id).await;
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
    let caller_id = CallerId::new();
    prepare_delegation(&pool, &schema, &scope, caller_id).await;
    let object_id = ObjectId::new();
    let first_id = InvocationId::new();

    let first = reserve_and_finish(&pool, &schema, &scope, caller_id, object_id, "request-123", PAYLOAD_HASH, first_id)
        .await.expect("first request should reserve the key");
    assert_eq!(first, IdempotencyReservation::Reserved { invocation_id: first_id });

    let replay = reserve_and_finish(&pool, &schema, &scope, caller_id, object_id, "request-123", PAYLOAD_HASH, InvocationId::new())
        .await.expect("same key and payload should replay");
    assert_eq!(replay, IdempotencyReservation::Replay { invocation_id: first_id });
    assert_eq!(invocation_count(&pool, &schema).await, 1);

    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE")).execute(&pool).await.expect("test schema should be removed");
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_same_key_requests_converge_on_one_invocation() {
    let (pool, schema) = setup().await;
    let scope = scope_for_tenant(TenantId::new());
    let caller_id = CallerId::new();
    prepare_delegation(&pool, &schema, &scope, caller_id).await;
    let object_id = ObjectId::new();
    let barrier = Arc::new(Barrier::new(2));

    let first_pool = pool.clone();
    let first_schema = schema.clone();
    let first_scope = scope;
    let first_barrier = Arc::clone(&barrier);
    let first = tokio::spawn(async move {
        first_barrier.wait();
        reserve_and_finish(&first_pool, &first_schema, &first_scope, caller_id, object_id, "concurrent-key", PAYLOAD_HASH, InvocationId::new())
            .await.expect("first concurrent request should resolve")
    });

    let second_pool = pool.clone();
    let second_schema = schema.clone();
    let second_scope = scope;
    let second_barrier = Arc::clone(&barrier);
    let second = tokio::spawn(async move {
        second_barrier.wait();
        reserve_and_finish(&second_pool, &second_schema, &second_scope, caller_id, object_id, "concurrent-key", PAYLOAD_HASH, InvocationId::new())
            .await.expect("second concurrent request should resolve")
    });

    let first_id = match first.await.expect("first task should finish") {
        IdempotencyReservation::Reserved { invocation_id } | IdempotencyReservation::Replay { invocation_id } => invocation_id,
    };
    let second_id = match second.await.expect("second task should finish") {
        IdempotencyReservation::Reserved { invocation_id } | IdempotencyReservation::Replay { invocation_id } => invocation_id,
    };
    assert_eq!(first_id, second_id);
    assert_eq!(invocation_count(&pool, &schema).await, 1);

    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE")).execute(&pool).await.expect("test schema should be removed");
    pool.close().await;
}

#[tokio::test]
async fn same_key_with_different_payload_is_rejected() {
    let (pool, schema) = setup().await;
    let scope = scope_for_tenant(TenantId::new());
    let caller_id = CallerId::new();
    prepare_delegation(&pool, &schema, &scope, caller_id).await;
    let object_id = ObjectId::new();
    let original_id = InvocationId::new();

    reserve_and_finish(&pool, &schema, &scope, caller_id, object_id, "payload-bound-key", b"payload-a", original_id)
        .await.expect("first request should reserve the key");

    let mut transaction = begin_tenant_transaction(&pool, &scope).await.expect("tenant transaction should begin");
    set_schema(&mut transaction, &schema).await;
    let caller = resolve_delegated_caller(&mut transaction, caller_id).await.expect("Caller remains delegated");
    let mismatch = reserve_invocation_idempotency(
        &mut transaction, &scope, &caller, object_id, "payload-bound-key", b"payload-b",
        InvocationId::new(), SystemTime::now() + Duration::from_secs(3_600),
    ).await.expect_err("same key with a different payload must conflict");
    assert!(matches!(mismatch, IdempotencyReservationError::PayloadMismatch));
    transaction.rollback().await.expect("mismatched request transaction should roll back");
    assert_eq!(invocation_count(&pool, &schema).await, 1);

    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE")).execute(&pool).await.expect("test schema should be removed");
    pool.close().await;
}

#[tokio::test]
async fn rolling_back_a_new_claim_releases_the_key() {
    let (pool, schema) = setup().await;
    let scope = scope_for_tenant(TenantId::new());
    let caller_id = CallerId::new();
    prepare_delegation(&pool, &schema, &scope, caller_id).await;
    let object_id = ObjectId::new();

    let mut abandoned = begin_tenant_transaction(&pool, &scope).await.expect("tenant transaction should begin");
    set_schema(&mut abandoned, &schema).await;
    let caller = resolve_delegated_caller(&mut abandoned, caller_id).await.expect("Caller remains delegated");
    let abandoned_id = InvocationId::new();
    let result = reserve_invocation_idempotency(
        &mut abandoned, &scope, &caller, object_id, "rollback-key", PAYLOAD_HASH, abandoned_id,
        SystemTime::now() + Duration::from_secs(3_600),
    ).await.expect("first transaction should acquire the key");
    assert_eq!(result, IdempotencyReservation::Reserved { invocation_id: abandoned_id });
    abandoned.rollback().await.expect("abandoned reservation should roll back");

    let committed_id = InvocationId::new();
    let retry = reserve_and_finish(&pool, &schema, &scope, caller_id, object_id, "rollback-key", PAYLOAD_HASH, committed_id)
        .await.expect("a rolled-back key should be available again");
    assert_eq!(retry, IdempotencyReservation::Reserved { invocation_id: committed_id });
    assert_eq!(invocation_count(&pool, &schema).await, 1);

    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE")).execute(&pool).await.expect("test schema should be removed");
    pool.close().await;
}

#[tokio::test]
async fn idempotency_key_is_scoped_by_tenant_object_and_effective_caller() {
    let (pool, schema) = setup().await;
    let tenant_a = TenantId::new();
    let scope_a = scope_for_tenant(tenant_a);
    let scope_b = scope_for_tenant(TenantId::new());
    let caller_a = CallerId::new();
    let caller_b = CallerId::new();
    let caller_other_tenant = CallerId::new();
    prepare_delegation(&pool, &schema, &scope_a, caller_a).await;
    prepare_delegation(&pool, &schema, &scope_a, caller_b).await;
    prepare_delegation(&pool, &schema, &scope_b, caller_other_tenant).await;
    let object_a = ObjectId::new();
    let object_b = ObjectId::new();

    let first_id = InvocationId::new();
    assert!(matches!(
        reserve_and_finish(&pool, &schema, &scope_a, caller_a, object_a, "same-key", PAYLOAD_HASH, first_id).await.expect("first scope should acquire key"),
        IdempotencyReservation::Reserved { .. }
    ));

    for (scope, caller, object_id) in [
        (&scope_a, caller_b, object_a),
        (&scope_a, caller_a, object_b),
        (&scope_b, caller_other_tenant, object_a),
    ] {
        assert!(matches!(
            reserve_and_finish(&pool, &schema, scope, caller, object_id, "same-key", PAYLOAD_HASH, InvocationId::new()).await.expect("different idempotency scope should acquire key"),
            IdempotencyReservation::Reserved { .. }
        ));
    }

    assert_eq!(invocation_count(&pool, &schema).await, 4);
    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE")).execute(&pool).await.expect("test schema should be removed");
    pool.close().await;
}
