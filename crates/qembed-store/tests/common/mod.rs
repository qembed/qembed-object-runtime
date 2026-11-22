use qembed_store::{
    ContextIssuer, ContextIssuerConfig, PgContext, PgPoolConfig, RingEd25519SigningKey,
    connect_pool,
};
use sqlx::PgPool;
use uuid::Uuid;

pub async fn connect() -> PgPool {
    connect_with_max_connections(PgPoolConfig::default().max_connections).await
}

pub async fn connect_with_max_connections(max_connections: u32) -> PgPool {
    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set by scripts/test-postgres.sh");

    connect_pool(
        &database_url,
        PgPoolConfig {
            max_connections,
            ..PgPoolConfig::default()
        },
    )
    .await
    .expect("the PostgreSQL test database should accept connections")
}

/// Creates a test-only signer and registers its public key through the dedicated
/// PostgreSQL key-control-plane identity. Each test gets a unique key ID so
/// parallel integration tests cannot interfere with key lifecycle state.
#[allow(dead_code)]
pub async fn signed_context(pool: &PgPool) -> PgContext {
    let deployment_id = "rt050-integration-test";
    let database_oid: String =
        sqlx::query_scalar("SELECT oid::text FROM pg_database WHERE datname = current_database()")
            .fetch_one(pool)
            .await
            .expect("database OID should be readable");
    let audience =
        format!("urn:qembed:postgres:deployment:{deployment_id}:database:{database_oid}");
    let issuer_name = "qembed-runtime";
    let key_id = format!("rt050-test-{}", Uuid::new_v4());
    let signing_key = RingEd25519SigningKey::from_seed(key_id.clone(), &[42_u8; 32])
        .expect("the fixed test seed should create a valid Ed25519 signer");
    let public_key = signing_key.public_key();

    let mut control_plane = pool
        .acquire()
        .await
        .expect("key control-plane connection should be acquired");
    sqlx::query("SET SESSION AUTHORIZATION qembed_key_control_plane")
        .execute(&mut *control_plane)
        .await
        .expect("test harness should assume the dedicated key-control-plane identity");

    sqlx::query("SELECT qembed_crypto.stage_verification_key($1, $2, $3, $4, $5, $6, $7, $8)")
        .bind(&key_id)
        .bind(public_key.to_vec())
        .bind(issuer_name)
        .bind(&audience)
        .bind(deployment_id)
        .bind("operator:rt050-integration-test")
        .bind(Uuid::new_v4())
        .bind("register integration-test signer")
        .execute(&mut *control_plane)
        .await
        .expect("control plane should stage the public verification key");

    let _: String = sqlx::query_scalar(
        "SELECT qembed_crypto.activate_verification_key($1, $2, $3, $4, $5)::text",
    )
    .bind(&key_id)
    .bind("operator:rt050-integration-test")
    .bind("approver:rt050-integration-test")
    .bind(Uuid::new_v4())
    .bind("activate integration-test signer")
    .fetch_one(&mut *control_plane)
    .await
    .expect("a distinct approver should activate the integration-test key");

    sqlx::query("RESET SESSION AUTHORIZATION")
        .execute(&mut *control_plane)
        .await
        .expect("control-plane identity must be reset before pool reuse");
    drop(control_plane);

    let config =
        ContextIssuerConfig::with_default_lifetime(issuer_name, audience, deployment_id, key_id)
            .expect("test issuer configuration should be valid");
    let issuer = ContextIssuer::new(config, signing_key)
        .expect("test issuer configuration and signing key should match");
    PgContext::new(pool.clone(), issuer)
}
