use std::time::Duration;

use qembed_domain::ids::TenantId;
use qembed_domain::test_support::scope_for_tenant;
use qembed_store::{PgPoolConfig, begin_tenant_transaction, connect_pool};
use sqlx::PgPool;

async fn assert_tenant_setting(pool: &PgPool, expected: Option<&str>) {
    let actual: Option<String> =
        sqlx::query_scalar("SELECT NULLIF(current_setting('app.tenant_id', true), '')")
            .fetch_one(pool)
            .await
            .expect("tenant setting should be readable outside a transaction");

    assert_eq!(actual.as_deref(), expected);
}

async fn assert_principal_setting(pool: &PgPool, expected: Option<&str>) {
    let actual: Option<String> = sqlx::query_scalar(
        "SELECT NULLIF(current_setting('app.principal_id', true), '')",
    )
    .fetch_one(pool)
    .await
    .expect("principal setting should be readable outside a transaction");
    assert_eq!(actual.as_deref(), expected);
}

#[tokio::test]
async fn tenant_setting_is_transaction_local_after_commit_and_rollback() {
    // A single connection makes the post-transaction assertions prove that the
    // same pooled session did not retain the previous tenant context.
    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set by scripts/test-postgres.sh");
    let pool = connect_pool(
        &database_url,
        PgPoolConfig {
            max_connections: 1,
            acquire_timeout: Duration::from_secs(5),
        },
    )
    .await
    .expect("the PostgreSQL test database should accept connections");

    let tenant_id = TenantId::new();
    let scope = scope_for_tenant(tenant_id);

    let mut transaction = begin_tenant_transaction(&pool, &scope)
        .await
        .expect("a trusted scope should begin a tenant transaction");
    let inside: String = sqlx::query_scalar("SELECT current_setting('app.tenant_id', true)")
        .fetch_one(transaction.connection())
        .await
        .expect("tenant setting should be present inside the transaction");
    assert_eq!(inside, tenant_id.to_string());
    let principal_inside: String = sqlx::query_scalar("SELECT current_setting('app.principal_id', true)")
        .fetch_one(transaction.connection()).await.expect("Principal context should be present");
    assert_eq!(principal_inside, scope.principal().principal_id().to_string());

    transaction
        .commit()
        .await
        .expect("tenant transaction should commit");
    assert_tenant_setting(&pool, None).await;
    assert_principal_setting(&pool, None).await;

    let mut transaction = begin_tenant_transaction(&pool, &scope)
        .await
        .expect("a trusted scope should begin another tenant transaction");
    let inside: String = sqlx::query_scalar("SELECT current_setting('app.tenant_id', true)")
        .fetch_one(transaction.connection())
        .await
        .expect("tenant setting should be present inside the transaction");
    assert_eq!(inside, tenant_id.to_string());
    let principal_inside: String = sqlx::query_scalar("SELECT current_setting('app.principal_id', true)")
        .fetch_one(transaction.connection()).await.expect("Principal context should be present");
    assert_eq!(principal_inside, scope.principal().principal_id().to_string());

    transaction
        .rollback()
        .await
        .expect("tenant transaction should roll back");
    assert_tenant_setting(&pool, None).await;
    assert_principal_setting(&pool, None).await;

    pool.close().await;
}
