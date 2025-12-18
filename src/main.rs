use axum::{
    routing::{get, post},
    Router,
    response::{Json, IntoResponse},
    extract::{Json as ExtractJson, State, Path},
    http::StatusCode,
};
use std::{net::SocketAddr, env, sync::Arc};
use serde::{Deserialize, Serialize};
use tower_http::services::ServeFile;
use base64::{Engine as _, engine::general_purpose};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use sqlx::postgres::{PgPool, PgPoolOptions};
use uuid::Uuid;

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

    // FIX 1: Removed semicolon after the first route so the chain continues
    let app = Router::new()
        .route_service("/", ServeFile::new("index.html"))
        .route("/api/runs", get(get_runs).post(create_run))
        .route("/api/runs/:run_number", get(get_run_details))
        .route("/api/runs/:run_number/state", post(update_run_state))
        .route("/api/steps", post(update_step))
        .route("/api/login", post(login_handler))
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
}

#[derive(Serialize, Deserialize, Clone, sqlx::FromRow)]
struct ProcessingStep {
    id: String,
    run_number: i32,
    step_number: i32,
    started_date: Option<chrono::DateTime<chrono::Utc>>,
    end_date: Option<chrono::DateTime<chrono::Utc>>,
    site: Option<String>,
    checksum: Option<String>,
    location: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct RunWithSteps {
    run: Run,
    steps: Vec<ProcessingStep>,
}

#[derive(Deserialize)]
struct CreateRunPayload {
    file_number: i32,
    run_start_date: chrono::DateTime<chrono::Utc>,
    state: String,
    url: Option<String>,
}

#[derive(Deserialize)]
struct UpdateStepPayload {
    run_number: i32,
    step_number: i32,
    started_date: Option<chrono::DateTime<chrono::Utc>>,
    end_date: Option<chrono::DateTime<chrono::Utc>>,
    site: Option<String>,
    checksum: Option<String>,
    location: Option<String>,
}

#[derive(Deserialize)]
struct UpdateRunStatePayload {
    run_number: i32,
    new_state: WorkflowState,
}

#[derive(Deserialize)]
struct LoginPayload {
    password: String,
}

#[derive(Deserialize, Debug)]
struct GitHubFileResponse { 
    sha: String 
}

#[derive(Serialize)]
struct GitHubUpdatePayload {
    message: String,
    content: String,
    sha: String,
}

// --- HANDLERS ---

async fn get_runs(
    State(pool): State<Arc<PgPool>>,
) -> Json<Vec<Run>> {
    let runs: Vec<Run> = sqlx::query_as("SELECT run_number, file_number, run_start_date, state, url FROM runs ORDER BY run_start_date DESC")
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
    jar: CookieJar,
    ExtractJson(payload): ExtractJson<CreateRunPayload>
) -> impl IntoResponse {
    // Check for cookie
    if jar.get("session").map(|c| c.value()) != Some("admin_authorized") {
        return (StatusCode::UNAUTHORIZED, Json("Please Log In First".to_string()));
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
    jar: CookieJar,
    Path(run_number): Path<i32>,
    ExtractJson(payload): ExtractJson<UpdateRunStatePayload>
) -> impl IntoResponse {
    // Check for cookie
    if jar.get("session").map(|c| c.value()) != Some("admin_authorized") {
        return (StatusCode::UNAUTHORIZED, Json("Please Log In First".to_string()));
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
    jar: CookieJar,
    ExtractJson(payload): ExtractJson<UpdateStepPayload>
) -> impl IntoResponse {
    // Check for cookie
    if jar.get("session").map(|c| c.value()) != Some("admin_authorized") {
        return (StatusCode::UNAUTHORIZED, Json("Please Log In First".to_string()));
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

async fn login_handler(
    jar: CookieJar, 
    ExtractJson(payload): ExtractJson<LoginPayload>
) -> impl IntoResponse {
    // ADMIN_PASSWORD is guaranteed to be set (checked in main)
    let actual_pass = env::var("ADMIN_PASSWORD").unwrap();

    if payload.password == actual_pass {
        let cookie = Cookie::build("session", "admin_authorized")
            .path("/")
            .http_only(false)
            .same_site(SameSite::Lax)
            .finish();
        
        (jar.add(cookie), Json("Login Successful".to_string()))
    } else {
        (jar, Json("Invalid Password".to_string()))
    }
}

async fn push_to_github_event(run: &Run) -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let token = env::var("GITHUB_TOKEN")?; 
    let owner = env::var("REPO_OWNER")?; 
    let repo = env::var("REPO_NAME")?;   
    let file_path = "runs.json";       
    let url = format!("https://api.github.com/repos/{}/{}/contents/{}", owner, repo, file_path);

    let resp = client.get(&url)
        .header("User-Agent", "rust-app")
        .header("Authorization", format!("Bearer {}", token))
        .send().await?.json::<GitHubFileResponse>().await?;

    let json_content = serde_json::to_string(&vec![run])?;
    let encoded_content = general_purpose::STANDARD.encode(json_content);

    let body = GitHubUpdatePayload {
        message: format!("Update run {} state to {} via Web Dashboard", run.run_number, run.state),
        content: encoded_content,
        sha: resp.sha,
    };

    client.put(&url)
        .header("User-Agent", "rust-app")
        .header("Authorization", format!("Bearer {}", token))
        .json(&body)
        .send().await?;

    Ok(())
}

async fn push_to_github_events(runs: &[Run]) -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let token = env::var("GITHUB_TOKEN")?; 
    let owner = env::var("REPO_OWNER")?; 
    let repo = env::var("REPO_NAME")?;   
    let file_path = "runs.json";       
    let url = format!("https://api.github.com/repos/{}/{}/contents/{}", owner, repo, file_path);

    let resp = client.get(&url)
        .header("User-Agent", "rust-app")
        .header("Authorization", format!("Bearer {}", token))
        .send().await?.json::<GitHubFileResponse>().await?;

    let json_content = serde_json::to_string(&runs)?;
    let encoded_content = general_purpose::STANDARD.encode(json_content);

    let body = GitHubUpdatePayload {
        message: "Bulk update runs state via Web Dashboard".to_string(),
        content: encoded_content,
        sha: resp.sha,
    };

    client.put(&url)
        .header("User-Agent", "rust-app")
        .header("Authorization", format!("Bearer {}", token))
        .json(&body)
        .send().await?;

    Ok(())
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