use std::str::FromStr;

use qembed_domain::knowledge_embedding::{EmbeddingDistanceMetric, EmbeddingModelSpec};
use qembed_store::hnsw_index::{HnswIndexOutcome, PgHnswIndexManager};
use sqlx::{
    Connection, PgConnection,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use uuid::Uuid;

#[tokio::test]
async fn hnsw_index_creation_is_concurrent_idempotent_and_recovers_invalid_artifacts() {
    let database_url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set by the test harness");
    let admin_options = PgConnectOptions::from_str(&database_url).expect("DATABASE_URL is valid");
    let mut admin = PgConnection::connect_with(&admin_options)
        .await
        .expect("admin connection opens");

    let database_name = format!("qembed_hnsw_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE \"{database_name}\""))
        .execute(&mut admin)
        .await
        .expect("isolated HNSW test database is created");

    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(admin_options.database(&database_name))
        .await
        .expect("isolated database connection opens");

    sqlx::raw_sql(
        "CREATE EXTENSION vector; \
         CREATE TABLE embedding_models ( \
             id TEXT PRIMARY KEY, dimension INTEGER NOT NULL, distance_metric TEXT NOT NULL \
         ); \
         CREATE TABLE knowledge_chunk_embeddings ( \
             model_id TEXT NOT NULL, embedding VECTOR NOT NULL \
         );",
    )
    .execute(&pool)
    .await
    .expect("minimal pgvector schema is installed");

    let model = EmbeddingModelSpec::new(
        "rust071-recovery-model",
        "test",
        "recovery",
        3,
        EmbeddingDistanceMetric::Cosine,
    )
    .unwrap();
    sqlx::query("INSERT INTO embedding_models(id, dimension, distance_metric) VALUES ($1, $2, $3)")
        .bind(model.id())
        .bind(i32::try_from(model.dimension()).unwrap())
        .bind(model.distance_metric().as_database_value())
        .execute(&pool)
        .await
        .expect("model metadata inserted");

    let manager = PgHnswIndexManager::new(pool.clone());
    assert_eq!(
        manager.ensure_registered_models().await.unwrap(),
        vec![(model.id().to_owned(), HnswIndexOutcome::Created)]
    );
    assert_eq!(
        manager.ensure_model_index(&model).await.unwrap(),
        HnswIndexOutcome::AlreadyPresent
    );

    let index_name: String = sqlx::query_scalar(
        "SELECT index_class.relname \
         FROM pg_class AS index_class \
         JOIN pg_index AS i ON i.indexrelid = index_class.oid \
         WHERE index_class.relname LIKE 'idx_kce_hnsw_%' \
           AND pg_get_indexdef(index_class.oid) LIKE '%rust071-recovery-model%'",
    )
    .fetch_one(&pool)
    .await
    .expect("model-scoped index exists");

    sqlx::query(&format!("DROP INDEX CONCURRENTLY public.\"{index_name}\""))
        .execute(&pool)
        .await
        .expect("existing HNSW index is dropped outside a transaction");

    let fixture = format!("hnsw_failed_build_{}", Uuid::new_v4().simple());
    sqlx::query(&format!(
        "CREATE TABLE public.\"{fixture}\" (value TEXT NOT NULL)"
    ))
    .execute(&pool)
    .await
    .expect("failure fixture table created");
    sqlx::query(&format!(
        "INSERT INTO public.\"{fixture}\" (value) VALUES ('duplicate'), ('duplicate')"
    ))
    .execute(&pool)
    .await
    .expect("duplicate fixture values inserted");

    let failed_build = sqlx::query(&format!(
        "CREATE UNIQUE INDEX CONCURRENTLY \"{index_name}\" \
         ON public.\"{fixture}\" (value)"
    ))
    .execute(&pool)
    .await;
    assert!(
        failed_build.is_err(),
        "duplicate values must fail unique index build"
    );

    let invalid_artifact: bool = sqlx::query_scalar(
        "SELECT NOT i.indisvalid \
         FROM pg_class AS index_class \
         JOIN pg_index AS i ON i.indexrelid = index_class.oid \
         WHERE index_class.relname = $1",
    )
    .bind(&index_name)
    .fetch_one(&pool)
    .await
    .expect("failed concurrent build leaves an invalid index artifact");
    assert!(invalid_artifact);

    assert_eq!(
        manager.ensure_model_index(&model).await.unwrap(),
        HnswIndexOutcome::Created
    );
    let recovered_definition: String = sqlx::query_scalar(
        "SELECT pg_get_indexdef(index_class.oid) \
         FROM pg_class AS index_class \
         JOIN pg_index AS i ON i.indexrelid = index_class.oid \
         WHERE index_class.relname = $1 AND i.indisvalid AND i.indisready",
    )
    .bind(&index_name)
    .fetch_one(&pool)
    .await
    .expect("recovered HNSW index is valid");
    assert!(recovered_definition.contains("USING hnsw"));
    assert!(recovered_definition.contains("vector_cosine_ops"));
    assert!(recovered_definition.contains("knowledge_chunk_embeddings"));

    sqlx::query(&format!("DROP TABLE public.\"{fixture}\""))
        .execute(&pool)
        .await
        .expect("failure fixture removed");
    pool.close().await;
    sqlx::query(&format!("DROP DATABASE \"{database_name}\" WITH (FORCE)"))
        .execute(&mut admin)
        .await
        .expect("isolated HNSW test database removed");
}

#[tokio::test]
async fn hnsw_manager_skips_dimensions_outside_pgvector_vector_index_limit() {
    let database_url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set by the test harness");
    let pool = qembed_store::connect_pool(&database_url, qembed_store::PgPoolConfig::default())
        .await
        .expect("PostgreSQL connection opens");
    let model = EmbeddingModelSpec::new(
        "rust071-too-wide",
        "test",
        "wide",
        2_001,
        EmbeddingDistanceMetric::L2,
    )
    .unwrap();
    assert_eq!(
        PgHnswIndexManager::new(pool.clone())
            .ensure_model_index(&model)
            .await
            .unwrap(),
        HnswIndexOutcome::SkippedUnsupportedDimension
    );
    pool.close().await;
}
