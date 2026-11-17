mod common;

use qembed_domain::ids::{ObjectId, ObjectVersionId, TenantId};
use qembed_domain::object::Object;
use qembed_domain::test_support::scope_for_tenant;
use qembed_store::object_repository::{ObjectRepository, ObjectRepositoryError};
use qembed_store::run_migrations;
use sqlx::PgPool;

async fn setup() -> (PgPool, ObjectRepository, qembed_domain::scope::Scope, ObjectId) {
    let pool = common::connect().await;
    run_migrations(&pool)
        .await
        .expect("migrations should be applied");

    let repository = ObjectRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = scope_for_tenant(tenant_id);
    let object_id = ObjectId::new();
    let object = Object::new(object_id, tenant_id);

    repository
        .create_object(&scope, &object)
        .await
        .expect("object should be created");
    repository
        .begin_validation(&scope, object_id)
        .await
        .expect("Draft object should enter validation");

    (pool, repository, scope, object_id)
}

async fn create_draft_version(
    repository: &ObjectRepository,
    scope: &qembed_domain::scope::Scope,
    object_id: ObjectId,
    tenant_id: TenantId,
    number: u64,
) -> ObjectVersionId {
    let version_id = ObjectVersionId::new();
    let version = qembed_domain::object_version::ObjectVersion::new(
        version_id,
        tenant_id,
        object_id,
        number,
    )
    .expect("version number must be positive");
    repository
        .create_draft_version(scope, &version)
        .await
        .expect("draft version should be created");
    version_id
}

#[tokio::test]
async fn publication_and_replacement_update_version_and_object_atomically() {
    let (pool, repository, scope, object_id) = setup().await;
    let tenant_id = scope.tenant_id();
    let first = create_draft_version(&repository, &scope, object_id, tenant_id, 1).await;

    repository
        .publish_version(&scope, object_id, first, None)
        .await
        .expect("first publication should activate the object");

    let first_state: (String, Option<String>) = sqlx::query_as(
        "SELECT state, published_version_id::text FROM objects WHERE tenant_id = $1::uuid AND object_id = $2::uuid",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("published object should exist");
    assert_eq!(first_state, ("active".to_owned(), Some(first.to_string())));

    let first_status: String = sqlx::query_scalar(
        "SELECT status FROM object_versions WHERE tenant_id = $1::uuid AND object_id = $2::uuid AND object_version_id = $3::uuid",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .bind(first.to_string())
    .fetch_one(&pool)
    .await
    .expect("first version should exist");
    assert_eq!(first_status, "published");

    let second = create_draft_version(&repository, &scope, object_id, tenant_id, 2).await;
    repository
        .publish_version(&scope, object_id, second, Some(first))
        .await
        .expect("replacement should atomically supersede the previous version");

    let final_object: (String, Option<String>) = sqlx::query_as(
        "SELECT state, published_version_id::text FROM objects WHERE tenant_id = $1::uuid AND object_id = $2::uuid",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("object should remain active");
    assert_eq!(final_object, ("active".to_owned(), Some(second.to_string())));

    let statuses: Vec<(String,)> = sqlx::query_as(
        "SELECT status FROM object_versions WHERE tenant_id = $1::uuid AND object_id = $2::uuid ORDER BY version_number",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .fetch_all(&pool)
    .await
    .expect("both versions should exist");
    assert_eq!(statuses, vec![("superseded".to_owned(),), ("published".to_owned(),)]);

    let stale = repository
        .publish_version(&scope, object_id, first, Some(first))
        .await
        .expect_err("a stale expected pointer must not replace the current version");
    assert!(matches!(
        stale,
        ObjectRepositoryError::PublishedVersionMismatch
    ));

    pool.close().await;
}

#[tokio::test]
async fn failed_object_pointer_update_rolls_back_both_version_changes() {
    let (pool, repository, scope, object_id) = setup().await;
    let tenant_id = scope.tenant_id();
    let version_id = create_draft_version(&repository, &scope, object_id, tenant_id, 1).await;

    // Inject a failure at the final object-pointer update, after the version
    // row has been changed. The enclosing transaction must undo that change.
    let suffix = object_id.to_string().replace('-', "");
    let function_name = format!("test_reject_publication_{suffix}");
    let trigger_name = format!("test_reject_publication_trigger_{suffix}");
    let function_ddl = format!(
        "CREATE FUNCTION {function_name}() RETURNS trigger LANGUAGE plpgsql AS $function$ BEGIN IF NEW.object_id = '{object_id}'::uuid AND NEW.published_version_id IS DISTINCT FROM OLD.published_version_id THEN RAISE EXCEPTION 'forced publication failure'; END IF; RETURN NEW; END $function$"
    );
    sqlx::query(&function_ddl)
        .execute(&pool)
        .await
        .expect("test trigger function should be installed");
    let trigger_ddl = format!(
        "CREATE TRIGGER {trigger_name} BEFORE UPDATE ON objects FOR EACH ROW EXECUTE FUNCTION {function_name}()"
    );
    sqlx::query(&trigger_ddl)
        .execute(&pool)
        .await
        .expect("test trigger should be installed");

    let error = repository
        .publish_version(&scope, object_id, version_id, None)
        .await
        .expect_err("the injected pointer update failure must abort publication");
    assert!(matches!(
        error,
        ObjectRepositoryError::Database(sqlx::Error::Database(_))
    ));

    let persisted: (String, Option<String>) = sqlx::query_as(
        "SELECT o.state, o.published_version_id::text FROM objects o WHERE o.tenant_id = $1::uuid AND o.object_id = $2::uuid",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("object state should remain queryable");
    assert_eq!(persisted, ("validating".to_owned(), None));

    let version_status: String = sqlx::query_scalar(
        "SELECT status FROM object_versions WHERE tenant_id = $1::uuid AND object_id = $2::uuid AND object_version_id = $3::uuid",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .bind(version_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("version status should remain queryable");
    assert_eq!(version_status, "draft");

    let drop_trigger = format!("DROP TRIGGER {trigger_name} ON objects");
    sqlx::query(&drop_trigger)
        .execute(&pool)
        .await
        .expect("test trigger should be removed");
    let drop_function = format!("DROP FUNCTION {function_name}()");
    sqlx::query(&drop_function)
        .execute(&pool)
        .await
        .expect("test trigger function should be removed");

    pool.close().await;
}

#[tokio::test]
async fn concurrent_first_publications_allow_exactly_one_winner() {
    let (pool, repository, scope, object_id) = setup().await;
    let tenant_id = scope.tenant_id();
    let first = create_draft_version(&repository, &scope, object_id, tenant_id, 1).await;
    let second = create_draft_version(&repository, &scope, object_id, tenant_id, 2).await;

    let (first_result, second_result) = tokio::join!(
        repository.publish_version(&scope, object_id, first, None),
        repository.publish_version(&scope, object_id, second, None),
    );

    assert_ne!(first_result.is_ok(), second_result.is_ok());
    let winner = if first_result.is_ok() { first } else { second };

    let persisted: (String, Option<String>, i64) = sqlx::query_as(
        "SELECT o.state, o.published_version_id::text, count(*) FILTER (WHERE v.status = 'published') FROM objects o JOIN object_versions v ON v.tenant_id = o.tenant_id AND v.object_id = o.object_id WHERE o.tenant_id = $1::uuid AND o.object_id = $2::uuid GROUP BY o.state, o.published_version_id",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("the concurrent publication result should be persisted");

    assert_eq!(persisted.0, "active");
    assert_eq!(persisted.1, Some(winner.to_string()));
    assert_eq!(persisted.2, 1);

    pool.close().await;
}
