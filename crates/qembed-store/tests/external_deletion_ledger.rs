mod common;

use qembed_domain::ids::{CallerId, TenantId};
use qembed_store::external_deletion_ledger::{
    CallerDeletionKey, ExternalCallerDeletionLedger, run_control_plane_migrations,
};
use sqlx::Row;

#[tokio::test]
async fn external_ledger_is_idempotent_and_returns_a_stable_snapshot() {
    let pool = common::connect_control_plane().await;
    run_control_plane_migrations(&pool)
        .await
        .expect("Control-plane migrations should apply independently");

    let key = CallerDeletionKey::new(&[0x71; 32]).expect("32-byte HMAC key");
    let ledger = ExternalCallerDeletionLedger::new(pool.clone(), key);
    let tenant_id = TenantId::new();
    let caller_id = CallerId::new();

    let first = ledger
        .record(tenant_id, caller_id)
        .await
        .expect("first tombstone should commit");
    let repeated = ledger
        .record(tenant_id, caller_id)
        .await
        .expect("repeated tombstone should be idempotent");

    assert!(first.inserted);
    assert!(!repeated.inserted);
    assert_eq!(first.sequence_id, repeated.sequence_id);

    let snapshot = ledger
        .snapshot()
        .await
        .expect("the immutable ledger snapshot should be readable");
    assert!(snapshot.high_water_mark >= first.sequence_id);
    let matches = snapshot
        .tombstones
        .iter()
        .filter(|row| row.tenant_id == tenant_id)
        .collect::<Vec<_>>();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].sequence_id, first.sequence_id);
    assert_eq!(matches[0].key_version, "v1");
    assert_eq!(matches[0].caller_hash.len(), 32);

    assert_eq!(
        ledger
            .high_water_mark()
            .await
            .expect("watermark should be readable"),
        snapshot.high_water_mark
    );
    pool.close().await;
}

#[tokio::test]
async fn control_plane_runtime_role_can_read_and_append_but_cannot_mutate_or_truncate() {
    let pool = common::connect_control_plane().await;
    run_control_plane_migrations(&pool)
        .await
        .expect("Control-plane migrations should apply independently");

    let tenant_id = TenantId::new();
    let caller_id = CallerId::new();
    let key = CallerDeletionKey::new(&[0x39; 32]).expect("32-byte HMAC key");
    let hash = key.caller_hash(tenant_id, caller_id);

    let mut connection = pool.acquire().await.expect("connection should be acquired");
    sqlx::query("SET ROLE qembed_control_plane_runtime")
        .execute(&mut *connection)
        .await
        .expect("test connection should assume the application role");

    sqlx::query(
        "INSERT INTO qembed_control_plane.caller_deletion_tombstones \
         (tenant_id, caller_hash, key_version) VALUES ($1, $2, 'v1')",
    )
    .bind(tenant_id.into_uuid())
    .bind(hash.as_slice())
    .execute(&mut *connection)
    .await
    .expect("application role should append tombstones");

    let count: i64 = sqlx::query(
        "SELECT count(*) AS count FROM qembed_control_plane.caller_deletion_tombstones \
         WHERE tenant_id = $1 AND caller_hash = $2",
    )
    .bind(tenant_id.into_uuid())
    .bind(hash.as_slice())
    .fetch_one(&mut *connection)
    .await
    .expect("application role should read tombstones")
    .get("count");
    assert_eq!(count, 1);

    let update = sqlx::query(
        "UPDATE qembed_control_plane.caller_deletion_tombstones SET deleted_at = now() \
         WHERE tenant_id = $1 AND caller_hash = $2",
    )
    .bind(tenant_id.into_uuid())
    .bind(hash.as_slice())
    .execute(&mut *connection)
    .await;
    assert!(
        update.is_err(),
        "application role must not update tombstones"
    );

    let delete = sqlx::query(
        "DELETE FROM qembed_control_plane.caller_deletion_tombstones \
         WHERE tenant_id = $1 AND caller_hash = $2",
    )
    .bind(tenant_id.into_uuid())
    .bind(hash.as_slice())
    .execute(&mut *connection)
    .await;
    assert!(
        delete.is_err(),
        "application role must not delete tombstones"
    );

    let truncate = sqlx::query("TRUNCATE qembed_control_plane.caller_deletion_tombstones")
        .execute(&mut *connection)
        .await;
    assert!(
        truncate.is_err(),
        "application role must not truncate tombstones"
    );

    sqlx::query("RESET ROLE")
        .execute(&mut *connection)
        .await
        .expect("test role must be reset before pooled connection reuse");
    drop(connection);
    pool.close().await;
}
