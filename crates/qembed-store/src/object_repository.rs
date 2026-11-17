//! Tenant-scoped persistence for Object aggregates and draft versions.

use std::fmt;

use qembed_domain::object::{Object, ObjectState};
use qembed_domain::object_version::{ObjectVersion, ObjectVersionStatus};
use qembed_domain::scope::Scope;
use sqlx::PgPool;

use crate::{begin_tenant_transaction, execute};

/// Persists tenant-owned objects and their draft versions.
#[derive(Clone, Debug)]
pub struct ObjectRepository {
    pool: PgPool,
}

impl ObjectRepository {
    /// Creates a repository backed by the application's PostgreSQL pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Inserts a new Draft object in the tenant identified by the trusted scope.
    pub async fn create_object(
        &self,
        scope: &Scope,
        object: &Object,
    ) -> Result<(), ObjectRepositoryError> {
        if object.tenant_id() != scope.tenant_id() {
            return Err(ObjectRepositoryError::TenantMismatch);
        }
        if object.state() != ObjectState::Draft {
            return Err(ObjectRepositoryError::ObjectMustBeDraft);
        }

        let mut transaction = begin_tenant_transaction(&self.pool, scope)
            .await
            .map_err(ObjectRepositoryError::Database)?;

        execute(
            &mut transaction,
            sqlx::query(
                "INSERT INTO objects (tenant_id, object_id, state) VALUES ($1::uuid, $2::uuid, 'draft')",
            )
            .bind(scope.tenant_id().to_string())
            .bind(object.id().to_string()),
        )
        .await
        .map_err(ObjectRepositoryError::Database)?;

        transaction
            .commit()
            .await
            .map_err(ObjectRepositoryError::Database)
    }

    /// Inserts a Draft version for an existing object in the same tenant.
    ///
    /// The composite foreign key ensures that the referenced object belongs
    /// to this tenant; a version cannot attach to an object in another tenant.
    pub async fn create_draft_version(
        &self,
        scope: &Scope,
        version: &ObjectVersion,
    ) -> Result<(), ObjectRepositoryError> {
        if version.tenant_id() != scope.tenant_id() {
            return Err(ObjectRepositoryError::TenantMismatch);
        }
        if version.status() != ObjectVersionStatus::Draft {
            return Err(ObjectRepositoryError::VersionMustBeDraft);
        }
        let version_number = i64::try_from(version.version_number())
            .map_err(|_| ObjectRepositoryError::VersionNumberOutOfRange)?;

        let mut transaction = begin_tenant_transaction(&self.pool, scope)
            .await
            .map_err(ObjectRepositoryError::Database)?;

        execute(
            &mut transaction,
            sqlx::query(
                "INSERT INTO object_versions (tenant_id, object_id, object_version_id, version_number, status) VALUES ($1::uuid, $2::uuid, $3::uuid, $4, 'draft')",
            )
            .bind(scope.tenant_id().to_string())
            .bind(version.object_id().to_string())
            .bind(version.id().to_string())
            .bind(version_number),
        )
        .await
        .map_err(ObjectRepositoryError::Database)?;

        transaction
            .commit()
            .await
            .map_err(ObjectRepositoryError::Database)
    }
}

/// Validation or persistence failure while creating an Object or Draft version.
#[derive(Debug)]
pub enum ObjectRepositoryError {
    /// The entity tenant does not match the authenticated scope.
    TenantMismatch,
    /// Only a newly-created Draft object can be inserted by this operation.
    ObjectMustBeDraft,
    /// Only a Draft version can be inserted by this operation.
    VersionMustBeDraft,
    /// The domain's unsigned version number does not fit PostgreSQL BIGINT.
    VersionNumberOutOfRange,
    /// PostgreSQL rejected the operation or could not complete it.
    Database(sqlx::Error),
}

impl fmt::Display for ObjectRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TenantMismatch => formatter.write_str("entity tenant does not match scope"),
            Self::ObjectMustBeDraft => formatter.write_str("new object must be in Draft state"),
            Self::VersionMustBeDraft => formatter.write_str("new object version must be Draft"),
            Self::VersionNumberOutOfRange => {
                formatter.write_str("object version number exceeds PostgreSQL BIGINT")
            }
            Self::Database(error) => write!(formatter, "object repository database error: {error}"),
        }
    }
}

impl std::error::Error for ObjectRepositoryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

impl From<sqlx::Error> for ObjectRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}