//! Global PostgreSQL registry for embedding model metadata.
//!
//! Construct this adapter only with a dedicated connection pool authenticated as
//! qembed_admin. Runtime pools are intentionally read-only for embedding_models.

use std::{error::Error, fmt};
use qembed_domain::knowledge_embedding::{EmbeddingDistanceMetric, EmbeddingModel, EmbeddingModelError, EmbeddingModelSpec};
use sqlx::{PgPool, Row};

#[derive(Debug)]
pub enum PgEmbeddingModelRegistryError {
    Database(sqlx::Error), DuplicateModelId, ModelNotFound, InvalidPersistedModel,
}
impl fmt::Display for PgEmbeddingModelRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(e) => write!(f,"embedding model registry database error: {e}"),
            Self::DuplicateModelId => f.write_str("embedding model ID is already registered"),
            Self::ModelNotFound => f.write_str("embedding model was not found"),
            Self::InvalidPersistedModel => f.write_str("persisted embedding model metadata violates the domain contract"),
        }
    }
}
impl Error for PgEmbeddingModelRegistryError {
    fn source(&self) -> Option<&(dyn Error+'static)> { match self { Self::Database(e)=>Some(e), _=>None } }
}
impl From<sqlx::Error> for PgEmbeddingModelRegistryError { fn from(e:sqlx::Error)->Self { Self::Database(e) } }

#[derive(Clone)]
pub struct PgEmbeddingModelRegistry { admin_pool: PgPool }
impl PgEmbeddingModelRegistry {
    /// The supplied pool must connect as qembed_admin; never pass the runtime pool.
    #[must_use] pub fn new(admin_pool:PgPool)->Self { Self{admin_pool} }

    /// Registers a new immutable model specification.
    pub async fn register(&self,spec:&EmbeddingModelSpec,active:bool)->Result<EmbeddingModel,PgEmbeddingModelRegistryError>{
        let mut tx=self.admin_pool.begin().await?;
        sqlx::query("LOCK TABLE embedding_models IN SHARE ROW EXCLUSIVE MODE").execute(&mut *tx).await?;
        let result=sqlx::query("INSERT INTO embedding_models(id,provider,model_name,dimension,distance_metric,active) VALUES($1,$2,$3,$4,$5,$6)")
            .bind(spec.id()).bind(spec.provider()).bind(spec.model_name())
            .bind(i32::try_from(spec.dimension()).map_err(|_|PgEmbeddingModelRegistryError::InvalidPersistedModel)?)
            .bind(spec.distance_metric().as_database_value()).bind(active).execute(&mut *tx).await;
        if let Err(error)=result {
            if matches!(&error,sqlx::Error::Database(db) if db.code().as_deref()==Some("23505")) {
                return Err(PgEmbeddingModelRegistryError::DuplicateModelId);
            }
            return Err(error.into());
        }
        tx.commit().await?;
        Ok(EmbeddingModel::new(spec.clone(),active))
    }

    /// Toggles activation without changing immutable vector-space metadata.
    /// The table lock serializes changes with RUST-066 revision commits.
    pub async fn set_active(&self,model_id:&str,active:bool)->Result<EmbeddingModel,PgEmbeddingModelRegistryError>{
        let mut tx=self.admin_pool.begin().await?;
        sqlx::query("LOCK TABLE embedding_models IN SHARE ROW EXCLUSIVE MODE").execute(&mut *tx).await?;
        let row=sqlx::query("SELECT provider,model_name,dimension,distance_metric,active FROM embedding_models WHERE id=$1 FOR UPDATE")
            .bind(model_id).fetch_optional(&mut *tx).await?.ok_or(PgEmbeddingModelRegistryError::ModelNotFound)?;
        let was_active:bool=row.try_get("active")?;
        if was_active!=active {
            sqlx::query("UPDATE embedding_models SET active=$2 WHERE id=$1").bind(model_id).bind(active).execute(&mut *tx).await?;
        }
        let spec=decode_spec(model_id,&row)?;
        tx.commit().await?;
        Ok(EmbeddingModel::new(spec,active))
    }

    pub async fn get(&self,model_id:&str)->Result<Option<EmbeddingModel>,PgEmbeddingModelRegistryError>{
        let row=sqlx::query("SELECT provider,model_name,dimension,distance_metric,active FROM embedding_models WHERE id=$1")
            .bind(model_id).fetch_optional(&self.admin_pool).await?;
        row.map(|row| {
            let active:bool=row.try_get("active")?;
            let spec=decode_spec(model_id,&row)?;
            Ok(EmbeddingModel::new(spec,active))
        }).transpose()
    }

    pub async fn list(&self)->Result<Vec<EmbeddingModel>,PgEmbeddingModelRegistryError>{
        let rows=sqlx::query("SELECT id,provider,model_name,dimension,distance_metric,active FROM embedding_models ORDER BY id")
            .fetch_all(&self.admin_pool).await?;
        rows.iter().map(|row| {
            let id:String=row.try_get("id")?;
            let active:bool=row.try_get("active")?;
            let spec=decode_spec(&id,row)?;
            Ok(EmbeddingModel::new(spec,active))
        }).collect()
    }
}

fn decode_spec(id:&str,row:&sqlx::postgres::PgRow)->Result<EmbeddingModelSpec,PgEmbeddingModelRegistryError>{
    let provider:String=row.try_get("provider")?;
    let model_name:String=row.try_get("model_name")?;
    let dimension:i32=row.try_get("dimension")?;
    let metric:String=row.try_get("distance_metric")?;
    let dimension=usize::try_from(dimension).map_err(|_|PgEmbeddingModelRegistryError::InvalidPersistedModel)?;
    let metric=EmbeddingDistanceMetric::from_database_value(&metric)
        .map_err(|_:EmbeddingModelError|PgEmbeddingModelRegistryError::InvalidPersistedModel)?;
    EmbeddingModelSpec::new(id,provider,model_name,dimension,metric)
        .map_err(|_|PgEmbeddingModelRegistryError::InvalidPersistedModel)
}
