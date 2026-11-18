mod common;

use std::str::FromStr;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;
use qembed_store::run_migrations;
use uuid::Uuid;

const RUNTIME_PASSWORD: &str = "runtime-test-only";
const DISPATCHER_PASSWORD: &str = "dispatcher-test-only";
const ADMIN_PASSWORD: &str = "admin-test-only";

async fn provision_test_logins(owner: &PgPool) {
    // These LOGIN identities are test-only. Production migrations create only
    // NOLOGIN capability roles; process credentials are provisioned externally.
    for (login, password, group) in [
        ("qembed_runtime_test", RUNTIME_PASSWORD, "qembed_runtime"),
        (
            "qembed_dispatcher_test",
            DISPATCHER_PASSWORD,
            "qembed_dispatcher",
        ),
        ("qembed_admin_test", ADMIN_PASSWORD, "qembed_admin"),
    ] {
        let statement = format!(
            "DO $provision$ BEGIN
                IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = '{login}') THEN
                    CREATE ROLE {login} LOGIN PASSWORD '{password}'
                        INHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;
                ELSE
                    ALTER ROLE {login} LOGIN PASSWORD '{password}'
                        INHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;
                END IF;
            END $provision$"
        );
        sqlx::query(&statement)
            .execute(owner)
            .await
            .expect("test login should be provisioned");
        let grant = format!("GRANT {group} TO {login}");
        sqlx::query(&grant)
            .execute(owner)
            .await
            .expect("test login should inherit exactly its application capability");
    }
}

async fn connect_as(login: &str, password: &str) -> PgPool {
    let database_url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set by scripts/test-postgres.sh");
    let options = PgConnectOptions::from_str(&database_url)
        .expect("DATABASE_URL should be a valid PostgreSQL URL")
        .username(login)
        .password(password);
    PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options)
        .await
        .expect("the test identity should connect with its own PostgreSQL credentials")
}

async fn set_tenant(pool: &PgPool, tenant_id: Uuid) -> sqlx::Transaction<'_, sqlx::Postgres> {
    let mut transaction = pool.begin().await.expect("role transaction should begin");
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(tenant_id.to_string())
        .execute(&mut *transaction)
        .await
        .expect("tenant context should be transaction-local");
    transaction
}

fn assert_sqlstate(error: sqlx::Error, expected: &str) {
    assert_eq!(
        error
            .as_database_error()
            .and_then(|database| database.code())
            .as_deref(),
        Some(expected),
        "unexpected PostgreSQL error"
    );
}

#[tokio::test]
async fn runtime_dispatcher_and_admin_have_separate_database_capabilities() {
    let owner = common::connect().await;
    run_migrations(&owner)
        .await
        .expect("all store migrations should apply");
    provision_test_logins(&owner).await;

    let tenant_a = Uuid::new_v4();
    let tenant_b = Uuid::new_v4();
    let object_a = Uuid::new_v4();
    let object_b = Uuid::new_v4();
    let version_a = Uuid::new_v4();
    let version_b = Uuid::new_v4();
    let event_a = Uuid::new_v4();
    let event_b = Uuid::new_v4();
    let delivery_a = Uuid::new_v4();
    let delivery_b = Uuid::new_v4();

    for (tenant, object, version, event, delivery) in [
        (tenant_a, object_a, version_a, event_a, delivery_a),
        (tenant_b, object_b, version_b, event_b, delivery_b),
    ] {
        sqlx::query(
            "INSERT INTO objects (tenant_id, object_id) VALUES ($1, $2)",
        )
        .bind(tenant)
        .bind(object)
        .execute(&owner)
        .await
        .expect("owner should seed object fixtures");
        sqlx::query(
            "INSERT INTO object_versions (tenant_id, object_id, object_version_id, version_number)
             VALUES ($1, $2, $3, 1)",
        )
        .bind(tenant)
        .bind(object)
        .bind(version)
        .execute(&owner)
        .await
        .expect("owner should seed version fixtures");
        sqlx::query(
            "INSERT INTO events (tenant_id, event_id, aggregate_type, aggregate_id, event_type, payload)
             VALUES ($1, $2, 'object', $3, 'object.created', '{}'::jsonb)",
        )
        .bind(tenant)
        .bind(event)
        .bind(object)
        .execute(&owner)
        .await
        .expect("owner should seed event fixtures");
        sqlx::query(
            "INSERT INTO outbox_messages (tenant_id, delivery_id, event_id, topic, payload)
             VALUES ($1, $2, $3, 'object.events', '{}'::jsonb)",
        )
        .bind(tenant)
        .bind(delivery)
        .bind(event)
        .execute(&owner)
        .await
        .expect("owner should seed outbox fixtures");
    }

    let runtime = connect_as("qembed_runtime_test", RUNTIME_PASSWORD).await;
    let dispatcher = connect_as("qembed_dispatcher_test", DISPATCHER_PASSWORD).await;
    let admin = connect_as("qembed_admin_test", ADMIN_PASSWORD).await;

    // Runtime can see and mutate only the selected tenant's object.
    let mut tx = set_tenant(&runtime, tenant_a).await;
    let own_objects: i64 =
        sqlx::query_scalar("SELECT count(*) FROM objects WHERE tenant_id = $1")
            .bind(tenant_a)
            .fetch_one(&mut *tx)
            .await
            .expect("runtime should read its tenant");
    let foreign_objects: i64 =
        sqlx::query_scalar("SELECT count(*) FROM objects WHERE tenant_id = $1")
            .bind(tenant_b)
            .fetch_one(&mut *tx)
            .await
            .expect("RLS should hide foreign tenant rows");
    assert_eq!(own_objects, 1);
    assert_eq!(foreign_objects, 0);
    sqlx::query("UPDATE objects SET state = 'validating' WHERE tenant_id = $1 AND object_id = $2")
        .bind(tenant_a)
        .bind(object_a)
        .execute(&mut *tx)
        .await
        .expect("runtime should update its own object");
    let hidden_update = sqlx::query("UPDATE objects SET state = 'validating' WHERE tenant_id = $1")
        .bind(tenant_b)
        .execute(&mut *tx)
        .await
        .expect("RLS should hide foreign tenant rows from UPDATE");
    assert_eq!(hidden_update.rows_affected(), 0);
    tx.rollback().await.expect("runtime transaction should roll back");

    // Runtime may append event/outbox records but cannot rewrite immutable rows
    // or acknowledge delivery (that capability belongs to the dispatcher).
    let mut tx = set_tenant(&runtime, tenant_a).await;
    let new_event = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO events (tenant_id, event_id, aggregate_type, aggregate_id, event_type, payload)
         VALUES ($1, $2, 'object', $3, 'object.tested', '{}'::jsonb)",
    )
    .bind(tenant_a)
    .bind(new_event)
    .bind(object_a)
    .execute(&mut *tx)
    .await
    .expect("runtime should append events");
    sqlx::query(
        "INSERT INTO outbox_messages (tenant_id, delivery_id, event_id, topic, payload)
         VALUES ($1, $2, $3, 'object.events', '{}'::jsonb)",
    )
    .bind(tenant_a)
    .bind(Uuid::new_v4())
    .bind(new_event)
    .execute(&mut *tx)
    .await
    .expect("runtime should append outbox messages");
    sqlx::query(
        "INSERT INTO snapshots
            (tenant_id, snapshot_id, object_id, object_version_id, schema_version,
             canonical_json, content_hash, json_projection)
         VALUES ($1, $2, $3, $4, 1, convert_to('{}', 'UTF8'), decode(repeat('00', 32), 'hex'), '{}'::jsonb)",
    )
    .bind(tenant_a)
    .bind(Uuid::new_v4())
    .bind(object_a)
    .bind(version_a)
    .execute(&mut *tx)
    .await
    .expect("runtime should append immutable snapshots");
    let error = sqlx::query("UPDATE snapshots SET json_projection = '{}'::jsonb WHERE tenant_id = $1")
        .bind(tenant_a)
        .execute(&mut *tx)
        .await
        .expect_err("runtime must not update immutable snapshots");
    assert_sqlstate(error, "42501");
    let error = sqlx::query("DELETE FROM events WHERE tenant_id = $1")
        .bind(tenant_a)
        .execute(&mut *tx)
        .await
        .expect_err("runtime must not delete append-only events");
    assert_sqlstate(error, "42501");
    let error = sqlx::query("UPDATE events SET event_type = 'forged' WHERE tenant_id = $1")
        .bind(tenant_a)
        .execute(&mut *tx)
        .await
        .expect_err("runtime must not update append-only events");
    assert_sqlstate(error, "42501");
    let error = sqlx::query("UPDATE outbox_messages SET delivered_at = now() WHERE tenant_id = $1")
        .bind(tenant_a)
        .execute(&mut *tx)
        .await
        .expect_err("runtime must not acknowledge outbox delivery");
    assert_sqlstate(error, "42501");
    tx.rollback().await.expect("runtime append test should roll back");

    // Dispatcher has a deliberate cross-tenant view of the outbox only.
    let mut tx = dispatcher.begin().await.expect("dispatcher transaction should begin");
    let queue_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox_messages WHERE tenant_id IN ($1, $2)")
        .bind(tenant_a)
        .bind(tenant_b)
        .fetch_one(&mut *tx)
        .await
        .expect("dispatcher should read the global outbox");
    assert_eq!(queue_rows, 2);
    sqlx::query("UPDATE outbox_messages SET delivered_at = now() WHERE tenant_id = $1 AND delivery_id = $2")
        .bind(tenant_b)
        .bind(delivery_b)
        .execute(&mut *tx)
        .await
        .expect("dispatcher should acknowledge delivery for any tenant");
    let error = sqlx::query("UPDATE outbox_messages SET payload = '{\"forged\":true}'::jsonb WHERE tenant_id = $1")
        .bind(tenant_a)
        .execute(&mut *tx)
        .await
        .expect_err("dispatcher must not rewrite message payload");
    assert_sqlstate(error, "42501");
    let error = sqlx::query("SELECT count(*) FROM objects")
        .fetch_one(&mut *tx)
        .await
        .expect_err("dispatcher must not read domain objects");
    assert_sqlstate(error, "42501");
    tx.rollback().await.expect("dispatcher transaction should roll back");

    // Admin is read-only and tenant-scoped; credential tables are not exposed.
    let mut tx = set_tenant(&admin, tenant_a).await;
    let visible: i64 = sqlx::query_scalar("SELECT count(*) FROM objects")
        .fetch_one(&mut *tx)
        .await
        .expect("admin should read business data in its tenant");
    assert_eq!(visible, 1);
    let error = sqlx::query("UPDATE objects SET state = 'suspended' WHERE object_id = $1")
        .bind(object_a)
        .execute(&mut *tx)
        .await
        .expect_err("admin must not mutate business data");
    assert_sqlstate(error, "42501");
    let error = sqlx::query("SELECT count(*) FROM owner_credentials")
        .fetch_one(&mut *tx)
        .await
        .expect_err("admin must not read credential hashes");
    assert_sqlstate(error, "42501");
    tx.rollback().await.expect("admin transaction should roll back");

    // Tenant-scoped runtime fails closed when no transaction-local context exists.
    let mut tx = runtime.begin().await.expect("unscoped runtime transaction should begin");
    let unscoped: i64 = sqlx::query_scalar("SELECT count(*) FROM objects")
        .fetch_one(&mut *tx)
        .await
        .expect("missing tenant context should yield no rows, not a bypass");
    assert_eq!(unscoped, 0);
    tx.rollback().await.expect("unscoped transaction should roll back");

    runtime.close().await;
    dispatcher.close().await;
    admin.close().await;
    owner.close().await;
}
