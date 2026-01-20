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
        .args(["pfraw_file_ndjson", "step1_file_json", "gcd_dir"]),
))]
struct Args {
    /// Path to the PFRaw ndjson file to import
    #[arg(long)]
    pfraw_file_ndjson: Option<String>,

    /// Path to the Step 1 JSON file to import
    #[arg(long)]
    step1_file_json: Option<String>,

    /// Path to directory containing GCD files
    #[arg(long)]
    gcd_dir: Option<String>,

    /// Stage for GCD files: "step1" or "step2"
    #[arg(long, required_if_present = "gcd_dir")]
    gcd_stage: Option<String>,

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
    } else if let Some(gcd_dir_path) = args.gcd_dir {
        println!("Importing GCD files from directory: {}", gcd_dir_path);
        
        let stage = match args.gcd_stage.as_deref() {
            Some("step1") => Stage::Step1,
            Some("step2") => Stage::Step2,
            Some("raw") => Stage::RawData,
            _ => {
                eprintln!("Invalid stage '{}'. Must be 'step1', 'step2', or 'raw'", args.gcd_stage.unwrap_or_default());
                return Ok(());
            }
        };

        // Regex to extract run number from GCD filename
        // Pattern: OnlinePass3_IC86.2019_data_Run00133574_78_503_GCD.i3.zst
        let re = Regex::new(r"Run(\d+)_").unwrap();

        let dir = std::fs::read_dir(&gcd_dir_path)?;
        
        for entry in dir {
            let entry = entry?;
            let path = entry.path();
            
            // Skip if not a file or doesn't have GCD in name
            if !path.is_file() {
                continue;
            }
            
            let filename = path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            
            if !filename.contains("GCD") || !filename.ends_with(".i3.zst") {
                continue;
            }

            // Extract run number
            let run_number = if let Some(caps) = re.captures(filename) {
                caps[1].parse::<i32>().unwrap_or(0)
            } else {
                println!("Skipping file '{}': Could not parse run number", filename);
                skipped += 1;
                continue;
            };

            // Get absolute path
            let absolute_path = path.canonicalize()
                .unwrap_or(path.clone())
                .to_string_lossy()
                .to_string();

            // Compute SHA512
            let sha512 = match compute_sha512(&path) {
                Ok(hash) => hash,
                Err(e) => {
                    println!("Error computing SHA512 for '{}': {}", filename, e);
                    skipped += 1;
                    continue;
                }
            };

            // Insert GCD file
            if let Err(e) = insert_gcd_file(&pool, run_number, stage, &absolute_path, &sha512).await {
                println!("Error importing GCD file for run {}: {}", run_number, e);
                skipped += 1;
            } else {
                imported += 1;
                if imported % 10 == 0 { 
                    println!("Imported {} GCD files...", imported); 
                }
            }
        }
    }

    println!("\n=== Import Complete ===");
    println!("Imported: {}", imported);
    println!("Skipped:  {}", skipped);

    Ok(())
}

/// Compute SHA512 hash of a file
fn compute_sha512(path: &Path) -> Result<String, std::io::Error> {
    use sha2::{Sha512, Digest};
    
    let mut file = File::open(path)?;
    let mut hasher = Sha512::new();
    std::io::copy(&mut file, &mut hasher)?;
    let hash = hasher.finalize();
    Ok(format!("{:x}", hash))
}

/// Insert a GCD file into the database
async fn insert_gcd_file(
    pool: &sqlx::PgPool,
    run_number: i32,
    stage: Stage,
    location: &str,
    sha512: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO gcd_files (id, run_number, stage, location, sha512) VALUES ($1, $2, $3::stage, $4, $5)
         ON CONFLICT (run_number, stage) DO UPDATE SET location = EXCLUDED.location, sha512 = EXCLUDED.sha512, updated_at = CURRENT_TIMESTAMP"
    )
    .bind(Uuid::new_v4())
    .bind(run_number)
    .bind(stage)
    .bind(location)
    .bind(sha512)
    .execute(pool)
    .await?;
    Ok(())
}
