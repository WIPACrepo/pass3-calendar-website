#!/bin/bash
# Setup blank database with schema only (no data imports)

set -e

# Set defaults for database credentials
export DB_USER="${DB_USER:-postgres}"
export DB_PASSWORD="${DB_PASSWORD:-postgres}"
export DB_HOST="${DB_HOST:-localhost}"
export DB_PORT="${DB_PORT:-5432}"
export DB_NAME="${DB_NAME:-calendar}"

echo "Setting up blank database..."
echo "Host: $DB_HOST:$DB_PORT"
echo "Database: $DB_NAME"
echo "User: $DB_USER"

# Create a simple Rust program to run migrations only
cd "$(dirname "$0")/.."

# Create temporary binary for DB setup if it doesn't exist
if [ ! -f "src/bin/setup_db.rs" ]; then
    cat > src/bin/setup_db.rs << 'EOF'
use sqlx::postgres::PgPoolOptions;
use std::env;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db_user = env::var("DB_USER").unwrap_or_else(|_| "postgres".to_string());
    let db_password = env::var("DB_PASSWORD").unwrap_or_else(|_| "postgres".to_string());
    let db_host = env::var("DB_HOST").unwrap_or_else(|_| "localhost".to_string());
    let db_port = env::var("DB_PORT").unwrap_or_else(|_| "5432".to_string());
    let db_name = env::var("DB_NAME").unwrap_or_else(|_| "calendar".to_string());

    let database_url = format!(
        "postgres://{}:{}@{}:{}/{}",
        db_user, db_password, db_host, db_port, db_name
    );

    println!("Connecting to database at {}:{}...", db_host, db_port);

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await?;

    println!("Running migrations to create schema...");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await?;

    println!("\n✓ Blank database setup complete!");
    println!("Database is ready at {}:{}/{}", db_host, db_port, db_name);

    Ok(())
}
EOF
fi

# Build and run
echo "Building setup_db binary..."
$HOME/.cargo/bin/cargo build --bin setup_db

if [ -f "./target/debug/setup_db" ]; then
    ./target/debug/setup_db
else
    echo "Error: Failed to build setup_db binary"
    exit 1
fi

echo "Done!"
