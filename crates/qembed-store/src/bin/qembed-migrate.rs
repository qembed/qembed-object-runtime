use std::env;
use std::error::Error;

use qembed_store::{connect_pool, run_migrations, PgPoolConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let database_url = env::var("DATABASE_URL")?;
    let pool = connect_pool(&database_url, PgPoolConfig::default()).await?;

    run_migrations(&pool).await?;
    pool.close().await;

    println!("Database migrations completed successfully.");
    Ok(())
}
