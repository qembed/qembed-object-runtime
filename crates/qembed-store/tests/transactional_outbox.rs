mod common;

use qembed_domain::ids::{EventId, ObjectId, ObjectVersionId, TenantId};
use qembed_domain::object::Object;
use qembed_domain::object_version::ObjectVersion;
use qembed_domain::scope::Scope;
use qembed_domain::test_support::scope_for_tenant;
use qembed_store::object_repository::ObjectRepository;
use qembed_store::run_migrations;
use sqlx::PgPool;

async fn setup() -> (PgPool, ObjectRepository, Scope, ObjectId) {
    let pool = common::connect().await;
    run_migrations(&pool)
        .await
        .expect("migrations should apply");
    let repository = ObjectRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = scope_for_tenant(tenant_id);
    let object_id = ObjectId::new();
    repository
        .create_object(&scope, &Object::new(object_id, tenant_id))
        .await
        .expect("object should be created");
    (pool, repository, scope, object_id)
}

async fn create_draft_version(
    repository: &ObjectRepository,
    scope: &Scope,
    object_id: ObjectId,
    number: u64,
) -> ObjectVersionId {
    let id = ObjectVersionId::new();
    let version = ObjectVersion::new(id, scope.tenant_id(), object_id, number)
        .expect("version number must be positive");
    repository
        .create_draft_version(scope, &version)
        .await
        .expect("draft version should be created");
    id
}

#[tokio::test]
async fn state_event_and_outbox_commit_together() {
    let (pool, repository, scope, object_id) = setup().await;
    let version_id = create_draft_version(&repository, &scope, object_id, 1).await;

    repository
        .begin_validation(&scope, object_id)
        .await
        .expect("validation transition should commit with its event");
    repository
        .publish_version(&scope, object_id, version_id, None)
        .await
        .expect("publication should commit with its event");

    let state: (String, Option<String>) = sqlx::query_as(
        "SELECT state, published_version_id::text FROM objects WHERE tenant_id = $1::uuid AND object_id = $2::uuid",
    )
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("object state should be readable");
    assert_eq!(state, ("active".to_owned(), Some(version_id.to_string())));

    let events: Vec<(String, String)> = sqlx::query_as(
        "SELECT event_type, payload->>'state' FROM events WHERE tenant_id = $1::uuid AND aggregate_id = $2::uuid ORDER BY event_type",
    )
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .fetch_all(&pool)
    .await
    .expect("events should be readable");
    assert_eq!(
        events,
        vec![
            (
                "object.validation_started".to_owned(),
                "validating".to_owned()
            ),
            ("object.version_published".to_owned(), "active".to_owned())
        ]
    );

    let outbox: Vec<(String, String)> = sqlx::query_as(
        "SELECT topic, payload->>'state' FROM outbox_messages WHERE tenant_id = $1::uuid ORDER BY topic",
    )
    .bind(scope.tenant_id().to_string())
    .fetch_all(&pool)
    .await
    .expect("outbox rows should be readable");
    assert_eq!(outbox, events);

    let identities: (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(DISTINCT delivery_id) FROM outbox_messages WHERE tenant_id = $1::uuid",
    )
    .bind(scope.tenant_id().to_string())
    .fetch_one(&pool)
    .await
    .expect("delivery identities should be unique");
    assert_eq!(identities, (2, 2));

    pool.close().await;
}

#[tokio::test]
async fn failed_state_transition_rolls_back_event_and_outbox() {
    let (pool, repository, scope, object_id) = setup().await;
    let version_id = create_draft_version(&repository, &scope, object_id, 1).await;
    repository
        .begin_validation(&scope, object_id)
        .await
        .expect("validation transition should commit");

    // Fail after the state update and event insert, at the outbox insert.
    // This proves all three writes share one transaction.
    let suffix = object_id.to_string().replace('-', "");
    let function_name = format!("test_reject_outbox_{suffix}");
    let trigger_name = format!("test_reject_outbox_trigger_{suffix}");
    let function_ddl = format!(
        "CREATE FUNCTION {function_name}() RETURNS trigger LANGUAGE plpgsql AS $function$ BEGIN RAISE EXCEPTION 'forced outbox failure'; END $function$"
    );
    sqlx::query(&function_ddl)
        .execute(&pool)
        .await
        .expect("failure trigger function should be installed");
    // Scope the injected failure to this fixture's tenant. The integration
    // test binaries share one PostgreSQL database, so a table-wide trigger
    // would race with unrelated tests that append outbox rows concurrently.
    let trigger_ddl = format!(
        "CREATE TRIGGER {trigger_name} BEFORE INSERT ON outbox_messages FOR EACH ROW WHEN (NEW.tenant_id = '{}'::uuid) EXECUTE FUNCTION {function_name}()",
        scope.tenant_id()
    );
    sqlx::query(&trigger_ddl)
        .execute(&pool)
        .await
        .expect("failure trigger should be installed");

    let before: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM events WHERE tenant_id = $1::uuid), (SELECT count(*) FROM outbox_messages WHERE tenant_id = $1::uuid)",
    )
    .bind(scope.tenant_id().to_string())
    .fetch_one(&pool)
    .await
    .expect("baseline event and outbox counts should be available");

    assert!(
        repository
            .publish_version(&scope, object_id, version_id, None)
            .await
            .is_err(),
        "injected outbox failure should abort publication"
    );

    let after: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM events WHERE tenant_id = $1::uuid), (SELECT count(*) FROM outbox_messages WHERE tenant_id = $1::uuid)",
    )
    .bind(scope.tenant_id().to_string())
    .fetch_one(&pool)
    .await
    .expect("event and outbox counts should remain queryable");
    assert_eq!(after, before);

    let persisted: (String, Option<String>, String) = sqlx::query_as(
        "SELECT o.state, o.published_version_id::text, v.status FROM objects o JOIN object_versions v ON v.tenant_id = o.tenant_id AND v.object_id = o.object_id WHERE o.tenant_id = $1::uuid AND o.object_id = $2::uuid AND v.object_version_id = $3::uuid",
    )
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .bind(version_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("state should remain queryable");
    assert_eq!(
        persisted,
        ("validating".to_owned(), None, "draft".to_owned())
    );

    sqlx::query(&format!("DROP TRIGGER {trigger_name} ON outbox_messages"))
        .execute(&pool)
        .await
        .expect("failure trigger should be removed");
    sqlx::query(&format!("DROP FUNCTION {function_name}()"))
        .execute(&pool)
        .await
        .expect("failure trigger function should be removed");

    pool.close().await;
}
#[tokio::test]
async fn persisted_events_reject_update_and_delete() {
    let (pool, repository, scope, object_id) = setup().await;
    repository
        .begin_validation(&scope, object_id)
        .await
        .expect("validation event should commit");

    let event_id: String = sqlx::query_scalar(
        "SELECT event_id::text FROM events WHERE tenant_id = $1::uuid AND aggregate_id = $2::uuid",
    )
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("event should exist");

    let update = sqlx::query(
        "UPDATE events SET event_type = 'object.forged' WHERE tenant_id = $1::uuid AND event_id = $2::uuid",
    )
    .bind(scope.tenant_id().to_string())
    .bind(&event_id)
    .execute(&pool)
    .await
    .expect_err("append-only event must reject UPDATE");
    assert!(matches!(update, sqlx::Error::Database(_)));

    let delete =
        sqlx::query("DELETE FROM events WHERE tenant_id = $1::uuid AND event_id = $2::uuid")
            .bind(scope.tenant_id().to_string())
            .bind(event_id)
            .execute(&pool)
            .await
            .expect_err("append-only event must reject DELETE");
    assert!(matches!(delete, sqlx::Error::Database(_)));

    pool.close().await;
}

#[tokio::test]
async fn duplicate_delivery_identity_is_rejected() {
    let (pool, repository, scope, object_id) = setup().await;
    repository
        .begin_validation(&scope, object_id)
        .await
        .expect("initial event and delivery should commit");

    let existing_delivery_id: String = sqlx::query_scalar(
        "SELECT delivery_id::text FROM outbox_messages WHERE tenant_id = $1::uuid",
    )
    .bind(scope.tenant_id().to_string())
    .fetch_one(&pool)
    .await
    .expect("initial delivery identity should exist");

    let new_event_id = EventId::new();
    let mut transaction = pool.begin().await.expect("transaction should begin");
    sqlx::query(
        "INSERT INTO events (tenant_id, event_id, aggregate_type, aggregate_id, event_type, payload) VALUES ($1::uuid, $2::uuid, 'object', $3::uuid, 'object.test', '{}'::jsonb)",
    )
    .bind(scope.tenant_id().to_string())
    .bind(new_event_id.to_string())
    .bind(object_id.to_string())
    .execute(&mut *transaction)
    .await
    .expect("second event should be inserted in test transaction");

    let duplicate = sqlx::query(
        "INSERT INTO outbox_messages (tenant_id, delivery_id, event_id, topic, payload) VALUES ($1::uuid, $2::uuid, $3::uuid, 'object.test', '{}'::jsonb)",
    )
    .bind(scope.tenant_id().to_string())
    .bind(existing_delivery_id)
    .bind(new_event_id.to_string())
    .execute(&mut *transaction)
    .await
    .expect_err("duplicate delivery identity must violate unique constraint");
    assert!(matches!(duplicate, sqlx::Error::Database(_)));
    transaction
        .rollback()
        .await
        .expect("failed attempt should roll back");

    let event_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM events WHERE tenant_id = $1::uuid AND event_id = $2::uuid",
    )
    .bind(scope.tenant_id().to_string())
    .bind(new_event_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("event count should be queryable");
    assert_eq!(event_count, 0);

    pool.close().await;
}
