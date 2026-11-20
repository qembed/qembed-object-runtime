mod common;

use qembed_domain::ids::{ObjectId, ObjectVersionId, SnapshotId, TenantId};
use qembed_domain::object::Object;
use qembed_domain::object_version::ObjectVersion;
use qembed_domain::snapshot::SnapshotBuilder;
use qembed_domain::test_support::scope_for_tenant;
use qembed_store::object_repository::ObjectRepository;
use qembed_store::run_migrations;

#[tokio::test]
async fn same_tenant_objects_cannot_cross_reference_versions_or_snapshots() {
    let pool = common::connect().await;
    run_migrations(&pool)
        .await
        .expect("migrations should apply");

    let tenant_id = TenantId::new();
    let scope = scope_for_tenant(tenant_id);
    let repository = ObjectRepository::new(pool.clone());
    let object_a = ObjectId::new();
    let object_b = ObjectId::new();

    repository
        .create_object(&scope, &Object::new(object_a, tenant_id))
        .await
        .expect("Object A should be created");
    repository
        .create_object(&scope, &Object::new(object_b, tenant_id))
        .await
        .expect("Object B should be created");

    let version_b_id = ObjectVersionId::new();
    let version_b = ObjectVersion::new(version_b_id, tenant_id, object_b, 1)
        .expect("version number should be positive");
    let snapshot_b = SnapshotBuilder::new(
        SnapshotId::new(),
        tenant_id,
        object_b,
        version_b_id,
        1,
        br#"{"limits":{"max_tokens_per_invocation":128}}"#.to_vec(),
    )
    .build()
    .expect("Object B snapshot should be canonical");
    repository
        .create_draft_version_with_snapshot(&scope, &version_b, &snapshot_b)
        .await
        .expect("Object B version and snapshot should be created");

    let candidate = repository
        .load_publication_candidate(&scope, object_a, version_b_id)
        .await
        .expect("Object A query should be tenant-scoped and object-scoped");
    assert!(
        candidate.is_none(),
        "Object A must not resolve Object B's version even within the same tenant"
    );

    // Tenant equality is not sufficient: the published-version pointer is
    // also bound to its owning Object by the composite foreign key.
    let pointer_error = sqlx::query(
        "UPDATE objects SET published_version_id = $1::uuid WHERE tenant_id = $2::uuid AND object_id = $3::uuid",
    )
    .bind(version_b_id.to_string())
    .bind(tenant_id.to_string())
    .bind(object_a.to_string())
    .execute(&pool)
    .await
    .expect_err("Object A must not point at Object B's version");
    assert_eq!(
        pointer_error
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23503")
    );

    // The same rule applies to snapshot provenance, even when both Objects
    // belong to the same tenant.
    let snapshot_error = sqlx::query(
        "INSERT INTO snapshots (tenant_id, snapshot_id, object_id, object_version_id, schema_version, canonical_json, content_hash, json_projection) VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, 1, $5, $6, $7::jsonb)",
    )
    .bind(tenant_id.to_string())
    .bind(SnapshotId::new().to_string())
    .bind(object_a.to_string())
    .bind(version_b_id.to_string())
    .bind(br#"{"forged":true}"#.as_slice())
    .bind(vec![0_u8; 32])
    .bind(r#"{"forged":true}"#)
    .execute(&pool)
    .await
    .expect_err("Object A snapshot must not claim Object B's version");
    assert_eq!(
        snapshot_error
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23503")
    );

    let object_a_pointer: Option<String> = sqlx::query_scalar(
        "SELECT published_version_id::text FROM objects WHERE tenant_id = $1::uuid AND object_id = $2::uuid",
    )
    .bind(tenant_id.to_string())
    .bind(object_a.to_string())
    .fetch_one(&pool)
    .await
    .expect("Object A should remain queryable");
    assert_eq!(object_a_pointer, None);

    pool.close().await;
}
