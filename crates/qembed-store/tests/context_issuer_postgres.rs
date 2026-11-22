use std::time::Duration;

use qembed_domain::ids::{CallerId, TenantId};
use qembed_domain::test_support::scope_for_tenant;
use qembed_store::{
    ContextIssuer, ContextIssuerConfig, PgPoolConfig, RingEd25519SigningKey,
    begin_context_transaction, connect_pool, run_migrations,
};
use sqlx::PgPool;
use uuid::Uuid;

async fn pool_with_one_connection() -> PgPool {
    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set by scripts/test-postgres.sh");
    connect_pool(
        &database_url,
        PgPoolConfig {
            max_connections: 1,
            acquire_timeout: Duration::from_secs(5),
        },
    )
    .await
    .expect("the PostgreSQL test database should accept connections")
}

async fn seed_delegation(
    pool: &PgPool,
    tenant_id: TenantId,
    principal_id: Uuid,
    caller_id: CallerId,
) {
    sqlx::query("INSERT INTO tenants (id) VALUES ($1::uuid)")
        .bind(tenant_id.to_string())
        .execute(pool)
        .await
        .expect("tenant fixture should be inserted");

    sqlx::query(
        "INSERT INTO owner_principals (tenant_id, caller_id, principal_kind) \
         VALUES ($1::uuid, $2::uuid, 'tenant_owner')",
    )
    .bind(tenant_id.to_string())
    .bind(principal_id.to_string())
    .execute(pool)
    .await
    .expect("authenticated Principal fixture should be inserted");

    sqlx::query(
        "INSERT INTO callers (id, tenant_id, token_hash, caller_kind) \
         VALUES ($1::uuid, $2::uuid, $3, 'Authenticated')",
    )
    .bind(caller_id.to_string())
    .bind(tenant_id.to_string())
    .bind(vec![0_u8; 32])
    .execute(pool)
    .await
    .expect("Caller fixture should be inserted");

    sqlx::query(
        "INSERT INTO principal_caller_delegations \
         (delegation_id, tenant_id, principal_id, caller_id) \
         VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(tenant_id.to_string())
    .bind(principal_id.to_string())
    .bind(caller_id.to_string())
    .execute(pool)
    .await
    .expect("active direct delegation fixture should be inserted");
}

async fn seed_additional_delegation(
    pool: &PgPool,
    tenant_id: TenantId,
    principal_id: Uuid,
    caller_id: CallerId,
) -> Uuid {
    sqlx::query(
        "INSERT INTO callers (id, tenant_id, token_hash, caller_kind) \
         VALUES ($1::uuid, $2::uuid, $3, 'Authenticated')",
    )
    .bind(caller_id.to_string())
    .bind(tenant_id.to_string())
    .bind(vec![1_u8; 32])
    .execute(pool)
    .await
    .expect("additional Caller fixture should be inserted");

    let delegation_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO principal_caller_delegations \
         (delegation_id, tenant_id, principal_id, caller_id) \
         VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid)",
    )
    .bind(delegation_id.to_string())
    .bind(tenant_id.to_string())
    .bind(principal_id.to_string())
    .bind(caller_id.to_string())
    .execute(pool)
    .await
    .expect("additional direct delegation fixture should be inserted");

    delegation_id
}

#[tokio::test]
async fn rust_context_issuer_token_is_verified_by_postgres_and_bound_to_pool_transaction() {
    let pool = pool_with_one_connection().await;

    let server_version_num: String = sqlx::query_scalar("SHOW server_version_num")
        .fetch_one(&pool)
        .await
        .expect("PostgreSQL version should be readable");
    let server_version_num = server_version_num
        .parse::<i32>()
        .expect("server_version_num should be an integer");
    if !(190_000..200_000).contains(&server_version_num) {
        pool.close().await;
        return;
    }

    run_migrations(&pool)
        .await
        .expect("Rust store migrations should apply before the RT-050 fixture");

    let tenant_id = TenantId::new();
    let scope = scope_for_tenant(tenant_id);
    let principal_id = scope.principal().principal_id();
    let caller_id = CallerId::new();
    seed_delegation(&pool, tenant_id, principal_id.into_uuid(), caller_id).await;

    let issuer_name = "qembed-runtime";
    let deployment_id = "rt050-test-deployment";
    let database_oid: String =
        sqlx::query_scalar("SELECT oid::text FROM pg_database WHERE datname = current_database()")
            .fetch_one(&pool)
            .await
            .expect("database OID should be readable");
    let audience =
        format!("urn:qembed:postgres:deployment:{deployment_id}:database:{database_oid}");
    let key_id = format!("rt050-test-{}", Uuid::new_v4());
    let signing_key = RingEd25519SigningKey::from_seed(key_id.clone(), &[42_u8; 32])
        .expect("the fixed test seed should create a valid Ed25519 signer");
    let public_key = signing_key.public_key();
    let config = ContextIssuerConfig::with_default_lifetime(
        issuer_name,
        audience.clone(),
        deployment_id,
        key_id.clone(),
    )
    .expect("test issuer configuration should be valid");

    sqlx::query(
        "INSERT INTO qembed_crypto.verification_keys \
         (key_id, public_key, issuer, audience, deployment_id) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(&key_id)
    .bind(public_key.to_vec())
    .bind(issuer_name)
    .bind(&audience)
    .bind(deployment_id)
    .execute(&pool)
    .await
    .expect("the SQL verifier should receive only the public key");

    let issuer = ContextIssuer::new(config, signing_key)
        .expect("issuer configuration and signing key IDs should match");

    let mut transaction = begin_context_transaction(&pool, &scope, caller_id, &issuer)
        .await
        .expect("trusted Scope and database-resolved Caller should issue a context token");

    let transaction_isolation: String = sqlx::query_scalar("SHOW transaction_isolation")
        .fetch_one(transaction.connection())
        .await
        .expect("signed context transaction isolation should be readable");
    assert_eq!(
        transaction_isolation, "read committed",
        "key revocation must be visible to subsequent statement snapshots"
    );

    let backend_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(transaction.connection())
        .await
        .expect("transaction backend PID should be readable");
    let token: String = sqlx::query_scalar("SELECT current_setting('app.context_token', true)")
        .fetch_one(transaction.connection())
        .await
        .expect("Rust should install the single qctx1 token setting");
    assert!(token.starts_with("qctx1:"));

    let verified_claims: serde_json::Value =
        sqlx::query_scalar("SELECT qembed_crypto.verified_context($1)")
            .bind(&token)
            .fetch_one(transaction.connection())
            .await
            .expect("PostgreSQL should verify the Rust serde_jcs/Ed25519 payload");
    assert_eq!(verified_claims["version"], "1");
    assert_eq!(verified_claims["tenant_id"], tenant_id.to_string());
    assert_eq!(verified_claims["principal_id"], principal_id.to_string());
    assert_eq!(verified_claims["caller_id"], caller_id.to_string());
    assert_eq!(verified_claims["backend_pid"], backend_pid.to_string());

    sqlx::query(
        "INSERT INTO qembed_crypto.tenant_rows (tenant_id, row_id) \
         VALUES ($1::uuid, 91001), ($2::uuid, 91002)",
    )
    .bind(tenant_id.to_string())
    .bind(TenantId::new().to_string())
    .execute(transaction.connection())
    .await
    .expect("RLS test rows should be inserted before switching to the runtime role");

    sqlx::query("SET LOCAL ROLE qembed_runtime")
        .execute(transaction.connection())
        .await
        .expect("test transaction should assume the restricted runtime role");

    let runtime_can_read_key_registry: bool = sqlx::query_scalar(
        "SELECT has_table_privilege(current_user, 'qembed_crypto.verification_keys', 'SELECT')",
    )
    .fetch_one(transaction.connection())
    .await
    .expect("runtime key-registry privilege should be inspectable");
    let runtime_can_retire_keys: bool = sqlx::query_scalar(
        "SELECT has_function_privilege(current_user, 'qembed_crypto.retire_verification_key(text,interval)', 'EXECUTE')",
    )
    .fetch_one(transaction.connection())
    .await
    .expect("runtime retirement privilege should be inspectable");
    let runtime_can_revoke_keys: bool = sqlx::query_scalar(
        "SELECT has_function_privilege(current_user, 'qembed_crypto.revoke_verification_key(text)', 'EXECUTE')",
    )
    .fetch_one(transaction.connection())
    .await
    .expect("runtime revocation privilege should be inspectable");
    assert!(!runtime_can_read_key_registry);
    assert!(!runtime_can_retire_keys);
    assert!(!runtime_can_revoke_keys);

    let tenant_claim: String = sqlx::query_scalar("SELECT qembed_crypto.current_tenant_id()::text")
        .fetch_one(transaction.connection())
        .await
        .expect("typed tenant accessor should verify the token");
    let principal_claim: String =
        sqlx::query_scalar("SELECT qembed_crypto.current_principal_id()::text")
            .fetch_one(transaction.connection())
            .await
            .expect("typed Principal accessor should verify the token");
    let caller_claim: String = sqlx::query_scalar("SELECT qembed_crypto.current_caller_id()::text")
        .fetch_one(transaction.connection())
        .await
        .expect("typed Caller accessor should verify the token");
    assert_eq!(tenant_claim, tenant_id.to_string());
    assert_eq!(principal_claim, principal_id.to_string());
    assert_eq!(caller_claim, caller_id.to_string());

    let visible_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM qembed_crypto.tenant_rows")
        .fetch_one(transaction.connection())
        .await
        .expect("RLS should execute through the signed tenant context");
    assert_eq!(
        visible_rows, 1,
        "RLS must return only the signed tenant's row"
    );

    sqlx::query("SAVEPOINT rt050_savepoint")
        .execute(transaction.connection())
        .await
        .expect("savepoint should be created");
    sqlx::query("SELECT set_config('app.caller_id', $1, true)")
        .bind(TenantId::new().to_string())
        .execute(transaction.connection())
        .await
        .expect("forged legacy Caller selector should be settable");
    sqlx::query("ROLLBACK TO SAVEPOINT rt050_savepoint")
        .execute(transaction.connection())
        .await
        .expect("savepoint rollback should preserve the outer transaction");
    let caller_after_savepoint: String =
        sqlx::query_scalar("SELECT qembed_crypto.current_caller_id()::text")
            .fetch_one(transaction.connection())
            .await
            .expect("outer signed Caller should survive savepoint rollback");
    assert_eq!(caller_after_savepoint, caller_id.to_string());

    transaction
        .commit()
        .await
        .expect("signed-context transaction should commit");

    let backend_after_commit: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&pool)
        .await
        .expect("the pooled backend should remain available");
    assert_eq!(
        backend_after_commit, backend_pid,
        "single-connection pool should reuse the same backend"
    );

    let token_after_commit: Option<String> =
        sqlx::query_scalar("SELECT NULLIF(current_setting('app.context_token', true), '')")
            .fetch_one(&pool)
            .await
            .expect("transaction-local token setting should be readable after commit");
    assert_eq!(token_after_commit, None, "commit must clear the token GUC");

    let committed_replay: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT qembed_crypto.verified_context($1)")
            .bind(&token)
            .fetch_one(&pool)
            .await
            .expect("replay probe should execute");
    assert!(
        committed_replay.is_none(),
        "committed token must fail in the next transaction"
    );

    let mut rollback_transaction = begin_context_transaction(&pool, &scope, caller_id, &issuer)
        .await
        .expect("a retry must mint a fresh transaction-bound token");
    let rollback_token: String =
        sqlx::query_scalar("SELECT current_setting('app.context_token', true)")
            .fetch_one(rollback_transaction.connection())
            .await
            .expect("rollback transaction should install a token");
    rollback_transaction
        .rollback()
        .await
        .expect("signed-context transaction should roll back");

    let rollback_replay: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT qembed_crypto.verified_context($1)")
            .bind(&rollback_token)
            .fetch_one(&pool)
            .await
            .expect("rollback replay probe should execute");
    assert!(
        rollback_replay.is_none(),
        "rolled-back token must fail on pooled connection reuse"
    );

    let revocable_caller = CallerId::new();
    let revocable_delegation_id =
        seed_additional_delegation(&pool, tenant_id, principal_id.into_uuid(), revocable_caller)
            .await;
    let mut revocation_race_transaction =
        begin_context_transaction(&pool, &scope, revocable_caller, &issuer)
            .await
            .expect("active delegation should allow context issuance");

    let revocation_backend_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(revocation_race_transaction.connection())
        .await
        .expect("revocation-race transaction backend should be readable");
    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set by scripts/test-postgres.sh");
    let revocation_pool = connect_pool(
        &database_url,
        PgPoolConfig {
            max_connections: 1,
            acquire_timeout: Duration::from_secs(5),
        },
    )
    .await
    .expect("revocation pool should open an independent backend");
    let revoker_backend_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&revocation_pool)
        .await
        .expect("revoker backend PID should be readable");
    assert_ne!(revoker_backend_pid, revocation_backend_pid);

    let monitor_pool = connect_pool(
        &database_url,
        PgPoolConfig {
            max_connections: 1,
            acquire_timeout: Duration::from_secs(5),
        },
    )
    .await
    .expect("monitor pool should open an independent backend");
    let revocation_pool_for_task = revocation_pool.clone();
    let revocation_task = tokio::spawn(async move {
        sqlx::query(
            "UPDATE principal_caller_delegations \
             SET revoked_at = clock_timestamp(), revoked_by_principal_id = $1::uuid \
             WHERE delegation_id = $2::uuid",
        )
        .bind(principal_id.to_string())
        .bind(revocable_delegation_id.to_string())
        .execute(&revocation_pool_for_task)
        .await
    });

    let observed_block = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let blocking_pids: Vec<i32> = sqlx::query_scalar("SELECT pg_blocking_pids($1)")
                .bind(revoker_backend_pid)
                .fetch_one(&monitor_pool)
                .await
                .expect("blocking backend list should be readable");
            if blocking_pids.contains(&revocation_backend_pid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        observed_block.is_ok(),
        "Caller revocation must wait while the issuer holds the delegation row lock"
    );

    revocation_race_transaction
        .commit()
        .await
        .expect("context transaction should release its delegation lock");
    tokio::time::timeout(Duration::from_secs(5), revocation_task)
        .await
        .expect("revocation should proceed after context transaction completes")
        .expect("revocation task should not panic")
        .expect("revocation update should succeed");

    let revoked_caller_attempt =
        begin_context_transaction(&pool, &scope, revocable_caller, &issuer).await;
    assert!(
        revoked_caller_attempt.is_err(),
        "a revoked Caller must not receive a new signed context"
    );
    monitor_pool.close().await;
    revocation_pool.close().await;

    let other_database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set by scripts/test-postgres.sh");
    let other_pool = connect_pool(
        &other_database_url,
        PgPoolConfig {
            max_connections: 1,
            acquire_timeout: Duration::from_secs(5),
        },
    )
    .await
    .expect("second pool should open an independent PostgreSQL backend");
    let other_backend_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&other_pool)
        .await
        .expect("second backend PID should be readable");
    assert_ne!(
        other_backend_pid, backend_pid,
        "second pool must use another backend"
    );
    let other_backend_replay: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT qembed_crypto.verified_context($1)")
            .bind(&token)
            .fetch_one(&other_pool)
            .await
            .expect("cross-backend replay probe should execute");
    assert!(
        other_backend_replay.is_none(),
        "token from another backend must be rejected"
    );

    other_pool.close().await;

    // Graceful retirement accepts a token minted before the retirement
    // timestamp, rejects any token minted afterwards, and revocation is
    // immediate even while a pre-retirement transaction remains open.
    let mut lifecycle_transaction = begin_context_transaction(&pool, &scope, caller_id, &issuer)
        .await
        .expect("active key should issue a token before retirement");
    let pre_retirement_token: String =
        sqlx::query_scalar("SELECT current_setting('app.context_token', true)")
            .fetch_one(lifecycle_transaction.connection())
            .await
            .expect("pre-retirement token should be installed");
    let admin_pool = connect_pool(
        &database_url,
        PgPoolConfig {
            max_connections: 1,
            acquire_timeout: Duration::from_secs(5),
        },
    )
    .await
    .expect("key lifecycle admin pool should open an independent backend");

    let pre_retirement_valid: bool =
        sqlx::query_scalar("SELECT qembed_crypto.verified_context($1) IS NOT NULL")
            .bind(&pre_retirement_token)
            .fetch_one(lifecycle_transaction.connection())
            .await
            .expect("pre-retirement token should verify while the key is active");
    assert!(pre_retirement_valid);

    let verify_until: String =
        sqlx::query_scalar("SELECT qembed_crypto.retire_verification_key($1)::text")
            .bind(&key_id)
            .fetch_one(&admin_pool)
            .await
            .expect("active key should transition to retired with a verification grace");
    assert!(!verify_until.is_empty());

    let retired_grace_valid: bool =
        sqlx::query_scalar("SELECT qembed_crypto.verified_context($1) IS NOT NULL")
            .bind(&pre_retirement_token)
            .fetch_one(lifecycle_transaction.connection())
            .await
            .expect("pre-retirement token should remain valid during the grace period");
    assert!(retired_grace_valid);

    let mut stale_signing_transaction =
        begin_context_transaction(&admin_pool, &scope, caller_id, &issuer)
            .await
            .expect("Rust signer can still produce bytes during rollout overlap");
    let post_retirement_token: String =
        sqlx::query_scalar("SELECT current_setting('app.context_token', true)")
            .fetch_one(stale_signing_transaction.connection())
            .await
            .expect("post-retirement token should be installed for fail-closed verification");
    let post_retirement_valid: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT qembed_crypto.verified_context($1)")
            .bind(&post_retirement_token)
            .fetch_one(stale_signing_transaction.connection())
            .await
            .expect("post-retirement verification should return a fail-closed NULL");
    assert!(
        post_retirement_valid.is_none(),
        "a stale Rust instance must not obtain accepted tokens after key retirement"
    );
    stale_signing_transaction
        .commit()
        .await
        .expect("transaction with rejected token should still be cleanly closable");

    sqlx::query("SELECT qembed_crypto.revoke_verification_key($1)")
        .bind(&key_id)
        .execute(&admin_pool)
        .await
        .expect("retired key should be immediately revocable");

    let revoked_pre_retirement_valid: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT qembed_crypto.verified_context($1)")
            .bind(&pre_retirement_token)
            .fetch_one(lifecycle_transaction.connection())
            .await
            .expect("revoked key verification should return NULL");
    assert!(
        revoked_pre_retirement_valid.is_none(),
        "revocation must invalidate an otherwise-live pre-retirement token immediately"
    );
    lifecycle_transaction
        .commit()
        .await
        .expect("lifecycle transaction should close after emergency revocation");

    let emergency_key_id = format!("rt050-emergency-{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO qembed_crypto.verification_keys \
         (key_id, public_key, issuer, audience, deployment_id) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(&emergency_key_id)
    .bind(public_key.to_vec())
    .bind(issuer_name)
    .bind(&audience)
    .bind(deployment_id)
    .execute(&admin_pool)
    .await
    .expect("emergency key fixture should start active");
    let emergency_revoked_at: String =
        sqlx::query_scalar("SELECT qembed_crypto.revoke_verification_key($1)::text")
            .bind(&emergency_key_id)
            .fetch_one(&admin_pool)
            .await
            .expect("active key should support direct emergency revocation");
    assert!(!emergency_revoked_at.is_empty());

    admin_pool.close().await;
    pool.close().await;
}
