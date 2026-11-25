mod common;

use qembed_domain::{
    ids::{CallerId, ObjectId, TenantId},
    test_support::scope_for_tenant,
};
use qembed_store::{
    ContextIssuer, ContextIssuerConfig, PgContext, RingEd25519SigningKey,
    episodic_memory::{EpisodicMemoryError, PgEpisodicMemoryRepository},
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

async fn runtime_pool(database_url: &str) -> PgPool {
    PgPoolOptions::new()
        .max_connections(2)
        .after_connect(|connection, _metadata| {
            Box::pin(async move {
                sqlx::query("SET ROLE qembed_runtime")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect(database_url)
        .await
        .expect("restricted Runtime pool should connect as qembed_runtime")
}

async fn stage_issuer(admin_pool: &PgPool) -> ContextIssuer {
    let deployment_id = "rust-074-caller-isolation";
    let database_oid: String =
        sqlx::query_scalar("SELECT oid::text FROM pg_database WHERE datname = current_database()")
            .fetch_one(admin_pool)
            .await
            .expect("database OID should be readable");
    let issuer_name = "qembed-runtime";
    let audience =
        format!("urn:qembed:postgres:deployment:{deployment_id}:database:{database_oid}");
    let key_id = format!("rust074-test-{}", Uuid::new_v4());
    let signing_key = RingEd25519SigningKey::from_seed(key_id.clone(), &[0x74_u8; 32])
        .expect("RUST-074 test seed should create a valid Ed25519 signer");
    let public_key = signing_key.public_key();

    let mut control_plane = admin_pool
        .acquire()
        .await
        .expect("key control-plane connection should be acquired");
    sqlx::query("SET SESSION AUTHORIZATION qembed_key_control_plane")
        .execute(&mut *control_plane)
        .await
        .expect("test harness should assume the key control-plane identity");
    sqlx::query("SELECT qembed_crypto.stage_verification_key($1, $2, $3, $4, $5, $6, $7, $8)")
        .bind(&key_id)
        .bind(public_key.to_vec())
        .bind(issuer_name)
        .bind(&audience)
        .bind(deployment_id)
        .bind("operator:rust-074-test")
        .bind(Uuid::new_v4())
        .bind("stage RUST-074 Caller isolation test key")
        .execute(&mut *control_plane)
        .await
        .expect("control plane should stage the public verification key");
    let _: String = sqlx::query_scalar(
        "SELECT qembed_crypto.activate_verification_key($1, $2, $3, $4, $5)::text",
    )
    .bind(&key_id)
    .bind("operator:rust-074-test")
    .bind("approver:rust-074-test")
    .bind(Uuid::new_v4())
    .bind("activate RUST-074 Caller isolation test key")
    .fetch_one(&mut *control_plane)
    .await
    .expect("a distinct approver should activate the test key");
    sqlx::query("RESET SESSION AUTHORIZATION")
        .execute(&mut *control_plane)
        .await
        .expect("control-plane authorization should be reset before pool reuse");
    drop(control_plane);

    let config =
        ContextIssuerConfig::with_default_lifetime(issuer_name, audience, deployment_id, key_id)
            .expect("RUST-074 ContextIssuer configuration should be valid");
    ContextIssuer::new(config, signing_key).expect("RUST-074 ContextIssuer should be valid")
}

async fn seed_caller(
    pool: &PgPool,
    tenant_id: TenantId,
    principal_id: Uuid,
    caller_id: CallerId,
    token_byte: u8,
    delegated: bool,
) {
    sqlx::query(
        "INSERT INTO callers (id, tenant_id, token_hash, caller_kind) \
         VALUES ($1::uuid, $2::uuid, $3, 'Authenticated')",
    )
    .bind(caller_id.to_string())
    .bind(tenant_id.to_string())
    .bind(vec![token_byte; 32])
    .execute(pool)
    .await
    .expect("Caller fixture inserted");

    if delegated {
        sqlx::query(
            "INSERT INTO principal_caller_delegations \
             (delegation_id, tenant_id, principal_id, caller_id) \
             VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid)",
        )
        .bind(Uuid::new_v4())
        .bind(tenant_id.to_string())
        .bind(principal_id.to_string())
        .bind(caller_id.to_string())
        .execute(pool)
        .await
        .expect("direct delegation fixture inserted");
    }
}

#[tokio::test]
#[ignore = "requires canonical qembed-sql RT-050 signed-context migration; run from scripts/test-postgres.sh"]
async fn caller_scoped_application_path_cannot_read_another_callers_episodic_memory() {
    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set by scripts/test-postgres.sh");
    let admin_pool = common::connect().await;
    let tenant_id = TenantId::new();
    let scope = scope_for_tenant(tenant_id);
    let principal_id = scope.principal().principal_id().into_uuid();
    let caller_a = CallerId::new();
    let caller_b = CallerId::new();
    let caller_without_delegation = CallerId::new();
    let object_id = ObjectId::new();

    sqlx::query("INSERT INTO tenants (id) VALUES ($1::uuid)")
        .bind(tenant_id.to_string())
        .execute(&admin_pool)
        .await
        .expect("tenant fixture inserted");
    sqlx::query(
        "INSERT INTO owner_principals (tenant_id, caller_id, principal_kind) \
         VALUES ($1::uuid, $2::uuid, 'tenant_owner')",
    )
    .bind(tenant_id.to_string())
    .bind(principal_id.to_string())
    .execute(&admin_pool)
    .await
    .expect("Owner Principal fixture inserted");

    seed_caller(&admin_pool, tenant_id, principal_id, caller_a, 11, true).await;
    seed_caller(&admin_pool, tenant_id, principal_id, caller_b, 22, true).await;
    seed_caller(
        &admin_pool,
        tenant_id,
        principal_id,
        caller_without_delegation,
        33,
        false,
    )
    .await;

    sqlx::query(
        "INSERT INTO objects (tenant_id, object_id, state) \
         VALUES ($1::uuid, $2::uuid, 'draft')",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .execute(&admin_pool)
    .await
    .expect("Object fixture inserted");

    let caller_a_memory_id = Uuid::new_v4();
    let caller_b_memory_id = Uuid::new_v4();
    for (memory_id, caller_id, claim) in [
        (caller_a_memory_id, caller_a, "Caller A private memory"),
        (caller_b_memory_id, caller_b, "Caller B secret"),
    ] {
        sqlx::query(
            "INSERT INTO memories \
             (id, tenant_id, object_id, caller_id, memory_kind, claim, source_kind, verification, confidence, expires_at) \
             VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, 'episodic', $5, 'User', 'Unverified', 0.5, clock_timestamp() + interval '1 day')",
        )
        .bind(memory_id)
        .bind(tenant_id.to_string())
        .bind(object_id.to_string())
        .bind(caller_id.to_string())
        .bind(claim)
        .execute(&admin_pool)
        .await
        .expect("episodic memory fixture inserted");
    }

    let issuer = stage_issuer(&admin_pool).await;
    let runtime_pool = runtime_pool(&database_url).await;
    let context = PgContext::new(runtime_pool.clone(), issuer);
    let repository = PgEpisodicMemoryRepository::new(context.clone());

    // Prove signed-context RLS independently of the repository's explicit
    // caller_id predicate: the qembed_runtime role sees only Caller A's row.
    let (mut transaction, resolved) = context
        .begin_caller(&scope, caller_a)
        .await
        .expect("Caller A delegation should resolve");
    assert_eq!(resolved.caller_id(), caller_a);
    let rls_visible_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memories \
          WHERE tenant_id = $1::uuid AND object_id = $2::uuid AND memory_kind = 'episodic'",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .fetch_one(transaction.connection())
    .await
    .expect("signed Caller context should apply the canonical RLS policy");
    assert_eq!(rls_visible_count, 1);
    transaction
        .commit()
        .await
        .expect("Caller A context transaction should commit");

    let visible_to_a = repository
        .list_episodic(&scope, caller_a, object_id)
        .await
        .expect("Caller A can list its own memories");
    assert_eq!(visible_to_a.len(), 1);
    assert_eq!(visible_to_a[0].claim(), "Caller A private memory");
    assert_eq!(visible_to_a[0].source_kind(), "User");
    assert_eq!(visible_to_a[0].verification(), "Unverified");
    assert_eq!(visible_to_a[0].id(), caller_a_memory_id);
    assert_ne!(visible_to_a[0].id(), caller_b_memory_id);

    let visible_to_b = repository
        .list_episodic(&scope, caller_b, object_id)
        .await
        .expect("Caller B can list its own memories");
    assert_eq!(visible_to_b.len(), 1);
    assert_eq!(visible_to_b[0].claim(), "Caller B secret");
    assert_eq!(visible_to_b[0].id(), caller_b_memory_id);
    assert_ne!(visible_to_b[0].id(), caller_a_memory_id);

    assert!(matches!(
        repository
            .list_episodic(&scope, caller_without_delegation, object_id)
            .await,
        Err(EpisodicMemoryError::Context(_))
    ));

    runtime_pool.close().await;
    admin_pool.close().await;
}
