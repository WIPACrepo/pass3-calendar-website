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
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use sqlx::postgres::{PgPool, PgPoolOptions};
use uuid::Uuid;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use once_cell::sync::Lazy;
use std::sync::Mutex;

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
        .route("/api/files/upload", post(upload_files))
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

#[derive(Deserialize)]
struct UploadFilesPayload {
    run_number: i32,
    stage: String,  // "raw", "step1", or "step2"
    files: Vec<FileEntry>,
}

#[derive(Deserialize)]
struct FileEntry {
    file_path: String,
    sha512: String,
}

// --- KEYCLOAK / OIDC TYPES ---

#[derive(Debug, Clone, Deserialize)]
struct KeycloakJwk {
    kid: String,
    kty: String,
    r#use: String,
    n: String,
    e: String,
}

#[derive(Debug, Clone, Deserialize)]
struct KeycloakJwks {
    keys: Vec<KeycloakJwk>,
}

#[derive(Debug, Deserialize, Serialize)]
struct TokenClaims {
    sub: String,
    preferred_username: Option<String>,
    scope: Option<String>,
    #[serde(default)]
    resource_access: std::collections::HashMap<String, ResourceAccess>,
    exp: i64,
    iat: i64,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct ResourceAccess {
    roles: Vec<String>,
}

// Global cache for JWKS
static JWKS_CACHE: Lazy<Mutex<Option<KeycloakJwks>>> = Lazy::new(|| Mutex::new(None));

async fn fetch_keycloak_jwks() -> Result<KeycloakJwks, Box<dyn std::error::Error>> {
    // Check cache first
    if let Ok(cache) = JWKS_CACHE.lock() {
        if let Some(jwks) = cache.as_ref() {
            return Ok(jwks.clone());
        }
    }

    // Fetch from Keycloak
    let keycloak_url = env::var("KEYCLOAK_URL")?;
    let realm = env::var("KEYCLOAK_REALM")?;
    let jwks_url = format!("{}/realms/{}/protocol/openid-connect/certs", keycloak_url, realm);

    let client = reqwest::Client::new();
    let response = client.get(&jwks_url).send().await?;
    let jwks: KeycloakJwks = response.json().await?;

    // Cache it
    if let Ok(mut cache) = JWKS_CACHE.lock() {
        *cache = Some(jwks.clone());
    }

    Ok(jwks)
}

fn find_key<'a>(kid: &str, jwks: &'a KeycloakJwks) -> Option<&'a KeycloakJwk> {
    jwks.keys.iter().find(|key| key.kid == kid)
}

async fn validate_oidc_token(token: &str) -> Result<TokenClaims, String> {
    // Decode header to get kid
    let header = decode_header(token)
        .map_err(|e| format!("Invalid token header: {}", e))?;

    let kid = header.kid
        .ok_or_else(|| "Token missing 'kid' in header".to_string())?;

    // Fetch JWKS
    let jwks = fetch_keycloak_jwks()
        .await
        .map_err(|e| format!("Failed to fetch JWKS: {}", e))?;

    // Find the key
    let jwk = find_key(&kid, &jwks)
        .ok_or_else(|| format!("Key '{}' not found in JWKS", kid))?;

    // Convert JWK to decoding key
    let decoding_key = DecodingKey::from_rsa_components(&jwk.n, &jwk.e)
        .map_err(|e| format!("Invalid RSA key: {}", e))?;

    // Decode and validate token
    let keycloak_url = env::var("KEYCLOAK_URL")
        .map_err(|e| format!("KEYCLOAK_URL not set: {}", e))?;
    let realm = env::var("KEYCLOAK_REALM")
        .map_err(|e| format!("KEYCLOAK_REALM not set: {}", e))?;
    let iss = format!("{}/realms/{}", keycloak_url, realm);

    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&[&iss]);

    let token_data = decode::<TokenClaims>(token, &decoding_key, &validation)
        .map_err(|e| format!("Token validation failed: {}", e))?;

    Ok(token_data.claims)
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