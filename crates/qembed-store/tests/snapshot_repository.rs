mod common;

use qembed_domain::ids::{ObjectId, ObjectVersionId, SnapshotId, TenantId};
use qembed_domain::jcs::JcsError;
use qembed_domain::object::Object;
use qembed_domain::object_version::ObjectVersion;
use qembed_domain::scope::Scope;
use qembed_domain::snapshot::{SnapshotBuilder, SnapshotError};
use qembed_domain::test_support::scope_for_tenant;
use qembed_store::object_repository::{DraftCreation, ObjectRepository};
use qembed_store::{PgContext, run_migrations};
use qembed_store::snapshot_repository::{SnapshotRepository, SnapshotRepositoryError};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

struct Fixture {
    pool: PgPool,
    context: PgContext,
    scope: Scope,
    object_id: ObjectId,
}

async fn setup() -> Fixture {
    let pool = common::connect().await;
    run_migrations(&pool)
        .await
        .expect("migrations should be applied");
    let context = common::signed_context(&pool).await;
    let tenant_id = TenantId::new();
    let scope = scope_for_tenant(tenant_id);
    let object_id = ObjectId::new();
    ObjectRepository::new(context.clone())
        .create_object(&scope, &Object::new(object_id, tenant_id))
        .await
        .expect("object should be created");
    Fixture {
        pool,
        context,
        scope,
        object_id,
    }
}

fn build_snapshot(
    fixture: &Fixture,
    version_id: ObjectVersionId,
    bytes: &[u8],
) -> qembed_domain::snapshot::Snapshot {
    SnapshotBuilder::new(
        SnapshotId::new(),
        fixture.scope.tenant_id(),
        fixture.object_id,
        version_id,
        1,
        bytes.to_vec(),
    )
    .build()
    .expect("snapshot should be built")
}

async fn persist_pair(
    fixture: &Fixture,
    number: u64,
    bytes: &[u8],
) -> qembed_domain::snapshot::Snapshot {
    let version = ObjectVersion::new(
        ObjectVersionId::new(),
        fixture.scope.tenant_id(),
        fixture.object_id,
        number,
    )
    .expect("positive version number is valid");
    let snapshot = build_snapshot(fixture, version.id(), bytes);
    let result = ObjectRepository::new(fixture.context.clone())
        .create_draft_version_with_snapshot(&fixture.scope, &version, &snapshot)
        .await
        .expect("version and snapshot should be atomically persisted");
    assert_eq!(
        result,
        DraftCreation::Created {
            version_id: version.id(),
            snapshot_id: snapshot.id()
        }
    );
    snapshot
}

#[tokio::test]
async fn snapshot_read_is_immutable_and_preserves_exact_bytes() {
    let fixture = setup().await;
    let repository = SnapshotRepository::new(fixture.context.clone());
    let bytes = br#"{"config":{"enabled":true},"name":"demo"}"#;
    let snapshot = persist_pair(&fixture, 1, bytes).await;
    let loaded = repository
        .get(&fixture.scope, snapshot.id())
        .await
        .expect("snapshot should be read")
        .expect("snapshot should exist");
    assert_eq!(loaded.canonical_json(), bytes);
    assert_eq!(loaded.content_hash(), snapshot.content_hash());
    assert_eq!(loaded.object_id(), fixture.object_id);
    assert_eq!(loaded.object_version_id(), snapshot.object_version_id());

    let update = sqlx::query(
        "UPDATE snapshots SET canonical_json = $1 WHERE tenant_id = $2::uuid AND snapshot_id = $3::uuid",
    )
    .bind(br#"{"tampered":true}"#.as_slice())
    .bind(fixture.scope.tenant_id().to_string())
    .bind(snapshot.id().to_string())
    .execute(&fixture.pool)
    .await
    .expect_err("snapshot updates must be rejected");
    assert_eq!(
        update.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("55000")
    );

    let delete =
        sqlx::query("DELETE FROM snapshots WHERE tenant_id = $1::uuid AND snapshot_id = $2::uuid")
            .bind(fixture.scope.tenant_id().to_string())
            .bind(snapshot.id().to_string())
            .execute(&fixture.pool)
            .await
            .expect_err("snapshot deletes must be rejected");
    assert_eq!(
        delete.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("55000")
    );
    fixture.pool.close().await;
}

#[tokio::test]
async fn repository_reads_source_bytes_instead_of_jsonb_projection() {
    let fixture = setup().await;
    let repository = SnapshotRepository::new(fixture.context.clone());
    let bytes = br#"{"a":1,"z":0}"#;
    let snapshot = persist_pair(&fixture, 1, bytes).await;
    let (stored_bytes, stored_hash, projection): (Vec<u8>, Vec<u8>, String) = sqlx::query_as(
        "SELECT canonical_json, content_hash, json_projection::text FROM snapshots WHERE tenant_id = $1::uuid AND snapshot_id = $2::uuid",
    ).bind(fixture.scope.tenant_id().to_string()).bind(snapshot.id().to_string())
     .fetch_one(&fixture.pool).await.expect("stored snapshot columns should be readable");
    assert_eq!(stored_bytes, bytes);
    assert_eq!(stored_hash, Sha256::digest(bytes).to_vec());
    assert_ne!(projection.as_bytes(), bytes);
    let loaded = repository
        .get(&fixture.scope, snapshot.id())
        .await
        .expect("read should succeed")
        .expect("snapshot should exist");
    assert_eq!(loaded.canonical_json(), bytes);
    fixture.pool.close().await;
}

#[tokio::test]
async fn repository_rejects_noncanonical_source_even_when_hash_and_projection_match() {
    let fixture = setup().await;
    let repository = SnapshotRepository::new(fixture.context.clone());
    let seed = persist_pair(&fixture, 1, br#"{"seed":true}"#).await;
    let snapshot_id = SnapshotId::new();
    let bytes = br#"{ "a":1}"#;
    sqlx::query(
        "INSERT INTO snapshots (tenant_id, snapshot_id, object_id, object_version_id, schema_version, canonical_json, content_hash, json_projection) VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, 1, $5, $6, convert_from($5, 'UTF8')::jsonb)",
    )
    .bind(fixture.scope.tenant_id().to_string())
    .bind(snapshot_id.to_string())
    .bind(fixture.object_id.to_string())
    .bind(seed.object_version_id().to_string())
    .bind(bytes.as_slice())
    .bind(Sha256::digest(bytes).to_vec())
    .execute(&fixture.pool)
    .await
    .expect("projection and digest should be internally consistent");
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
    let repository = SnapshotRepository::new(fixture.context.clone());
    let seed = persist_pair(&fixture, 1, br#"{"seed":true}"#).await;
    let snapshot_id = SnapshotId::new();
    let bytes = br#"{"trusted":true}"#;
    sqlx::query(
        "INSERT INTO snapshots (tenant_id, snapshot_id, object_id, object_version_id, schema_version, canonical_json, content_hash, json_projection) VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, 1, $5, $6, convert_from($5, 'UTF8')::jsonb)",
    )
    .bind(fixture.scope.tenant_id().to_string())
    .bind(snapshot_id.to_string())
    .bind(fixture.object_id.to_string())
    .bind(seed.object_version_id().to_string())
    .bind(bytes.as_slice())
    .bind(vec![0_u8; 32])
    .execute(&fixture.pool)
    .await
    .expect("deliberately inconsistent hash row should be insertable");
    let error = repository
        .get(&fixture.scope, snapshot_id)
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
    let seed = persist_pair(&fixture, 1, br#"{"seed":true}"#).await;
    let snapshot_id = SnapshotId::new();
    let bytes = br#"{"source":"canonical"}"#;
    let error = sqlx::query(
        "INSERT INTO snapshots (tenant_id, snapshot_id, object_id, object_version_id, schema_version, canonical_json, content_hash, json_projection) VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, 1, $5, $6, $7::jsonb)",
    )
    .bind(fixture.scope.tenant_id().to_string())
    .bind(snapshot_id.to_string())
    .bind(fixture.object_id.to_string())
    .bind(seed.object_version_id().to_string())
    .bind(bytes.as_slice())
    .bind(Sha256::digest(bytes).to_vec())
    .bind(r#"{"source":"tampered"}"#)
    .execute(&fixture.pool)
    .await
    .expect_err("projection guard must reject a mismatch");
    assert_eq!(
        error.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("23514")
    );
    fixture.pool.close().await;
}
