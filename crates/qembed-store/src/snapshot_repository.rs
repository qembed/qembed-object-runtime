//! Tenant-scoped persistence for immutable, content-addressed snapshots.
//!
//! The public repository surface intentionally supports insertion and retrieval,
//! not mutation. Attempts to add update/delete operations are compile-time errors:
//!
//! ```compile_fail
//! use qembed_domain::ids::SnapshotId;
//! use qembed_domain::scope::Scope;
//! use qembed_store::snapshot_repository::SnapshotRepository;
//!
//! async fn mutate(repository: &SnapshotRepository, scope: &Scope, id: SnapshotId) {
//!     repository.delete(scope, id).await;
//! }
//! ```
//!
//! ```compile_fail
//! use qembed_domain::ids::SnapshotId;
//! use qembed_domain::scope::Scope;
//! use qembed_store::snapshot_repository::SnapshotRepository;
//!
//! async fn mutate(repository: &SnapshotRepository, scope: &Scope, id: SnapshotId) {
//!     repository.update(scope, id).await;
//! }
//! ```

use std::fmt;
use std::str::FromStr;

use qembed_domain::ids::{ObjectId, ObjectVersionId, SnapshotId, TenantId};
use qembed_domain::scope::Scope;
use qembed_domain::snapshot::{Snapshot, SnapshotError};
use sqlx::PgPool;

use crate::{begin_tenant_transaction, execute, fetch_optional};

/// Retrieves immutable snapshots. Creation is owned by ObjectRepository.
#[derive(Clone, Debug)]
pub struct SnapshotRepository {
    pool: PgPool,
}

impl SnapshotRepository {
    /// Snapshot insertion is intentionally unavailable outside atomic draft creation.
    ///
    /// ```compile_fail
    /// use qembed_domain::scope::Scope;
    /// use qembed_domain::snapshot::Snapshot;
    /// use qembed_store::snapshot_repository::SnapshotRepository;
    /// async fn bypass(repository: &SnapshotRepository, scope: &Scope, snapshot: &Snapshot) {
    ///     repository.insert(scope, snapshot).await;
    /// }
    /// ```
    /// Creates a repository backed by the application's PostgreSQL pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Reads a snapshot and verifies SHA-256 against the exact persisted bytes.
    pub async fn get(
        &self,
        scope: &Scope,
        snapshot_id: SnapshotId,
    ) -> Result<Option<Snapshot>, SnapshotRepositoryError> {
        let mut transaction = begin_tenant_transaction(&self.pool, scope)
            .await
            .map_err(SnapshotRepositoryError::Database)?;

        let row = fetch_optional(
            &mut transaction,
            sqlx::query_as::<
                _,
                (String, String, String, String, i32, Vec<u8>, Vec<u8>),
            >(
                "SELECT snapshot_id::text, tenant_id::text, object_id::text, object_version_id::text, schema_version, canonical_json, content_hash FROM snapshots WHERE tenant_id = $1::uuid AND snapshot_id = $2::uuid",
            )
            .bind(scope.tenant_id().to_string())
            .bind(snapshot_id.to_string()),
        )
        .await
        .map_err(SnapshotRepositoryError::Database)?;

        let snapshot = row
            .map(
                |(
                    id,
                    tenant_id,
                    object_id,
                    object_version_id,
                    schema_version,
                    canonical_json,
                    content_hash,
                )| {
                    let id = parse_id::<SnapshotId>(&id)?;
                    let tenant_id = parse_id::<TenantId>(&tenant_id)?;
                    let object_id = parse_id::<ObjectId>(&object_id)?;
                    let object_version_id = parse_id::<ObjectVersionId>(&object_version_id)?;
                    let schema_version = u32::try_from(schema_version)
                        .map_err(|_| SnapshotRepositoryError::InvalidSchemaVersion)?;
                    let content_hash: [u8; 32] = content_hash
                        .try_into()
                        .map_err(|_| SnapshotRepositoryError::InvalidHashLength)?;

                    Snapshot::from_persisted(
                        id,
                        tenant_id,
                        object_id,
                        object_version_id,
                        schema_version,
                        canonical_json,
                        content_hash,
                    )
                    .map_err(SnapshotRepositoryError::Domain)
                },
            )
            .transpose()?;

        transaction
            .commit()
            .await
            .map_err(SnapshotRepositoryError::Database)?;

        Ok(snapshot)
    }
}

fn parse_id<T>(value: &str) -> Result<T, SnapshotRepositoryError>
where
    T: FromStr,
{
    value
        .parse()
        .map_err(|_| SnapshotRepositoryError::InvalidIdentifier)
}

/// Validation or persistence failure while storing or reading a snapshot.
#[derive(Debug)]
pub enum SnapshotRepositoryError {
    /// The snapshot tenant does not match the authenticated scope.
    TenantMismatch,
    /// The schema version cannot be represented by the PostgreSQL INTEGER column.
    InvalidSchemaVersion,
    /// A persisted UUID could not be parsed into its domain identifier.
    InvalidIdentifier,
    /// The persisted SHA-256 column is not exactly 32 bytes.
    InvalidHashLength,
    /// The persisted snapshot failed a domain invariant, including hash verification.
    Domain(SnapshotError),
    /// PostgreSQL rejected the operation or could not complete it.
    Database(sqlx::Error),
}

impl fmt::Display for SnapshotRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TenantMismatch => formatter.write_str("snapshot tenant does not match scope"),
            Self::InvalidSchemaVersion => {
                formatter.write_str("snapshot schema version is out of range")
            }
            Self::InvalidIdentifier => {
                formatter.write_str("persisted snapshot contains an invalid identifier")
            }
            Self::InvalidHashLength => {
                formatter.write_str("persisted snapshot hash is not 32 bytes")
            }
            Self::Domain(error) => write!(formatter, "snapshot domain invariant failed: {error:?}"),
            Self::Database(error) => {
                write!(formatter, "snapshot repository database error: {error}")
            }
        }
    }
}

impl std::error::Error for SnapshotRepositoryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

impl From<sqlx::Error> for SnapshotRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}
