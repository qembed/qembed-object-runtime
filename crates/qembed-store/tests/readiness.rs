mod common;

use qembed_store::{
    PgReadinessFailure, PgReadinessProbe, external_deletion_ledger::run_control_plane_migrations,
    run_migrations,
};

#[tokio::test]
async fn reports_ready_only_when_both_postgres_clusters_and_migrations_match() {
    let runtime_pool = common::connect().await;
    let control_plane_pool = common::connect_control_plane().await;
    run_migrations(&runtime_pool)
        .await
        .expect("Runtime migrations should be applicable");
    run_control_plane_migrations(&control_plane_pool)
        .await
        .expect("Control-plane migrations should be applicable");

    let probe = PgReadinessProbe::new(runtime_pool.clone(), control_plane_pool.clone());
    assert_eq!(probe.check().await, Ok(()));
    runtime_pool.close().await;
    control_plane_pool.close().await;
}

#[tokio::test]
async fn reports_runtime_database_unavailable_when_runtime_pool_is_closed() {
    let runtime_pool = common::connect().await;
    let control_plane_pool = common::connect_control_plane().await;
    run_migrations(&runtime_pool)
        .await
        .expect("Runtime migrations should be applicable");
    run_control_plane_migrations(&control_plane_pool)
        .await
        .expect("Control-plane migrations should be applicable");
    runtime_pool.close().await;

    let probe = PgReadinessProbe::new(runtime_pool, control_plane_pool);
    assert_eq!(
        probe.check().await,
        Err(PgReadinessFailure::DatabaseUnavailable)
    );
}

#[tokio::test]
async fn reports_control_plane_unavailable_when_external_ledger_pool_is_closed() {
    let runtime_pool = common::connect().await;
    let control_plane_pool = common::connect_control_plane().await;
    run_migrations(&runtime_pool)
        .await
        .expect("Runtime migrations should be applicable");
    run_control_plane_migrations(&control_plane_pool)
        .await
        .expect("Control-plane migrations should be applicable");
    control_plane_pool.close().await;

    let probe = PgReadinessProbe::new(runtime_pool, control_plane_pool);
    assert_eq!(
        probe.check().await,
        Err(PgReadinessFailure::ControlPlaneUnavailable)
    );
}
