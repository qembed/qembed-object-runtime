mod common;

use qembed_domain::ids::{ObjectId, ObjectVersionId, SnapshotId, TenantId};
use qembed_domain::object::{Object, ObjectState};
use qembed_domain::object_version::ObjectVersion;
use qembed_domain::snapshot::SnapshotBuilder;
use qembed_domain::test_support::scope_for_tenant;
use qembed_store::object_repository::{DraftCreation, ObjectRepository, ObjectRepositoryError};
use qembed_store::run_migrations;

fn snapshot(version: &ObjectVersion, bytes: &[u8]) -> qembed_domain::snapshot::Snapshot {
    SnapshotBuilder::new(
        SnapshotId::new(),
        version.tenant_id(),
        version.object_id(),
        version.id(),
        1,
        bytes.to_vec(),
    )
    .build()
    .expect("snapshot should be valid canonical JSON")
}

#[tokio::test]
async fn object_repository_creates_objects_and_only_atomic_draft_version_snapshot_pairs() {
    let pool = common::connect().await;
    run_migrations(&pool).await.expect("migrations should run");
    let repository = ObjectRepository::new(pool.clone());
    let tenant_a = TenantId::new();
    let scope_a = scope_for_tenant(tenant_a);
    let object = Object::new(ObjectId::new(), tenant_a);

    repository
        .create_object(&scope_a, &object)
        .await
        .expect("Draft object should be created");
    let stored_state: String = sqlx::query_scalar(
        "SELECT state FROM objects WHERE tenant_id = $1::uuid AND object_id = $2::uuid",
    )
    .bind(tenant_a.to_string())
    .bind(object.id().to_string())
    .fetch_one(&pool)
    .await
    .expect("object should be persisted");
    assert_eq!(stored_state, "draft");

    let version = ObjectVersion::new(ObjectVersionId::new(), tenant_a, object.id(), 1)
        .expect("version number must be positive");
    let snap = snapshot(&version, br#"{"version":1}"#);
    assert_eq!(
        repository
            .create_draft_version_with_snapshot(&scope_a, &version, &snap)
            .await
            .expect("version and snapshot should be atomic"),
        DraftCreation::Created {
            version_id: version.id(),
            snapshot_id: snap.id()
        }
    );

    let stored: (i64, String, i64) = sqlx::query_as(
        "SELECT v.version_number, v.status, count(s.snapshot_id) FROM object_versions v LEFT JOIN snapshots s ON s.tenant_id = v.tenant_id AND s.object_id = v.object_id AND s.object_version_id = v.object_version_id WHERE v.tenant_id = $1::uuid AND v.object_id = $2::uuid AND v.object_version_id = $3::uuid GROUP BY v.version_number, v.status",
    )
    .bind(tenant_a.to_string())
    .bind(object.id().to_string())
    .bind(version.id().to_string())
    .fetch_one(&pool)
    .await
    .expect("version and snapshot should be queryable");
    assert_eq!(stored, (1, "draft".to_owned(), 1));

    // The repository allocates the next version number while holding the
    // tenant-scoped object row lock; callers cannot choose or collide on it.
    let second_version = ObjectVersion::new(ObjectVersionId::new(), tenant_a, object.id(), 1)
        .expect("the caller-supplied placeholder number is positive");
    let second_snapshot = snapshot(&second_version, br#"{"version":2}"#);
    assert!(matches!(
        repository
            .create_draft_version_with_snapshot(&scope_a, &second_version, &second_snapshot)
            .await
            .expect("the repository should allocate version number 2"),
        DraftCreation::Created { .. }
    ));
    let allocated_numbers: Vec<i64> = sqlx::query_scalar(
        "SELECT version_number FROM object_versions WHERE tenant_id = $1::uuid AND object_id = $2::uuid ORDER BY version_number",
    )
    .bind(tenant_a.to_string())
    .bind(object.id().to_string())
    .fetch_all(&pool)
    .await
    .expect("allocated version numbers should be queryable");
    assert_eq!(allocated_numbers, vec![1, 2]);

    // The trusted scope, not caller-provided tenant data, determines ownership.
    let tenant_b = TenantId::new();
    let scope_b = scope_for_tenant(tenant_b);
    let mismatch = repository
        .create_object(&scope_b, &object)
        .await
        .expect_err("tenant A object cannot be inserted through tenant B scope");
    assert!(matches!(mismatch, ObjectRepositoryError::TenantMismatch));

    let foreign_version = ObjectVersion::new(ObjectVersionId::new(), tenant_b, object.id(), 2)
        .expect("version number must be positive");
    let foreign_snapshot = snapshot(&foreign_version, br#"{"version":3}"#);
    let foreign_error = repository
        .create_draft_version_with_snapshot(&scope_b, &foreign_version, &foreign_snapshot)
        .await
        .expect_err("cross-tenant object references must be rejected");
    assert!(matches!(
        foreign_error,
        ObjectRepositoryError::ObjectNotFound
    ));

    // A failed atomic insert leaves no orphaned version.
    let missing_object_version =
        ObjectVersion::new(ObjectVersionId::new(), tenant_a, ObjectId::new(), 1)
            .expect("version number must be positive");
    let missing_snapshot = snapshot(&missing_object_version, br#"{"version":4}"#);
    let failed_id = missing_object_version.id();
    let rollback_error = repository
        .create_draft_version_with_snapshot(&scope_a, &missing_object_version, &missing_snapshot)
        .await
        .expect_err("a version cannot be created without its parent object");
    assert!(matches!(
        rollback_error,
        ObjectRepositoryError::ObjectNotFound
    ));
    let persisted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM object_versions WHERE tenant_id = $1::uuid AND object_version_id = $2::uuid",
    )
    .bind(tenant_a.to_string())
    .bind(failed_id.to_string())
    .fetch_one(&pool).await.expect("rollback result should be queryable");
    assert_eq!(persisted, 0);

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
        .expect_err("create_object must reject an already-active object");
    assert!(matches!(
        non_draft_object,
        ObjectRepositoryError::ObjectMustBeDraft
    ));

    let mut published_version =
        ObjectVersion::new(ObjectVersionId::new(), tenant_a, object.id(), 2)
            .expect("version number must be positive");
    published_version.publish().expect("Draft can be published");
    let published_snapshot = snapshot(&published_version, br#"{"version":5}"#);
    let non_draft_version = repository
        .create_draft_version_with_snapshot(&scope_a, &published_version, &published_snapshot)
        .await
        .expect_err("atomic creation must reject a published version");
    assert!(matches!(
        non_draft_version,
        ObjectRepositoryError::VersionMustBeDraft
    ));

    pool.close().await;
}

#[tokio::test]
async fn concurrent_draft_creations_allocate_distinct_sequential_version_numbers() {
    let pool = common::connect().await;
    run_migrations(&pool).await.expect("migrations should run");
    let repository = ObjectRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = scope_for_tenant(tenant_id);
    let object = Object::new(ObjectId::new(), tenant_id);
    repository
        .create_object(&scope, &object)
        .await
        .expect("object should be created");

    let first = ObjectVersion::new(ObjectVersionId::new(), tenant_id, object.id(), 1)
        .expect("placeholder version number is positive");
    let first_snapshot = snapshot(&first, br#"{"payload":"first"}"#);
    let second = ObjectVersion::new(ObjectVersionId::new(), tenant_id, object.id(), 1)
        .expect("placeholder version number is positive");
    let second_snapshot = snapshot(&second, br#"{"payload":"second"}"#);

    let (first_result, second_result) = tokio::join!(
        repository.create_draft_version_with_snapshot(&scope, &first, &first_snapshot),
        repository.create_draft_version_with_snapshot(&scope, &second, &second_snapshot),
    );
    assert!(matches!(first_result, Ok(DraftCreation::Created { .. })));
    assert!(matches!(second_result, Ok(DraftCreation::Created { .. })));

    let numbers: Vec<i64> = sqlx::query_scalar(
        "SELECT version_number FROM object_versions WHERE tenant_id = $1::uuid AND object_id = $2::uuid ORDER BY version_number",
    )
    .bind(tenant_id.to_string())
    .bind(object.id().to_string())
    .fetch_all(&pool)
    .await
    .expect("version numbers should be queryable");
    assert_eq!(numbers, vec![1, 2]);

    pool.close().await;
}
