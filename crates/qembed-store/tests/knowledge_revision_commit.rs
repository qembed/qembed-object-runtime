mod common;

use qembed_domain::{
    ids::{KnowledgeChunkId, KnowledgeDocumentId, ObjectId, TenantId},
    object::Object,
    scope::Scope,
    test_support::scope_for_tenant,
};
use qembed_store::{
    knowledge_repository::{KnowledgeRevisionCommitError, PgKnowledgeRepository},
    object_repository::ObjectRepository,
    run_migrations,
};
use sqlx::PgPool;

const TEST_MODEL_ID: &str = "rust066-integration-model";

async fn setup() -> (PgPool, PgKnowledgeRepository, Scope, ObjectId) {
    let pool = common::connect().await;
    run_migrations(&pool).await.expect("migrations should apply");

    sqlx::query(
        "INSERT INTO embedding_models (id, provider, model_name, dimension, distance_metric, active) \
         VALUES ($1, 'test', 'rust066-integration-model', 2, 'cosine', true) \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(TEST_MODEL_ID)
    .execute(&pool)
    .await
    .expect("the shared test embedding model should exist");

    let context = common::signed_context(&pool).await;
    let object_repository = ObjectRepository::new(context.clone());
    let knowledge_repository = PgKnowledgeRepository::new(context);
    let tenant_id = TenantId::new();
    let scope = scope_for_tenant(tenant_id);
    let object_id = ObjectId::new();
    object_repository
        .create_object(&scope, &Object::new(object_id, tenant_id))
        .await
        .expect("object should be created");

    (pool, knowledge_repository, scope, object_id)
}

async fn add_indexed_document(
    pool: &PgPool,
    scope: &Scope,
    object_id: ObjectId,
    doc_key: &str,
    with_embedding: bool,
) -> (KnowledgeDocumentId, KnowledgeChunkId) {
    let document_id = KnowledgeDocumentId::new();
    let chunk_id = KnowledgeChunkId::new();

    sqlx::query(
        "INSERT INTO knowledge_documents (id, tenant_id, object_id, doc_key, title, status) \
         VALUES ($1::uuid, $2::uuid, $3::uuid, $4, 'Integration document', 'indexed')",
    )
    .bind(document_id.to_string())
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .bind(doc_key)
    .execute(pool)
    .await
    .expect("indexed document should be inserted");

    sqlx::query(
        "INSERT INTO knowledge_chunks (id, tenant_id, object_id, document_id, content, position) \
         VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, 'indexed test content', 0)",
    )
    .bind(chunk_id.to_string())
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .bind(document_id.to_string())
    .execute(pool)
    .await
    .expect("document chunk should be inserted");

    if with_embedding {
        sqlx::query(
            "INSERT INTO knowledge_chunk_embeddings (tenant_id, object_id, chunk_id, model_id, embedding) \
             VALUES ($1::uuid, $2::uuid, $3::uuid, $4, '[0.1,0.2]'::vector)",
        )
        .bind(scope.tenant_id().to_string())
        .bind(object_id.to_string())
        .bind(chunk_id.to_string())
        .bind(TEST_MODEL_ID)
        .execute(pool)
        .await
        .expect("chunk embedding should be inserted");
    }

    (document_id, chunk_id)
}

#[tokio::test]
async fn commit_publishes_documents_retires_replacements_and_appends_log_event_outbox() {
    let (pool, repository, scope, object_id) = setup().await;
    let (first_document, first_chunk) =
        add_indexed_document(&pool, &scope, object_id, "handbook", true).await;

    let first = repository
        .commit_knowledge_revision(&scope, object_id)
        .await
        .expect("complete indexed document should commit at revision one");
    assert_eq!(first.previous_revision().value(), 0);
    assert_eq!(first.revision().value(), 1);
    assert_eq!(first.published_documents(), &[first_document]);
    assert!(first.retired_documents().is_empty());

    let (replacement_document, replacement_chunk) =
        add_indexed_document(&pool, &scope, object_id, "handbook", true).await;
    let second = repository
        .commit_knowledge_revision(&scope, object_id)
        .await
        .expect("replacement document should commit at revision two");
    assert_eq!(second.previous_revision().value(), 1);
    assert_eq!(second.revision().value(), 2);
    assert_eq!(second.published_documents(), &[replacement_document]);
    assert_eq!(second.retired_documents(), &[first_document]);

    let documents: Vec<(String, Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT status, valid_from_revision, valid_to_revision FROM knowledge_documents \
         WHERE tenant_id = $1::uuid AND object_id = $2::uuid AND id IN ($3::uuid, $4::uuid) \
         ORDER BY valid_from_revision",
    )
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .bind(first_document.to_string())
    .bind(replacement_document.to_string())
    .fetch_all(&pool)
    .await
    .expect("document intervals should be readable");
    assert_eq!(
        documents,
        vec![
            ("retired".to_owned(), Some(1), Some(2)),
            ("live".to_owned(), Some(2), None)
        ]
    );

    let chunk_intervals: Vec<(Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT valid_from_revision, valid_to_revision FROM knowledge_chunks \
         WHERE tenant_id = $1::uuid AND object_id = $2::uuid AND id IN ($3::uuid, $4::uuid) \
         ORDER BY valid_from_revision",
    )
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .bind(first_chunk.to_string())
    .bind(replacement_chunk.to_string())
    .fetch_all(&pool)
    .await
    .expect("chunk intervals should be synchronized by the document trigger");
    assert_eq!(chunk_intervals, vec![(Some(1), Some(2)), (Some(2), None)]);

    let persisted: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT r.current_revision, \
                (SELECT count(*) FROM knowledge_revision_log l WHERE l.tenant_id = r.tenant_id AND l.object_id = r.object_id), \
                (SELECT count(*) FROM events e WHERE e.tenant_id = r.tenant_id AND e.aggregate_id = r.object_id AND e.event_type = 'knowledge.revision_committed'), \
                (SELECT count(*) FROM outbox_messages o WHERE o.tenant_id = r.tenant_id AND o.topic = 'knowledge.revision_committed') \
           FROM knowledge_revisions r WHERE r.tenant_id = $1::uuid AND r.object_id = $2::uuid",
    )
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("revision, log, event and outbox should be readable");
    assert_eq!(persisted, (2, 2, 2, 2));

    pool.close().await;
}

#[tokio::test]
async fn incomplete_indexing_does_not_advance_revision_or_publish_document() {
    let (pool, repository, scope, object_id) = setup().await;
    let (document_id, _) =
        add_indexed_document(&pool, &scope, object_id, "incomplete", false).await;

    let result = repository
        .commit_knowledge_revision(&scope, object_id)
        .await;
    assert!(matches!(
        result,
        Err(KnowledgeRevisionCommitError::IncompleteIndexing)
    ));

    let state: (i64, String, i64) = sqlx::query_as(
        "SELECT r.current_revision, d.status, \
                (SELECT count(*) FROM knowledge_revision_log l WHERE l.tenant_id = r.tenant_id AND l.object_id = r.object_id) \
           FROM knowledge_revisions r JOIN knowledge_documents d \
             ON d.tenant_id = r.tenant_id AND d.object_id = r.object_id \
          WHERE r.tenant_id = $1::uuid AND r.object_id = $2::uuid AND d.id = $3::uuid",
    )
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .bind(document_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("failed commit must leave its source state intact");
    assert_eq!(state, (0, "indexed".to_owned(), 0));

    pool.close().await;
}

#[tokio::test]
async fn outbox_failure_rolls_back_revision_intervals_and_revision_log() {
    let (pool, repository, scope, object_id) = setup().await;
    let (document_id, _) =
        add_indexed_document(&pool, &scope, object_id, "rollback", true).await;

    let suffix = scope.tenant_id().to_string().replace('-', "");
    let function_name = format!("test_knowledge_outbox_failure_{suffix}");
    let trigger_name = format!("test_knowledge_outbox_failure_trigger_{suffix}");
    sqlx::query(&format!(
        "CREATE FUNCTION {function_name}() RETURNS trigger LANGUAGE plpgsql AS $failure$ \
         BEGIN RAISE EXCEPTION 'forced knowledge outbox failure'; END $failure$"
    ))
    .execute(&pool)
    .await
    .expect("failure function should be created");
    sqlx::query(&format!(
        "CREATE TRIGGER {trigger_name} BEFORE INSERT ON outbox_messages \
         FOR EACH ROW WHEN (NEW.tenant_id = '{}'::uuid) EXECUTE FUNCTION {function_name}()",
        scope.tenant_id()
    ))
    .execute(&pool)
    .await
    .expect("tenant-scoped outbox failure trigger should be created");

    let result = repository
        .commit_knowledge_revision(&scope, object_id)
        .await;
    assert!(matches!(
        result,
        Err(KnowledgeRevisionCommitError::Database(_))
    ));

    let state: (i64, String, Option<i64>, i64, i64, i64) = sqlx::query_as(
        "SELECT r.current_revision, d.status, d.valid_from_revision, \
                (SELECT count(*) FROM knowledge_revision_log l WHERE l.tenant_id = r.tenant_id AND l.object_id = r.object_id), \
                (SELECT count(*) FROM events e WHERE e.tenant_id = r.tenant_id AND e.aggregate_id = r.object_id AND e.event_type = 'knowledge.revision_committed'), \
                (SELECT count(*) FROM outbox_messages o WHERE o.tenant_id = r.tenant_id AND o.topic = 'knowledge.revision_committed') \
           FROM knowledge_revisions r JOIN knowledge_documents d \
             ON d.tenant_id = r.tenant_id AND d.object_id = r.object_id \
          WHERE r.tenant_id = $1::uuid AND r.object_id = $2::uuid AND d.id = $3::uuid",
    )
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .bind(document_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("rollback state should be readable");
    assert_eq!(state, (0, "indexed".to_owned(), None, 0, 0, 0));

    sqlx::query(&format!("DROP TRIGGER {trigger_name} ON outbox_messages"))
        .execute(&pool)
        .await
        .expect("failure trigger should be removed");
    sqlx::query(&format!("DROP FUNCTION {function_name}()"))
        .execute(&pool)
        .await
        .expect("failure function should be removed");

    pool.close().await;
}

#[tokio::test]
async fn concurrent_commits_serialize_on_revision_row_and_publish_once() {
    let (pool, repository, scope, object_id) = setup().await;
    let (document_id, _) =
        add_indexed_document(&pool, &scope, object_id, "concurrent", true).await;

    let (first, second) = tokio::join!(
        repository.commit_knowledge_revision(&scope, object_id),
        repository.commit_knowledge_revision(&scope, object_id)
    );
    let successes = usize::from(first.is_ok()) + usize::from(second.is_ok());
    assert_eq!(successes, 1, "only one transaction may publish the indexed batch");
    let loser = if first.is_err() { first } else { second };
    assert!(matches!(
        loser,
        Err(KnowledgeRevisionCommitError::NoIndexedDocuments)
    ));

    let state: (i64, String, Option<i64>, i64, i64) = sqlx::query_as(
        "SELECT r.current_revision, d.status, d.valid_from_revision, \
                (SELECT count(*) FROM knowledge_revision_log l WHERE l.tenant_id = r.tenant_id AND l.object_id = r.object_id), \
                (SELECT count(*) FROM events e WHERE e.tenant_id = r.tenant_id AND e.aggregate_id = r.object_id AND e.event_type = 'knowledge.revision_committed') \
           FROM knowledge_revisions r JOIN knowledge_documents d \
             ON d.tenant_id = r.tenant_id AND d.object_id = r.object_id \
          WHERE r.tenant_id = $1::uuid AND r.object_id = $2::uuid AND d.id = $3::uuid",
    )
    .bind(scope.tenant_id().to_string())
    .bind(object_id.to_string())
    .bind(document_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("concurrent state should be readable");
    assert_eq!(state, (1, "live".to_owned(), Some(1), 1, 1));

    pool.close().await;
}
