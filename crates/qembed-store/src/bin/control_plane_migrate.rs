use std::{env, error::Error};

use qembed_store::{PgPoolConfig, connect_pool, external_deletion_ledger::run_control_plane_migrations};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let database_url = env::var("CONTROL_PLANE_DATABASE_URL")?;
    let pool = connect_pool(&database_url, PgPoolConfig::default()).await?;

    run_control_plane_migrations(&pool).await?;
    pool.close().await;

    Ok(())
}
