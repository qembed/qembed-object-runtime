use std::time::Duration;

use qembed_domain::test_support::scope_for_tenant;
use qembed_domain::{
    anonymous::AnonymousContext,
    ids::{CallerId, ObjectId, ObjectVersionId, SnapshotId, TenantId, TraceId},
    ports::{
        AnonymousInvocationStartCommand, AnonymousInvocationStartStore,
        InvocationIdempotencyPreflight, InvocationStartOutcome,
    },
    rate_limit::RateLimitRequestId,
};
use qembed_store::{
    ContextIssuer, ContextIssuerConfig, PgContext, PgPoolConfig, RingEd25519SigningKey,
    invocation_start::PgAnonymousInvocationStartStore,
    begin_context_transaction, begin_principal_context_transaction, connect_pool,
};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::time::{Duration, SystemTime};
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
#[ignore = "requires the canonical qembed-sql QSA-003 signed-context migration; run from scripts/test-postgres.sh"]
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

    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set by scripts/test-postgres.sh");
    let admin_pool = connect_pool(
        &database_url,
        PgPoolConfig {
            max_connections: 2,
            acquire_timeout: Duration::from_secs(5),
        },
    )
    .await
    .expect("key lifecycle admin pool should open independent backends");
    let mut admin_connection = admin_pool
        .acquire()
        .await
        .expect("key control-plane connection should be acquired");
    sqlx::query("SET SESSION AUTHORIZATION qembed_key_control_plane")
        .execute(&mut *admin_connection)
        .await
        .expect("test harness superuser should assume the dedicated control-plane identity");

    let control_plane_can_read_keys: bool = sqlx::query_scalar(
        "SELECT has_table_privilege(current_user, 'qembed_crypto.verification_keys', 'SELECT')",
    )
    .fetch_one(&mut *admin_connection)
    .await
    .expect("control-plane registry read privilege should be inspectable");
    let control_plane_can_insert_keys: bool = sqlx::query_scalar(
        "SELECT has_table_privilege(current_user, 'qembed_crypto.verification_keys', 'INSERT')",
    )
    .fetch_one(&mut *admin_connection)
    .await
    .expect("control-plane registry insert privilege should be inspectable");
    let control_plane_can_stage_keys: bool = sqlx::query_scalar(
        "SELECT has_function_privilege(current_user, 'qembed_crypto.stage_verification_key(text,bytea,text,text,text,text,uuid,text)', 'EXECUTE')",
    )
    .fetch_one(&mut *admin_connection)
    .await
    .expect("control-plane staging privilege should be inspectable");
    let control_plane_can_list_keys: bool = sqlx::query_scalar(
        "SELECT has_function_privilege(current_user, 'qembed_crypto.list_verification_keys(text)', 'EXECUTE')",
    )
    .fetch_one(&mut *admin_connection)
    .await
    .expect("control-plane metadata reader privilege should be inspectable");
    let control_plane_can_read_operation: bool = sqlx::query_scalar(
        "SELECT has_function_privilege(current_user, 'qembed_crypto.get_key_lifecycle_operation(uuid)', 'EXECUTE')",
    )
    .fetch_one(&mut *admin_connection)
    .await
    .expect("control-plane audit reader privilege should be inspectable");
    assert!(!control_plane_can_read_keys);
    assert!(!control_plane_can_insert_keys);
    assert!(control_plane_can_stage_keys);
    assert!(control_plane_can_list_keys);
    assert!(control_plane_can_read_operation);

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

    let stage_request_id = Uuid::new_v4();
    sqlx::query("SELECT qembed_crypto.stage_verification_key($1, $2, $3, $4, $5, $6, $7, $8)")
        .bind(&key_id)
        .bind(public_key.to_vec())
        .bind(issuer_name)
        .bind(&audience)
        .bind(deployment_id)
        .bind("operator:rt050-test")
        .bind(stage_request_id)
        .bind("integration test key staging")
        .execute(&mut *admin_connection)
        .await
        .expect("the control plane should stage only the public key");

    let _: String = sqlx::query_scalar(
        "SELECT qembed_crypto.activate_verification_key($1, $2, $3, $4, $5)::text",
    )
    .bind(&key_id)
    .bind("operator:rt050-test")
    .bind("approver:rt050-test")
    .bind(Uuid::new_v4())
    .bind("integration test key activation")
    .fetch_one(&mut *admin_connection)
    .await
    .expect("a distinct approver should activate the staged key");

    let listed_key_status: String = sqlx::query_scalar(
        "SELECT status FROM qembed_crypto.list_verification_keys($1) WHERE key_id = $2",
    )
    .bind(deployment_id)
    .bind(&key_id)
    .fetch_one(&mut *admin_connection)
    .await
    .expect("control plane should read public key metadata through its definer function");
    assert_eq!(listed_key_status, "active");
    let stage_audit_state: (String, String) = sqlx::query_as(
        "SELECT new_state, audit_export_status FROM qembed_crypto.get_key_lifecycle_operation($1)",
    )
    .bind(stage_request_id)
    .fetch_one(&mut *admin_connection)
    .await
    .expect(
        "control plane should read audit evidence and export status through its definer function",
    );
    assert_eq!(
        stage_audit_state,
        ("staged".to_owned(), "pending".to_owned())
    );

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

    let other_caller_id = CallerId::new();
    sqlx::query(
        "INSERT INTO qembed_crypto.caller_rows (caller_id, row_id) \
         VALUES ($1::uuid, 91011), ($2::uuid, 91012)",
    )
    .bind(caller_id.to_string())
    .bind(other_caller_id.to_string())
    .execute(transaction.connection())
    .await
    .expect("Caller-scoped RLS rows should be inserted before switching to runtime");

    sqlx::query("SET LOCAL ROLE qembed_runtime")
        .execute(transaction.connection())
        .await
        .expect("test transaction should assume the restricted runtime role");

    let legacy_selectors_before_forgery: (Option<String>, Option<String>, Option<String>) =
        sqlx::query_as(
            "SELECT NULLIF(current_setting('app.tenant_id', true), ''), \
                    NULLIF(current_setting('app.principal_id', true), ''), \
                    NULLIF(current_setting('app.caller_id', true), '')",
        )
        .fetch_one(transaction.connection())
        .await
        .expect("legacy identity selectors should be inspectable");
    assert_eq!(legacy_selectors_before_forgery, (None, None, None));

    sqlx::query(
        "SELECT set_config('app.tenant_id', $1, true), \
                set_config('app.principal_id', $2, true), \
                set_config('app.caller_id', $3, true)",
    )
    .bind(TenantId::new().to_string())
    .bind(Uuid::new_v4().to_string())
    .bind(other_caller_id.to_string())
    .execute(transaction.connection())
    .await
    .expect("runtime role can write legacy selectors but must not change signed identity");

    let runtime_privileges: (bool, bool, bool, bool, bool, bool, bool, bool, bool) =
        sqlx::query_as(
            r#"
            SELECT
              has_table_privilege(current_user, 'qembed_crypto.verification_keys', 'SELECT'),
              has_table_privilege(current_user, 'qembed_crypto.key_lifecycle_audit', 'SELECT'),
              has_function_privilege(current_user, 'qembed_crypto.stage_verification_key(text,bytea,text,text,text,text,uuid,text)', 'EXECUTE'),
              has_function_privilege(current_user, 'qembed_crypto.activate_verification_key(text,text,text,uuid,text)', 'EXECUTE'),
              has_function_privilege(current_user, 'qembed_crypto.retire_verification_key(text,interval,text,text,uuid,text)', 'EXECUTE'),
              has_function_privilege(current_user, 'qembed_crypto.revoke_verification_key(text,text,uuid,text)', 'EXECUTE'),
              has_function_privilege(current_user, 'qembed_crypto.list_verification_keys(text)', 'EXECUTE'),
              has_function_privilege(current_user, 'qembed_crypto.get_key_lifecycle_operation(uuid)', 'EXECUTE'),
              has_function_privilege(current_user, 'qembed_crypto.qembed_context_verify_detached(bytea,bytea,bytea)', 'EXECUTE')
            "#,
        )
        .fetch_one(transaction.connection())
        .await
        .expect("runtime key-management privileges should be inspectable");
    assert_eq!(
        runtime_privileges,
        (
            false, false, false, false, false, false, false, false, false
        )
    );

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
    let visible_caller_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM qembed_crypto.caller_rows")
            .fetch_one(transaction.connection())
            .await
            .expect("Caller-scoped RLS should use the signed Caller claim");
    assert_eq!(visible_caller_rows, 1);

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

    // A Principal-only transaction deliberately carries JSON null for Caller.
    // It must not infer a Caller from Principal identity or an available delegation.
    let mut principal_transaction = begin_principal_context_transaction(&pool, &scope, &issuer)
        .await
        .expect("Principal-only context must not require Caller selection");
    let principal_only_token: String =
        sqlx::query_scalar("SELECT current_setting('app.context_token', true)")
            .fetch_one(principal_transaction.connection())
            .await
            .expect("Principal-only context token should be installed");
    sqlx::query("SET LOCAL ROLE qembed_runtime")
        .execute(principal_transaction.connection())
        .await
        .expect("Principal-only probe should run under the restricted runtime role");
    let principal_only_claims: serde_json::Value =
        sqlx::query_scalar("SELECT qembed_crypto.verified_context($1)")
            .bind(&principal_only_token)
            .fetch_one(principal_transaction.connection())
            .await
            .expect("PostgreSQL should verify Principal-only claims");
    assert_eq!(principal_only_claims["tenant_id"], tenant_id.to_string());
    assert_eq!(
        principal_only_claims["principal_id"],
        principal_id.to_string()
    );
    assert!(principal_only_claims["caller_id"].is_null());

    sqlx::query(
        "SELECT set_config('app.tenant_id', $1, true), \
                set_config('app.principal_id', $2, true), \
                set_config('app.caller_id', $3, true)",
    )
    .bind(TenantId::new().to_string())
    .bind(Uuid::new_v4().to_string())
    .bind(caller_id.to_string())
    .execute(principal_transaction.connection())
    .await
    .expect("Principal-only runtime role can forge legacy selectors without gaining identity");

    let principal_after_forgery: String =
        sqlx::query_scalar("SELECT qembed_crypto.current_principal_id()::text")
            .fetch_one(principal_transaction.connection())
            .await
            .expect("Principal-only context should ignore the forged Principal GUC");
    assert_eq!(principal_after_forgery, principal_id.to_string());

    let no_effective_caller: Option<String> =
        sqlx::query_scalar("SELECT qembed_crypto.current_caller_id()::text")
            .fetch_one(principal_transaction.connection())
            .await
            .expect("Principal-only context should expose SQL NULL for Caller");
    assert_eq!(no_effective_caller, None);
    let principal_tenant_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM qembed_crypto.tenant_rows")
            .fetch_one(principal_transaction.connection())
            .await
            .expect("Principal-only context should retain tenant-scoped RLS access");
    assert_eq!(principal_tenant_rows, 1);
    let principal_caller_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM qembed_crypto.caller_rows")
            .fetch_one(principal_transaction.connection())
            .await
            .expect("Principal-only context should safely deny Caller-scoped rows");
    assert_eq!(principal_caller_rows, 0);
    principal_transaction
        .commit()
        .await
        .expect("Principal-only context transaction should commit");

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
    let pre_retirement_valid: bool =
        sqlx::query_scalar("SELECT qembed_crypto.verified_context($1) IS NOT NULL")
            .bind(&pre_retirement_token)
            .fetch_one(lifecycle_transaction.connection())
            .await
            .expect("pre-retirement token should verify while the key is active");
    assert!(pre_retirement_valid);

    let verify_until: String = sqlx::query_scalar(
        "SELECT qembed_crypto.retire_verification_key(         $1, interval '120 seconds', $2, $3, $4, $5)::text",
    )
    .bind(&key_id)
    .bind("operator:rt050-test")
    .bind("approver:rt050-test")
    .bind(Uuid::new_v4())
    .bind("integration test planned retirement")
    .fetch_one(&mut *admin_connection)
    .await
    .expect("a distinct approver should retire the active key with a verification grace");
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

    sqlx::query("SELECT qembed_crypto.revoke_verification_key($1, $2, $3, $4)")
        .bind(&key_id)
        .bind("break-glass:rt050-test")
        .bind(Uuid::new_v4())
        .bind("incident: RT-050 integration emergency revoke")
        .execute(&mut *admin_connection)
        .await
        .expect("break-glass control-plane identity should immediately revoke the retired key");

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
    sqlx::query("SELECT qembed_crypto.stage_verification_key($1, $2, $3, $4, $5, $6, $7, $8)")
        .bind(&emergency_key_id)
        .bind(public_key.to_vec())
        .bind(issuer_name)
        .bind(&audience)
        .bind(deployment_id)
        .bind("operator:rt050-test")
        .bind(Uuid::new_v4())
        .bind("stage emergency test key")
        .execute(&mut *admin_connection)
        .await
        .expect("emergency test key should be staged");
    let _: String = sqlx::query_scalar(
        "SELECT qembed_crypto.activate_verification_key($1, $2, $3, $4, $5)::text",
    )
    .bind(&emergency_key_id)
    .bind("operator:rt050-test")
    .bind("approver:rt050-test")
    .bind(Uuid::new_v4())
    .bind("activate emergency test key")
    .fetch_one(&mut *admin_connection)
    .await
    .expect("emergency test key should be activated by a distinct approver");
    let emergency_revoked_at: String =
        sqlx::query_scalar("SELECT qembed_crypto.revoke_verification_key($1, $2, $3, $4)::text")
            .bind(&emergency_key_id)
            .bind("break-glass:rt050-test")
            .bind(Uuid::new_v4())
            .bind("incident: direct active-key revocation test")
            .fetch_one(&mut *admin_connection)
            .await
            .expect("active key should support direct emergency revocation");
    assert!(!emergency_revoked_at.is_empty());

    sqlx::query("RESET SESSION AUTHORIZATION")
        .execute(&mut *admin_connection)
        .await
        .expect("control-plane session authorization should be reset before pool reuse");
    drop(admin_connection);
    admin_pool.close().await;
    pool.close().await;
}

async fn seed_public_object(pool: &PgPool, tenant_id: TenantId, object_id: ObjectId) {
    let version_id = ObjectVersionId::new();
    let snapshot_id = SnapshotId::new();
    let canonical_json = br#"{"limits":{"max_tokens_per_invocation":10000}}"#;
    let content_hash = Sha256::digest(canonical_json).to_vec();
    sqlx::query("INSERT INTO tenants (id) VALUES ($1::uuid)")
        .bind(tenant_id.to_string())
        .execute(pool)
        .await
        .expect("Anonymous tenant fixture should be inserted");

    sqlx::query(
        "INSERT INTO objects (tenant_id, object_id, state, anonymous_invocation_enabled) \
         VALUES ($1::uuid, $2::uuid, 'draft', TRUE)",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .execute(pool)
    .await
    .expect("public Object fixture should be inserted");

    sqlx::query(
        "INSERT INTO object_versions \
         (tenant_id, object_id, object_version_id, version_number, status) \
         VALUES ($1::uuid, $2::uuid, $3::uuid, 1, 'draft')",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .bind(version_id.to_string())
    .execute(pool)
    .await
    .expect("Draft ObjectVersion fixture should be inserted");

    sqlx::query(
        "UPDATE objects SET state = 'validating' \
         WHERE tenant_id = $1::uuid AND object_id = $2::uuid",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .execute(pool)
    .await
    .expect("Object should enter validation");

    sqlx::query(
        "UPDATE object_versions SET status = 'published', published_at = now() \
         WHERE tenant_id = $1::uuid AND object_id = $2::uuid AND object_version_id = $3::uuid",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .bind(version_id.to_string())
    .execute(pool)
    .await
    .expect("ObjectVersion should be published through its lifecycle");

    sqlx::query(
        "INSERT INTO snapshots \
         (tenant_id, snapshot_id, object_id, object_version_id, schema_version, canonical_json, content_hash, json_projection) \
         VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, 1, $5, $6, '{"limits":{"max_tokens_per_invocation":10000}}'::jsonb)",
    )
    .bind(tenant_id.to_string())
    .bind(snapshot_id.to_string())
    .bind(object_id.to_string())
    .bind(version_id.to_string())
    .bind(canonical_json.as_slice())
    .bind(content_hash)
    .execute(pool)
    .await
    .expect("immutable snapshot fixture should be inserted");

    sqlx::query(
        "INSERT INTO object_budgets \
         (tenant_id, object_id, daily_token_limit, daily_cost_limit_micros, per_caller_daily_token_limit, max_concurrent_invocations, max_llm_calls, max_tool_calls, max_tokens_per_invocation, max_duration_ms) \
         VALUES ($1::uuid, $2::uuid, 100000, 0, 100000, 10, 10, 0, 10000, 60000)",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .execute(pool)
    .await
    .expect("Object budget fixture should be inserted");

    sqlx::query(
        "UPDATE objects SET state = 'active', published_version_id = $3::uuid \
         WHERE tenant_id = $1::uuid AND object_id = $2::uuid",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .bind(version_id.to_string())
    .execute(pool)
    .await
    .expect("Object should become active with its published version");
}

async fn stage_anonymous_test_issuer(
    pool: &PgPool,
    admin_pool: &PgPool,
    deployment_id: &str,
) -> ContextIssuer {
    let mut admin_connection = admin_pool
        .acquire()
        .await
        .expect("key control-plane connection should be acquired");
    sqlx::query("SET SESSION AUTHORIZATION qembed_key_control_plane")
        .execute(&mut *admin_connection)
        .await
        .expect("test harness should assume the dedicated key control-plane identity");

    let database_oid: String =
        sqlx::query_scalar("SELECT oid::text FROM pg_database WHERE datname = current_database()")
            .fetch_one(pool)
            .await
            .expect("database OID should be readable");
    let issuer_name = "qembed-runtime";
    let audience =
        format!("urn:qembed:postgres:deployment:{deployment_id}:database:{database_oid}");
    let key_id = format!("rt059-anonymous-test-{}", Uuid::new_v4());
    let signing_key = RingEd25519SigningKey::from_seed(key_id.clone(), &[73_u8; 32])
        .expect("fixed test seed should create an Ed25519 signer");
    let public_key = signing_key.public_key();

    sqlx::query("SELECT qembed_crypto.stage_verification_key($1, $2, $3, $4, $5, $6, $7, $8)")
        .bind(&key_id)
        .bind(public_key.to_vec())
        .bind(issuer_name)
        .bind(&audience)
        .bind(deployment_id)
        .bind("operator:rt059-anonymous-test")
        .bind(Uuid::new_v4())
        .bind("stage qctx2 Anonymous integration key")
        .execute(&mut *admin_connection)
        .await
        .expect("control plane should stage the public verification key");

    let _: String = sqlx::query_scalar(
        "SELECT qembed_crypto.activate_verification_key($1, $2, $3, $4, $5)::text",
    )
    .bind(&key_id)
    .bind("operator:rt059-anonymous-test")
    .bind("approver:rt059-anonymous-test")
    .bind(Uuid::new_v4())
    .bind("activate qctx2 Anonymous integration key")
    .fetch_one(&mut *admin_connection)
    .await
    .expect("a distinct approver should activate the verification key");

    sqlx::query("RESET SESSION AUTHORIZATION")
        .execute(&mut *admin_connection)
        .await
        .expect("control-plane authorization should be reset before pool reuse");
    drop(admin_connection);

    let config =
        ContextIssuerConfig::with_default_lifetime(issuer_name, audience, deployment_id, key_id)
            .expect("Anonymous test issuer configuration should be valid");
    ContextIssuer::new(config, signing_key).expect("Anonymous test issuer should be valid")
}

#[tokio::test]
#[ignore = "requires canonical qembed-sql RT-050, RT-059 and RT-060; run from scripts/test-postgres.sh"]
async fn rust_context_issuer_qctx2_resolves_object_anonymous_caller_without_principal() {
    let pool = pool_with_one_connection().await;
    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set by scripts/test-postgres.sh");
    let admin_pool = connect_pool(
        &database_url,
        PgPoolConfig {
            max_connections: 2,
            acquire_timeout: Duration::from_secs(5),
        },
    )
    .await
    .expect("key control-plane pool should open independent backends");

    let tenant_id = TenantId::new();
    let object_id = ObjectId::new();
    seed_public_object(&pool, tenant_id, object_id).await;

    let deployment_id = format!("rt059-anonymous-deployment-{}", Uuid::new_v4());
    let issuer = stage_anonymous_test_issuer(&pool, &admin_pool, &deployment_id).await;
    let context = PgContext::new(pool.clone(), issuer);
    let fingerprint = [0x5a; 32];
    let anonymous = AnonymousContext::from_server_fingerprint(tenant_id, object_id, fingerprint)
        .expect("server-derived Anonymous context should be valid");

    let (mut transaction, caller) = context
        .begin_anonymous(&anonymous)
        .await
        .expect("qctx2 should provision and resolve the Object-scoped Anonymous Caller");
    assert_eq!(caller.kind(), qembed_domain::caller::CallerKind::Anonymous);

    let token: String = sqlx::query_scalar("SELECT current_setting('app.context_token', true)")
        .fetch_one(transaction.connection())
        .await
        .expect("qctx2 token should be installed");
    assert!(token.starts_with("qctx2:"));

    let claims: serde_json::Value = sqlx::query_scalar("SELECT qembed_crypto.verified_context($1)")
        .bind(&token)
        .fetch_one(transaction.connection())
        .await
        .expect("PostgreSQL should verify the Rust qctx2 token");
    assert_eq!(
        claims.as_object().expect("verified claims object").len(),
        17
    );
    assert_eq!(claims["version"], "2");
    assert_eq!(claims["subject_kind"], "anonymous");
    assert_eq!(claims["principal_id"], serde_json::Value::Null);
    assert_eq!(claims["tenant_id"], tenant_id.to_string());
    assert_eq!(claims["object_id"], object_id.to_string());
    assert_eq!(claims["caller_id"], caller.caller_id().to_string());

    let current_principal: Option<String> =
        sqlx::query_scalar("SELECT qembed_crypto.current_principal_id()::text")
            .fetch_one(transaction.connection())
            .await
            .expect("qctx2 Principal accessor should return SQL NULL");
    let current_caller: Option<String> =
        sqlx::query_scalar("SELECT qembed_crypto.current_caller_id()::text")
            .fetch_one(transaction.connection())
            .await
            .expect("qctx2 Caller accessor should return the resolved Caller");
    let current_object: Option<String> =
        sqlx::query_scalar("SELECT qembed_crypto.current_object_id()::text")
            .fetch_one(transaction.connection())
            .await
            .expect("qctx2 Object accessor should return the bound Object");
    let current_fingerprint: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT qembed_crypto.current_anonymous_fingerprint()")
            .fetch_one(transaction.connection())
            .await
            .expect("qctx2 fingerprint accessor should return the signed fingerprint");
    assert_eq!(current_principal, None);
    assert_eq!(current_caller, Some(caller.caller_id().to_string()));
    assert_eq!(current_object, Some(object_id.to_string()));
    assert_eq!(current_fingerprint, Some(fingerprint.to_vec()));
    transaction
        .commit()
        .await
        .expect("Anonymous invocation context transaction should commit");

    // Transaction-local signed context must not leak when the pooled backend
    // is reused for the next request.
    let leaked_context: Option<String> =
        sqlx::query_scalar("SELECT NULLIF(current_setting('app.context_token', true), '')")
            .fetch_one(&pool)
            .await
            .expect("post-commit context setting should be readable");
    assert_eq!(leaked_context, None);

    // The Object owns one stable Caller; the fingerprint remains per-subject.
    let other_fingerprint = [0x6b; 32];
    let other_anonymous =
        AnonymousContext::from_server_fingerprint(tenant_id, object_id, other_fingerprint)
            .expect("second server-derived Anonymous context should be valid");
    let (second_transaction, second_caller) = context
        .begin_anonymous(&other_anonymous)
        .await
        .expect("another Anonymous subject should resolve the same Object Caller");
    assert_eq!(second_caller.caller_id(), caller.caller_id());
    second_transaction
        .commit()
        .await
        .expect("second Anonymous context transaction should commit");

    // Exercise the actual invocation Store: the same fingerprint/key replays,
    // while the same raw key from another fingerprint gets a separate durable
    // idempotency namespace but retains the Object's stable Caller.
    let store = PgAnonymousInvocationStartStore::new(context.clone());
    let raw_idempotency_key = "same-browser-key";
    let request_id = RateLimitRequestId::for_anonymous_invocation(
        tenant_id,
        object_id,
        fingerprint,
        raw_idempotency_key,
    )
    .expect("Anonymous idempotency namespace should be valid");
    let payload_hash = Sha256::digest(br#"{"input":{"message":"hello"}}"#).to_vec();
    assert_eq!(
        store
            .inspect_anonymous_invocation_idempotency(
                &anonymous,
                request_id.as_str().to_owned(),
                payload_hash.clone(),
            )
            .await
            .expect("first request preflight should succeed"),
        InvocationIdempotencyPreflight::New
    );

    let command = || AnonymousInvocationStartCommand {
        idempotency_key: request_id.as_str().to_owned(),
        payload_hash: payload_hash.clone(),
        input_message: r#"{"message":"hello"}"#.to_owned(),
        message_expires_at: SystemTime::now() + Duration::from_secs(86_400),
        idempotency_expires_at: SystemTime::now() + Duration::from_secs(86_400),
        trace_id: TraceId::new(),
    };
    let created = store
        .begin_anonymous_invocation(&anonymous, command())
        .await
        .expect("public Anonymous invocation should be created");
    let invocation_id = match created {
        InvocationStartOutcome::Created { invocation_id } => invocation_id,
        InvocationStartOutcome::Replay { .. } => panic!("first invocation cannot be a replay"),
    };
    assert!(matches!(
        store
            .inspect_anonymous_invocation_idempotency(
                &anonymous,
                request_id.as_str().to_owned(),
                payload_hash.clone(),
            )
            .await
            .expect("replay preflight should succeed"),
        InvocationIdempotencyPreflight::Replay { invocation_id: replay_id }
            if replay_id == invocation_id
    ));
    assert!(matches!(
        store
            .begin_anonymous_invocation(&anonymous, command())
            .await
            .expect("replayed invocation should resolve"),
        InvocationStartOutcome::Replay { invocation_id: replay_id }
            if replay_id == invocation_id
    ));

    let other_request_id = RateLimitRequestId::for_anonymous_invocation(
        tenant_id,
        object_id,
        other_fingerprint,
        raw_idempotency_key,
    )
    .expect("other Anonymous idempotency namespace should be valid");
    let other_command = AnonymousInvocationStartCommand {
        idempotency_key: other_request_id.as_str().to_owned(),
        payload_hash,
        input_message: r#"{"message":"hello"}"#.to_owned(),
        message_expires_at: SystemTime::now() + Duration::from_secs(86_400),
        idempotency_expires_at: SystemTime::now() + Duration::from_secs(86_400),
        trace_id: TraceId::new(),
    };
    let other_created = store
        .begin_anonymous_invocation(&other_anonymous, other_command)
        .await
        .expect("same raw key from another subject should create a separate invocation");
    assert!(matches!(
        other_created,
        InvocationStartOutcome::Created { invocation_id: other_id }
            if other_id != invocation_id
    ));

    sqlx::query(
        "UPDATE objects SET anonymous_invocation_enabled = FALSE \
         WHERE tenant_id = $1::uuid AND object_id = $2::uuid",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .execute(&pool)
    .await
    .expect("Object should be able to disable Anonymous invocation");
    let disabled = context.begin_anonymous(&anonymous).await;
    assert!(matches!(
        disabled,
        Err(
            qembed_store::ContextTransactionError::AnonymousCallerResolution(
                qembed_store::caller::AnonymousCallerResolutionError::NotPubliclyInvokable
            )
        )
    ));

    admin_pool.close().await;
    pool.close().await;
}
