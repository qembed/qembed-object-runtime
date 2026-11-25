mod common;

use qembed_domain::{
    ids::{KnowledgeChunkId, KnowledgeDocumentId, ObjectId, TenantId},
    knowledge::{PinnedKnowledgeRevision, RevisionNumber},
    object::Object,
    scope::Scope,
    test_support::scope_for_tenant,
};
use qembed_store::{
    knowledge_retrieval::{PgKnowledgeRetrievalError, PgKnowledgeRetrievalStore},
    object_repository::ObjectRepository,
    run_migrations,
};
use sqlx::PgPool;

struct FixtureChunk {
    document_id: KnowledgeDocumentId,
    chunk_id: KnowledgeChunkId,
}

async fn setup() -> (PgPool, PgKnowledgeRetrievalStore, Scope, ObjectId) {
    let pool = common::connect().await;
    run_migrations(&pool).await.expect("migrations apply");

    let context = common::signed_context(&pool).await;
    let tenant_id = TenantId::new();
    let scope = scope_for_tenant(tenant_id);
    let object_id = ObjectId::new();
    ObjectRepository::new(context.clone())
        .create_object(&scope, &Object::new(object_id, tenant_id))
        .await
        .expect("object created");

    (
        pool,
        PgKnowledgeRetrievalStore::new(context),
        scope,
        object_id,
    )
}

async fn insert_chunk(
    pool: &PgPool,
    scope: &Scope,
    object_id: ObjectId,
    status: &str,
    valid_from: Option<i64>,
    valid_to: Option<i64>,
    erased: bool,
    content: &str,
) -> FixtureChunk {
    let document_id = KnowledgeDocumentId::new();
    let chunk_id = KnowledgeChunkId::new();

    sqlx::query(
        "INSERT INTO knowledge_documents \
         (id, tenant_id, object_id, doc_key, title, status, valid_from_revision, valid_to_revision) \
         VALUES ($1::uuid, $2::uuid, $3::uuid, $4, 'Retrieval fixture', $5, $6, $7)",
    )
    .bind(document_id.to_string())
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .bind(format!("{}-{status}-{content}", object_id))
    .bind(status)
    .bind(valid_from)
    .bind(valid_to)
    .execute(pool)
    .await
    .expect("fixture document inserted");

    sqlx::query(
        "INSERT INTO knowledge_chunks \
         (id, tenant_id, object_id, document_id, content, erased, position, valid_from_revision, valid_to_revision) \
         VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, $5, $6, 0, $7, $8)",
    )
    .bind(chunk_id.to_string())
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .bind(document_id.to_string())
    .bind(content)
    .bind(erased)
    .bind(valid_from)
    .bind(valid_to)
    .execute(pool)
    .await
    .expect("fixture chunk inserted");

    FixtureChunk {
        document_id,
        chunk_id,
    }
}

async fn set_current_revision(pool: &PgPool, scope: &Scope, object_id: ObjectId, revision: i64) {
    sqlx::query(
        "UPDATE knowledge_revisions SET current_revision = $3 \
         WHERE tenant_id = $1::uuid AND object_id = $2::uuid",
    )
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .bind(revision)
    .execute(pool)
    .await
    .expect("fixture revision pointer updated");
}

#[tokio::test]
async fn pinned_revision_excludes_staged_future_closed_and_erased_chunks() {
    let (pool, store, scope, object_id) = setup().await;

    let staged = insert_chunk(&pool, &scope, object_id, "staged", None, None, false, "staged")
        .await;
    let indexed = insert_chunk(&pool, &scope, object_id, "indexed", None, None, false, "indexed")
        .await;
    let live = insert_chunk(&pool, &scope, object_id, "live", Some(2), None, false, "live-at-two")
        .await;
    let retired =
        insert_chunk(&pool, &scope, object_id, "retired", Some(1), Some(3), false, "retired-at-two")
            .await;
    let future =
        insert_chunk(&pool, &scope, object_id, "live", Some(3), None, false, "future-winner")
            .await;
    let closed =
        insert_chunk(&pool, &scope, object_id, "retired", Some(1), Some(2), false, "closed-at-two")
            .await;
    let erased =
        insert_chunk(&pool, &scope, object_id, "erased", Some(1), None, true, "")
            .await;

    set_current_revision(&pool, &scope, object_id, 3).await;

    let at_revision_two = store
        .visible_chunks(
            &scope,
            PinnedKnowledgeRevision::new(
                scope.tenant_id(),
                object_id,
                RevisionNumber::new(2),
            ),
        )
        .await
        .expect("revision two candidates load");
    let ids = at_revision_two.iter().map(|chunk| chunk.id()).collect::<Vec<_>>();

    assert!(ids.contains(&live.chunk_id));
    assert!(ids.contains(&retired.chunk_id));
    assert!(!ids.contains(&staged.chunk_id));
    assert!(!ids.contains(&indexed.chunk_id));
    assert!(!ids.contains(&future.chunk_id));
    assert!(!ids.contains(&closed.chunk_id));
    assert!(!ids.contains(&erased.chunk_id));

    let at_revision_one = store
        .visible_chunks(
            &scope,
            PinnedKnowledgeRevision::new(
                scope.tenant_id(),
                object_id,
                RevisionNumber::new(1),
            ),
        )
        .await
        .expect("revision one candidates load");
    let revision_one_ids = at_revision_one
        .iter()
        .map(|chunk| chunk.id())
        .collect::<Vec<_>>();
    assert!(revision_one_ids.contains(&retired.chunk_id));
    assert!(revision_one_ids.contains(&closed.chunk_id));
    assert!(!revision_one_ids.contains(&live.chunk_id));

    // The function is the production pre-filter boundary. Materializing it
    // before ranking prevents the high-scoring future chunk from consuming top-1.
    let correct_top_one: Option<uuid::Uuid> = sqlx::query_scalar(
        "WITH visible_candidates AS MATERIALIZED ( \
             SELECT * FROM knowledge_visible_chunks_at($1::uuid, $2::uuid, 2) \
         ), ranked AS ( \
             SELECT id, CASE content \
                 WHEN 'future-winner' THEN 0.001 \
                 WHEN 'retired-at-two' THEN 0.10 \
                 ELSE 0.20 END AS distance \
             FROM visible_candidates \
         ) \
         SELECT id FROM ranked ORDER BY distance, id LIMIT 1",
    )
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .fetch_optional(&pool)
    .await
    .expect("pre-filtered top-one query succeeds");
    assert_eq!(correct_top_one, Some(retired.chunk_id.into_uuid()));

    // Regression control: top-k before revision filtering selects the future
    // chunk and leaves no result after the visibility join.
    let wrong_top_one: Option<uuid::Uuid> = sqlx::query_scalar(
        "WITH unfiltered_top_one AS ( \
             SELECT id FROM knowledge_chunks \
              WHERE tenant_id = $1::uuid AND object_id = $2::uuid AND NOT erased \
              ORDER BY CASE content \
                  WHEN 'future-winner' THEN 0.001 \
                  WHEN 'retired-at-two' THEN 0.10 \
                  ELSE 0.20 END, id \
              LIMIT 1 \
         ) \
         SELECT u.id FROM unfiltered_top_one u \
         JOIN knowledge_visible_chunks_at($1::uuid, $2::uuid, 2) v ON v.id = u.id",
    )
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .fetch_optional(&pool)
    .await
    .expect("regression control query succeeds");
    assert_eq!(wrong_top_one, None);

    assert_ne!(staged.document_id, live.document_id);
    pool.close().await;
}

#[tokio::test]
async fn future_and_cross_tenant_pins_are_rejected() {
    let (pool, store, scope, object_id) = setup().await;

    let future = store
        .visible_chunks(
            &scope,
            PinnedKnowledgeRevision::new(
                scope.tenant_id(),
                object_id,
                RevisionNumber::new(1),
            ),
        )
        .await;
    assert!(matches!(future, Err(PgKnowledgeRetrievalError::FutureRevision)));

    let foreign = store
        .visible_chunks(
            &scope,
            PinnedKnowledgeRevision::new(
                TenantId::new(),
                object_id,
                RevisionNumber::INITIAL,
            ),
        )
        .await;
    assert!(matches!(
        foreign,
        Err(PgKnowledgeRetrievalError::PinnedTenantMismatch)
    ));

    pool.close().await;
}
