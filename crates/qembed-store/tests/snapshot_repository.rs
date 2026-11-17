mod common;

use qembed_domain::ids::{ObjectId, ObjectVersionId, SnapshotId, TenantId};
use qembed_domain::object::Object;
use qembed_domain::object_version::ObjectVersion;
use qembed_domain::scope::Scope;
use qembed_domain::jcs::JcsError;
use qembed_domain::snapshot::{SnapshotBuilder, SnapshotError};
use sha2::{Digest, Sha256};
use qembed_domain::test_support::scope_for_tenant;
use qembed_store::object_repository::ObjectRepository;
use qembed_store::run_migrations;
use qembed_store::snapshot_repository::{SnapshotRepository, SnapshotRepositoryError};
use sqlx::PgPool;

struct Fixture {
    pool: PgPool,
    scope: Scope,
    object_id: ObjectId,
    version_id: ObjectVersionId,
}

async fn setup() -> Fixture {
    let pool = common::connect().await;
    run_migrations(&pool)
        .await
        .expect("migrations should be applied");

    let tenant_id = TenantId::new();
    let scope = scope_for_tenant(tenant_id);
    let object_id = ObjectId::new();
    let version_id = ObjectVersionId::new();

    let objects = ObjectRepository::new(pool.clone());
    let object = Object::new(object_id, tenant_id);
    objects
        .create_object(&scope, &object)
        .await
        .expect("object should be created");

    let version = ObjectVersion::new(version_id, tenant_id, object_id, 1)
        .expect("positive version number is valid");
    objects
        .create_draft_version(&scope, &version)
        .await
        .expect("draft version should be created");

    Fixture {
        pool,
        scope,
        object_id,
        version_id,
    }
}

fn build_snapshot(fixture: &Fixture, bytes: &[u8]) -> qembed_domain::snapshot::Snapshot {
    SnapshotBuilder::new(
        SnapshotId::new(),
        fixture.scope.tenant_id(),
        fixture.object_id,
        fixture.version_id,
        1,
        bytes.to_vec(),
    )
    .build()
    .expect("snapshot should be built")
}

#[tokio::test]
async fn snapshot_insert_read_is_immutable_and_preserves_exact_bytes() {
    let fixture = setup().await;
    let repository = SnapshotRepository::new(fixture.pool.clone());
    let bytes = br#"{"config":{"enabled":true},"name":"demo"}"#;
    let snapshot = build_snapshot(&fixture, bytes);
    let snapshot_id = snapshot.id();
    let expected_hash = *snapshot.content_hash();

    repository
        .insert(&fixture.scope, &snapshot)
        .await
        .expect("snapshot should be inserted");

    let loaded = repository
        .get(&fixture.scope, snapshot_id)
        .await
        .expect("snapshot should be read")
        .expect("snapshot should exist");

    assert_eq!(loaded.canonical_json(), bytes);
    assert_eq!(loaded.content_hash(), &expected_hash);
    assert_eq!(loaded.object_id(), fixture.object_id);
    assert_eq!(loaded.object_version_id(), fixture.version_id);

    let update = sqlx::query(
        "UPDATE snapshots SET canonical_json = $1 WHERE tenant_id = $2::uuid AND snapshot_id = $3::uuid",
    )
    .bind(br#"{"tampered":true}"#.as_slice())
    .bind(fixture.scope.tenant_id().to_string())
    .bind(snapshot_id.to_string())
    .execute(&fixture.pool)
    .await
    .expect_err("snapshot updates must be rejected");
    assert_eq!(
        update
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("55000")
    );

    let delete =
        sqlx::query("DELETE FROM snapshots WHERE tenant_id = $1::uuid AND snapshot_id = $2::uuid")
            .bind(fixture.scope.tenant_id().to_string())
            .bind(snapshot_id.to_string())
            .execute(&fixture.pool)
            .await
            .expect_err("snapshot deletes must be rejected");
    assert_eq!(
        delete
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("55000")
    );

    fixture.pool.close().await;
}

#[tokio::test]
async fn repository_reads_source_bytes_instead_of_jsonb_projection() {
    let fixture = setup().await;
    let repository = SnapshotRepository::new(fixture.pool.clone());
    let bytes = br#"{\"a\":1,\"z\":0}"#;
    let snapshot = build_snapshot(&fixture, bytes);

    repository
        .insert(&fixture.scope, &snapshot)
        .await
        .expect("snapshot should be inserted");

    let (stored_bytes, stored_hash, projection): (Vec<u8>, Vec<u8>, String) =
        sqlx::query_as(
            "SELECT canonical_json, content_hash, json_projection::text FROM snapshots WHERE tenant_id = $1::uuid AND snapshot_id = $2::uuid",
        )
        .bind(fixture.scope.tenant_id().to_string())
        .bind(snapshot.id().to_string())
        .fetch_one(&fixture.pool)
        .await
        .expect("stored snapshot columns should be readable");

    assert_eq!(stored_bytes, bytes);
    assert_eq!(
        stored_hash,
        Sha256::digest(bytes).as_slice(),
        "the persisted digest must cover the exact BYTEA source"
    );
    assert_ne!(
        projection.as_bytes(),
        bytes,
        "JSONB's textual projection must not be mistaken for the source bytes"
    );

    let loaded = repository
        .get(&fixture.scope, snapshot.id())
        .await
        .expect("repository read should succeed")
        .expect("snapshot should exist");
    assert_eq!(loaded.canonical_json(), bytes);

    fixture.pool.close().await;
}

#[tokio::test]
async fn repository_rejects_noncanonical_source_even_when_hash_and_projection_match() {
    let fixture = setup().await;
    let repository = SnapshotRepository::new(fixture.pool.clone());
    let snapshot_id = SnapshotId::new();
    let bytes = br#"{ "a":1}"#;
    let content_hash = Sha256::digest(bytes).to_vec();

    // Bypass the repository to simulate a legacy/corrupt row. The database
    // projection and digest are internally consistent, but the source bytes
    // are not RFC 8785 canonical and must be rejected by Domain on read.
    sqlx::query(
        "INSERT INTO snapshots (tenant_id, snapshot_id, object_id, object_version_id, schema_version, canonical_json, content_hash, json_projection) VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, 1, $5, $6, convert_from($5, 'UTF8')::jsonb)",
    )
    .bind(fixture.scope.tenant_id().to_string())
    .bind(snapshot_id.to_string())
    .bind(fixture.object_id.to_string())
    .bind(fixture.version_id.to_string())
    .bind(bytes.as_slice())
    .bind(content_hash)
    .execute(&fixture.pool)
    .await
    .expect("database projection guard should accept matching JSON values");

    let error = repository
        .get(&fixture.scope, snapshot_id)
        .await
        .expect_err("noncanonical source bytes must be rejected");
    assert!(matches!(
        error,
        SnapshotRepositoryError::Domain(SnapshotError::Jcs(JcsError::NonCanonicalJson))
    ));

    fixture.pool.close().await;
}

#[tokio::test]
async fn repository_rejects_persisted_hash_mismatch() {
    let fixture = setup().await;
    let repository = SnapshotRepository::new(fixture.pool.clone());
    let snapshot = build_snapshot(&fixture, br#"{"trusted":true}"#);

    // Simulate a malformed row written outside the repository API. The read
    // boundary must recompute SHA-256 from canonical_json and reject the row.
    sqlx::query(
        "INSERT INTO snapshots (tenant_id, snapshot_id, object_id, object_version_id, schema_version, canonical_json, content_hash, json_projection) VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, 1, $5, $6, convert_from($5, 'UTF8')::jsonb)",
    )
    .bind(fixture.scope.tenant_id().to_string())
    .bind(snapshot.id().to_string())
    .bind(fixture.object_id.to_string())
    .bind(fixture.version_id.to_string())
    .bind(snapshot.canonical_json())
    .bind(vec![0_u8; 32])
    .execute(&fixture.pool)
    .await
    .expect("test should be able to insert deliberately inconsistent hash");

    let error = repository
        .get(&fixture.scope, snapshot.id())
        .await
        .expect_err("hash mismatch must not be accepted");
    assert!(matches!(
        error,
        SnapshotRepositoryError::Domain(SnapshotError::HashMismatch)
    ));

    fixture.pool.close().await;
}

#[tokio::test]
async fn database_rejects_a_mismatched_jsonb_projection() {
    let fixture = setup().await;
    let snapshot = build_snapshot(&fixture, br#"{"source":"canonical"}"#);

    let error = sqlx::query(
        "INSERT INTO snapshots (tenant_id, snapshot_id, object_id, object_version_id, schema_version, canonical_json, content_hash, json_projection) VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, 1, $5, $6, $7::jsonb)",
    )
    .bind(fixture.scope.tenant_id().to_string())
    .bind(snapshot.id().to_string())
    .bind(fixture.object_id.to_string())
    .bind(fixture.version_id.to_string())
    .bind(snapshot.canonical_json())
    .bind(snapshot.content_hash().to_vec())
    .bind(r#"{"source":"tampered"}"#)
    .execute(&fixture.pool)
    .await
    .expect_err("projection guard must reject a mismatch");

    assert_eq!(
        error
            .as_database_error()
            .and_then(|database| database.code())
            .as_deref(),
        Some("23514")
    );

    fixture.pool.close().await;
}
