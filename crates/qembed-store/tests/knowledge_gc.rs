mod common;

use qembed_domain::{
    ids::{
        CallerId, InvocationId, ObjectId, ObjectVersionId, SnapshotId, TenantId, TraceId,
        KnowledgeChunkId, KnowledgeDocumentId,
    },
    object::Object,
    scope::Scope,
    test_support::scope_for_tenant,
};
use qembed_store::{
    knowledge_gc::{PgKnowledgeGarbageCollector, PgKnowledgeGcError},
    object_repository::ObjectRepository,
    run_migrations,
};
use sqlx::PgPool;

async fn setup() -> (PgPool, qembed_store::PgContext, Scope, ObjectId, InvocationId, KnowledgeDocumentId) {
    let pool = common::connect().await;
    run_migrations(&pool).await.expect("migrations apply");
    let context = common::signed_context(&pool).await;
    let tenant_id = TenantId::new();
    let scope = scope_for_tenant(tenant_id);
    let object_id = ObjectId::new();

    sqlx::query("INSERT INTO tenants (id) VALUES ($1::uuid)")
        .bind(tenant_id.to_string())
        .execute(&pool)
        .await
        .expect("tenant fixture inserted");

    ObjectRepository::new(context.clone())
        .create_object(&scope, &Object::new(object_id, tenant_id))
        .await
        .expect("Object created");

    let caller_id = CallerId::new();
    sqlx::query(
        "INSERT INTO callers (id, tenant_id, token_hash, caller_kind) VALUES ($1::uuid, $2::uuid, $3, 'Service')",
    )
    .bind(caller_id.to_string())
    .bind(tenant_id.to_string())
    .bind(caller_id.into_uuid().as_bytes().to_vec())
    .execute(&pool)
    .await
    .expect("Caller fixture inserted");

    let version_id = ObjectVersionId::new();
    sqlx::query(
        "INSERT INTO object_versions (tenant_id, object_id, object_version_id, version_number, status) VALUES ($1::uuid, $2::uuid, $3::uuid, 1, 'published')",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .bind(version_id.to_string())
    .execute(&pool)
    .await
    .expect("published Object version inserted");

    let snapshot_id = SnapshotId::new();
    let projection = br#"{"limits":{"max_tokens_per_invocation":10000}}"#;
    sqlx::query(
        "INSERT INTO snapshots (tenant_id, snapshot_id, object_id, object_version_id, schema_version, canonical_json, content_hash, json_projection) VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, 1, $5, decode(repeat('00',32),'hex'), convert_from($5, 'UTF8')::jsonb)",
    )
    .bind(tenant_id.to_string())
    .bind(snapshot_id.to_string())
    .bind(object_id.to_string())
    .bind(version_id.to_string())
    .bind(projection.to_vec())
    .execute(&pool)
    .await
    .expect("snapshot fixture inserted");

    sqlx::query(
        "UPDATE objects SET state='active', published_version_id=$3::uuid WHERE tenant_id=$1::uuid AND object_id=$2::uuid",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .bind(version_id.to_string())
    .execute(&pool)
    .await
    .expect("Object published");

    sqlx::query(
        "INSERT INTO knowledge_revision_log (tenant_id, object_id, revision) VALUES ($1::uuid, $2::uuid, 1)",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .execute(&pool)
    .await
    .expect("revision log inserted");
    sqlx::query(
        "UPDATE knowledge_revisions SET current_revision=1 WHERE tenant_id=$1::uuid AND object_id=$2::uuid",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .execute(&pool)
    .await
    .expect("revision pointer advanced");

    let document_id = KnowledgeDocumentId::new();
    let chunk_id = KnowledgeChunkId::new();
    sqlx::query(
        "INSERT INTO knowledge_documents (id, tenant_id, object_id, doc_key, title, status, valid_from_revision, valid_to_revision) VALUES ($1::uuid, $2::uuid, $3::uuid, 'retired-fixture', 'Retired fixture', 'retired', 1, 2)",
    )
    .bind(document_id.to_string())
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .execute(&pool)
    .await
    .expect("retired document inserted");
    sqlx::query(
        "INSERT INTO knowledge_chunks (id, tenant_id, object_id, document_id, content, position, valid_from_revision, valid_to_revision) VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, 'historical content', 0, 1, 2)",
    )
    .bind(chunk_id.to_string())
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .bind(document_id.to_string())
    .execute(&pool)
    .await
    .expect("retired chunk inserted");

    let invocation_id = InvocationId::new();
    sqlx::query(
        "INSERT INTO object_invocations (id, tenant_id, object_id, caller_id, snapshot_id, knowledge_revision, pool, status, trace_id) VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, $5::uuid, 1, 'authenticated', 'Received', $6::uuid)",
    )
    .bind(invocation_id.to_string())
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .bind(caller_id.to_string())
    .bind(snapshot_id.to_string())
    .bind(TraceId::new().to_string())
    .execute(&pool)
    .await
    .expect("active invocation inserted");

    sqlx::query(
        "INSERT INTO knowledge_revision_pins (tenant_id, object_id, invocation_id, revision) VALUES ($1::uuid, $2::uuid, $3::uuid, 1)",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .bind(invocation_id.to_string())
    .execute(&pool)
    .await
    .expect("invocation revision pin inserted");

    (pool, context, scope, object_id, invocation_id, document_id)
}

#[tokio::test]
async fn active_invocation_pin_blocks_gc_until_terminal_transition_releases_it() {
    let (pool, context, scope, object_id, invocation_id, document_id) = setup().await;
    let collector = PgKnowledgeGarbageCollector::new(context);

    let protected = collector
        .collect_retired_documents(&scope, object_id, 10)
        .await
        .expect("GC should run");
    assert_eq!(protected, 0, "active invocation must protect its revision");

    let document_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM knowledge_documents WHERE tenant_id=$1::uuid AND object_id=$2::uuid AND id=$3::uuid",
    )
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .bind(document_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("document remains readable");
    assert_eq!(document_count, 1);

    sqlx::query(
        "UPDATE object_invocations SET status='Cancelled', finished_at=now(), version=version+1 WHERE tenant_id=$1::uuid AND object_id=$2::uuid AND id=$3::uuid",
    )
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .bind(invocation_id.to_string())
    .execute(&pool)
    .await
    .expect("terminal transition releases the pin");

    let pin_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM knowledge_revision_pins WHERE tenant_id=$1::uuid AND object_id=$2::uuid AND invocation_id=$3::uuid",
    )
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .bind(invocation_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("pin count readable");
    assert_eq!(pin_count, 0);

    let deleted = collector
        .collect_retired_documents(&scope, object_id, 10)
        .await
        .expect("GC can collect after invocation is terminal");
    assert_eq!(deleted, 1);

    let remaining_chunks: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM knowledge_chunks WHERE tenant_id=$1::uuid AND object_id=$2::uuid",
    )
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("chunk count readable");
    assert_eq!(remaining_chunks, 0, "document deletion cascades to chunks");

    assert!(matches!(
        collector.collect_retired_documents(&scope, object_id, 0).await,
        Err(PgKnowledgeGcError::InvalidBatchSize)
    ));
    pool.close().await;
}
