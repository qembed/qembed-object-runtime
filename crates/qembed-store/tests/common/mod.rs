use postgres::{Client, NoTls};

pub fn connect() -> Client {
    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set by scripts/test-postgres.sh");

    Client::connect(&database_url, NoTls)
        .expect("the PostgreSQL test database should accept connections")
}
