use qembed_domain::knowledge_embedding::{EmbeddingDistanceMetric, EmbeddingModelSpec};
use qembed_store::{
    PgPoolConfig, connect_pool, embedding_model_registry::PgEmbeddingModelRegistry, run_migrations,
};
use sqlx::PgPool;
use uuid::Uuid;

async fn setup() -> (PgPool, PgEmbeddingModelRegistry) {
    let pool = connect_pool(
        &std::env::var("DATABASE_URL").expect("DATABASE_URL is set"),
        PgPoolConfig {
            max_connections: 1,
            ..PgPoolConfig::default()
        },
    )
    .await
    .expect("admin test pool connects");
    run_migrations(&pool).await.expect("migrations apply");

    let mut connection = pool.acquire().await.expect("single connection acquired");
    let grants: (bool, bool) = sqlx::query_as(
        "SELECT has_table_privilege('qembed_admin', 'public.embedding_models', 'INSERT'),                 has_table_privilege('qembed_runtime', 'public.embedding_models', 'INSERT')",
    )
    .fetch_one(&mut *connection)
    .await
    .expect("registry role grants are readable");
    assert!(grants.0, "platform admin can register models");
    assert!(!grants.1, "runtime cannot mutate the global model registry");

    // Shadow the global table with a connection-local fixture so tests never
    // change the active model set observed by parallel ingestion tests.
    sqlx::query(
        "CREATE TEMP TABLE embedding_models (             id TEXT PRIMARY KEY,             provider TEXT NOT NULL,             model_name TEXT NOT NULL,             dimension INTEGER NOT NULL CHECK (dimension > 0),             distance_metric TEXT NOT NULL CHECK (distance_metric IN ('cosine', 'l2', 'ip')),             active BOOLEAN NOT NULL DEFAULT true,             created_at TIMESTAMPTZ NOT NULL DEFAULT now()         ) ON COMMIT PRESERVE ROWS",
    )
    .execute(&mut *connection)
    .await
    .expect("connection-local registry fixture is created");
    sqlx::query("GRANT ALL ON TABLE pg_temp.embedding_models TO qembed_admin")
        .execute(&mut *connection)
        .await
        .expect("admin can use the connection-local registry fixture");
    sqlx::query("SET SESSION AUTHORIZATION qembed_admin")
        .execute(&mut *connection)
        .await
        .expect("registry tests use the dedicated admin database role");
    drop(connection);

    (pool.clone(), PgEmbeddingModelRegistry::new(pool))
}

fn spec(id: String, dimension: usize, metric: EmbeddingDistanceMetric) -> EmbeddingModelSpec {
    EmbeddingModelSpec::new(
        id,
        "test-provider",
        "test-embedding-model",
        dimension,
        metric,
    )
    .unwrap()
}

#[tokio::test]
async fn register_read_list_and_activation_preserve_model_contract() {
    let (pool, registry) = setup().await;
    let id = format!("rust068-{}", Uuid::new_v4());
    let model = registry
        .register(&spec(id.clone(), 3, EmbeddingDistanceMetric::Cosine), true)
        .await
        .expect("register");

    assert!(model.is_active());
    assert_eq!(model.spec().dimension(), 3);
    assert_eq!(
        model.spec().distance_metric(),
        EmbeddingDistanceMetric::Cosine
    );
    assert_eq!(registry.get(&id).await.expect("get"), Some(model.clone()));
    assert!(
        registry
            .list()
            .await
            .expect("list")
            .iter()
            .any(|entry| entry == &model)
    );
    assert!(
        !registry
            .set_active(&id, false)
            .await
            .expect("deactivate")
            .is_active()
    );
    assert!(
        registry
            .set_active(&id, true)
            .await
            .expect("reactivate")
            .is_active()
    );

    pool.close().await;
}

#[tokio::test]
async fn duplicate_ids_are_rejected_and_model_metadata_is_not_mutated_by_activation() {
    let (pool, registry) = setup().await;
    let id = format!("rust068-immutable-{}", Uuid::new_v4());
    let model_spec = spec(id.clone(), 7, EmbeddingDistanceMetric::InnerProduct);
    let original = registry
        .register(&model_spec, true)
        .await
        .expect("register");

    assert!(registry.register(&model_spec, true).await.is_err());

    let updated = registry.set_active(&id, false).await.expect("deactivate");
    assert_eq!(updated.spec(), original.spec());
    assert_eq!(updated.spec().dimension(), 7);
    assert_eq!(
        updated.spec().distance_metric(),
        EmbeddingDistanceMetric::InnerProduct
    );

    pool.close().await;
}

#[tokio::test]
async fn missing_model_cannot_be_activated() {
    let (pool, registry) = setup().await;
    assert!(registry.set_active("does-not-exist", true).await.is_err());
    pool.close().await;
}
