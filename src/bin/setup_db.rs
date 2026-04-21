use clap::Parser;
use pass3_calendar_website::{
    importers::{import_grl_file, validate_grl_json_file},
    run_app_migrations,
};
use sqlx::postgres::PgPoolOptions;
use std::path::Path;

/// Setup database with schema and optionally import runs from GRL JSON
#[derive(Parser, Debug)]
#[command(name = "setup_db")]
#[command(about = "Setup database and import runs from GRL JSON", long_about = None)]
struct Args {
    /// Path to GRL JSON file containing run data
    #[arg(long, value_name = "FILE")]
    grl_json: Option<String>,

    /// Validate GRL JSON without database operations
    #[arg(long)]
    dry_run: bool,

    /// Skip running database migrations (use if schema already exists)
    #[arg(long)]
    skip_migrations: bool,

    /// Database user
    #[arg(long, default_value = "postgres")]
    db_user: String,

    /// Database password
    #[arg(long, default_value = "postgres")]
    db_password: String,

    /// Database host
    #[arg(long, default_value = "localhost")]
    db_host: String,

    /// Database port
    #[arg(long, default_value = "5432")]
    db_port: String,

    /// Database name
    #[arg(long, default_value = "calendar")]
    db_name: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = Args::parse();
    
    // If dry-run mode, only parse GRL JSON without database operations
    if args.dry_run {
        if let Some(path) = args.grl_json {
            if Path::new(&path).exists() {
                println!("Running in DRY-RUN mode (no database changes)");
                println!("Validating GRL JSON file: {}...\n", path);
                println!("Note: This does NOT create database schema or validate migrations.");
                println!("      Only JSON parsing and data validation will be performed.\n");
                validate_grl_json_file(Path::new(&path))?;
                return Ok(());
            } else {
                eprintln!("GRL JSON file not found: {}", path);
                return Err("GRL JSON file not found".into());
            }
        } else {
            eprintln!("Dry-run requires a GRL JSON file path");
            eprintln!("Usage: cargo run --bin setup_db -- --dry-run --grl-json <path_to_json>");
            return Err("GRL JSON path required for dry-run".into());
        }
    }
    
    let database_url = format!(
        "postgres://{}:{}@{}:{}/{}",
        args.db_user, args.db_password, args.db_host, args.db_port, args.db_name
    );

    println!("Connecting to database at {}:{}...", args.db_host, args.db_port);

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await?;

    if args.skip_migrations {
        println!("Skipping migrations (--skip-migrations flag set)...");
    } else {
        println!("Running migrations to create schema...");
        run_app_migrations(&pool).await?;
    }

    // Import GRL JSON if provided as argument
    if let Some(path) = args.grl_json {
        if Path::new(&path).exists() {
            println!("\nImporting runs from {}...", path);
            let report = import_grl_file(&pool, Path::new(&path)).await?;
            println!("\n=== Import Complete ===");
            println!("Imported: {}", report.imported);
            println!("Skipped:  {}", report.skipped);
            println!("Total processed: {}", report.imported + report.skipped);
        } else {
            eprintln!("GRL JSON file not found: {}", path);
            return Err("GRL JSON file not found".into());
        }
    } else {
        println!("\n✓ Blank database setup complete!");
        println!("Database is ready at {}:{}/{}", args.db_host, args.db_port, args.db_name);
        println!("\nTo import runs from GRL JSON, provide the file path:");
        println!("  cargo run --bin setup_db -- --grl-json <path_to_json>");
        println!("\nTo validate GRL JSON without importing:");
        println!("  cargo run --bin setup_db -- --dry-run --grl-json <path_to_json>");
        println!("\nTo skip migrations (if schema already exists):");
        println!("  cargo run --bin setup_db -- --skip-migrations --grl-json <path_to_json>");
        println!("\nTo use custom database connection:");
        println!("  cargo run --bin setup_db -- --db-host myhost --db-user myuser --db-password mypass --db-name mydb");
    }

    Ok(())
}
