//! Mandatory append-only Caller deletion ledger in the independent Control-plane PostgreSQL cluster.
//!
//! The Control-plane stores only tenant-scoped HMAC tombstones. It never stores
//! raw Caller IDs, credentials, or Caller-derived content. This adapter has no
//! Runtime-local implementation or fallback.

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use qembed_domain::ids::{CallerId, TenantId};
use ring::hmac;
use sqlx::{
    PgPool, Postgres,
    migrate::{MigrateError, Migrator},
    pool::PoolConnection,
};

const HMAC_DOMAIN: &[u8] = b"qembed/caller-deletion/v1\0";
const KEY_VERSION: &str = "v1";
const ADVISORY_LOCK_KEY: i64 = 61_061;

/// Stable HMAC key used to recognize Caller identities after a Runtime restore.
///
/// The key must be supplied by deployment secret management and retained for as
/// long as any tombstone using this version exists. Debug output never reveals it.
pub struct CallerDeletionKey([u8; 32]);

impl CallerDeletionKey {
    /// Creates the v1 key from exactly 32 secret bytes.
    pub fn new(bytes: &[u8]) -> Result<Self, CallerDeletionKeyError> {
        let key: [u8; 32] = bytes
            .try_into()
            .map_err(|_| CallerDeletionKeyError::InvalidLength)?;
        Ok(Self(key))
    }

    /// Loads the required v1 key from CALLER_DELETION_HMAC_KEY. The value
    /// must be 64 hexadecimal characters supplied by deployment secret management.
    pub fn from_env() -> Result<Self, CallerDeletionKeyError> {
        let value = std::env::var("CALLER_DELETION_HMAC_KEY")
            .map_err(|_| CallerDeletionKeyError::MissingEnvironmentVariable)?;
        Self::from_hex(&value)
    }

    /// Parses a 64-character hexadecimal representation suitable for an
    /// environment variable backed by a secret manager.
    pub fn from_hex(value: &str) -> Result<Self, CallerDeletionKeyError> {
        if value.len() != 64 {
            return Err(CallerDeletionKeyError::InvalidEncoding);
        }
        let mut bytes = [0_u8; 32];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            let high = (pair[0] as char)
                .to_digit(16)
                .ok_or(CallerDeletionKeyError::InvalidEncoding)?;
            let low = (pair[1] as char)
                .to_digit(16)
                .ok_or(CallerDeletionKeyError::InvalidEncoding)?;
            bytes[index] = ((high << 4) | low) as u8;
        }
        Ok(Self(bytes))
    }

    /// Computes the tenant-bound, domain-separated digest for one Caller.
    #[must_use]
    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    #[must_use]
    pub fn caller_hash(&self, tenant_id: TenantId, caller_id: CallerId) -> [u8; 32] {
        let key = hmac::Key::new(hmac::HMAC_SHA256, &self.0);
        let mut input = Vec::with_capacity(HMAC_DOMAIN.len() + 32);
        input.extend_from_slice(HMAC_DOMAIN);
        input.extend_from_slice(tenant_id.as_uuid().as_bytes());
        input.extend_from_slice(caller_id.as_uuid().as_bytes());
        let tag = hmac::sign(&key, &input);
        let mut digest = [0_u8; 32];
        digest.copy_from_slice(tag.as_ref());
        digest
    }
}

impl fmt::Debug for CallerDeletionKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("CallerDeletionKey")
            .field(&"[REDACTED]")
            .finish()
    }
}

/// Invalid secret material for the v1 Caller deletion HMAC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallerDeletionKeyError {
    MissingEnvironmentVariable,
    InvalidLength,
    InvalidEncoding,
}

impl fmt::Display for CallerDeletionKeyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingEnvironmentVariable => {
                formatter.write_str("CALLER_DELETION_HMAC_KEY is required")
            }
            Self::InvalidLength => formatter.write_str("Caller deletion HMAC key must be 32 bytes"),
            Self::InvalidEncoding => {
                formatter.write_str("Caller deletion HMAC key must be 64 hexadecimal characters")
            }
        }
    }
}

impl std::error::Error for CallerDeletionKeyError {}

/// One immutable external deletion tombstone.
#[derive(Clone, PartialEq, Eq)]
pub struct CallerDeletionTombstone {
    pub sequence_id: i64,
    pub tenant_id: TenantId,
    pub caller_hash: [u8; 32],
    pub key_version: String,
}

impl fmt::Debug for CallerDeletionTombstone {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CallerDeletionTombstone")
            .field("sequence_id", &self.sequence_id)
            .field("tenant_id", &self.tenant_id)
            .field("caller_hash", &"[REDACTED]")
            .field("key_version", &self.key_version)
            .finish()
    }
}

/// Consistent append-only ledger snapshot and its replay cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallerDeletionLedgerSnapshot {
    pub high_water_mark: i64,
    pub tombstones: Vec<CallerDeletionTombstone>,
}

/// Result of an idempotent tombstone insert.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallerDeletionRecord {
    pub sequence_id: i64,
    pub inserted: bool,
}

/// Failures from the mandatory external ledger.
#[derive(Debug)]
pub enum CallerDeletionLedgerError {
    Database(sqlx::Error),
    InvalidRecord,
    UnsupportedKeyVersion,
}

impl fmt::Display for CallerDeletionLedgerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => {
                write!(formatter, "Control-plane ledger database error: {error}")
            }
            Self::InvalidRecord => {
                formatter.write_str("Control-plane ledger returned an invalid tombstone")
            }
            Self::UnsupportedKeyVersion => {
                formatter.write_str("Control-plane ledger contains an unsupported HMAC key version")
            }
        }
    }
}

impl std::error::Error for CallerDeletionLedgerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

/// External-only PostgreSQL adapter. The pool must point at the dedicated
/// Control-plane cluster and use a login that inherits only
/// qembed_control_plane_runtime privileges.
#[derive(Clone)]
pub struct ExternalCallerDeletionLedger {
    pool: PgPool,
    key: std::sync::Arc<CallerDeletionKey>,
    identity: Arc<()>,
}

impl ExternalCallerDeletionLedger {
    #[must_use]
    pub fn new(pool: PgPool, key: CallerDeletionKey) -> Self {
        Self {
            pool,
            key: std::sync::Arc::new(key),
            identity: Arc::new(()),
        }
    }

    pub(crate) fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub(crate) fn shares_identity(&self, identity: &Arc<()>) -> bool {
        Arc::ptr_eq(&self.identity, identity)
    }

    /// Writes a durable tombstone before Runtime physical cleanup. Repeated
    /// writes return the original sequence and never modify deleted_at.
    pub async fn record(
        &self,
        tenant_id: TenantId,
        caller_id: CallerId,
    ) -> Result<CallerDeletionRecord, CallerDeletionLedgerError> {
        let caller_hash = self.key.caller_hash(tenant_id, caller_id);
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(CallerDeletionLedgerError::Database)?;

        // The BEFORE INSERT trigger takes advisory lock 61061 before assigning
        // the sequence ID. Do not acquire it here as well: the database owns
        // serialization for every writer, including non-Rust maintenance code.
        let inserted = sqlx::query_scalar::<_, i64>(
            r#"INSERT INTO qembed_control_plane.caller_deletion_tombstones
               (tenant_id, caller_hash, key_version) VALUES ($1, $2, $3)
               ON CONFLICT (tenant_id, caller_hash) DO NOTHING RETURNING sequence_id"#,
        )
        .bind(tenant_id.into_uuid())
        .bind(caller_hash.as_slice())
        .bind(KEY_VERSION)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(CallerDeletionLedgerError::Database)?;

        let (sequence_id, was_inserted) = match inserted {
            Some(sequence_id) => (sequence_id, true),
            None => {
                let sequence_id = sqlx::query_scalar::<_, i64>(
                    r#"SELECT sequence_id FROM qembed_control_plane.caller_deletion_tombstones
                       WHERE tenant_id = $1 AND caller_hash = $2"#,
                )
                .bind(tenant_id.into_uuid())
                .bind(caller_hash.as_slice())
                .fetch_one(&mut *transaction)
                .await
                .map_err(CallerDeletionLedgerError::Database)?;
                (sequence_id, false)
            }
        };

        transaction
            .commit()
            .await
            .map_err(CallerDeletionLedgerError::Database)?;
        Ok(CallerDeletionRecord {
            sequence_id,
            inserted: was_inserted,
        })
    }

    /// Reads a consistent immutable snapshot. Writers serialize on the same
    /// transaction-scoped advisory lock, so no transaction can commit later with
    /// a sequence ID at or below this snapshot's high-water mark.
    pub async fn snapshot(
        &self,
    ) -> Result<CallerDeletionLedgerSnapshot, CallerDeletionLedgerError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(CallerDeletionLedgerError::Database)?;
        lock_ledger(&mut transaction).await?;
        let high_water_mark = sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(MAX(sequence_id), 0) FROM qembed_control_plane.caller_deletion_tombstones",
        )
        .fetch_one(&mut *transaction)
        .await
        .map_err(CallerDeletionLedgerError::Database)?;
        let rows = sqlx::query_as::<_, (i64, uuid::Uuid, Vec<u8>, String)>(
            r#"SELECT sequence_id, tenant_id, caller_hash, key_version
               FROM qembed_control_plane.caller_deletion_tombstones
               WHERE sequence_id <= $1 ORDER BY sequence_id"#,
        )
        .bind(high_water_mark)
        .fetch_all(&mut *transaction)
        .await
        .map_err(CallerDeletionLedgerError::Database)?;
        transaction
            .commit()
            .await
            .map_err(CallerDeletionLedgerError::Database)?;

        let mut tombstones = Vec::with_capacity(rows.len());
        for (sequence_id, tenant_uuid, hash, key_version) in rows {
            let caller_hash: [u8; 32] = hash
                .try_into()
                .map_err(|_| CallerDeletionLedgerError::InvalidRecord)?;
            if key_version != KEY_VERSION {
                return Err(CallerDeletionLedgerError::UnsupportedKeyVersion);
            }
            tombstones.push(CallerDeletionTombstone {
                sequence_id,
                tenant_id: TenantId::from_uuid(tenant_uuid),
                caller_hash,
                key_version,
            });
        }

        Ok(CallerDeletionLedgerSnapshot {
            high_water_mark,
            tombstones,
        })
    }

    /// Returns a serialized high-water mark suitable for the final catch-up check.
    pub async fn high_water_mark(&self) -> Result<i64, CallerDeletionLedgerError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(CallerDeletionLedgerError::Database)?;
        lock_ledger(&mut transaction).await?;
        let value = sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(MAX(sequence_id), 0) FROM qembed_control_plane.caller_deletion_tombstones",
        )
        .fetch_one(&mut *transaction)
        .await
        .map_err(CallerDeletionLedgerError::Database)?;
        transaction
            .commit()
            .await
            .map_err(CallerDeletionLedgerError::Database)?;
        Ok(value)
    }
}

async fn lock_ledger(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<(), CallerDeletionLedgerError> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(ADVISORY_LOCK_KEY)
        .execute(&mut **transaction)
        .await
        .map_err(CallerDeletionLedgerError::Database)?;
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PostgresClusterIdentity {
    server_address: Option<String>,
    server_port: Option<i32>,
    postmaster_start_time: String,
}

async fn cluster_identity_from_pool(
    pool: &PgPool,
) -> Result<PostgresClusterIdentity, sqlx::Error> {
    let (server_address, server_port, postmaster_start_time) = sqlx::query_as(
        "SELECT inet_server_addr()::text, inet_server_port(), pg_postmaster_start_time()::text",
    )
    .fetch_one(pool)
    .await?;
    Ok(PostgresClusterIdentity {
        server_address,
        server_port,
        postmaster_start_time,
    })
}

async fn cluster_identity_from_connection(
    connection: &mut PoolConnection<Postgres>,
) -> Result<PostgresClusterIdentity, sqlx::Error> {
    let (server_address, server_port, postmaster_start_time) = sqlx::query_as(
        "SELECT inet_server_addr()::text, inet_server_port(), pg_postmaster_start_time()::text",
    )
    .fetch_one(&mut **connection)
    .await?;
    Ok(PostgresClusterIdentity {
        server_address,
        server_port,
        postmaster_start_time,
    })
}

/// A startup capability proving that the complete external ledger was replayed.
///
/// The Control-plane session advisory lock remains held until the first readiness
/// check activates this token. That closes the race between replay's final
/// high-water check and admitting Runtime traffic.
#[derive(Clone)]
pub struct CallerDeletionReplayComplete {
    connection: Arc<tokio::sync::Mutex<Option<PoolConnection<Postgres>>>>,
    activated: Arc<AtomicBool>,
    high_water_mark: i64,
    ledger_identity: Arc<()>,
    runtime_cluster_identity: PostgresClusterIdentity,
    control_plane_cluster_identity: PostgresClusterIdentity,
}

impl CallerDeletionReplayComplete {
    #[must_use]
    pub const fn high_water_mark(&self) -> i64 {
        self.high_water_mark
    }

    pub(crate) fn belongs_to(&self, ledger: &ExternalCallerDeletionLedger) -> bool {
        ledger.shares_identity(&self.ledger_identity)
    }

    pub(crate) async fn activate(
        &self,
        runtime_pool: &PgPool,
    ) -> Result<(), CallerDeletionReplayError> {
        if self.activated.load(Ordering::Acquire) {
            return Ok(());
        }

        let mut guard = self.connection.lock().await;
        let Some(mut connection) = guard.take() else {
            return Err(CallerDeletionReplayError::ReplayLockLost);
        };

        let runtime_identity = cluster_identity_from_pool(runtime_pool)
            .await
            .map_err(CallerDeletionReplayError::Runtime)?;
        let control_plane_identity = cluster_identity_from_connection(&mut connection)
            .await
            .map_err(CallerDeletionReplayError::Runtime)?;

        if runtime_identity != self.runtime_cluster_identity {
            return Err(CallerDeletionReplayError::RuntimeDatabaseMismatch);
        }
        if control_plane_identity != self.control_plane_cluster_identity {
            return Err(CallerDeletionReplayError::ControlPlaneDatabaseMismatch);
        }
        if runtime_identity == control_plane_identity {
            return Err(CallerDeletionReplayError::ControlPlaneNotIndependent);
        }

        let unlocked = sqlx::query_scalar::<_, bool>("SELECT pg_advisory_unlock($1)")
            .bind(ADVISORY_LOCK_KEY)
            .fetch_one(&mut *connection)
            .await
            .map_err(CallerDeletionReplayError::Runtime)?;

        if !unlocked {
            return Err(CallerDeletionReplayError::ReplayLockLost);
        }

        self.activated.store(true, Ordering::Release);
        Ok(())
    }
}

/// Why a startup restore replay could not be proven complete.
#[derive(Debug)]
pub enum CallerDeletionReplayError {
    Ledger(CallerDeletionLedgerError),
    Runtime(sqlx::Error),
    RestoreRoleRequired,
    ActiveWorkPending { sequence_id: i64 },
    ReplayLockLost,
    RuntimeDatabaseMismatch,
    ControlPlaneDatabaseMismatch,
    ControlPlaneNotIndependent,
}

impl fmt::Display for CallerDeletionReplayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ledger(error) => write!(formatter, "Control-plane replay ledger error: {error}"),
            Self::Runtime(error) => write!(formatter, "Runtime restore database error: {error}"),
            Self::RestoreRoleRequired => {
                formatter.write_str("restore connection must inherit qembed_restore")
            }
            Self::ActiveWorkPending { sequence_id } => {
                write!(
                    formatter,
                    "Caller deletion tombstone {sequence_id} is waiting for active work"
                )
            }
            Self::ReplayLockLost => {
                formatter.write_str("Control-plane replay lock was lost before readiness")
            }
            Self::RuntimeDatabaseMismatch => {
                formatter.write_str("Runtime database differs from the database used for replay")
            }
            Self::ControlPlaneDatabaseMismatch => {
                formatter.write_str("Control-plane database differs from the database used for replay")
            }
            Self::ControlPlaneNotIndependent => {
                formatter.write_str("Runtime and Control-plane must be separate PostgreSQL clusters")
            }
        }
    }
}

impl std::error::Error for CallerDeletionReplayError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Ledger(error) => Some(error),
            Self::Runtime(error) => Some(error),
            _ => None,
        }
    }
}

/// Runs the restore replay through the narrowly privileged qembed_restore role.
#[derive(Clone)]
pub struct PgCallerDeletionRestore {
    runtime_restore_pool: PgPool,
    ledger: ExternalCallerDeletionLedger,
}

impl PgCallerDeletionRestore {
    #[must_use]
    pub fn new(runtime_restore_pool: PgPool, ledger: ExternalCallerDeletionLedger) -> Self {
        Self {
            runtime_restore_pool,
            ledger,
        }
    }

    /// Replays every tombstone while holding the Control-plane writer lock.
    ///
    /// The returned capability must be passed to PgReadinessProbe. Its first
    /// successful readiness check releases the lock, allowing new tombstones.
    pub async fn run(&self) -> Result<CallerDeletionReplayComplete, CallerDeletionReplayError> {
        let has_restore_role = sqlx::query_scalar::<_, bool>(
            "SELECT pg_has_role(current_user, 'qembed_restore', 'MEMBER')",
        )
        .fetch_one(&self.runtime_restore_pool)
        .await
        .map_err(CallerDeletionReplayError::Runtime)?;

        if !has_restore_role {
            return Err(CallerDeletionReplayError::RestoreRoleRequired);
        }

        let mut control_plane_connection = self.ledger.pool.acquire().await.map_err(|error| {
            CallerDeletionReplayError::Ledger(CallerDeletionLedgerError::Database(error))
        })?;

        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(ADVISORY_LOCK_KEY)
            .execute(&mut *control_plane_connection)
            .await
            .map_err(|error| {
                CallerDeletionReplayError::Ledger(CallerDeletionLedgerError::Database(error))
            })?;

        // With the session lock held, no ledger writer can commit until the
        // readiness probe activates the returned capability.
        let runtime_cluster_identity = cluster_identity_from_pool(&self.runtime_restore_pool)
            .await
            .map_err(CallerDeletionReplayError::Runtime)?;
        let control_plane_cluster_identity =
            cluster_identity_from_connection(&mut control_plane_connection)
                .await
                .map_err(CallerDeletionReplayError::Runtime)?;
        if runtime_cluster_identity == control_plane_cluster_identity {
            return Err(CallerDeletionReplayError::ControlPlaneNotIndependent);
        }

        let snapshot = read_snapshot_with_lock(&mut control_plane_connection)
            .await
            .map_err(CallerDeletionReplayError::Ledger)?;

        for tombstone in &snapshot.tombstones {
            let complete = sqlx::query_scalar::<_, bool>(
                "SELECT qembed_crypto.replay_caller_deletion($1, $2, $3, $4)",
            )
            .bind(tombstone.tenant_id.into_uuid())
            .bind(tombstone.caller_hash.as_slice())
            .bind(&tombstone.key_version)
            .bind(self.ledger.key.as_bytes().as_slice())
            .fetch_one(&self.runtime_restore_pool)
            .await
            .map_err(CallerDeletionReplayError::Runtime)?;

            if !complete {
                return Err(CallerDeletionReplayError::ActiveWorkPending {
                    sequence_id: tombstone.sequence_id,
                });
            }
        }

        let high_water_mark = sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(MAX(sequence_id), 0) FROM qembed_control_plane.caller_deletion_tombstones",
        )
        .fetch_one(&mut *control_plane_connection)
        .await
        .map_err(|error| CallerDeletionReplayError::Ledger(
            CallerDeletionLedgerError::Database(error),
        ))?;

        if high_water_mark != snapshot.high_water_mark {
            return Err(CallerDeletionReplayError::ReplayLockLost);
        }

        Ok(CallerDeletionReplayComplete {
            connection: Arc::new(tokio::sync::Mutex::new(Some(control_plane_connection))),
            activated: Arc::new(AtomicBool::new(false)),
            high_water_mark,
            ledger_identity: Arc::clone(&self.ledger.identity),
            runtime_cluster_identity,
            control_plane_cluster_identity,
        })
    }
}

async fn read_snapshot_with_lock(
    connection: &mut PoolConnection<Postgres>,
) -> Result<CallerDeletionLedgerSnapshot, CallerDeletionLedgerError> {
    let high_water_mark = sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(MAX(sequence_id), 0) FROM qembed_control_plane.caller_deletion_tombstones",
    )
    .fetch_one(&mut **connection)
    .await
    .map_err(CallerDeletionLedgerError::Database)?;

    let rows = sqlx::query_as::<_, (i64, uuid::Uuid, Vec<u8>, String)>(
        r#"SELECT sequence_id, tenant_id, caller_hash, key_version
           FROM qembed_control_plane.caller_deletion_tombstones
           WHERE sequence_id <= $1 ORDER BY sequence_id"#,
    )
    .bind(high_water_mark)
    .fetch_all(&mut **connection)
    .await
    .map_err(CallerDeletionLedgerError::Database)?;

    let mut tombstones = Vec::with_capacity(rows.len());
    for (sequence_id, tenant_uuid, hash, key_version) in rows {
        let caller_hash: [u8; 32] = hash
            .try_into()
            .map_err(|_| CallerDeletionLedgerError::InvalidRecord)?;
        if key_version != KEY_VERSION {
            return Err(CallerDeletionLedgerError::UnsupportedKeyVersion);
        }
        tombstones.push(CallerDeletionTombstone {
            sequence_id,
            tenant_id: TenantId::from_uuid(tenant_uuid),
            caller_hash,
            key_version,
        });
    }

    Ok(CallerDeletionLedgerSnapshot {
        high_water_mark,
        tombstones,
    })
}

/// Applies the standalone SQLx migration set to the Control-plane database.
/// This is an explicit deployment operation and is never run by pool creation.
pub async fn run_control_plane_migrations(pool: &PgPool) -> Result<(), MigrateError> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("control_plane_migrations");
    let migrator = Migrator::new(path).await?;
    migrator.run(pool).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caller_hash_is_stable_and_tenant_scoped() {
        let key = CallerDeletionKey::new(&[7_u8; 32]).unwrap();
        let tenant = TenantId::new();
        let caller = CallerId::new();
        let hash = key.caller_hash(tenant, caller);

        assert_eq!(hash, key.caller_hash(tenant, caller));
        assert_ne!(hash, key.caller_hash(TenantId::new(), caller));
        assert_ne!(hash, key.caller_hash(tenant, CallerId::new()));
    }

    #[test]
    fn key_parser_requires_exact_32_byte_hex_secret() {
        assert!(CallerDeletionKey::from_hex(&"ab".repeat(32)).is_ok());
        assert_eq!(
            CallerDeletionKey::new(&[1_u8; 31]).unwrap_err(),
            CallerDeletionKeyError::InvalidLength
        );
        assert_eq!(
            CallerDeletionKey::from_hex("not-hex").unwrap_err(),
            CallerDeletionKeyError::InvalidEncoding
        );
        assert_eq!(
            CallerDeletionKey::from_hex(&"a".repeat(64)).unwrap_err(),
            CallerDeletionKeyError::InvalidEncoding
        );
    }

    #[test]
    fn key_debug_output_is_redacted() {
        let key = CallerDeletionKey::new(&[0x5a; 32]).unwrap();
        let debug = format!("{key:?}");
        assert!(debug.contains("REDACTED"));
        assert!(!debug.contains("5a5a5a"));
    }
}
