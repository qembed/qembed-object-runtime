mod common;

use std::time::Duration;

use qembed_domain::ids::{ObjectId, TenantId};
use qembed_domain::object::Object;
use qembed_domain::test_support::scope_for_tenant;
use qembed_store::object_repository::ObjectRepository;
use qembed_store::{
    CasError, PgPoolConfig, connect_pool, execute_cas, run_migrations,
};
use sqlx::PgPool;

async fn single_connection_pool() -> PgPool {
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
    .expect("independent PostgreSQL pool should connect")
}

#[tokio::test]
async fn stale_compare_and_set_conflicts_across_independent_sessions() {
    let first_pool = single_connection_pool().await;
    let second_pool = single_connection_pool().await;
    run_migrations(&first_pool)
        .await
        .expect("migrations should be applied");

    let tenant_id = TenantId::new();
    let scope = scope_for_tenant(tenant_id);
    let object_id = ObjectId::new();
    let first_context = common::signed_context(&first_pool).await;
    let second_context = common::signed_context(&second_pool).await;
    ObjectRepository::new(first_context.clone())
        .create_object(&scope, &Object::new(object_id, tenant_id))
        .await
        .expect("object should be created");

    // Both transactions hold different physical PostgreSQL sessions. The
    // second statement is issued after the first commits, so its expected
    // Draft state is stale even though its transaction began earlier.
    let mut first = first_context.begin_principal(&scope)
        .await
        .expect("first tenant transaction should start");
    let mut second = second_context.begin_principal(&scope)
        .await
        .expect("second tenant transaction should start");

    let first_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(first.connection())
        .await
        .expect("first backend PID should be available");
    let second_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(second.connection())
        .await
        .expect("second backend PID should be available");
    assert_ne!(
        first_pid, second_pid,
        "CAS test must use independent sessions"
    );

    let guarded_update = || {
        sqlx::query(
            "UPDATE objects SET state = 'validating' WHERE tenant_id = $1::uuid AND object_id = $2::uuid AND state = 'draft' AND published_version_id IS NULL",
        )
        .bind(tenant_id.to_string())
        .bind(object_id.to_string())
    };

    execute_cas(&mut first, guarded_update())
        .await
        .expect("the first CAS should change exactly one row");
    first
        .commit()
        .await
        .expect("first CAS transaction should commit");

    assert!(matches!(
        execute_cas(&mut second, guarded_update()).await,
        Err(CasError::Conflict)
    ));
    second
        .rollback()
        .await
        .expect("stale CAS transaction should roll back");

    first_pool.close().await;
    second_pool.close().await;
}

#[tokio::test]
async fn compare_and_set_rejects_updates_that_match_multiple_rows() {
    let pool = common::connect().await;
    run_migrations(&pool)
        .await
        .expect("migrations should be applied");

    let tenant_id = TenantId::new();
    let scope = scope_for_tenant(tenant_id);
    let first_id = ObjectId::new();
    let second_id = ObjectId::new();
    let context = common::signed_context(&pool).await;
    let repository = ObjectRepository::new(context.clone());

    repository
        .create_object(&scope, &Object::new(first_id, tenant_id))
        .await
        .expect("first object should be created");
    repository
        .create_object(&scope, &Object::new(second_id, tenant_id))
        .await
        .expect("second object should be created");

    let mut transaction = context.begin_principal(&scope)
        .await
        .expect("tenant transaction should start");
    let error = execute_cas(
        &mut transaction,
        sqlx::query(
            "UPDATE objects SET state = 'validating' WHERE tenant_id = $1::uuid AND state = 'draft' AND published_version_id IS NULL",
        )
        .bind(tenant_id.to_string()),
    )
    .await
    .expect_err("a CAS matching multiple rows must not be treated as success");

    assert!(matches!(error, CasError::UnexpectedRowsAffected(2)));
    transaction
        .rollback()
        .await
        .expect("invalid multi-row CAS should be rolled back");

    pool.close().await;
}
