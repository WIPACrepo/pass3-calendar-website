use sqlx::postgres::PgPoolOptions;
use serde::Deserialize;
use std::env;
use std::path::Path;
use clap::Parser;

/// Setup database with schema and optionally import runs from CSV
#[derive(Parser, Debug)]
#[command(name = "setup_db")]
#[command(about = "Setup database and import runs from CSV", long_about = None)]
struct Args {
    /// Path to CSV file containing run data
    #[arg(long, value_name = "FILE")]
    grl_csv: Option<String>,

    /// Validate CSV without database operations
    #[arg(long)]
    dry_run: bool,

    /// Skip running database migrations (use if schema already exists)
    #[arg(long)]
    skip_migrations: bool,
}

#[derive(Debug, Deserialize)]
struct RunRecord {
    #[serde(rename = "Run number")]
    run_number: i32,
    #[serde(rename = "Start time")]
    start_time: String,
    #[serde(rename = "Stop time")]
    stop_time: Option<String>,
    #[serde(rename = "GRL InIce")]
    grl_in_ice: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    
    // If dry-run mode, only parse CSV without database operations
    if args.dry_run {
        if let Some(path) = args.grl_csv {
            if Path::new(&path).exists() {
                println!("Running in DRY-RUN mode (no database changes)");
                println!("Validating CSV file: {}...\n", path);
                println!("Note: This does NOT create database schema or validate migrations.");
                println!("      Only CSV parsing and data validation will be performed.\n");
                validate_csv_file(&path)?;
                return Ok(());
            } else {
                eprintln!("CSV file not found: {}", path);
                return Err("CSV file not found".into());
            }
        } else {
            eprintln!("Dry-run requires a CSV file path");
            eprintln!("Usage: cargo run --bin setup_db -- --dry-run --grl-csv <path_to_csv>");
            return Err("CSV path required for dry-run".into());
        }
    }
    
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

    if args.skip_migrations {
        println!("Skipping migrations (--skip-migrations flag set)...");
    } else {
        println!("Running migrations to create schema...");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await?;
    }

    // Import CSV if provided as argument
    if let Some(path) = args.grl_csv {
        if Path::new(&path).exists() {
            println!("\nImporting runs from {}...", path);
            import_runs_from_csv(&pool, &path).await?;
        } else {
            eprintln!("CSV file not found: {}", path);
            return Err("CSV file not found".into());
        }
    } else {
        println!("\n✓ Blank database setup complete!");
        println!("Database is ready at {}:{}/{}", db_host, db_port, db_name);
        println!("\nTo import runs from CSV, provide the file path:");
        println!("  cargo run --bin setup_db -- --grl-csv <path_to_csv>");
        println!("\nTo validate CSV without importing:");
        println!("  cargo run --bin setup_db -- --dry-run --grl-csv <path_to_csv>");
        println!("\nTo skip migrations (if schema already exists):");
        println!("  cargo run --bin setup_db -- --skip-migrations --grl-csv <path_to_csv>");
    }

    Ok(())
}

async fn import_runs_from_csv(
    pool: &sqlx::PgPool,
    csv_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let file = std::fs::File::open(csv_path)?;
    let mut reader = csv::Reader::from_reader(file);

    let mut imported = 0;
    let mut skipped = 0;

    for (idx, result) in reader.deserialize().enumerate() {
        let record: RunRecord = match result {
            Ok(r) => r,
            Err(e) => {
                println!("  Error parsing row {}: {}", idx + 2, e);
                skipped += 1;
                continue;
            }
        };

        // Only import if GRL InIce is "GOOD"
        if record.grl_in_ice.trim() != "GOOD" {
            skipped += 1;
            continue;
        }

        // Parse start time
        let run_start_date = match chrono::DateTime::parse_from_rfc3339(&record.start_time) {
            Ok(dt) => dt.with_timezone(&chrono::Utc),
            Err(_) => {
                // Try alternative format
                match chrono::NaiveDateTime::parse_from_str(
                    &record.start_time,
                    "%Y-%m-%d %H:%M:%S%.f",
                ) {
                    Ok(ndt) => ndt.and_utc(),
                    Err(_) => {
                        println!(
                            "  Skipping row {}: invalid start time '{}'",
                            idx + 2,
                            record.start_time
                        );
                        skipped += 1;
                        continue;
                    }
                }
            }
        };

        // Parse end time
        let run_end_date = if let Some(stop_time) = record.stop_time {
            if stop_time.is_empty() {
                run_start_date // Default to start time if no stop time
            } else {
                match chrono::DateTime::parse_from_rfc3339(&stop_time) {
                    Ok(dt) => dt.with_timezone(&chrono::Utc),
                    Err(_) => {
                        match chrono::NaiveDateTime::parse_from_str(
                            &stop_time,
                            "%Y-%m-%d %H:%M:%S%.f",
                        ) {
                            Ok(ndt) => ndt.and_utc(),
                            Err(_) => {
                                println!(
                                    "  Skipping row {}: invalid stop time '{}'",
                                    idx + 2,
                                    stop_time
                                );
                                skipped += 1;
                                continue;
                            }
                        }
                    }
                }
            }
        } else {
            run_start_date
        };

        // Insert into runs table
        match sqlx::query(
            "INSERT INTO runs (run_number, run_start_date, run_end_date, state, url) 
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (run_number) DO UPDATE SET state = EXCLUDED.state, url = EXCLUDED.url"
        )
        .bind(record.run_number)
        .bind(run_start_date)
        .bind(run_end_date)
        .bind("Not Yet Started") // default state
        .bind("") // empty url
        .execute(pool)
        .await {
            Ok(_) => {
                imported += 1;
                if imported % 100 == 0 {
                    println!("  Imported {} runs...", imported);
                }
            }
            Err(e) => {
                println!("  Error importing run {}: {}", record.run_number, e);
                skipped += 1;
            }
        }
    }

    println!("\n=== Import Complete ===");
    println!("Imported: {}", imported);
    println!("Skipped:  {}", skipped);
    println!("Total processed: {}", imported + skipped);

    Ok(())
}

fn validate_csv_file(csv_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let file = std::fs::File::open(csv_path)?;
    let mut reader = csv::Reader::from_reader(file);

    let mut valid = 0;
    let mut skipped = 0;
    let mut errors: Vec<String> = Vec::new();

    for (idx, result) in reader.deserialize().enumerate() {
        let record: RunRecord = match result {
            Ok(r) => r,
            Err(e) => {
                let error_msg = format!("Row {}: parsing error - {}", idx + 2, e);
                errors.push(error_msg);
                skipped += 1;
                continue;
            }
        };

        // Only validate if GRL InIce is "GOOD"
        if record.grl_in_ice.trim() != "GOOD" {
            skipped += 1;
            continue;
        }

        // Validate start time
        let run_start_date = match chrono::DateTime::parse_from_rfc3339(&record.start_time) {
            Ok(dt) => dt.with_timezone(&chrono::Utc),
            Err(_) => {
                // Try alternative format
                match chrono::NaiveDateTime::parse_from_str(
                    &record.start_time,
                    "%Y-%m-%d %H:%M:%S%.f",
                ) {
                    Ok(ndt) => ndt.and_utc(),
                    Err(_) => {
                        let error_msg =
                            format!("Row {}: invalid start time '{}'", idx + 2, record.start_time);
                        errors.push(error_msg);
                        skipped += 1;
                        continue;
                    }
                }
            }
        };

        // Validate end time
        let _run_end_date = if let Some(stop_time) = record.stop_time {
            if stop_time.is_empty() {
                run_start_date // Default to start time if no stop time
            } else {
                match chrono::DateTime::parse_from_rfc3339(&stop_time) {
                    Ok(dt) => dt.with_timezone(&chrono::Utc),
                    Err(_) => {
                        match chrono::NaiveDateTime::parse_from_str(
                            &stop_time,
                            "%Y-%m-%d %H:%M:%S%.f",
                        ) {
                            Ok(ndt) => ndt.and_utc(),
                            Err(_) => {
                                let error_msg = format!(
                                    "Row {}: invalid stop time '{}'",
                                    idx + 2, stop_time
                                );
                                errors.push(error_msg);
                                skipped += 1;
                                continue;
                            }
                        }
                    }
                }
            }
        } else {
            run_start_date
        };

        valid += 1;
    }

    println!("\n=== CSV Validation Report ===");
    println!("Valid records (GRL InIce = GOOD): {}", valid);
    println!("Skipped records: {}", skipped);
    println!("Total processed: {}", valid + skipped);

    if !errors.is_empty() {
        println!("\n⚠ Validation Errors:");
        for error in errors.iter().take(10) {
            println!("  {}", error);
        }
        if errors.len() > 10 {
            println!("  ... and {} more errors", errors.len() - 10);
        }
        return Err("CSV validation found errors".into());
    }

    println!("\n✓ CSV is valid and ready for import!");
    println!("Run the following to import: cargo run --bin setup_db -- --grl-csv {}", csv_path);

    Ok(())
}