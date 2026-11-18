mod common;

use qembed_domain::ids::{ObjectId, ObjectVersionId, SnapshotId, TenantId};
use qembed_domain::object::Object;
use qembed_domain::object_version::ObjectVersion;
use qembed_domain::scope::Scope;
use qembed_domain::snapshot::SnapshotBuilder;
use qembed_domain::test_support::scope_for_tenant;
use qembed_store::object_repository::ObjectRepository;
use qembed_store::snapshot_repository::SnapshotRepository;
use qembed_store::{begin_tenant_transaction, run_migrations};
use sqlx::PgPool;

struct Fixture {
    pool: PgPool,
    scope: Scope,
    object_id: ObjectId,
    snapshot_id: SnapshotId,
}

async fn setup() -> Fixture {
    let pool = common::connect().await;
    run_migrations(&pool)
        .await
        .expect("migrations should apply");

    sqlx::query(
        "DO $role$ BEGIN
            IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'qembed_runtime_immutable_test') THEN
                CREATE ROLE qembed_runtime_immutable_test
                    NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT
                    NOREPLICATION NOBYPASSRLS;
            END IF;
        END $role$",
    )
    .execute(&pool)
    .await
    .expect("restricted runtime test role should exist");

    sqlx::query(
        "ALTER ROLE qembed_runtime_immutable_test
            NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT
            NOREPLICATION NOBYPASSRLS",
    )
    .execute(&pool)
    .await
    .expect("runtime test role must not have elevated attributes");

    sqlx::query("GRANT USAGE ON SCHEMA public TO qembed_runtime_immutable_test")
        .execute(&pool)
        .await
        .expect("runtime role should resolve application tables");
    sqlx::query(
        "REVOKE ALL PRIVILEGES ON TABLE snapshots, events FROM qembed_runtime_immutable_test",
    )
    .execute(&pool)
    .await
    .expect("previous test grants should be reset");
    sqlx::query("GRANT SELECT, INSERT ON TABLE snapshots, events TO qembed_runtime_immutable_test")
        .execute(&pool)
        .await
        .expect("runtime role should receive only append/read table privileges");

    let tenant_id = TenantId::new();
    let scope = scope_for_tenant(tenant_id);
    let object_id = ObjectId::new();
    let version_id = ObjectVersionId::new();
    let objects = ObjectRepository::new(pool.clone());

    objects
        .create_object(&scope, &Object::new(object_id, tenant_id))
        .await
        .expect("object should be created");
    objects
        .create_draft_version(
            &scope,
            &ObjectVersion::new(version_id, tenant_id, object_id, 1)
                .expect("version number should be valid"),
        )
        .await
        .expect("draft version should be created");

    let snapshot = SnapshotBuilder::new(
        SnapshotId::new(),
        tenant_id,
        object_id,
        version_id,
        1,
        br#"{"immutable":true}"#.to_vec(),
    )
    .build()
    .expect("snapshot should be valid");
    let snapshot_id = snapshot.id();
    SnapshotRepository::new(pool.clone())
        .insert(&scope, &snapshot)
        .await
        .expect("snapshot should be persisted");

    objects
        .begin_validation(&scope, object_id)
        .await
        .expect("validation should append an event");

    Fixture {
        pool,
        scope,
        object_id,
        snapshot_id,
    }
}

async fn runtime_transaction(pool: &PgPool, scope: &Scope) -> qembed_store::TenantTransaction {
    let mut transaction = begin_tenant_transaction(pool, scope)
        .await
        .expect("tenant transaction should begin");
    sqlx::query("SET LOCAL ROLE qembed_runtime_immutable_test")
        .execute(transaction.connection())
        .await
        .expect("transaction should assume the restricted runtime role");
    transaction
}

fn assert_insufficient_privilege(error: sqlx::Error) {
    assert_eq!(
        error
            .as_database_error()
            .and_then(|database| database.code())
            .as_deref(),
        Some("42501"),
        "runtime mutation must be rejected by PostgreSQL privileges"
    );
}

#[tokio::test]
async fn runtime_role_can_read_immutable_rows_but_cannot_update_or_delete_them() {
    let fixture = setup().await;

    let mut transaction = runtime_transaction(&fixture.pool, &fixture.scope).await;
    let snapshot_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM snapshots WHERE tenant_id = $1::uuid AND snapshot_id = $2::uuid",
    )
    .bind(fixture.scope.tenant_id().to_string())
    .bind(fixture.snapshot_id.to_string())
    .fetch_one(transaction.connection())
    .await
    .expect("runtime role should read snapshots");
    assert_eq!(snapshot_count, 1);

    let event_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM events WHERE tenant_id = $1::uuid AND aggregate_id = $2::uuid",
    )
    .bind(fixture.scope.tenant_id().to_string())
    .bind(fixture.object_id.to_string())
    .fetch_one(transaction.connection())
    .await
    .expect("runtime role should read events");
    assert_eq!(event_count, 1);
    transaction
        .commit()
        .await
        .expect("read-only transaction should commit");

    let mut transaction = runtime_transaction(&fixture.pool, &fixture.scope).await;
    let error = sqlx::query(
        "UPDATE snapshots SET canonical_json = canonical_json WHERE tenant_id = $1::uuid AND snapshot_id = $2::uuid",
    )
    .bind(fixture.scope.tenant_id().to_string())
    .bind(fixture.snapshot_id.to_string())
    .execute(transaction.connection())
    .await
    .expect_err("runtime role must not update snapshots");
    assert_insufficient_privilege(error);
    transaction
        .rollback()
        .await
        .expect("denied snapshot update should roll back");

    let mut transaction = runtime_transaction(&fixture.pool, &fixture.scope).await;
    let error =
        sqlx::query("DELETE FROM snapshots WHERE tenant_id = $1::uuid AND snapshot_id = $2::uuid")
            .bind(fixture.scope.tenant_id().to_string())
            .bind(fixture.snapshot_id.to_string())
            .execute(transaction.connection())
            .await
            .expect_err("runtime role must not delete snapshots");
    assert_insufficient_privilege(error);
    transaction
        .rollback()
        .await
        .expect("denied snapshot delete should roll back");

    let mut transaction = runtime_transaction(&fixture.pool, &fixture.scope).await;
    let error = sqlx::query(
        "UPDATE events SET event_type = 'object.forged' WHERE tenant_id = $1::uuid AND aggregate_id = $2::uuid",
    )
    .bind(fixture.scope.tenant_id().to_string())
    .bind(fixture.object_id.to_string())
    .execute(transaction.connection())
    .await
    .expect_err("runtime role must not update events");
    assert_insufficient_privilege(error);
    transaction
        .rollback()
        .await
        .expect("denied event update should roll back");

    let mut transaction = runtime_transaction(&fixture.pool, &fixture.scope).await;
    let error =
        sqlx::query("DELETE FROM events WHERE tenant_id = $1::uuid AND aggregate_id = $2::uuid")
            .bind(fixture.scope.tenant_id().to_string())
            .bind(fixture.object_id.to_string())
            .execute(transaction.connection())
            .await
            .expect_err("runtime role must not delete events");
    assert_insufficient_privilege(error);
    transaction
        .rollback()
        .await
        .expect("denied event delete should roll back");

    fixture.pool.close().await;
}
