//! PostgreSQL adapter for the global immutable ToolVersion registry.

use std::{error::Error, fmt};

use qembed_domain::{
    ids::{ToolId, ToolVersionId},
    ports::ToolVersionRegistry,
    tool_version::{ToolDefinition, ToolRiskLevel, ToolVersion, ToolVersionError},
};
use sqlx::{PgPool, Row};

#[derive(Debug)]
pub enum PgToolVersionRegistryError {
    Database(sqlx::Error),
    DuplicateDefinition,
    DuplicateVersion,
    InvalidPersistedValue,
    InvalidDomainValue(ToolVersionError),
}

impl fmt::Display for PgToolVersionRegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => {
                write!(formatter, "ToolVersion registry database error: {error}")
            }
            Self::DuplicateDefinition => formatter.write_str("Tool definition already exists"),
            Self::DuplicateVersion => formatter.write_str("ToolVersion already exists"),
            Self::InvalidPersistedValue => {
                formatter.write_str("persisted ToolVersion violates the domain contract")
            }
            Self::InvalidDomainValue(error) => write!(formatter, "invalid ToolVersion: {error:?}"),
        }
    }
}

impl Error for PgToolVersionRegistryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

impl From<sqlx::Error> for PgToolVersionRegistryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

/// Global registry adapter. Reads require the runtime role; registration requires
/// the dedicated administrative pool with INSERT privileges on registry tables.
#[derive(Clone)]
pub struct PgToolVersionRegistry {
    pool: PgPool,
}

impl PgToolVersionRegistry {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Registers a Tool definition. Tool identity and display name are global.
    pub async fn register_definition(
        &self,
        definition: &ToolDefinition,
    ) -> Result<(), PgToolVersionRegistryError> {
        let result = sqlx::query(
            "INSERT INTO tool_definitions (id, name, description) VALUES ($1::uuid, $2, $3)",
        )
        .bind(definition.id().to_string())
        .bind(definition.name())
        .bind(definition.description())
        .execute(&self.pool)
        .await;
        match result {
            Ok(_) => Ok(()),
            Err(error) if is_unique_violation(&error) => {
                Err(PgToolVersionRegistryError::DuplicateDefinition)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Registers a new ToolVersion. Existing versions can never be updated or deleted.
    pub async fn register_version(
        &self,
        version: &ToolVersion,
    ) -> Result<(), PgToolVersionRegistryError> {
        let result = sqlx::query(
            "INSERT INTO tool_versions (id, tool_id, version, risk_level, input_schema, output_schema, timeout_ms, idempotent) VALUES ($1::uuid, $2::uuid, $3, $4, $5, $6, $7, $8)",
        )
        .bind(version.id().to_string())
        .bind(version.tool_id().to_string())
        .bind(version.version())
        .bind(version.risk_level().as_database_value())
        .bind(version.input_schema())
        .bind(version.output_schema())
        .bind(i32::try_from(version.timeout_ms()).map_err(|_| PgToolVersionRegistryError::InvalidPersistedValue)?)
        .bind(version.idempotent())
        .execute(&self.pool)
        .await;
        match result {
            Ok(_) => Ok(()),
            Err(error) if is_unique_violation(&error) => {
                Err(PgToolVersionRegistryError::DuplicateVersion)
            }
            Err(error) => Err(error.into()),
        }
    }
}

impl ToolVersionRegistry for PgToolVersionRegistry {
    type Error = PgToolVersionRegistryError;

    async fn get_tool_version(
        &self,
        tool_id: ToolId,
        version_id: ToolVersionId,
    ) -> Result<Option<ToolVersion>, Self::Error> {
        let row = sqlx::query(
            "SELECT id::text, tool_id::text, version, risk_level, input_schema, output_schema, timeout_ms, idempotent FROM tool_versions WHERE tool_id=$1::uuid AND id=$2::uuid",
        )
        .bind(tool_id.to_string())
        .bind(version_id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        row.map(|row| {
            let id: String = row.try_get("id")?;
            let stored_tool_id: String = row.try_get("tool_id")?;
            let version: String = row.try_get("version")?;
            let risk: String = row.try_get("risk_level")?;
            let input_schema: serde_json::Value = row.try_get("input_schema")?;
            let output_schema: serde_json::Value = row.try_get("output_schema")?;
            let timeout_ms: i32 = row.try_get("timeout_ms")?;
            let idempotent: bool = row.try_get("idempotent")?;

            let id = id
                .parse()
                .map_err(|_| PgToolVersionRegistryError::InvalidPersistedValue)?;
            let stored_tool_id = stored_tool_id
                .parse()
                .map_err(|_| PgToolVersionRegistryError::InvalidPersistedValue)?;
            let risk = ToolRiskLevel::from_database_value(&risk)
                .map_err(PgToolVersionRegistryError::InvalidDomainValue)?;
            let timeout_ms = u32::try_from(timeout_ms)
                .map_err(|_| PgToolVersionRegistryError::InvalidPersistedValue)?;
            ToolVersion::new(
                id,
                stored_tool_id,
                version,
                risk,
                input_schema,
                output_schema,
                timeout_ms,
                idempotent,
            )
            .map_err(PgToolVersionRegistryError::InvalidDomainValue)
        })
        .transpose()
    }
}

fn is_unique_violation(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(db) if db.code().as_deref() == Some("23505"))
}
