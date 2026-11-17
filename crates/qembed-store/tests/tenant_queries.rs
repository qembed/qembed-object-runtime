use std::time::Duration;

use qembed_domain::ids::TenantId;
use qembed_domain::test_support::scope_for_tenant;
use qembed_store::{
    PgPoolConfig, TenantTransaction, begin_tenant_transaction, connect_pool, execute, fetch_all,
};
use sqlx::PgPool;

const RLS_TEST_ROLE: &str = "qembed_tenant_rls_test";

async fn create_rls_fixture(pool: &PgPool, transaction: &mut TenantTransaction) {
    sqlx::query(
        "DO $role$ BEGIN
            IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'qembed_tenant_rls_test') THEN
                CREATE ROLE qembed_tenant_rls_test NOLOGIN NOSUPERUSER NOBYPASSRLS;
            END IF;
        END $role$",
    )
    .execute(transaction.connection())
    .await
    .expect("the test role should exist");

    sqlx::query(
        "CREATE TEMP TABLE tenant_query_probe (
            tenant_id TEXT NOT NULL,
            payload TEXT NOT NULL
        )",
    )
    .execute(transaction.connection())
    .await
    .expect("the temporary fixture table should be created");

    sqlx::query(
        "INSERT INTO tenant_query_probe (tenant_id, payload)
         VALUES ($1, 'tenant-a'), ($2, 'tenant-b')",
    )
    .bind("tenant-a")
    .bind("tenant-b")
    .execute(transaction.connection())
    .await
    .expect("fixture rows should be inserted before RLS is enabled");

    sqlx::query("ALTER TABLE tenant_query_probe ENABLE ROW LEVEL SECURITY")
        .execute(transaction.connection())
        .await
        .expect("RLS should be enabled");
    sqlx::query("ALTER TABLE tenant_query_probe FORCE ROW LEVEL SECURITY")
        .execute(transaction.connection())
        .await
        .expect("RLS should also apply to the table owner");
    sqlx::query(
        "CREATE POLICY tenant_query_isolation ON tenant_query_probe
         USING (tenant_id = current_setting('app.tenant_id', true))
         WITH CHECK (tenant_id = current_setting('app.tenant_id', true))",
    )
    .execute(transaction.connection())
    .await
    .expect("the tenant isolation policy should be created");
    sqlx::query("GRANT SELECT ON tenant_query_probe TO qembed_tenant_rls_test")
        .execute(transaction.connection())
        .await
        .expect("the test role should be allowed to read the fixture");
    let _ = pool;
}

async fn tenant_transaction(
    pool: &PgPool,
    tenant_id: TenantId,
) -> TenantTransaction {
    let scope = scope_for_tenant(tenant_id);
    begin_tenant_transaction(pool, &scope)
        .await
        .expect("trusted scope should begin a tenant transaction")
}

#[tokio::test]
async fn typed_queries_enforce_tenant_isolation_and_fail_closed_without_scope() {
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

    let tenant_a = TenantId::new();
    let mut setup = tenant_transaction(&pool, tenant_a).await;
    create_rls_fixture(&pool, &mut setup).await;
    setup.commit().await.expect("fixture setup should commit");

    let mut transaction_a = tenant_transaction(&pool, tenant_a).await;
    sqlx::query("SET LOCAL ROLE qembed_tenant_rls_test")
        .execute(transaction_a.connection())
        .await
        .expect("tenant A should use the non-bypass RLS role");
    let rows_a: Vec<(String, String)> = fetch_all(
        &mut transaction_a,
        sqlx::query_as(
            "SELECT tenant_id, payload FROM tenant_query_probe ORDER BY tenant_id",
        ),
    )
    .await
    .expect("typed query should return tenant A rows");
    assert_eq!(rows_a, vec![("tenant-a".to_owned(), "tenant-a".to_owned())]);
    transaction_a.commit().await.expect("tenant A should commit");

    let tenant_b = TenantId::new();
    let mut transaction_b = tenant_transaction(&pool, tenant_b).await;
    sqlx::query("SET LOCAL ROLE qembed_tenant_rls_test")
        .execute(transaction_b.connection())
        .await
        .expect("tenant B should use the non-bypass RLS role");
    let rows_b: Vec<(String, String)> = fetch_all(
        &mut transaction_b,
        sqlx::query_as(
            "SELECT tenant_id, payload FROM tenant_query_probe ORDER BY tenant_id",
        ),
    )
    .await
    .expect("typed query should return tenant B rows");
    assert!(rows_b.is_empty(), "tenant B must not see tenant A's rows");
    transaction_b.commit().await.expect("tenant B should commit");

    let mut unscoped = pool
        .begin()
        .await
        .expect("an unscoped transaction should still be possible for the negative test");
    sqlx::query("SET LOCAL ROLE qembed_tenant_rls_test")
        .execute(&mut *unscoped)
        .await
        .expect("the negative test should use the non-bypass RLS role");
    let unscoped_rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT tenant_id, payload FROM tenant_query_probe ORDER BY tenant_id",
    )
    .fetch_all(&mut *unscoped)
    .await
    .expect("RLS should return an empty result rather than an error without tenant context");
    assert!(
        unscoped_rows.is_empty(),
        "an operation without tenant context must not see tenant-owned rows"
    );
    unscoped
        .rollback()
        .await
        .expect("the unscoped negative-test transaction should roll back");

    pool.close().await;
}
