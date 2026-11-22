mod common;

use qembed_domain::ids::{ObjectId, ObjectVersionId, SnapshotId, TenantId};
use qembed_domain::object::Object;
use qembed_domain::object_validation::ObjectValidator;
use qembed_domain::object_version::ObjectVersion;
use qembed_domain::scope::Scope;
use qembed_domain::snapshot::SnapshotBuilder;
use qembed_domain::test_support::scope_for_tenant;
use qembed_store::object_repository::{DraftCreation, ObjectRepository, ObjectRepositoryError};
use qembed_store::run_migrations;
use sqlx::PgPool;

#[tokio::test]
async fn publication_rejects_a_legacy_version_without_a_snapshot() {
    let (pool, repository, scope, object_id) = setup().await;
    let version_id = ObjectVersionId::new();
    sqlx::query(
        "INSERT INTO object_versions (tenant_id, object_id, object_version_id, version_number, status) VALUES ($1::uuid, $2::uuid, $3::uuid, 1, 'draft')",
    )
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .bind(version_id.to_string())
    .execute(&pool)
    .await
    .expect("test fixture should insert a legacy version-only row");

    let unpersisted_snapshot = SnapshotBuilder::new(
        SnapshotId::new(),
        scope.tenant_id(),
        object_id,
        version_id,
        1,
        br#"{"limits":{"max_tokens_per_invocation":4096}}"#.to_vec(),
    )
    .build()
    .expect("candidate snapshot should be canonical");
    let evidence = ObjectValidator::validate(&unpersisted_snapshot)
        .expect("candidate snapshot should pass bounded validation");
    let error = repository
        .publish_version(&scope, object_id, version_id, None, &evidence)
        .await
        .expect_err("a version without a snapshot must not be published");
    assert!(matches!(error, ObjectRepositoryError::SnapshotNotFound));

    let state: (String, String) = sqlx::query_as(
        "SELECT o.state, v.status FROM objects o JOIN object_versions v ON v.tenant_id = o.tenant_id AND v.object_id = o.object_id WHERE o.tenant_id = $1::uuid AND o.object_id = $2::uuid AND v.object_version_id = $3::uuid",
    )
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .bind(version_id.to_string())
    .fetch_one(&pool).await.expect("rejected rows should remain queryable");
    assert_eq!(state, ("validating".to_owned(), "draft".to_owned()));
    pool.close().await;
}

#[tokio::test]
async fn publication_rejects_evidence_for_a_different_snapshot_of_the_same_version() {
    let (pool, repository, scope, object_id) = setup().await;
    let (version_id, _) =
        create_draft_version(&repository, &scope, object_id, scope.tenant_id(), 1).await;

    let alternate_snapshot = SnapshotBuilder::new(
        SnapshotId::new(),
        scope.tenant_id(),
        object_id,
        version_id,
        1,
        br#"{"limits":{"max_tokens_per_invocation":2048}}"#.to_vec(),
    )
    .build()
    .expect("alternate candidate should be canonical");
    let wrong_evidence = ObjectValidator::validate(&alternate_snapshot)
        .expect("alternate candidate should pass bounded validation");

    let error = repository
        .publish_version(&scope, object_id, version_id, None, &wrong_evidence)
        .await
        .expect_err("evidence for another snapshot must not authorize publication");
    assert!(matches!(
        error,
        ObjectRepositoryError::ValidationEvidenceMismatch
    ));

    let persisted: (String, String, Option<String>) = sqlx::query_as(
        "SELECT o.state, v.status, o.published_version_id::text FROM objects o JOIN object_versions v ON v.tenant_id = o.tenant_id AND v.object_id = o.object_id WHERE o.tenant_id = $1::uuid AND o.object_id = $2::uuid AND v.object_version_id = $3::uuid",
    )
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .bind(version_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("rejected publication state should remain queryable");
    assert_eq!(
        persisted,
        ("validating".to_owned(), "draft".to_owned(), None)
    );
    pool.close().await;
}

async fn setup() -> (PgPool, ObjectRepository, Scope, ObjectId) {
    let pool = common::connect().await;
    run_migrations(&pool)
        .await
        .expect("migrations should be applied");

    let context = common::signed_context(&pool).await;
    let repository = ObjectRepository::new(context);
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
    scope: &Scope,
    object_id: ObjectId,
    tenant_id: TenantId,
    number: u64,
) -> (
    ObjectVersionId,
    qembed_domain::object_validation::ValidationEvidence,
) {
    let version_id = ObjectVersionId::new();
    let version = ObjectVersion::new(version_id, tenant_id, object_id, number)
        .expect("version number must be positive");
    let snapshot = SnapshotBuilder::new(
        SnapshotId::new(),
        tenant_id,
        object_id,
        version_id,
        1,
        format!(r#"{{"limits":{{"max_tokens_per_invocation":4096}},"version":{number}}}"#)
            .into_bytes(),
    )
    .build()
    .expect("snapshot should be valid canonical JSON");
    let result = repository
        .create_draft_version_with_snapshot(scope, &version, &snapshot)
        .await
        .expect("Draft version and snapshot should be created atomically");
    assert_eq!(
        result,
        DraftCreation::Created {
            version_id,
            snapshot_id: snapshot.id()
        }
    );
    let evidence = ObjectValidator::validate(&snapshot)
        .expect("snapshot should pass the bounded semantic validator");
    (version_id, evidence)
}

#[tokio::test]
async fn publication_and_replacement_update_version_and_object_atomically() {
    let (pool, repository, scope, object_id) = setup().await;
    let tenant_id = scope.tenant_id();
    let (first, first_evidence) =
        create_draft_version(&repository, &scope, object_id, tenant_id, 1).await;

    repository
        .publish_version(&scope, object_id, first, None, &first_evidence)
        .await
        .expect("first publication should activate the object");

    let publication_event: (String, serde_json::Value) = sqlx::query_as(
        "SELECT event_type, payload FROM events WHERE tenant_id = $1::uuid AND aggregate_id = $2::uuid AND event_type = 'object.version_published' ORDER BY occurred_at DESC LIMIT 1",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("publication event should be committed with the pointer");
    assert_eq!(publication_event.0, "object.version_published");
    assert_eq!(
        publication_event.1["snapshot_id"],
        serde_json::json!(first_evidence.snapshot_id().to_string())
    );
    assert_eq!(
        publication_event.1["validator_version"],
        serde_json::json!("m0-r3-bounded-budget-v1")
    );

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

    let (second, second_evidence) =
        create_draft_version(&repository, &scope, object_id, tenant_id, 2).await;
    repository
        .publish_version(&scope, object_id, second, Some(first), &second_evidence)
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
    assert_eq!(
        final_object,
        ("active".to_owned(), Some(second.to_string()))
    );

    let statuses: Vec<(String,)> = sqlx::query_as(
        "SELECT status FROM object_versions WHERE tenant_id = $1::uuid AND object_id = $2::uuid ORDER BY version_number",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .fetch_all(&pool)
    .await
    .expect("both versions should exist");
    assert_eq!(
        statuses,
        vec![("superseded".to_owned(),), ("published".to_owned(),)]
    );

    let stale = repository
        .publish_version(&scope, object_id, first, Some(first), &first_evidence)
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
    let (version_id, evidence) =
        create_draft_version(&repository, &scope, object_id, tenant_id, 1).await;

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
        .publish_version(&scope, object_id, version_id, None, &evidence)
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
    let (first, first_evidence) =
        create_draft_version(&repository, &scope, object_id, tenant_id, 1).await;
    let (second, second_evidence) =
        create_draft_version(&repository, &scope, object_id, tenant_id, 2).await;

    let (first_result, second_result) = tokio::join!(
        repository.publish_version(&scope, object_id, first, None, &first_evidence),
        repository.publish_version(&scope, object_id, second, None, &second_evidence),
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
