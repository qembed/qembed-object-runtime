use qembed_store::{connect_pool, PgPoolConfig};
use sqlx::PgPool;

pub async fn connect() -> PgPool {
    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set by scripts/test-postgres.sh");

    connect_pool(&database_url, PgPoolConfig::default())
        .await
        .expect("the PostgreSQL test database should accept connections")
}
