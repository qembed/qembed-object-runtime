//! Tenant-scoped persistence for Object aggregates and draft versions.

use std::fmt;

use qembed_domain::ids::{EventId, ObjectId, ObjectVersionId};
use qembed_domain::object::{Object, ObjectError, ObjectState};
use qembed_domain::object_version::{ObjectVersion, ObjectVersionError, ObjectVersionStatus};
use qembed_domain::scope::Scope;
use sqlx::PgPool;

use crate::event_outbox::append_object_event;
use crate::{begin_tenant_transaction, execute, fetch_optional};

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

    /// Moves a Draft object into Validating before external validation begins.
    ///
    /// The state change is guarded by the current persisted state so concurrent
    /// callers cannot start validation twice or revive an object.
    pub async fn begin_validation(
        &self,
        scope: &Scope,
        object_id: ObjectId,
    ) -> Result<(), ObjectRepositoryError> {
        let mut transaction = begin_tenant_transaction(&self.pool, scope)
            .await
            .map_err(ObjectRepositoryError::Database)?;

        let result = execute(
            &mut transaction,
            sqlx::query(
                "UPDATE objects SET state = 'validating' WHERE tenant_id = $1::uuid AND object_id = $2::uuid AND state = 'draft' AND published_version_id IS NULL",
            )
            .bind(scope.tenant_id().to_string())
            .bind(object_id.to_string()),
        )
        .await
        .map_err(ObjectRepositoryError::Database)?;

        if result.rows_affected() != 1 {
            return Err(ObjectRepositoryError::InvalidObjectState);
        }

        append_object_event(
            &mut transaction,
            scope,
            EventId::new(),
            object_id,
            "object.validation_started",
            "validating",
            None,
            None,
        )
        .await
        .map_err(ObjectRepositoryError::Database)?;

        transaction
            .commit()
            .await
            .map_err(ObjectRepositoryError::Database)
    }

    /// Atomically publishes a Draft version and updates its object's pointer.
    ///
    /// The expected current version is None for first publication and the
    /// currently published version ID when replacing it. The object row is
    /// locked first, serializing concurrent publications for the same object.
    /// Replacements supersede the previous version, publish the new one, and
    /// move the pointer in the same transaction.
    pub async fn publish_version(
        &self,
        scope: &Scope,
        object_id: ObjectId,
        version_id: ObjectVersionId,
        expected_current: Option<ObjectVersionId>,
    ) -> Result<(), ObjectRepositoryError> {
        let mut transaction = begin_tenant_transaction(&self.pool, scope)
            .await
            .map_err(ObjectRepositoryError::Database)?;

        let object = fetch_optional(
            &mut transaction,
            sqlx::query_as::<_, (String, Option<String>)>(
                "SELECT state, published_version_id::text FROM objects WHERE tenant_id = $1::uuid AND object_id = $2::uuid FOR UPDATE",
            )
            .bind(scope.tenant_id().to_string())
            .bind(object_id.to_string()),
        )
        .await
        .map_err(ObjectRepositoryError::Database)?
        .ok_or(ObjectRepositoryError::ObjectNotFound)?;

        let (state, current_id) = object;
        let expected_id = expected_current.map(|id| id.to_string());
        if current_id != expected_id {
            return Err(ObjectRepositoryError::PublishedVersionMismatch);
        }

        match (state.as_str(), expected_current) {
            ("validating", None) => {}
            ("active" | "suspended", Some(_)) => {}
            _ => return Err(ObjectRepositoryError::InvalidObjectState),
        }

        let version_row = fetch_optional(
            &mut transaction,
            sqlx::query_as::<_, (String, i64)>(
                "SELECT status, version_number FROM object_versions WHERE tenant_id = $1::uuid AND object_id = $2::uuid AND object_version_id = $3::uuid FOR UPDATE",
            )
            .bind(scope.tenant_id().to_string())
            .bind(object_id.to_string())
            .bind(version_id.to_string()),
        )
        .await
        .map_err(ObjectRepositoryError::Database)?
        .ok_or(ObjectRepositoryError::VersionNotFound)?;

        let version_status = parse_version_status(&version_row.0)?;
        let version_number = u64::try_from(version_row.1)
            .map_err(|_| ObjectRepositoryError::VersionNumberOutOfRange)?;
        let mut next_version = ObjectVersion::from_persisted(
            version_id,
            scope.tenant_id(),
            object_id,
            version_number,
            version_status,
        )?;
        next_version.publish()?;

        let mut object = Object::from_persisted(
            object_id,
            scope.tenant_id(),
            parse_object_state(&state)?,
            expected_current,
        )?;

        if let Some(previous_id) = expected_current {
            let previous_row = fetch_optional(
                &mut transaction,
                sqlx::query_as::<_, (String, i64)>(
                    "SELECT status, version_number FROM object_versions WHERE tenant_id = $1::uuid AND object_id = $2::uuid AND object_version_id = $3::uuid FOR UPDATE",
                )
                .bind(scope.tenant_id().to_string())
                .bind(object_id.to_string())
                .bind(previous_id.to_string()),
            )
            .await
            .map_err(ObjectRepositoryError::Database)?
            .ok_or(ObjectRepositoryError::VersionNotFound)?;
            let previous_status = parse_version_status(&previous_row.0)?;
            let previous_number = u64::try_from(previous_row.1)
                .map_err(|_| ObjectRepositoryError::VersionNumberOutOfRange)?;
            let mut previous_version = ObjectVersion::from_persisted(
                previous_id,
                scope.tenant_id(),
                object_id,
                previous_number,
                previous_status,
            )?;
            previous_version.supersede()?;
            object.replace_published_version(previous_id, &next_version)?;

            let result = execute(
                &mut transaction,
                sqlx::query(
                    "UPDATE object_versions SET status = 'superseded' WHERE tenant_id = $1::uuid AND object_id = $2::uuid AND object_version_id = $3::uuid AND status = 'published'",
                )
                .bind(scope.tenant_id().to_string())
                .bind(object_id.to_string())
                .bind(previous_id.to_string()),
            )
            .await
            .map_err(ObjectRepositoryError::Database)?;

            if result.rows_affected() != 1 {
                return Err(ObjectRepositoryError::CurrentVersionNotPublished);
            }
        } else {
            object.activate(&next_version)?;
        }

        let result = execute(
            &mut transaction,
            sqlx::query(
                "UPDATE object_versions SET status = 'published' WHERE tenant_id = $1::uuid AND object_id = $2::uuid AND object_version_id = $3::uuid AND status = 'draft'",
            )
            .bind(scope.tenant_id().to_string())
            .bind(object_id.to_string())
            .bind(version_id.to_string()),
        )
        .await
        .map_err(ObjectRepositoryError::Database)?;

        if result.rows_affected() != 1 {
            return Err(ObjectRepositoryError::VersionMustBeDraft);
        }

        let next_state = object_state_as_str(object.state());
        let result = execute(
            &mut transaction,
            sqlx::query(
                "UPDATE objects SET state = $4, published_version_id = $3::uuid WHERE tenant_id = $1::uuid AND object_id = $2::uuid AND published_version_id IS NOT DISTINCT FROM $5::uuid",
            )
            .bind(scope.tenant_id().to_string())
            .bind(object_id.to_string())
            .bind(version_id.to_string())
            .bind(next_state)
            .bind(expected_id),
        )
        .await
        .map_err(ObjectRepositoryError::Database)?;

        if result.rows_affected() != 1 {
            return Err(ObjectRepositoryError::PublishedVersionMismatch);
        }

        append_object_event(
            &mut transaction,
            scope,
            EventId::new(),
            object_id,
            "object.version_published",
            next_state,
            expected_current,
            Some(version_id),
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
    /// The requested object does not exist in this tenant.
    ObjectNotFound,
    /// A persisted object lifecycle state is unknown or violates domain invariants.
    DomainObject(ObjectError),
    /// A persisted version lifecycle state is unknown or violates domain invariants.
    DomainVersion(ObjectVersionError),
    /// The requested version does not belong to this object and tenant.
    VersionNotFound,
    /// The persisted object state does not permit this operation.
    InvalidObjectState,
    /// The current published pointer differs from the expected value.
    PublishedVersionMismatch,
    /// The version expected to be current is not currently Published.
    CurrentVersionNotPublished,
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
            Self::ObjectNotFound => formatter.write_str("object does not exist in this tenant"),
            Self::DomainObject(error) => write!(formatter, "object lifecycle error: {error:?}"),
            Self::DomainVersion(error) => {
                write!(formatter, "object version lifecycle error: {error:?}")
            }
            Self::VersionNotFound => {
                formatter.write_str("version does not belong to this object and tenant")
            }
            Self::InvalidObjectState => {
                formatter.write_str("object state does not permit this operation")
            }
            Self::PublishedVersionMismatch => {
                formatter.write_str("published version does not match expected value")
            }
            Self::CurrentVersionNotPublished => {
                formatter.write_str("current object version is not Published")
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

impl From<ObjectError> for ObjectRepositoryError {
    fn from(error: ObjectError) -> Self {
        Self::DomainObject(error)
    }
}

impl From<ObjectVersionError> for ObjectRepositoryError {
    fn from(error: ObjectVersionError) -> Self {
        Self::DomainVersion(error)
    }
}

impl From<sqlx::Error> for ObjectRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

fn parse_object_state(value: &str) -> Result<ObjectState, ObjectRepositoryError> {
    match value {
        "draft" => Ok(ObjectState::Draft),
        "validating" => Ok(ObjectState::Validating),
        "active" => Ok(ObjectState::Active),
        "suspended" => Ok(ObjectState::Suspended),
        "archived" => Ok(ObjectState::Archived),
        _ => Err(ObjectRepositoryError::InvalidObjectState),
    }
}

fn parse_version_status(value: &str) -> Result<ObjectVersionStatus, ObjectRepositoryError> {
    match value {
        "draft" => Ok(ObjectVersionStatus::Draft),
        "published" => Ok(ObjectVersionStatus::Published),
        "superseded" => Ok(ObjectVersionStatus::Superseded),
        _ => Err(ObjectRepositoryError::VersionMustBeDraft),
    }
}

const fn object_state_as_str(state: ObjectState) -> &'static str {
    match state {
        ObjectState::Draft => "draft",
        ObjectState::Validating => "validating",
        ObjectState::Active => "active",
        ObjectState::Suspended => "suspended",
        ObjectState::Archived => "archived",
    }
}
