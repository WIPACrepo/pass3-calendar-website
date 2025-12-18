use sqlx::postgres::PgPoolOptions;
use serde::{Deserialize, Serialize};
use std::env;

#[derive(Debug, Serialize, Deserialize)]
struct EventRecord {
    title: String,
    date: String,
    status: String,
    url: String,
    description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type, Serialize, Deserialize)]
#[sqlx(type_name = "workflow_state")]
enum WorkflowState {
    #[serde(rename = "Not Yet Started")]
    NotYetStarted,
    #[serde(rename = "Transfer from Tape")]
    TransferFromTape,
    #[serde(rename = "Process Step 1")]
    ProcessStep1,
    #[serde(rename = "Finish Step 1")]
    FinishStep1,
    #[serde(rename = "Transfer WIPAC")]
    TransferWIPAC,
    #[serde(rename = "Process Step 2")]
    ProcessStep2,
    #[serde(rename = "Finish Step 2")]
    FinishStep2,
    Complete,
    #[serde(rename = "Step 1 Error")]
    Step1Error,
    #[serde(rename = "Step 2 Error")]
    Step2Error,
}

impl std::fmt::Display for WorkflowState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkflowState::NotYetStarted => write!(f, "Not Yet Started"),
            WorkflowState::TransferFromTape => write!(f, "Transfer from Tape"),
            WorkflowState::ProcessStep1 => write!(f, "Process Step 1"),
            WorkflowState::FinishStep1 => write!(f, "Finish Step 1"),
            WorkflowState::TransferWIPAC => write!(f, "Transfer WIPAC"),
            WorkflowState::ProcessStep2 => write!(f, "Process Step 2"),
            WorkflowState::FinishStep2 => write!(f, "Finish Step 2"),
            WorkflowState::Complete => write!(f, "Complete"),
            WorkflowState::Step1Error => write!(f, "Step 1 Error"),
            WorkflowState::Step2Error => write!(f, "Step 2 Error"),
        }
    }
}

fn parse_status_to_state(status: &str) -> WorkflowState {
    match status {
        "Not Yet Started" => WorkflowState::NotYetStarted,
        "Transfer from Tape" => WorkflowState::TransferFromTape,
        "Process Step 1" => WorkflowState::ProcessStep1,
        "Finish Step 1" => WorkflowState::FinishStep1,
        "Transfer WIPAC" => WorkflowState::TransferWIPAC,
        "Process Step 2" => WorkflowState::ProcessStep2,
        "Finish Step 2" => WorkflowState::FinishStep2,
        "Complete" => WorkflowState::Complete,
        "Step 1 Error" => WorkflowState::Step1Error,
        "Step 2 Error" => WorkflowState::Step2Error,
        _ => WorkflowState::NotYetStarted,
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Get database credentials from environment variables
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

    // Create connection pool
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await?;

    // Run migrations
    println!("Running migrations...");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await?;

    println!("Reading events.json...");
    let json_data = std::fs::read_to_string("events.json")?;
    let events: Vec<EventRecord> = serde_json::from_str(&json_data)?;

    println!("Found {} events to import", events.len());

    let mut imported = 0;
    let mut skipped = 0;

    for (idx, event) in events.iter().enumerate() {
        // Parse run_number from title (should be numeric)
        let run_number: i32 = match event.title.parse() {
            Ok(num) => num,
            Err(_) => {
                println!("  Skipping row {}: '{}' is not a valid run number", idx, event.title);
                skipped += 1;
                continue;
            }
        };

        // Parse date
        let run_start_date = match chrono::NaiveDate::parse_from_str(&event.date, "%Y-%m-%d") {
            Ok(date) => date.and_hms_opt(0, 0, 0).unwrap().and_utc(),
            Err(_) => {
                println!("  Skipping row {}: '{}' is not a valid date", idx, event.date);
                skipped += 1;
                continue;
            }
        };

        // Convert status to state
        let state = parse_status_to_state(&event.status);

        // Insert into runs table
        match sqlx::query(
            "INSERT INTO runs (run_number, file_number, run_start_date, state, url) VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (run_number) DO UPDATE SET state = EXCLUDED.state, url = EXCLUDED.url"
        )
        .bind(run_number)
        .bind(0) // file_number defaults to 0 since not in JSON
        .bind(run_start_date)
        .bind(state)
        .bind(&event.url)
        .execute(&pool)
        .await {
            Ok(_) => {
                // Create empty processing step records
                for step_num in [1, 2] {
                    let step_id = uuid::Uuid::new_v4();
                    let _ = sqlx::query(
                        "INSERT INTO processing_steps (id, run_number, step_number) 
                         VALUES ($1, $2, $3)
                         ON CONFLICT (run_number, step_number) DO NOTHING"
                    )
                    .bind(step_id.to_string())
                    .bind(run_number)
                    .bind(step_num)
                    .execute(&pool)
                    .await;
                }
                imported += 1;
                if imported % 100 == 0 {
                    println!("  Imported {} events...", imported);
                }
            }
            Err(e) => {
                println!("  Error importing run {}: {}", run_number, e);
                skipped += 1;
            }
        }
    }

    println!("\n=== Import Complete ===");
    println!("Imported: {}", imported);
    println!("Skipped:  {}", skipped);
    println!("Total:    {}", events.len());

    Ok(())
}
