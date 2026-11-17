//! PostgreSQL persistence infrastructure.

use std::time::Duration;

use sqlx::PgPool;
use sqlx::migrate::{MigrateError, Migrator};
use sqlx::postgres::PgPoolOptions;

/// Connection-pool settings shared by application and migration entry points.
#[derive(Clone, Debug)]
pub struct PgPoolConfig {
    /// Maximum number of simultaneously open PostgreSQL connections.
    pub max_connections: u32,
    /// Maximum time to wait when acquiring a connection from the pool.
    pub acquire_timeout: Duration,
}

impl Default for PgPoolConfig {
    fn default() -> Self {
        Self {
            max_connections: 10,
            acquire_timeout: Duration::from_secs(5),
        }
    }
}

/// Create a PostgreSQL pool without applying or modifying any database schema.
pub async fn connect_pool(database_url: &str, config: PgPoolConfig) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(config.max_connections)
        .acquire_timeout(config.acquire_timeout)
        .connect(database_url)
        .await
}

/// Apply the migrations bundled with this crate.
///
/// This function is intentionally explicit: connecting to PostgreSQL never runs
/// migrations as a side effect.
pub async fn run_migrations(pool: &PgPool) -> Result<(), MigrateError> {
    let migrations_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let migrator = Migrator::new(migrations_path).await?;
    migrator.run(pool).await
}
