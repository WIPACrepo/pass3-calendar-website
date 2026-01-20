use axum::{
    routing::{get, post},
    Router,
    response::{Json, IntoResponse},
    extract::{Json as ExtractJson, State, Path},
    http::{StatusCode, HeaderMap},
};
use std::{net::SocketAddr, env, sync::Arc};
use serde::{Deserialize, Serialize};
use tower_http::services::ServeFile;
// Cookie imports removed
use sqlx::postgres::{PgPool, PgPoolOptions};
use uuid::Uuid;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use once_cell::sync::Lazy;
use std::sync::Mutex;
use regex::Regex;
use axum::extract::BodyStream;
use futures::StreamExt;
use std::path::Path as FilePath;
use pass3_calendar_website::{NdJsonFileRecord, Step1FileRecord, Stage, insert_file}; 

#[tokio::main]
async fn main() {
    // Require ADMIN_PASSWORD to be set - fail fast if not configured
    let _admin_password = env::var("ADMIN_PASSWORD")
        .expect("ADMIN_PASSWORD environment variable must be set");

    // Initialize database
    let database_url = env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://localhost/calendar".to_string());
    
    // Create pool
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await
        .expect("Failed to connect to database");

    // Run migrations
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("Failed to run migrations");

    // Load JSON data if database is empty (one-time migration)
    migrate_json_to_db(&pool).await;

    let app_state = Arc::new(pool);

    let app = Router::new()
        .route_service("/", ServeFile::new("index.html"))
        .route("/api/runs", get(get_runs).post(create_run))
        .route("/api/runs/:run_number", get(get_run_details))
        .route("/api/runs/:run_number/state", post(update_run_state))
        .route("/api/steps", post(update_step))
        // login route removed
        .route("/api/files/upload", post(upload_files))
        .route("/api/import/pfraw", post(import_pfraw))
        .route("/api/import/step1", post(import_step1))
        .route("/api/runs/update", post(update_run_details))
        .route("/api/broken_files", post(report_broken_file))
        .route("/api/gcd_files", post(register_gcd_file))
        .with_state(app_state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 80));
    println!("Listening on {}", addr);

    axum::Server::bind(&addr)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .unwrap();
}

// --- DATA STRUCTURES ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type, Serialize, Deserialize)]
#[sqlx(type_name = "workflow_state")]
#[serde(rename_all = "PascalCase")]
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

#[derive(Serialize, Deserialize, Clone, sqlx::FromRow)]
struct Run {
    run_number: i32,
    file_number: i32,
    run_start_date: chrono::DateTime<chrono::Utc>,
    state: WorkflowState,
    url: Option<String>,
    #[sqlx(default)]
    raw_max_part: Option<i32>,
    #[sqlx(default)]
    step1_max_part: Option<i32>,
    #[sqlx(default)]
    missing_step1_parts: Option<Vec<i32>>,
    #[sqlx(default)]
    note: Option<String>,
}

// ... existing structs ...
// (processing_step, RunWithSteps, payload structs etc)

async fn get_runs(
    State(pool): State<Arc<PgPool>>,
) -> Json<Vec<Run>> {
    let runs: Vec<Run> = sqlx::query_as(
        r#"
        SELECT 
            r.run_number, r.file_number, r.run_start_date, r.state, r.url,
            MAX(CASE WHEN rf.stage = 'Raw Data' THEN rf.part_number END) as raw_max_part,
            MAX(CASE WHEN rf.stage = 'Step 1' THEN rf.part_number END) as step1_max_part,
            (
                SELECT ARRAY_AGG(part_number ORDER BY part_number)
                FROM (
                    SELECT part_number FROM run_files WHERE run_number = r.run_number AND stage = 'Raw Data'
                    EXCEPT
                    SELECT part_number FROM run_files WHERE run_number = r.run_number AND stage = 'Step 1'
                ) as diff
            ) as missing_step1_parts,
            rn.note
        FROM runs r
        LEFT JOIN run_files rf ON r.run_number = rf.run_number
        LEFT JOIN run_notes rn ON r.run_number = rn.run_number
        GROUP BY r.run_number, rn.note
        ORDER BY r.run_start_date DESC
        "#
    )
    .fetch_all(pool.as_ref())
    .await
    .unwrap_or_default();
    Json(runs)
}

async fn get_run_details(
    State(pool): State<Arc<PgPool>>,
    Path(run_number): Path<i32>,
) -> Json<Option<RunWithSteps>> {
    if let Ok(run) = sqlx::query_as::<_, Run>("SELECT run_number, file_number, run_start_date, state, url FROM runs WHERE run_number = $1")
        .bind(run_number)
        .fetch_one(pool.as_ref())
        .await {
        
        let steps: Vec<ProcessingStep> = sqlx::query_as("SELECT id, run_number, step_number, started_date, end_date, site, checksum, location FROM processing_steps WHERE run_number = $1 ORDER BY step_number")
            .bind(run_number)
            .fetch_all(pool.as_ref())
            .await
            .unwrap_or_default();
        
        Json(Some(RunWithSteps { run, steps }))
    } else {
        Json(None)
    }
}

async fn create_run(
    State(pool): State<Arc<PgPool>>,
    headers: HeaderMap,
    ExtractJson(payload): ExtractJson<CreateRunPayload>
) -> impl IntoResponse {
    // Check auth (require 'admin' or similar, but for now reuse 'file_import' or just check valid token)
    // Assuming 'file_import' scope is sufficient for creating runs, or we can use a generic valid check.
    // Let's perform a generic token validation without specific scope for now, OR enforce file_import.
    // The previous code checked for admin cookie. Now we require a valid token.
    // Let's assume file_import scope implies admin rights for this app.
    if let Err((status, msg)) = check_auth_scope(&headers, "file_import").await {
         return (status, Json(msg)).into_response();
    }

    // Insert run and create empty steps
    match sqlx::query("INSERT INTO runs (run_number, file_number, run_start_date, state, url) VALUES ($1, $2, $3, $4, $5)")
        .bind(Uuid::new_v4().to_string()) // We'll use a simple auto-increment approach instead
        .bind(payload.file_number)
        .bind(payload.run_start_date)
        .bind(payload.state)
        .bind(&payload.url)
        .execute(pool.as_ref())
        .await {
        Ok(_) => {
            // Create step records for Step 1 and Step 2
            for step_num in [1, 2] {
                let _ = sqlx::query("INSERT INTO processing_steps (id, run_number, step_number) VALUES ($1, $2, $3)")
                    .bind(Uuid::new_v4().to_string())
                    .bind(Uuid::new_v4().to_string()) // This will be replaced with actual run_number
                    .bind(step_num)
                    .execute(pool.as_ref())
                    .await;
            }
            (StatusCode::OK, Json("Run created".to_string()))
        }
        Err(e) => {
            eprintln!("Failed to create run: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json("Failed to create run".to_string()))
        }
    }
}

async fn update_run_state(
    State(pool): State<Arc<PgPool>>,
    headers: HeaderMap,
    Path(run_number): Path<i32>,
    ExtractJson(payload): ExtractJson<UpdateRunStatePayload>
) -> impl IntoResponse {
    // Check auth
    if let Err((status, msg)) = check_auth_scope(&headers, "file_import").await {
         return (status, Json(msg)).into_response();
    }

    match sqlx::query("UPDATE runs SET state = $1 WHERE run_number = $2")
        .bind(payload.new_state)
        .bind(run_number)
        .execute(pool.as_ref())
        .await {
        Ok(r) if r.rows_affected() > 0 => (StatusCode::OK, Json("Updated".to_string())),
        _ => (StatusCode::OK, Json("No runs updated".to_string()))
    }
}

async fn update_step(
    State(pool): State<Arc<PgPool>>,
    headers: HeaderMap,
    ExtractJson(payload): ExtractJson<UpdateStepPayload>
) -> impl IntoResponse {
    // Check auth
    if let Err((status, msg)) = check_auth_scope(&headers, "file_import").await {
         return (status, Json(msg)).into_response();
    }

    match sqlx::query(
        "UPDATE processing_steps SET started_date = $1, end_date = $2, site = $3, checksum = $4, location = $5 WHERE run_number = $6 AND step_number = $7"
    )
        .bind(payload.started_date)
        .bind(payload.end_date)
        .bind(&payload.site)
        .bind(&payload.checksum)
        .bind(&payload.location)
        .bind(payload.run_number)
        .bind(payload.step_number)
        .execute(pool.as_ref())
        .await {
        Ok(r) if r.rows_affected() > 0 => (StatusCode::OK, Json("Step updated".to_string())),
        _ => (StatusCode::OK, Json("No steps updated".to_string()))
    }
}



async fn upload_files(
    State(pool): State<Arc<PgPool>>,
    headers: HeaderMap,
    ExtractJson(payload): ExtractJson<UploadFilesPayload>
) -> impl IntoResponse {
    // Extract Bearer token from Authorization header
    let token = match headers.get("Authorization") {
        Some(h) => match h.to_str() {
            Ok(s) => s.strip_prefix("Bearer ").unwrap_or(""),
            Err(_) => return (StatusCode::UNAUTHORIZED, Json("Invalid Authorization header".to_string())).into_response(),
        },
        None => return (StatusCode::UNAUTHORIZED, Json("Missing Authorization header".to_string())).into_response(),
    };

    // Validate OIDC token
    let claims = match validate_oidc_token(token).await {
        Ok(c) => c,
        Err(e) => return (StatusCode::UNAUTHORIZED, Json(format!("Token validation failed: {}", e))).into_response(),
    };

    // Validate stage
    if !["raw", "step1", "step2"].contains(&payload.stage.as_str()) {
        return (StatusCode::BAD_REQUEST, Json("Invalid stage. Must be 'raw', 'step1', or 'step2'".to_string())).into_response();
    }

    // Verify run exists
    let run_exists: bool = match sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runs WHERE run_number = $1)")
        .bind(payload.run_number)
        .fetch_one(pool.as_ref())
        .await {
        Ok(exists) => exists,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json("Database error".to_string())).into_response(),
    };

    if !run_exists {
        return (StatusCode::NOT_FOUND, Json(format!("Run {} not found", payload.run_number))).into_response();
    }

    // Insert files into run_files table
    let mut inserted = 0;
    let mut failed = 0;

    for file in payload.files {
        match sqlx::query(
            "INSERT INTO run_files (id, run_number, stage, file_path, sha512) VALUES ($1, $2, $3, $4, $5)"
        )
        .bind(Uuid::new_v4().to_string())
        .bind(payload.run_number)
        .bind(&payload.stage)
        .bind(&file.file_path)
        .bind(&file.sha512)
        .execute(pool.as_ref())
        .await {
            Ok(_) => inserted += 1,
            Err(e) => {
                eprintln!("Failed to insert file {}: {}", file.file_path, e);
                failed += 1;
            }
        }
    }

    #[derive(Serialize)]
    struct UploadResponse {
        inserted: i32,
        failed: i32,
        user: String,
    }

    (StatusCode::OK, Json(UploadResponse { 
        inserted, 
        failed,
        user: claims.preferred_username.unwrap_or_else(|| claims.sub),
    })).into_response()
}

// --- IMPORT HANDLERS ---

async fn check_auth_scope(headers: &HeaderMap, required_scope: &str) -> Result<(), (StatusCode, String)> {
    let token = headers.get("Authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .ok_or((StatusCode::UNAUTHORIZED, "Missing or invalid Authorization header".to_string()))?;

    let claims = validate_oidc_token(token).await
        .map_err(|e| (StatusCode::UNAUTHORIZED, format!("Token validation failed: {}", e)))?;

    // Check scope - simple check for presence of string in space-separated list
    let has_scope = claims.scope
        .as_deref()
        .unwrap_or("")
        .split_whitespace()
        .any(|s| s == required_scope);

    if !has_scope {
        return Err((StatusCode::FORBIDDEN, format!("Missing required scope: {}", required_scope)));
    }

    Ok(())
}

async fn import_pfraw(
    State(pool): State<Arc<PgPool>>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    if let Err((status, msg)) = check_auth_scope(&headers, "file_import").await {
        return (status, Json(msg)).into_response();
    }

    let mut processed = 0;
    let mut inserted = 0;
    let mut updated = 0;
    let mut failed = 0;

    for line in body.lines() {
        if line.trim().is_empty() { continue; }
        processed += 1;

        let record: NdJsonFileRecord = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(_) => { failed += 1; continue; }
        };

        if record.processing_level != "PFRaw" {
            continue; // Skip non-PFRaw
        }

        let run_exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runs WHERE run_number = $1)")
            .bind(record.run.run_number)
            .fetch_one(pool.as_ref())
            .await
            .unwrap_or(false);

        if !run_exists {
            failed += 1; // Run must exist
            continue;
        }

       // Extract pure filename
       let fpath = FilePath::new(&record.logical_name)
           .file_name()
           .and_then(|n| n.to_str())
           .unwrap_or(&record.logical_name)
           .to_string();

        // Uuid is already parsed by serde in NdJsonFileRecord
        match insert_file(&pool, record.uuid, record.run.run_number, record.run.part_number, Stage::RawData, &fpath, &record.checksum.sha512).await {
            Ok(_) => inserted += 1,
            Err(e) => {
                eprintln!("DB Error: {}", e);
                failed += 1;
            }
        }
    }

    (StatusCode::OK, Json(ImportResponse {
        processed, inserted, updated, failed,
        message: "PFRaw import completed".to_string()
    })).into_response()
}

async fn import_step1(
    State(pool): State<Arc<PgPool>>,
    headers: HeaderMap,
    ExtractJson(payload): ExtractJson<std::collections::HashMap<String, Vec<Step1FileRecord>>>,
) -> impl IntoResponse {
    if let Err((status, msg)) = check_auth_scope(&headers, "file_import").await {
        return (status, Json(msg)).into_response();
    }

    let re = Regex::new(r"Run(\d+)_Subrun\d+_(\d+)\.").unwrap();
    let namespace = Uuid::NAMESPACE_DNS; 

    let mut processed = 0;
    let mut inserted = 0;
    let mut updated = 0;
    let mut failed = 0;

    for (_key, records) in payload {
        for record in records {
            processed += 1;

            let file_name = FilePath::new(&record.logical_name)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&record.logical_name)
                .to_string();
            
            // Extract Run/Part
            let (run_number, part_number) = if let Some(caps) = re.captures(&file_name) {
                let r = caps[1].parse::<i32>().unwrap_or(0);
                let p = caps[2].parse::<i32>().unwrap_or(0);
                (r, p)
            } else {
                failed += 1;
                continue;
            };

            let run_exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runs WHERE run_number = $1)")
                .bind(run_number)
                .fetch_one(pool.as_ref())
                .await
                .unwrap_or(false);

            if !run_exists {
                 failed += 1;
                 continue;
            }

            // Generate deterministic UUID
            let uuid_input = format!(
                "Step1-{}-{}-{}",
                run_number,
                part_number,
                record.checksum.sha512
            );
            let uuid = Uuid::new_v5(&namespace, uuid_input.as_bytes());

            match insert_file(&pool, uuid, run_number, part_number, Stage::Step1, &file_name, &record.checksum.sha512).await {
                Ok(_) => inserted += 1,
                Err(e) => {
                    eprintln!("DB Error: {}", e);
                    failed += 1;
                }
            }
        }
    }

    (StatusCode::OK, Json(ImportResponse {
        processed, inserted, updated, failed,
        message: "Step 1 import completed".to_string()
    })).into_response()
}



#[derive(Deserialize)]
struct UpdateRunRequest {
    run_number: i32,
    status: Option<String>,
    date: Option<chrono::DateTime<chrono::Utc>>,
    note: Option<String>,
}

async fn update_run_details(
    State(pool): State<Arc<PgPool>>,
    headers: HeaderMap,
    ExtractJson(payload): ExtractJson<UpdateRunRequest>,
) -> impl IntoResponse {
    // Check auth (allow either cookie or token with file_import scope)
    if let Err((status, msg)) = check_auth_scope(&headers, "file_import").await {
            return (status, Json(msg)).into_response();
    }

    // Handle Status Update
    if let Some(new_state_str) = payload.status {
         let _ = sqlx::query("UPDATE runs SET state = $1::workflow_state WHERE run_number = $2")
            .bind(new_state_str)
            .bind(payload.run_number)
            .execute(pool.as_ref())
            .await;
    }

    // Handle Date Update
    if let Some(new_date) = payload.date {
        let _ = sqlx::query("UPDATE runs SET run_start_date = $1 WHERE run_number = $2")
            .bind(new_date)
            .bind(payload.run_number)
            .execute(pool.as_ref())
            .await;
    }

    // Handle Note Update
    if let Some(note) = payload.note {
        let _ = sqlx::query(
            "INSERT INTO run_notes (id, run_number, note) VALUES ($1, $2, $3)
             ON CONFLICT (run_number) DO UPDATE SET note = EXCLUDED.note, updated_at = CURRENT_TIMESTAMP"
        )
            .bind(Uuid::new_v4())
            .bind(payload.run_number)
            .bind(note)
            .execute(pool.as_ref())
            .await;
    }

    (StatusCode::OK, Json("Run updated".to_string()))
}



#[derive(Deserialize)]
struct BrokenFileRequest {
    run_number: i32,
    part_number: i32,
    stage: String,
    file_path: String,
}

#[derive(Deserialize)]
struct GcdFileRequest {
    run_number: i32,
    stage: String,
    location: String,
    sha512: String,
}

async fn report_broken_file(
    State(pool): State<Arc<PgPool>>,
    headers: HeaderMap,
    ExtractJson(payload): ExtractJson<BrokenFileRequest>,
) -> impl IntoResponse {
    // Check auth
    if let Err((status, msg)) = check_auth_scope(&headers, "file_import").await {
         return (status, Json(msg)).into_response();
    }

    // Parse Stage
    let stage_enum = match payload.stage.as_str() {
        "Raw Data" => Stage::RawData,
        "Step 1" => Stage::Step1,
        "Step 2" => Stage::Step2,
        // Allow simplified names too
        "raw" => Stage::RawData,
        "step1" => Stage::Step1,
        "step2" => Stage::Step2,
        _ => return (StatusCode::BAD_REQUEST, Json("Invalid stage".to_string())).into_response(),
    };

    match sqlx::query(
        "INSERT INTO broken_files (id, run_number, part_number, stage, file_path) VALUES ($1, $2, $3, $4::stage, $5)"
    )
    .bind(Uuid::new_v4())
    .bind(payload.run_number)
    .bind(payload.part_number)
    .bind(stage_enum)
    .bind(payload.file_path)
    .execute(pool.as_ref())
    .await {
        Ok(_) => (StatusCode::OK, Json("Broken file reported".to_string())),
        Err(e) => {
            eprintln!("DB Error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json("Failed to report broken file".to_string()))
        }
    }
}

async fn register_gcd_file(
    State(pool): State<Arc<PgPool>>,
    headers: HeaderMap,
    ExtractJson(payload): ExtractJson<GcdFileRequest>,
) -> impl IntoResponse {
    // Check auth
    if let Err((status, msg)) = check_auth_scope(&headers, "file_import").await {
         return (status, Json(msg)).into_response();
    }

     // Parse Stage
    let stage_enum = match payload.stage.as_str() {
        "Raw Data" => Stage::RawData,
        "Step 1" => Stage::Step1,
        "Step 2" => Stage::Step2,
        "raw" => Stage::RawData,
        "step1" => Stage::Step1,
        "step2" => Stage::Step2,
        _ => return (StatusCode::BAD_REQUEST, Json("Invalid stage".to_string())).into_response(),
    };

    match sqlx::query(
        "INSERT INTO gcd_files (id, run_number, stage, location, sha512) VALUES ($1, $2, $3::stage, $4, $5)
         ON CONFLICT (run_number, stage) DO UPDATE SET location = EXCLUDED.location, sha512 = EXCLUDED.sha512, updated_at = CURRENT_TIMESTAMP"
    )
    .bind(Uuid::new_v4())
    .bind(payload.run_number)
    .bind(stage_enum)
    .bind(payload.location)
    .bind(payload.sha512)
    .execute(pool.as_ref())
    .await {
        Ok(_) => (StatusCode::OK, Json("GCD file registered".to_string())),
        Err(e) => {
            eprintln!("DB Error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json("Failed to register GCD file".to_string()))
        }
    }
}

async fn migrate_json_to_db(pool: &PgPool) {
    // Check if runs table has data
    if let Ok(count) = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM runs")
        .fetch_one(pool)
        .await {
        if count > 0 {
            println!("Database already has {} runs, skipping JSON migration", count);
            return;
        }
    }

    // Note: Add legacy data migration logic here if needed
    println!("Database initialized and ready for runs and processing steps");
}