//! PostgreSQL readiness checks. These checks are read-only and never apply migrations.

use std::path::Path;

use sqlx::PgPool;
use sqlx::migrate::Migrator;

use crate::external_deletion_ledger::{
    CallerDeletionReplayComplete, CallerDeletionReplayError, ExternalCallerDeletionLedger,
};

/// Why the database is not ready. This type is intentionally diagnostic-free.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PgReadinessFailure {
    /// Runtime PostgreSQL could not be reached or queried.
    DatabaseUnavailable,
    /// Runtime migration history differs from this binary.
    MigrationMismatch,
    /// The mandatory external Control-plane PostgreSQL could not be reached.
    ControlPlaneUnavailable,
    /// The Control-plane migration history differs from this binary.
    ControlPlaneMigrationMismatch,
    /// External tombstone replay has not been completed or its lock was lost.
    RestoreReplayIncomplete,
    /// The replay capability belongs to a different Control-plane ledger instance.
    ReplayLedgerMismatch,
}

/// Read-only probe for the database dependency required by the runtime.
#[derive(Clone)]
pub struct PgReadinessProbe {
    runtime_pool: PgPool,
    ledger: ExternalCallerDeletionLedger,
    replay_complete: CallerDeletionReplayComplete,
}

impl PgReadinessProbe {
    pub fn new(
        runtime_pool: PgPool,
        ledger: ExternalCallerDeletionLedger,
        replay_complete: CallerDeletionReplayComplete,
    ) -> Result<Self, PgReadinessFailure> {
        if !replay_complete.belongs_to(&ledger) {
            return Err(PgReadinessFailure::ReplayLedgerMismatch);
        }

        Ok(Self {
            runtime_pool,
            ledger,
            replay_complete,
        })
    }

    /// Returns ready only when both databases and their migration histories
    /// match this binary, and the completed replay lock can be safely released.
    ///
    /// The probe does not run migrations, acquire tenant context, or expose
    /// connection details and SQL diagnostics in its result.
    /// Returns the exact authoritative ledger used by restore and readiness.
    #[must_use]
    pub fn deletion_ledger(&self) -> ExternalCallerDeletionLedger {
        self.ledger.clone()
    }

    pub async fn check(&self) -> Result<(), PgReadinessFailure> {
        check_database_and_migrations(
            &self.runtime_pool,
            Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations"),
        )
        .await
        .map_err(|failure| match failure {
            PgReadinessFailure::DatabaseUnavailable => PgReadinessFailure::DatabaseUnavailable,
            _ => PgReadinessFailure::MigrationMismatch,
        })?;

        // Release the session-level replay lock before acquiring a second
        // Control-plane pool connection. This also works with max_connections=1.
        self.replay_complete
            .activate(&self.runtime_pool)
            .await
            .map_err(|_error: CallerDeletionReplayError| {
                PgReadinessFailure::RestoreReplayIncomplete
            })?;

        check_database_and_migrations(
            self.ledger.pool(),
            Path::new(env!("CARGO_MANIFEST_DIR")).join("control_plane_migrations"),
        )
        .await
        .map_err(|failure| match failure {
            PgReadinessFailure::DatabaseUnavailable => PgReadinessFailure::ControlPlaneUnavailable,
            _ => PgReadinessFailure::ControlPlaneMigrationMismatch,
        })?;

        Ok(())
    }
}

async fn check_database_and_migrations(
    pool: &PgPool,
    migrations_path: std::path::PathBuf,
) -> Result<(), PgReadinessFailure> {
    sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(pool)
        .await
        .map_err(|_| PgReadinessFailure::DatabaseUnavailable)?;

    let migrator = Migrator::new(migrations_path)
        .await
        .map_err(|_| PgReadinessFailure::MigrationMismatch)?;

    let applied: Vec<(i64, Vec<u8>, bool)> = match sqlx::query_as(
        "SELECT version, checksum, success FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("42P01") => {
            return Err(PgReadinessFailure::MigrationMismatch);
        }
        Err(_) => return Err(PgReadinessFailure::DatabaseUnavailable),
    };

    let expected = migrator
        .iter()
        .map(|migration| (migration.version, migration.checksum.as_ref().to_vec()))
        .collect::<Vec<_>>();

    if migration_state_matches(&expected, &applied) {
        Ok(())
    } else {
        Err(PgReadinessFailure::MigrationMismatch)
    }
}

fn migration_state_matches(expected: &[(i64, Vec<u8>)], applied: &[(i64, Vec<u8>, bool)]) -> bool {
    expected.len() == applied.len()
        && expected.iter().zip(applied).all(
            |(
                (expected_version, expected_checksum),
                (applied_version, applied_checksum, success),
            )| {
                *success
                    && expected_version == applied_version
                    && expected_checksum == applied_checksum
            },
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_history_must_match_exact_versions_checksums_and_success_state() {
        let expected = vec![(1, vec![1, 2]), (2, vec![3, 4])];
        assert!(migration_state_matches(
            &expected,
            &[(1, vec![1, 2], true), (2, vec![3, 4], true)]
        ));
        assert!(!migration_state_matches(
            &expected,
            &[(1, vec![1, 2], true)]
        ));
        assert!(!migration_state_matches(
            &expected,
            &[(1, vec![1, 2], true), (3, vec![3, 4], true)]
        ));
        assert!(!migration_state_matches(
            &expected,
            &[(1, vec![9, 9], true), (2, vec![3, 4], true)]
        ));
        assert!(!migration_state_matches(
            &expected,
            &[(1, vec![1, 2], true), (2, vec![3, 4], false)]
        ));
    }
}
