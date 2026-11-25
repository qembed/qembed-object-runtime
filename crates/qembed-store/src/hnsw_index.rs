//! Model-scoped HNSW index provisioning for pgvector.
//!
//! Embeddings from different models may have different dimensions. A single
//! HNSW index over the untyped `vector` column is therefore not valid for the
//! whole table: pgvector requires one indexed vector dimension per index.
//! This manager creates one partial index per registered model.

use std::{error::Error, fmt};

use qembed_domain::knowledge_embedding::{EmbeddingDistanceMetric, EmbeddingModelSpec};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};

/// Result of ensuring one model's HNSW index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HnswIndexOutcome {
    /// A new index was built with CREATE INDEX CONCURRENTLY.
    Created,
    /// A valid HNSW index already existed.
    AlreadyPresent,
    /// pgvector's vector HNSW index limit is 2,000 dimensions; exact search remains available.
    SkippedUnsupportedDimension,
}

/// Errors raised while provisioning a model-scoped HNSW index.
#[derive(Debug)]
pub enum PgHnswIndexError {
    Database(sqlx::Error),
    InvalidPersistedModel,
}

impl fmt::Display for PgHnswIndexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "HNSW index migration failed: {error}"),
            Self::InvalidPersistedModel => {
                formatter.write_str("persisted embedding model metadata is invalid")
            }
        }
    }
}

impl Error for PgHnswIndexError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::InvalidPersistedModel => None,
        }
    }
}

impl From<sqlx::Error> for PgHnswIndexError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

/// Provisions HNSW indexes using a pool authenticated as the schema migrator.
#[derive(Clone)]
pub struct PgHnswIndexManager {
    pool: PgPool,
}

impl PgHnswIndexManager {
    /// The supplied pool must have permission to create indexes on the Store schema.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Ensures indexes for every registered model.
    ///
    /// This is safe to call after every SQLx migration run. Catalog state is
    /// authoritative: valid indexes are retained, while invalid artifacts left
    /// by an interrupted CREATE INDEX CONCURRENTLY are dropped and rebuilt.
    pub async fn ensure_registered_models(
        &self,
    ) -> Result<Vec<(String, HnswIndexOutcome)>, PgHnswIndexError> {
        let rows = sqlx::query(
            "SELECT id, dimension, distance_metric FROM embedding_models ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut outcomes = Vec::with_capacity(rows.len());
        for row in rows {
            let id: String = row.try_get("id")?;
            let dimension: i32 = row.try_get("dimension")?;
            let metric: String = row.try_get("distance_metric")?;
            let dimension = usize::try_from(dimension)
                .map_err(|_| PgHnswIndexError::InvalidPersistedModel)?;
            let metric = EmbeddingDistanceMetric::from_database_value(&metric)
                .map_err(|_| PgHnswIndexError::InvalidPersistedModel)?;
            let spec = EmbeddingModelSpec::new(
                id.clone(),
                "persisted",
                "persisted",
                dimension,
                metric,
            )
            .map_err(|_| PgHnswIndexError::InvalidPersistedModel)?;
            outcomes.push((id, self.ensure_model_index(&spec).await?));
        }
        Ok(outcomes)
    }

    /// Ensures the partial HNSW index for one model.
    ///
    /// PostgreSQL executes CREATE/DROP INDEX CONCURRENTLY outside a transaction.
    /// A session advisory lock serializes concurrent migrators for this model.
    pub async fn ensure_model_index(
        &self,
        model: &EmbeddingModelSpec,
    ) -> Result<HnswIndexOutcome, PgHnswIndexError> {
        if model.dimension() > 2_000 {
            return Ok(HnswIndexOutcome::SkippedUnsupportedDimension);
        }

        let index_name = index_name(model.id());
        let lock_key = format!("qembed:hnsw:{index_name}");
        let mut connection = self.pool.acquire().await?;
        sqlx::query("SELECT pg_advisory_lock(hashtextextended($1, 0))")
            .bind(&lock_key)
            .execute(&mut *connection)
            .await?;

        let result = self
            .ensure_model_index_locked(&mut connection, model, &index_name)
            .await;
        let unlock = sqlx::query_scalar::<_, bool>(
            "SELECT pg_advisory_unlock(hashtextextended($1, 0))",
        )
        .bind(&lock_key)
        .fetch_one(&mut *connection)
        .await;

        match (result, unlock) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error.into()),
            (Ok(outcome), Ok(true)) => Ok(outcome),
            (Ok(_), Ok(false)) => Err(PgHnswIndexError::InvalidPersistedModel),
        }
    }

    async fn ensure_model_index_locked(
        &self,
        connection: &mut sqlx::PgConnection,
        model: &EmbeddingModelSpec,
        index_name: &str,
    ) -> Result<HnswIndexOutcome, PgHnswIndexError> {
        let existing = sqlx::query(
            "SELECT i.indisvalid AND i.indisready AS is_valid \
             FROM pg_class AS index_class \
             JOIN pg_namespace AS index_schema ON index_schema.oid = index_class.relnamespace \
             JOIN pg_index AS i ON i.indexrelid = index_class.oid \
             WHERE index_schema.nspname = 'public' AND index_class.relname = $1",
        )
        .bind(index_name)
        .fetch_optional(&mut *connection)
        .await?;

        if let Some(row) = existing {
            let is_valid: bool = row.try_get("is_valid")?;
            if is_valid {
                return Ok(HnswIndexOutcome::AlreadyPresent);
            }
            let drop_sql = format!("DROP INDEX CONCURRENTLY IF EXISTS public.\"{index_name}\"");
            sqlx::query(&drop_sql).execute(&mut *connection).await?;
        }

        let operator_class = match model.distance_metric() {
            EmbeddingDistanceMetric::Cosine => "vector_cosine_ops",
            EmbeddingDistanceMetric::L2 => "vector_l2_ops",
            EmbeddingDistanceMetric::InnerProduct => "vector_ip_ops",
        };
        let model_id = model.id().replace('\'', "''");
        let create_sql = format!(
            "CREATE INDEX CONCURRENTLY \"{index_name}\" \
             ON public.knowledge_chunk_embeddings USING hnsw \
             ((embedding::vector({dimension})) {operator_class}) \
             WITH (m = 16, ef_construction = 64) \
             WHERE model_id = '{model_id}'",
            dimension = model.dimension(),
        );
        sqlx::query(&create_sql).execute(&mut *connection).await?;

        let valid: bool = sqlx::query_scalar(
            "SELECT i.indisvalid AND i.indisready \
             FROM pg_class AS index_class \
             JOIN pg_namespace AS index_schema ON index_schema.oid = index_class.relnamespace \
             JOIN pg_index AS i ON i.indexrelid = index_class.oid \
             WHERE index_schema.nspname = 'public' AND index_class.relname = $1",
        )
        .bind(index_name)
        .fetch_one(&mut *connection)
        .await?;
        if !valid {
            return Err(PgHnswIndexError::InvalidPersistedModel);
        }

        Ok(HnswIndexOutcome::Created)
    }
}

fn index_name(model_id: &str) -> String {
    let digest = Sha256::digest(model_id.as_bytes());
    let suffix = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("idx_kce_hnsw_{suffix}")
}
