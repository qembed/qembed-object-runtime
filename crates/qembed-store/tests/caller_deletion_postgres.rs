use std::time::Duration;

use qembed_domain::{
    ids::{CallerId, ObjectId, ObjectVersionId, SnapshotId, TenantId},
    test_support::scope_for_tenant,
};
use qembed_store::{
    ContextIssuer, ContextIssuerConfig, PgContext, PgPoolConfig, RingEd25519SigningKey,
    caller_deletion::{CallerDeletionJobStatus, PgCallerDeletionRepository},
    connect_pool,
    external_deletion_ledger::{
        CallerDeletionKey, ExternalCallerDeletionLedger, run_control_plane_migrations,
    },
};
use sqlx::PgPool;
use uuid::Uuid;

async fn runtime_pool() -> PgPool {
    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set by scripts/test-postgres.sh");
    connect_pool(
        &database_url,
        PgPoolConfig {
            max_connections: 4,
            acquire_timeout: Duration::from_secs(5),
        },
    )
    .await
    .expect("canonical Runtime PostgreSQL should accept connections")
}

async fn control_plane_pool() -> PgPool {
    let database_url = std::env::var("CONTROL_PLANE_DATABASE_URL")
        .expect("CONTROL_PLANE_DATABASE_URL must be set by scripts/test-postgres.sh");
    connect_pool(&database_url, PgPoolConfig::default())
        .await
        .expect("independent Control-plane PostgreSQL should accept connections")
}

async fn seed_owner_and_caller(
    pool: &PgPool,
    tenant_id: TenantId,
    principal_id: Uuid,
    caller_id: CallerId,
) {
    sqlx::query("INSERT INTO tenants (id) VALUES ($1)")
        .bind(tenant_id.into_uuid())
        .execute(pool)
        .await
        .expect("tenant should be inserted");

    sqlx::query(
        "INSERT INTO owner_principals (tenant_id, caller_id, principal_kind) \
         VALUES ($1, $2, 'tenant_owner')",
    )
    .bind(tenant_id.into_uuid())
    .bind(principal_id)
    .execute(pool)
    .await
    .expect("Owner Principal should be inserted");

    sqlx::query(
        "INSERT INTO callers (id, tenant_id, token_hash, caller_kind) \
         VALUES ($1, $2, $3, 'Authenticated')",
    )
    .bind(caller_id.into_uuid())
    .bind(tenant_id.into_uuid())
    .bind([0x44_u8; 32].as_slice())
    .execute(pool)
    .await
    .expect("Caller should be inserted");
}

async fn context_issuer(pool: &PgPool) -> ContextIssuer {
    let database_oid: String =
        sqlx::query_scalar("SELECT oid::text FROM pg_database WHERE datname = current_database()")
            .fetch_one(pool)
            .await
            .expect("Runtime database OID should be readable");
    let deployment_id = "rt061-caller-deletion-test";
    let audience =
        format!("urn:qembed:postgres:deployment:{deployment_id}:database:{database_oid}");
    let key_id = format!("rt061-test-{}", Uuid::new_v4());
    let signing_key = RingEd25519SigningKey::from_seed(key_id.clone(), &[0x62_u8; 32])
        .expect("test Ed25519 seed should be valid");
    let public_key = signing_key.public_key();

    let mut admin = pool.acquire().await.expect("key control-plane connection");
    sqlx::query("SET SESSION AUTHORIZATION qembed_key_control_plane")
        .execute(&mut *admin)
        .await
        .expect("test superuser should assume the key control-plane role");

    sqlx::query("SELECT qembed_crypto.stage_verification_key($1, $2, $3, $4, $5, $6, $7, $8)")
    .bind(&key_id)
    .bind(public_key.to_vec())
    .bind("qembed-runtime")
    .bind(&audience)
    .bind(deployment_id)
    .bind("operator:rt061-test")
    .bind(Uuid::new_v4())
    .bind("RUST-061 caller deletion integration test")
    .execute(&mut *admin)
    .await
    .expect("test public key should be staged");

    let _: String = sqlx::query_scalar(
        "SELECT qembed_crypto.activate_verification_key($1, $2, $3, $4, $5)::text",
    )
    .bind(&key_id)
    .bind("operator:rt061-test")
    .bind("approver:rt061-test")
    .bind(Uuid::new_v4())
    .bind("activate RUST-061 integration test key")
    .fetch_one(&mut *admin)
    .await
    .expect("test public key should be activated");

    sqlx::query("RESET SESSION AUTHORIZATION")
        .execute(&mut *admin)
        .await
        .expect("session authorization should be reset");
    drop(admin);

    let config = ContextIssuerConfig::with_default_lifetime(
        "qembed-runtime",
        audience,
        deployment_id,
        key_id,
    )
    .expect("ContextIssuer config should be valid");
    ContextIssuer::new(config, signing_key).expect("ContextIssuer should be valid")
}

#[tokio::test]
#[ignore = "requires canonical qembed-sql RT-061 migrations; run from scripts/test-postgres.sh"]
async fn owner_deletion_commits_external_tombstone_before_idempotent_data_erasure() {
    let runtime = runtime_pool().await;
    let control_plane = control_plane_pool().await;
    run_control_plane_migrations(&control_plane)
        .await
        .expect("Control-plane schema should be migrated");

    let tenant_id = TenantId::new();
    let scope = scope_for_tenant(tenant_id);
    let caller_id = CallerId::new();
    seed_owner_and_caller(
        &runtime,
        tenant_id,
        scope.principal().principal_id().into_uuid(),
        caller_id,
    )
    .await;

    let object_id = ObjectId::new();
    let version_id = ObjectVersionId::new();
    let snapshot_id = SnapshotId::new();
    let invocation_id = Uuid::new_v4();

    sqlx::query("INSERT INTO objects (tenant_id, object_id) VALUES ($1, $2)")
        .bind(tenant_id.into_uuid())
        .bind(object_id.into_uuid())
        .execute(&runtime)
        .await
        .expect("Object fixture should be inserted");
    sqlx::query(
        "INSERT INTO object_versions \
         (tenant_id, object_id, object_version_id, version_number, status) \
         VALUES ($1, $2, $3, 1, 'draft')",
    )
    .bind(tenant_id.into_uuid())
    .bind(object_id.into_uuid())
    .bind(version_id.into_uuid())
    .execute(&runtime)
    .await
    .expect("ObjectVersion fixture should be inserted");
    let canonical = b"{}";
    let content_hash = ring::digest::digest(&ring::digest::SHA256, canonical);
    sqlx::query(
        "INSERT INTO snapshots \
         (tenant_id, snapshot_id, object_id, object_version_id, schema_version, \
          canonical_json, content_hash, json_projection) \
         VALUES ($1, $2, $3, $4, 1, $5, $6, '{}'::jsonb)",
    )
    .bind(tenant_id.into_uuid())
    .bind(snapshot_id.into_uuid())
    .bind(object_id.into_uuid())
    .bind(version_id.into_uuid())
    .bind(canonical.as_slice())
    .bind(content_hash.as_ref())
    .execute(&runtime)
    .await
    .expect("Snapshot fixture should be inserted");
    sqlx::query(
        "INSERT INTO object_invocations \
         (id, tenant_id, object_id, caller_id, snapshot_id, knowledge_revision, \
          pool, status, trace_id) \
         VALUES ($1, $2, $3, $4, $5, 0, 'authenticated', 'Received', $6)",
    )
    .bind(invocation_id)
    .bind(tenant_id.into_uuid())
    .bind(object_id.into_uuid())
    .bind(caller_id.into_uuid())
    .bind(snapshot_id.into_uuid())
    .bind(Uuid::new_v4())
    .execute(&runtime)
    .await
    .expect("Invocation fixture should be inserted");
    sqlx::query(
        "UPDATE object_invocations \
         SET status = 'Failed', error_code = 'test_terminal', \
             finished_at = clock_timestamp(), version = 1 WHERE id = $1",
    )
    .bind(invocation_id)
    .execute(&runtime)
    .await
    .expect("Invocation should be moved to a terminal state");

    sqlx::query(
        "INSERT INTO interaction_messages \
         (id, tenant_id, object_id, caller_id, invocation_id, role, content, expires_at) \
         VALUES ($1, $2, $3, $4, $5, 'user', 'caller private message', now() + interval '1 day')",
    )
    .bind(Uuid::new_v4())
    .bind(tenant_id.into_uuid())
    .bind(object_id.into_uuid())
    .bind(caller_id.into_uuid())
    .bind(invocation_id)
    .execute(&runtime)
    .await
    .expect("Caller message should be inserted");
    sqlx::query(
        "INSERT INTO memories \
         (id, tenant_id, object_id, caller_id, memory_kind, claim, source_kind, \
          verification, confidence, invocation_id, expires_at) \
         VALUES ($1, $2, $3, $4, 'episodic', 'caller private memory', 'User', \
                 'Unverified', 0.9, $5, now() + interval '1 day')",
    )
    .bind(Uuid::new_v4())
    .bind(tenant_id.into_uuid())
    .bind(object_id.into_uuid())
    .bind(caller_id.into_uuid())
    .bind(invocation_id)
    .execute(&runtime)
    .await
    .expect("Caller memory should be inserted");
    sqlx::query(
        "INSERT INTO invocation_idempotency \
         (tenant_id, object_id, caller_id, idempotency_key, payload_hash, invocation_id, expires_at) \
         VALUES ($1, $2, $3, 'rt061-idempotency', $4, $5, now() + interval '1 day')",
    )
    .bind(tenant_id.into_uuid())
    .bind(object_id.into_uuid())
    .bind(caller_id.into_uuid())
    .bind(content_hash.as_ref())
    .bind(invocation_id)
    .execute(&runtime)
    .await
    .expect("idempotency fixture should be inserted");

    let context = PgContext::new(runtime.clone(), context_issuer(&runtime).await);
    let repository = PgCallerDeletionRepository::new(context);
    let requested = repository
        .request(&scope, caller_id)
        .await
        .expect("Caller should be revoked and deletion job created");
    assert_eq!(requested.status, CallerDeletionJobStatus::Pending);

    let key = CallerDeletionKey::new(&[0x52; 32]).expect("32-byte HMAC key");
    let ledger = ExternalCallerDeletionLedger::new(control_plane.clone(), key);
    let tombstone = ledger
        .record(tenant_id, caller_id)
        .await
        .expect("external tombstone must commit before cleanup");
    assert!(tombstone.inserted);

    let completed = repository
        .process(&scope, caller_id, requested.id, tombstone.sequence_id)
        .await
        .expect("Caller-derived data should be erased");
    assert_eq!(completed.status, CallerDeletionJobStatus::Done);

    let message_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM interaction_messages WHERE tenant_id = $1 AND caller_id = $2",
    )
    .bind(tenant_id.into_uuid())
    .bind(caller_id.into_uuid())
    .fetch_one(&runtime)
    .await
    .expect("message count should be readable");
    let memory_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM memories WHERE tenant_id = $1 AND caller_id = $2")
            .bind(tenant_id.into_uuid())
            .bind(caller_id.into_uuid())
            .fetch_one(&runtime)
            .await
            .expect("memory count should be readable");
    let idempotency_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM invocation_idempotency WHERE tenant_id = $1 AND caller_id = $2",
    )
    .bind(tenant_id.into_uuid())
    .bind(caller_id.into_uuid())
    .fetch_one(&runtime)
    .await
    .expect("idempotency count should be readable");
    assert_eq!(message_count, 0);
    assert_eq!(memory_count, 0);
    assert_eq!(idempotency_count, 0);

    let repeated_request = repository
        .request(&scope, caller_id)
        .await
        .expect("repeated deletion should reuse the completed job");
    assert_eq!(repeated_request.id, requested.id);
    assert_eq!(repeated_request.status, CallerDeletionJobStatus::Done);

    let repeated_tombstone = ledger
        .record(tenant_id, caller_id)
        .await
        .expect("repeated tombstone should be idempotent");
    assert!(!repeated_tombstone.inserted);
    assert_eq!(repeated_tombstone.sequence_id, tombstone.sequence_id);

    let repeated_process = repository
        .process(&scope, caller_id, requested.id, tombstone.sequence_id)
        .await
        .expect("repeated cleanup should remain idempotent");
    assert_eq!(repeated_process.status, CallerDeletionJobStatus::Done);

    runtime.close().await;
    control_plane.close().await;
}
