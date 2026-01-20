use clap::{Parser, ArgGroup};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use uuid::Uuid;
use regex::Regex;
use pass3_calendar_website::{NdJsonFileRecord, Step1FileRecord, Stage, insert_file, Checksum, RunInfo};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
#[command(group(
    ArgGroup::new("input")
        .required(true)
        .args(["pfraw_file_ndjson", "step1_file_json"]),
))]
struct Args {
    /// Path to the PFRaw ndjson file to import
    #[arg(long)]
    pfraw_file_ndjson: Option<String>,

    /// Path to the Step 1 JSON file to import
    #[arg(long)]
    step1_file_json: Option<String>,

    /// Database user
    #[arg(long)]
    db_user: String,

    /// Database password
    #[arg(long)]
    db_password: String,

    /// Database host
    #[arg(long)]
    db_host: String,

    /// Database port
    #[arg(long, default_value = "5432")]
    db_port: String,

    /// Database name
    #[arg(long)]
    db_name: String,
}



#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let database_url = format!(
        "postgres://{}:{}@{}:{}/{}",
        args.db_user, args.db_password, args.db_host, args.db_port, args.db_name
    );

    println!("Connecting to database at {}:{}...", args.db_host, args.db_port);

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await?;

    let mut imported = 0;
    let mut skipped = 0;

    if let Some(pfraw_path) = args.pfraw_file_ndjson {
        println!("Importing PFRaw ndjson file: {}", pfraw_path);
        let file = File::open(pfraw_path)?;
        let reader = BufReader::new(file);

        for (line_num, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() { continue; }
            if line.contains("\"file_count\"") { continue; }

            let record: NdJsonFileRecord = match serde_json::from_str(&line) {
                Ok(r) => r,
                Err(e) => {
                    println!("Skipping line {}: Parse error: {}", line_num + 1, e);
                    skipped += 1;
                    continue;
                }
            };

            let stage = match Stage::from_processing_level(&record.processing_level) {
                Some(s) => s,
                None => {
                    println!("Skipping line {}: Unknown processing level '{}'", line_num + 1, record.processing_level);
                    skipped += 1;
                    continue;
                }
            };

            let file_path = Path::new(&record.logical_name)
                .file_name().and_then(|n| n.to_str()).unwrap_or(&record.logical_name).to_string();

            if let Err(e) = insert_file(&pool, record.uuid, record.run.run_number, record.run.part_number, stage, &file_path, &record.checksum.sha512).await {
                println!("Error importing run {}: {}", record.run.run_number, e);
                skipped += 1;
            } else {
                imported += 1;
                if imported % 100 == 0 { println!("Imported {} files...", imported); }
            }
        }

    } else if let Some(step1_path) = args.step1_file_json {
        println!("Importing Step 1 JSON file: {}", step1_path);
        let content = std::fs::read_to_string(step1_path)?;
        let data: HashMap<String, Vec<Step1FileRecord>> = serde_json::from_str(&content)?;

        // Regex to extract run number and part number from filename
        // Matches: Run00133578_Subrun00000000_00000033.i3.zst
        // Assumes part number is the last number group before extension
        let re = Regex::new(r"Run(\d+)_Subrun\d+_(\d+)\.").unwrap();

        for (_key, records) in data {
            for record in records {
                let file_path = Path::new(&record.logical_name)
                    .file_name().and_then(|n| n.to_str()).unwrap_or(&record.logical_name).to_string();

                let (run_number, part_number) = if let Some(caps) = re.captures(&file_path) {
                    let r = caps[1].parse::<i32>().unwrap_or(0);
                    let p = caps[2].parse::<i32>().unwrap_or(0);
                    (r, p)
                } else {
                    println!("Skipping file '{}': Could not parse run/part number", file_path);
                    skipped += 1;
                    continue;
                };

                // Generate deterministic UUID from sha512 (using random namespace or URL)
                let uuid = Uuid::new_v5(&Uuid::NAMESPACE_URL, record.checksum.sha512.as_bytes());

                if let Err(e) = insert_file(&pool, uuid, run_number, part_number, Stage::Step1, &file_path, &record.checksum.sha512).await {
                     println!("Error importing run {}: {}", run_number, e);
                    skipped += 1;
                } else {
                    imported += 1;
                     if imported % 100 == 0 { println!("Imported {} files...", imported); }
                }
            }
        }
    }

    println!("\n=== Import Complete ===");
    println!("Imported: {}", imported);
    println!("Skipped:  {}", skipped);

    Ok(())
}


