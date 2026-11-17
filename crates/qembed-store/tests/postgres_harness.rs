mod common;

use qembed_store::run_migrations;

#[tokio::test]
async fn postgres_harness_runs_migrations_and_connects_to_supported_server() {
    let pool = common::connect().await;

    // The test runner creates a fresh database. Applying migrations here proves
    // that the explicit migration path works before application queries run.
    run_migrations(&pool)
        .await
        .expect("migrations should run against an empty test database");

    let ready: i32 = sqlx::query_scalar("SELECT 1")
        .fetch_one(&pool)
        .await
        .expect("the test database should execute a simple query");
    assert_eq!(ready, 1);

    let version: String = sqlx::query_scalar("SHOW server_version_num")
        .fetch_one(&pool)
        .await
        .expect("the PostgreSQL server version should be readable");
    let version: i32 = version
        .parse()
        .expect("server_version_num should be an integer");

    assert!(
        (160_000..170_000).contains(&version),
        "expected PostgreSQL 16.x, got server_version_num={version}"
    );

    let migration_table_exists: bool = sqlx::query_scalar(
        "SELECT to_regclass('public._sqlx_migrations') IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .expect("the SQLx migration history table should be queryable");
    assert!(migration_table_exists);

    pool.close().await;
}
