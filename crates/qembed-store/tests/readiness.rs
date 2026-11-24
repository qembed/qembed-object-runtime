mod common;

use qembed_store::{
    PgReadinessFailure, PgReadinessProbe,
    external_deletion_ledger::{
        CallerDeletionKey, ExternalCallerDeletionLedger, PgCallerDeletionRestore,
        run_control_plane_migrations,
    },
    run_migrations,
};

async fn prepare_replay(
    runtime_pool: &sqlx::PgPool,
    ledger_pool: &sqlx::PgPool,
) -> (
    ExternalCallerDeletionLedger,
    qembed_store::external_deletion_ledger::CallerDeletionReplayComplete,
) {
    run_migrations(runtime_pool)
        .await
        .expect("Runtime migrations should be applicable");
    run_control_plane_migrations(ledger_pool)
        .await
        .expect("Control-plane migrations should be applicable");
    let key = CallerDeletionKey::new(&[0x21; 32]).expect("32-byte HMAC key");
    let ledger = ExternalCallerDeletionLedger::new(ledger_pool.clone(), key);
    let replay = PgCallerDeletionRestore::new(runtime_pool.clone(), ledger.clone())
        .run()
        .await
        .expect("empty external ledger should produce a completed replay capability");
    (ledger, replay)
}

#[tokio::test]
async fn reports_ready_only_when_both_databases_and_restore_replay_are_ready() {
    let runtime_pool = common::connect().await;
    let control_plane_pool = common::connect_control_plane().await;
    let (ledger, replay) = prepare_replay(&runtime_pool, &control_plane_pool).await;

    let probe = PgReadinessProbe::new(runtime_pool.clone(), ledger, replay)
        .expect("readiness must use the same ledger instance as replay");
    assert_eq!(probe.check().await, Ok(()));
    runtime_pool.close().await;
    control_plane_pool.close().await;
}

#[tokio::test]
async fn reports_runtime_database_unavailable_when_runtime_pool_is_closed() {
    let runtime_pool = common::connect().await;
    let control_plane_pool = common::connect_control_plane().await;
    let (ledger, replay) = prepare_replay(&runtime_pool, &control_plane_pool).await;
    runtime_pool.close().await;

    let probe = PgReadinessProbe::new(runtime_pool, ledger, replay)
        .expect("readiness must use the same ledger instance as replay");
    assert_eq!(
        probe.check().await,
        Err(PgReadinessFailure::DatabaseUnavailable)
    );
    drop(probe);
    control_plane_pool.close().await;
}

#[tokio::test]
async fn startup_restore_fails_closed_when_control_plane_is_unavailable() {
    let runtime_pool = common::connect().await;
    run_migrations(&runtime_pool)
        .await
        .expect("Runtime migrations should be applicable");
    let control_plane_pool = common::connect_control_plane().await;
    run_control_plane_migrations(&control_plane_pool)
        .await
        .expect("Control-plane migrations should be applicable");
    control_plane_pool.close().await;

    let key = CallerDeletionKey::new(&[0x31; 32]).expect("32-byte HMAC key");
    let ledger = ExternalCallerDeletionLedger::new(control_plane_pool, key);
    let replay = PgCallerDeletionRestore::new(runtime_pool.clone(), ledger);
    assert!(replay.run().await.is_err());
    runtime_pool.close().await;
}
