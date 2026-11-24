mod common;

use qembed_store::{
    PgReadinessFailure, PgReadinessProbe,
    external_deletion_ledger::{
        CallerDeletionKey, ExternalCallerDeletionLedger, PgCallerDeletionRestore,
        run_control_plane_migrations,
    },
    run_migrations,
};

async fn install_replay_stub(pool: &sqlx::PgPool) {
    sqlx::query(
        "CREATE OR REPLACE FUNCTION qembed_crypto.replay_caller_deletion(uuid, bytea, text, bytea) \
         RETURNS boolean LANGUAGE sql AS 'SELECT TRUE'",
    )
    .execute(pool)
    .await
    .expect("replay function stub should be installed");
    sqlx::query(
        "GRANT EXECUTE ON FUNCTION qembed_crypto.replay_caller_deletion(uuid, bytea, text, bytea) TO qembed_restore",
    )
    .execute(pool)
    .await
    .expect("restore role should receive replay execution");
}

async fn install_runtime_erasure_stubs(pool: &sqlx::PgPool) {
    for statement in [
        "CREATE OR REPLACE FUNCTION qembed_crypto.request_caller_deletion(uuid, uuid) RETURNS boolean LANGUAGE sql AS 'SELECT TRUE'",
        "CREATE OR REPLACE FUNCTION qembed_crypto.erase_caller_data(uuid, uuid) RETURNS boolean LANGUAGE sql AS 'SELECT TRUE'",
        "CREATE OR REPLACE FUNCTION qembed_crypto.erase_caller_data_verified(uuid, uuid) RETURNS boolean LANGUAGE sql AS 'SELECT TRUE'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("Runtime erasure function stub should be installed");
    }
    sqlx::query(
        "GRANT EXECUTE ON FUNCTION qembed_crypto.request_caller_deletion(uuid, uuid) TO qembed_runtime",
    )
    .execute(pool)
    .await
    .expect("Runtime role should receive deletion request execution");
    sqlx::query(
        "GRANT EXECUTE ON FUNCTION qembed_crypto.erase_caller_data(uuid, uuid) TO qembed_runtime",
    )
    .execute(pool)
    .await
    .expect("Runtime role should receive erasure execution");
}

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
    install_replay_stub(runtime_pool).await;
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
    let other_ledger = ExternalCallerDeletionLedger::new(
        control_plane_pool.clone(),
        CallerDeletionKey::new(&[0x22; 32]).expect("32-byte HMAC key"),
    );
    assert!(matches!(
        PgReadinessProbe::new(runtime_pool.clone(), other_ledger, replay.clone()),
        Err(PgReadinessFailure::ReplayLedgerMismatch)
    ));

    let probe = PgReadinessProbe::new(runtime_pool.clone(), ledger, replay)
        .expect("readiness must use the same ledger instance as replay");
    assert_eq!(
        probe.check().await,
        Err(PgReadinessFailure::RuntimeDeletionSchemaMismatch)
    );
    install_runtime_erasure_stubs(&runtime_pool).await;
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
    install_replay_stub(&runtime_pool).await;
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
