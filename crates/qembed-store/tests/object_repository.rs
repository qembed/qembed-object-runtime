mod common;

use qembed_domain::ids::{ObjectId, ObjectVersionId, TenantId};
use qembed_domain::object::{Object, ObjectState};
use qembed_domain::object_version::{ObjectVersion, ObjectVersionStatus};
use qembed_domain::test_support::scope_for_tenant;
use qembed_store::object_repository::{ObjectRepository, ObjectRepositoryError};
use qembed_store::run_migrations;

fn is_database_constraint(error: ObjectRepositoryError, expected_code: &str) -> bool {
    matches!(
        error,
        ObjectRepositoryError::Database(sqlx::Error::Database(database_error))
            if database_error.code().as_deref() == Some(expected_code)
    )
}

#[tokio::test]
async fn object_repository_creates_tenant_scoped_objects_and_draft_versions() {
    let pool = common::connect().await;
    run_migrations(&pool)
        .await
        .expect("migrations should be applied");

    let repository = ObjectRepository::new(pool.clone());
    let tenant_a = TenantId::new();
    let scope_a = scope_for_tenant(tenant_a);
    let object = Object::new(ObjectId::new(), tenant_a);

    repository
        .create_object(&scope_a, &object)
        .await
        .expect("a Draft object should be created in its tenant");

    let stored_state: String = sqlx::query_scalar(
        "SELECT state FROM objects WHERE tenant_id = $1::uuid AND object_id = $2::uuid",
    )
    .bind(tenant_a.to_string())
    .bind(object.id().to_string())
    .fetch_one(&pool)
    .await
    .expect("the object should be persisted");
    assert_eq!(stored_state, "draft");

    let version = ObjectVersion::new(
        ObjectVersionId::new(),
        tenant_a,
        object.id(),
        1,
    )
    .expect("version number must be positive");
    repository
        .create_draft_version(&scope_a, &version)
        .await
        .expect("a Draft version should be created for its object");

    let stored_version: (i64, String) = sqlx::query_as(
        "SELECT version_number, status FROM object_versions WHERE tenant_id = $1::uuid AND object_id = $2::uuid AND object_version_id = $3::uuid",
    )
    .bind(tenant_a.to_string())
    .bind(object.id().to_string())
    .bind(version.id().to_string())
    .fetch_one(&pool)
    .await
    .expect("the draft version should be persisted");
    assert_eq!(stored_version, (1, "draft".to_owned()));

    // Duplicate object IDs and duplicate version numbers are rejected by
    // database constraints rather than silently overwriting existing rows.
    let duplicate_object = Object::new(object.id(), tenant_a);
    let duplicate = repository
        .create_object(&scope_a, &duplicate_object)
        .await
        .expect_err("duplicate object IDs must be rejected");
    assert!(is_database_constraint(duplicate, "23505"));

    let duplicate_number = ObjectVersion::new(
        ObjectVersionId::new(),
        tenant_a,
        object.id(),
        1,
    )
    .expect("version number must be positive");
    let duplicate = repository
        .create_draft_version(&scope_a, &duplicate_number)
        .await
        .expect_err("duplicate version numbers for one object must be rejected");
    assert!(is_database_constraint(duplicate, "23505"));

    // An entity cannot be submitted under a different authenticated tenant.
    let tenant_b = TenantId::new();
    let scope_b = scope_for_tenant(tenant_b);
    let mismatch = repository
        .create_object(&scope_b, &object)
        .await
        .expect_err("tenant A object must not be inserted through tenant B scope");
    assert!(matches!(mismatch, ObjectRepositoryError::TenantMismatch));

    // Even with a matching scope tenant, a version cannot reference an object
    // owned by another tenant: the composite foreign key rejects the insert.
    let foreign_reference = ObjectVersion::new(
        ObjectVersionId::new(),
        tenant_b,
        object.id(),
        1,
    )
    .expect("version number must be positive");
    let foreign_error = repository
        .create_draft_version(&scope_b, &foreign_reference)
        .await
        .expect_err("cross-tenant object references must be rejected");
    assert!(is_database_constraint(foreign_error, "23503"));

    // A failed insert rolls its transaction back and leaves no partial row.
    let missing_object_version = ObjectVersion::new(
        ObjectVersionId::new(),
        tenant_a,
        ObjectId::new(),
        1,
    )
    .expect("version number must be positive");
    let failed_id = missing_object_version.id();
    let rollback_error = repository
        .create_draft_version(&scope_a, &missing_object_version)
        .await
        .expect_err("a version cannot be created without its parent object");
    assert!(is_database_constraint(rollback_error, "23503"));

    let persisted_after_failure: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM object_versions WHERE tenant_id = $1::uuid AND object_version_id = $2::uuid",
    )
    .bind(tenant_a.to_string())
    .bind(failed_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("the failed insert should be queryable");
    assert_eq!(persisted_after_failure, 0);

    // Repository methods accept only new Draft domain entities.
    let active_object = Object::from_persisted(
        ObjectId::new(),
        tenant_a,
        ObjectState::Active,
        Some(ObjectVersionId::new()),
    )
    .expect("active object requires a published-version pointer");
    let non_draft_object = repository
        .create_object(&scope_a, &active_object)
        .await
        .expect_err("create_object must not persist an already-active object");
    assert!(matches!(
        non_draft_object,
        ObjectRepositoryError::ObjectMustBeDraft
    ));

    let mut published_version = ObjectVersion::new(
        ObjectVersionId::new(),
        tenant_a,
        object.id(),
        2,
    )
    .expect("version number must be positive");
    published_version.publish().expect("Draft can be published");
    let non_draft_version = repository
        .create_draft_version(&scope_a, &published_version)
        .await
        .expect_err("create_draft_version must reject a published version");
    assert!(matches!(
        non_draft_version,
        ObjectRepositoryError::VersionMustBeDraft
    ));

    pool.close().await;
}
