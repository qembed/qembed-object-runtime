mod common;

use qembed_domain::{
    ids::{KnowledgeChunkId,KnowledgeDocumentId,ObjectId,TenantId},
    knowledge_ingestion::{IngestionChunk,IngestionPersistenceOutcome,ModelEmbedding},
    object::Object,
    ports::KnowledgeIngestionStore,
    test_support::scope_for_tenant,
};
use qembed_store::{
    knowledge_ingestion::{PgKnowledgeIngestionStore,PgKnowledgeIngestionError},
    knowledge_repository::PgKnowledgeRepository,
    object_repository::ObjectRepository,
    run_migrations,
};
use sqlx::PgPool;

const MODEL_ID:&str="rust066-integration-model";

async fn setup()->(PgPool,PgKnowledgeIngestionStore,qembed_domain::scope::Scope,ObjectId,KnowledgeDocumentId){
    let pool=common::connect().await;
    run_migrations(&pool).await.expect("migrations should apply");
    sqlx::query("INSERT INTO embedding_models(id,provider,model_name,dimension,distance_metric,active) VALUES($1,'test','rust066-integration-model',2,'cosine',true) ON CONFLICT(id) DO NOTHING")
        .bind(MODEL_ID).execute(&pool).await.expect("shared model exists");
    let context=common::signed_context(&pool).await;
    let tenant=TenantId::new();let scope=scope_for_tenant(tenant);let object=ObjectId::new();
    ObjectRepository::new(context.clone()).create_object(&scope,&Object::new(object,tenant)).await.expect("object created");
    let document=KnowledgeDocumentId::new();
    sqlx::query("INSERT INTO knowledge_documents(id,tenant_id,object_id,doc_key,title,source_uri,status) VALUES($1::uuid,$2::uuid,$3::uuid,'ingestion-test','Ingestion test','s3://bucket/document.txt','staged')")
        .bind(document.to_string()).bind(tenant.to_string()).bind(object.to_string()).execute(&pool).await.expect("staged document created");
    (pool,PgKnowledgeIngestionStore::new(context),scope,object,document)
}
fn complete_chunk(model_id:&str)->IngestionChunk{
    IngestionChunk::new(KnowledgeChunkId::new(),0,"complete staged text".to_owned(),vec![ModelEmbedding::new(model_id,vec![0.1,0.2]).unwrap()]).unwrap()
}

#[tokio::test]
async fn chunks_and_embeddings_commit_together_before_document_becomes_indexed(){
    let(pool,store,scope,object,document)=setup().await;
    let models=store.active_ingestion_embedding_models(&scope).await.expect("active models load");
    assert_eq!(models.len(),1);
    let chunk=complete_chunk(MODEL_ID);
    let outcome=store.persist_indexed_knowledge_document(&scope,document,&models,std::slice::from_ref(&chunk)).await.expect("complete payload persists atomically");
    assert_eq!(outcome,IngestionPersistenceOutcome::Indexed);

    let state:(String,Option<i64>,i64,i64)=sqlx::query_as(
        "SELECT d.status,c.valid_from_revision,(SELECT count(*) FROM knowledge_chunks c2 WHERE c2.document_id=d.id),(SELECT count(*) FROM knowledge_chunk_embeddings e JOIN knowledge_chunks c3 ON c3.id=e.chunk_id WHERE c3.document_id=d.id) FROM knowledge_documents d JOIN knowledge_chunks c ON c.document_id=d.id WHERE d.tenant_id=$1::uuid AND d.id=$2::uuid"
    ).bind(scope.tenant_id().to_string()).bind(document.to_string()).fetch_one(&pool).await.expect("persisted staged index is readable");
    assert_eq!(state,("indexed".to_owned(),None,1,1));

    let loaded=store.load_knowledge_ingestion_document(&scope,document).await.expect("state reloads");
    assert!(matches!(loaded,Some(qembed_domain::knowledge_ingestion::KnowledgeIngestionDocumentState::AlreadyIndexed)));

    // Indexing does not publish. RUST-066 remains the only visibility boundary.
    let revision=PgKnowledgeRepository::new(common::signed_context(&pool).await)
        .commit_knowledge_revision(&scope,object).await.expect("complete indexed document publishes");
    assert_eq!(revision.revision().value(),1);
    let visible:Option<i64>=sqlx::query_scalar("SELECT valid_from_revision FROM knowledge_documents WHERE tenant_id=$1::uuid AND id=$2::uuid")
        .bind(scope.tenant_id().to_string()).bind(document.to_string()).fetch_one(&pool).await.expect("published interval readable");
    assert_eq!(visible,Some(1));
    pool.close().await;
}

#[tokio::test]
async fn invalid_embedding_model_payload_leaves_document_staged_and_writes_nothing(){
    let(pool,store,scope,_object,document)=setup().await;
    let models=store.active_ingestion_embedding_models(&scope).await.expect("active models load");
    let chunk=complete_chunk("not-the-active-model");
    let result=store.persist_indexed_knowledge_document(&scope,document,&models,&[chunk]).await;
    assert!(matches!(result,Err(PgKnowledgeIngestionError::IncompleteChunkSet)));
    let state:(String,i64)=sqlx::query_as("SELECT status,(SELECT count(*) FROM knowledge_chunks c WHERE c.document_id=d.id) FROM knowledge_documents d WHERE tenant_id=$1::uuid AND id=$2::uuid")
        .bind(scope.tenant_id().to_string()).bind(document.to_string()).fetch_one(&pool).await.expect("failed attempt leaves source unchanged");
    assert_eq!(state,("staged".to_owned(),0));
    pool.close().await;
}

#[tokio::test]
async fn concurrent_or_repeated_persist_does_not_duplicate_chunks(){
    let(pool,store,scope,_object,document)=setup().await;
    let models=store.active_ingestion_embedding_models(&scope).await.expect("active models load");
    let chunk=complete_chunk(MODEL_ID);
    let first=store.persist_indexed_knowledge_document(&scope,document,&models,std::slice::from_ref(&chunk)).await.expect("first ingestion persists");
    let second=store.persist_indexed_knowledge_document(&scope,document,&models,std::slice::from_ref(&chunk)).await.expect("repeat is idempotent");
    assert_eq!(first,IngestionPersistenceOutcome::Indexed);
    assert_eq!(second,IngestionPersistenceOutcome::AlreadyIndexed);
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM knowledge_chunks WHERE tenant_id=$1::uuid AND document_id=$2::uuid")
        .bind(scope.tenant_id().to_string()).bind(document.to_string()).fetch_one(&pool).await.expect("chunk count readable");
    assert_eq!(count,1);
    pool.close().await;
}
