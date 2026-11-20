mod common;

use qembed_domain::ids::{ObjectId, ObjectVersionId, SnapshotId, TenantId};
use qembed_domain::object::Object;
use qembed_domain::object_version::ObjectVersion;
use qembed_domain::snapshot::SnapshotBuilder;
use qembed_domain::test_support::scope_for_tenant;
use qembed_store::object_repository::{DraftCreation, ObjectRepository, ObjectRepositoryError};
use qembed_store::run_migrations;
use sqlx::PgPool;

async fn setup() -> (
    PgPool,
    ObjectRepository,
    qembed_domain::scope::Scope,
    ObjectId,
) {
    let pool = common::connect().await;
    run_migrations(&pool).await.expect("migrations should run");
    let tenant_id = TenantId::new();
    let scope = scope_for_tenant(tenant_id);
    let object_id = ObjectId::new();
    let repository = ObjectRepository::new(pool.clone());
    repository
        .create_object(&scope, &Object::new(object_id, tenant_id))
        .await
        .expect("object should be created");
    (pool, repository, scope, object_id)
}

fn draft(
    tenant_id: TenantId,
    object_id: ObjectId,
    number: u64,
    bytes: &[u8],
) -> (ObjectVersion, qembed_domain::snapshot::Snapshot) {
    let version_id = ObjectVersionId::new();
    let version = ObjectVersion::new(version_id, tenant_id, object_id, number)
        .expect("version number should be positive");
    let snapshot = SnapshotBuilder::new(
        SnapshotId::new(),
        tenant_id,
        object_id,
        version_id,
        1,
        bytes.to_vec(),
    )
    .build()
    .expect("snapshot bytes should be valid JCS");
    (version, snapshot)
}

#[tokio::test]
async fn draft_version_and_snapshot_are_created_in_one_operation() {
    let (pool, repository, scope, object_id) = setup().await;
    let (version, snapshot) = draft(
        scope.tenant_id(),
        object_id,
        1,
        br#"{"limits":{"max_tokens_per_invocation":128}}"#,
    );
    let expected_version = version.id();
    let expected_snapshot = snapshot.id();

    let result = repository
        .create_draft_version_with_snapshot(&scope, &version, &snapshot)
        .await
        .expect("version and snapshot should commit together");
    assert_eq!(
        result,
        DraftCreation::Created {
            version_id: expected_version,
            snapshot_id: expected_snapshot,
        }
    );

    let counts: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM object_versions WHERE tenant_id = $1::uuid AND object_id = $2::uuid), (SELECT count(*) FROM snapshots WHERE tenant_id = $1::uuid AND object_id = $2::uuid)",
    )
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("created rows should be queryable");
    assert_eq!(counts, (1, 1));
    pool.close().await;
}

#[tokio::test]
async fn identical_content_reuses_existing_version_and_snapshot_without_new_rows() {
    let (pool, repository, scope, object_id) = setup().await;
    let bytes = br#"{"limits":{"max_tokens_per_invocation":128}}"#;
    let (first_version, first_snapshot) = draft(scope.tenant_id(), object_id, 1, bytes);
    let first = repository
        .create_draft_version_with_snapshot(&scope, &first_version, &first_snapshot)
        .await
        .expect("first version should be created");
    let (second_version, second_snapshot) = draft(scope.tenant_id(), object_id, 2, bytes);
    let second = repository
        .create_draft_version_with_snapshot(&scope, &second_version, &second_snapshot)
        .await
        .expect("identical bytes should be deduplicated");

    let (version_id, snapshot_id) = match first {
        DraftCreation::Created {
            version_id,
            snapshot_id,
        } => (version_id, snapshot_id),
        other => panic!("first insertion should be new, got {other:?}"),
    };
    assert_eq!(
        second,
        DraftCreation::IdenticalContent {
            version_id,
            snapshot_id
        }
    );
    let counts: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM object_versions WHERE tenant_id = $1::uuid AND object_id = $2::uuid), (SELECT count(*) FROM snapshots WHERE tenant_id = $1::uuid AND object_id = $2::uuid)",
    )
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("deduplicated rows should be queryable");
    assert_eq!(counts, (1, 1));
    assert_ne!(second_version.id(), version_id);
    assert_ne!(second_snapshot.id(), snapshot_id);
    pool.close().await;
}

#[tokio::test]
async fn concurrent_identical_content_has_one_creator_and_one_reuser() {
    let (pool, repository, scope, object_id) = setup().await;
    let bytes = br#"{"limits":{"max_tokens_per_invocation":256}}"#;
    let (first_version, first_snapshot) = draft(scope.tenant_id(), object_id, 1, bytes);
    let (second_version, second_snapshot) = draft(scope.tenant_id(), object_id, 2, bytes);

    let (first, second) = tokio::join!(
        repository.create_draft_version_with_snapshot(&scope, &first_version, &first_snapshot),
        repository.create_draft_version_with_snapshot(&scope, &second_version, &second_snapshot),
    );
    let first = first.expect("one concurrent call should succeed");
    let second = second.expect("the other concurrent call should reuse the snapshot");
    assert_ne!(
        matches!(first, DraftCreation::Created { .. }),
        matches!(second, DraftCreation::Created { .. })
    );

    let counts: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM object_versions WHERE tenant_id = $1::uuid AND object_id = $2::uuid), (SELECT count(*) FROM snapshots WHERE tenant_id = $1::uuid AND object_id = $2::uuid)",
    )
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("concurrent result should be queryable");
    assert_eq!(counts, (1, 1));
    pool.close().await;
}

#[tokio::test]
async fn same_hash_with_different_stored_bytes_fails_closed_and_rolls_back_version() {
    let (pool, repository, scope, object_id) = setup().await;
    let (version, snapshot) = draft(
        scope.tenant_id(),
        object_id,
        1,
        br#"{"limits":{"max_tokens_per_invocation":512}}"#,
    );

    // Simulate a corrupted/legacy row whose digest collides with the incoming
    // digest but whose source bytes differ. The projection trigger still passes.
    sqlx::query(
        "INSERT INTO object_versions (tenant_id, object_id, object_version_id, version_number, status) VALUES ($1::uuid, $2::uuid, $3::uuid, 1, 'draft')",
    )
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .bind(version.id().to_string())
    .execute(&pool)
    .await
    .expect("test version should exist");
    sqlx::query(
        "INSERT INTO snapshots (tenant_id, snapshot_id, object_id, object_version_id, schema_version, canonical_json, content_hash, json_projection) VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, 1, $5, $6, convert_from($5, 'UTF8')::jsonb)",
    )
    .bind(scope.tenant_id().to_string())
    .bind(SnapshotId::new().to_string())
    .bind(object_id.to_string())
    .bind(version.id().to_string())
    .bind(br#"{"legacy":true}"#.as_slice())
    .bind(snapshot.content_hash().to_vec())
    .execute(&pool)
    .await
    .expect("corrupt hash fixture should be inserted");

    let (attempt, attempted_snapshot) = draft(
        scope.tenant_id(),
        object_id,
        2,
        br#"{"limits":{"max_tokens_per_invocation":512}}"#,
    );
    let error = repository
        .create_draft_version_with_snapshot(&scope, &attempt, &attempted_snapshot)
        .await
        .expect_err("different bytes with the same hash must fail closed");
    assert!(matches!(error, ObjectRepositoryError::ContentHashCollision));

    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM object_versions WHERE tenant_id = $1::uuid AND object_version_id = $2::uuid",
    )
    .bind(scope.tenant_id().to_string())
    .bind(attempt.id().to_string())
    .fetch_one(&pool)
    .await
    .expect("rollback should leave no attempted version");
    assert_eq!(count, 0);
    pool.close().await;
}

#[tokio::test]
async fn snapshot_must_match_the_supplied_version_and_tenant() {
    let (pool, repository, scope, object_id) = setup().await;
    let (version, _) = draft(
        scope.tenant_id(),
        object_id,
        1,
        br#"{"limits":{"max_tokens_per_invocation":64}}"#,
    );
    let (_, wrong_snapshot) = draft(
        scope.tenant_id(),
        object_id,
        2,
        br#"{"limits":{"max_tokens_per_invocation":64}}"#,
    );
    let error = repository
        .create_draft_version_with_snapshot(&scope, &version, &wrong_snapshot)
        .await
        .expect_err("snapshot must point at the exact supplied version");
    assert!(matches!(
        error,
        ObjectRepositoryError::SnapshotVersionMismatch
    ));
    pool.close().await;
}


#[tokio::test]
async fn one_thousand_identical_configuration_creations_reuse_one_snapshot() {
    let (pool, repository, scope, object_id) = setup().await;
    let bytes = br#"{\"limits\":{\"max_tokens_per_invocation\":1024},\"tools\":[{\"tool_id\":\"11111111-0000-0000-0000-000000000001\",\"tool_version_id\":\"11111111-0000-0000-0000-0000000000a1\"}]}"#;
    let mut canonical_snapshot_id = None;
    let mut canonical_version_id = None;

    for number in 1..=1_000 {
        let (version, snapshot) = draft(scope.tenant_id(), object_id, number, bytes);
        let result = repository
            .create_draft_version_with_snapshot(&scope, &version, &snapshot)
            .await
            .expect("identical immutable configuration must reuse its snapshot");
        match result {
            DraftCreation::Created {
                version_id,
                snapshot_id,
            } => {
                assert_eq!(number, 1, "only the first configuration can create rows");
                canonical_version_id = Some(version_id);
                canonical_snapshot_id = Some(snapshot_id);
            }
            DraftCreation::IdenticalContent {
                version_id,
                snapshot_id,
            } => {
                assert_eq!(canonical_version_id, Some(version_id));
                assert_eq!(canonical_snapshot_id, Some(snapshot_id));
            }
        }
    }

    let counts: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM object_versions WHERE tenant_id = $1::uuid AND object_id = $2::uuid), (SELECT count(*) FROM snapshots WHERE tenant_id = $1::uuid AND object_id = $2::uuid)",
    )
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("deduplicated configuration should have one durable version and snapshot");
    assert_eq!(counts, (1, 1));
    pool.close().await;
}
