mod common;

use qembed_store::{PgReadinessFailure, PgReadinessProbe, run_migrations};

#[tokio::test]
async fn reports_ready_when_postgres_and_migration_history_match() {
    let pool = common::connect().await;
    run_migrations(&pool)
        .await
        .expect("test database migrations should be applicable");

    let probe = PgReadinessProbe::new(pool.clone());
    assert_eq!(probe.check().await, Ok(()));
    pool.close().await;
}

#[tokio::test]
async fn reports_database_unavailable_when_pool_is_closed() {
    let pool = common::connect().await;
    pool.close().await;

    let probe = PgReadinessProbe::new(pool);
    assert_eq!(
        probe.check().await,
        Err(PgReadinessFailure::DatabaseUnavailable)
    );
}
