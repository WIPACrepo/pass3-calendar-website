use axum::{
    extract::{Json as ExtractJson, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json},
    routing::{get, post},
    Router,
};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use once_cell::sync::Lazy;
use pass3_calendar_website::{insert_file, run_app_migrations, NdJsonFileRecord, Stage, Step1FileRecord};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sqlx::postgres::{PgPool, PgPoolOptions};
use std::{collections::HashMap, env, net::SocketAddr, path::Path as FilePath, sync::{Arc, Mutex}};
use tower_http::services::ServeFile;
use uuid::Uuid;

#[tokio::main]
async fn main() {
    let has_oidc = env::var("OIDC_ISSUER_URL").is_ok();

    assert!(has_oidc, "OIDC_ISSUER_URL must be set");

    let database_url = env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://localhost/calendar".to_string());

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await
        .expect("Failed to connect to database");

    run_app_migrations(&pool)
        .await
        .expect("Failed to run migrations");

    migrate_json_to_db(&pool).await;

    let app_state = Arc::new(pool);

    let app = Router::new()
        .route_service("/", ServeFile::new("index.html"))
        .route("/api/runs", get(get_runs).post(create_run))
        .route("/api/runs/:run_number", get(get_run_details))
        .route("/api/runs/:run_number/state", post(update_run_state))
        .route("/api/steps", post(update_step))
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type, Serialize, Deserialize)]
#[sqlx(type_name = "workflow_state")]
enum WorkflowState {
    #[sqlx(rename = "Not Yet Started")]
    #[serde(rename = "Not Yet Started")]
    NotYetStarted,
    #[sqlx(rename = "Step 1 GCD Generated")]
    #[serde(rename = "Step 1 GCD Generated")]
    Step1GcdGenerated,
    #[sqlx(rename = "Transfer from Tape")]
    #[serde(rename = "Transfer from Tape")]
    TransferFromTape,
    #[sqlx(rename = "Process Step 1")]
    #[serde(rename = "Process Step 1")]
    ProcessStep1,
    #[sqlx(rename = "Finish Step 1")]
    #[serde(rename = "Finish Step 1")]
    FinishStep1,
    #[sqlx(rename = "Transfer WIPAC")]
    #[serde(rename = "Transfer WIPAC")]
    TransferWipac,
    #[sqlx(rename = "Step 2 GCD Generated")]
    #[serde(rename = "Step 2 GCD Generated")]
    Step2GcdGenerated,
    #[sqlx(rename = "Process Step 2")]
    #[serde(rename = "Process Step 2")]
    ProcessStep2,
    #[sqlx(rename = "Finish Step 2")]
    #[serde(rename = "Finish Step 2")]
    FinishStep2,
    #[sqlx(rename = "Complete")]
    #[serde(rename = "Complete")]
    Complete,
    #[sqlx(rename = "Step 1 Error")]
    #[serde(rename = "Step 1 Error")]
    Step1Error,
    #[sqlx(rename = "Step 2 Error")]
    #[serde(rename = "Step 2 Error")]
    Step2Error,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
struct RunSummary {
    run_number: i32,
    run_start_date: chrono::DateTime<chrono::Utc>,
    run_end_date: chrono::DateTime<chrono::Utc>,
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

#[derive(Debug, Serialize, sqlx::FromRow)]
struct ProcessingStep {
    id: Uuid,
    run_number: i32,
    stage: String,
    started_date: Option<chrono::DateTime<chrono::Utc>>,
    end_date: Option<chrono::DateTime<chrono::Utc>>,
    site: Option<String>,
    location: Option<String>,
}

#[derive(Debug, Serialize)]
struct RunWithSteps {
    run: RunSummary,
    steps: Vec<ProcessingStep>,
}

#[derive(Debug, Deserialize)]
struct CreateRunPayload {
    run_number: i32,
    run_start_date: chrono::DateTime<chrono::Utc>,
    run_end_date: Option<chrono::DateTime<chrono::Utc>>,
    state: Option<String>,
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UpdateRunStatePayload {
    new_state: String,
}

#[derive(Debug, Deserialize)]
struct UpdateStepPayload {
    run_number: i32,
    stage: String,
    started_date: Option<chrono::DateTime<chrono::Utc>>,
    end_date: Option<chrono::DateTime<chrono::Utc>>,
    site: Option<String>,
    location: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UploadFilePayload {
    part_number: Option<i32>,
    file_path: String,
    sha512: String,
}

#[derive(Debug, Deserialize)]
struct UploadFilesPayload {
    run_number: i32,
    stage: String,
    files: Vec<UploadFilePayload>,
}

#[derive(Debug, Serialize)]
struct ImportResponse {
    processed: usize,
    inserted: usize,
    updated: usize,
    failed: usize,
    message: String,
}

#[derive(Debug, Deserialize)]
struct UpdateRunRequest {
    run_number: i32,
    status: Option<String>,
    date: Option<chrono::DateTime<chrono::Utc>>,
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BrokenFileRequest {
    run_number: i32,
    part_number: i32,
    stage: String,
    file_path: String,
}

#[derive(Debug, Deserialize)]
struct GcdFileRequest {
    run_number: i32,
    stage: String,
    location: String,
    sha512: String,
}

#[derive(Debug, Clone, Deserialize)]
struct OidcDiscoveryDocument {
    issuer: String,
    jwks_uri: String,
}

#[derive(Debug, Clone, Deserialize)]
struct JwkSet {
    keys: Vec<Jwk>,
}

#[derive(Debug, Clone, Deserialize)]
struct Jwk {
    kid: Option<String>,
    kty: String,
    alg: Option<String>,
    n: Option<String>,
    e: Option<String>,
}

#[derive(Debug, Clone)]
struct OidcCacheEntry {
    issuer_url: String,
    audience: Option<String>,
    issuer: String,
    jwks: JwkSet,
}

#[derive(Debug, Clone, Deserialize)]
struct OidcClaims {
    #[serde(rename = "sub")]
    _sub: String,
    #[serde(rename = "exp")]
    _exp: usize,
    #[serde(rename = "iss")]
    _iss: Option<String>,
    aud: Option<AudienceClaim>,
    scope: Option<String>,
    #[serde(rename = "preferred_username")]
    _preferred_username: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum AudienceClaim {
    One(String),
    Many(Vec<String>),
}

impl OidcClaims {
    fn has_scope(&self, required_scope: &str) -> bool {
        self.scope
            .as_deref()
            .unwrap_or("")
            .split_whitespace()
            .any(|scope| scope == required_scope)
    }

    fn has_audience(&self, expected_audience: &str) -> bool {
        match &self.aud {
            Some(AudienceClaim::One(audience)) => audience == expected_audience,
            Some(AudienceClaim::Many(audiences)) => audiences.iter().any(|audience| audience == expected_audience),
            None => false,
        }
    }
}

static OIDC_CACHE: Lazy<Mutex<Option<OidcCacheEntry>>> = Lazy::new(|| Mutex::new(None));

async fn get_runs(State(pool): State<Arc<PgPool>>) -> Json<Vec<RunSummary>> {
    let runs: Vec<RunSummary> = sqlx::query_as(
        r#"
        SELECT
            r.run_number,
            r.run_start_date,
            r.run_end_date,
            r.state,
            r.url,
            MAX(CASE WHEN rf.stage = 'Raw Data' THEN rf.part_number END) AS raw_max_part,
            MAX(CASE WHEN rf.stage = 'Step 1' THEN rf.part_number END) AS step1_max_part,
            (
                SELECT ARRAY_AGG(part_number ORDER BY part_number)
                FROM (
                    SELECT part_number FROM run_files WHERE run_number = r.run_number AND stage = 'Raw Data'
                    EXCEPT
                    SELECT part_number FROM run_files WHERE run_number = r.run_number AND stage = 'Step 1'
                ) AS diff
            ) AS missing_step1_parts,
            rn.note
        FROM runs r
        LEFT JOIN run_files rf ON r.run_number = rf.run_number
        LEFT JOIN run_notes rn ON r.run_number = rn.run_number
        GROUP BY r.run_number, rn.note
        ORDER BY r.run_start_date DESC
        "#,
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
    let run = sqlx::query_as::<_, RunSummary>(
        r#"
        SELECT
            r.run_number,
            r.run_start_date,
            r.run_end_date,
            r.state,
            r.url,
            MAX(CASE WHEN rf.stage = 'Raw Data' THEN rf.part_number END) AS raw_max_part,
            MAX(CASE WHEN rf.stage = 'Step 1' THEN rf.part_number END) AS step1_max_part,
            (
                SELECT ARRAY_AGG(part_number ORDER BY part_number)
                FROM (
                    SELECT part_number FROM run_files WHERE run_number = r.run_number AND stage = 'Raw Data'
                    EXCEPT
                    SELECT part_number FROM run_files WHERE run_number = r.run_number AND stage = 'Step 1'
                ) AS diff
            ) AS missing_step1_parts,
            rn.note
        FROM runs r
        LEFT JOIN run_files rf ON r.run_number = rf.run_number
        LEFT JOIN run_notes rn ON r.run_number = rn.run_number
        WHERE r.run_number = $1
        GROUP BY r.run_number, rn.note
        "#,
    )
    .bind(run_number)
    .fetch_optional(pool.as_ref())
    .await
    .ok()
    .flatten();

    let Some(run) = run else {
        return Json(None);
    };

    let steps: Vec<ProcessingStep> = sqlx::query_as(
        "SELECT id, run_number, stage::text AS stage, started_date, end_date, site, location
         FROM processing_steps
         WHERE run_number = $1
         ORDER BY stage"
    )
    .bind(run_number)
    .fetch_all(pool.as_ref())
    .await
    .unwrap_or_default();

    Json(Some(RunWithSteps { run, steps }))
}

async fn create_run(
    State(pool): State<Arc<PgPool>>,
    headers: HeaderMap,
    ExtractJson(payload): ExtractJson<CreateRunPayload>,
) -> impl IntoResponse {
    if let Err((status, msg)) = check_auth_scope(&headers, "file_import").await {
        return (status, Json(msg)).into_response();
    }

    let state = payload.state.unwrap_or_else(|| "Not Yet Started".to_string());
    let run_end_date = payload.run_end_date.unwrap_or(payload.run_start_date);
    let url = payload
        .url
        .unwrap_or_else(|| format!("https://live.icecube.wisc.edu/run/{}", payload.run_number));

    match sqlx::query(
        "INSERT INTO runs (run_number, run_start_date, run_end_date, state, url)
         VALUES ($1, $2, $3, $4::workflow_state, $5)
         ON CONFLICT (run_number) DO UPDATE
         SET run_start_date = EXCLUDED.run_start_date,
             run_end_date = EXCLUDED.run_end_date,
             state = EXCLUDED.state,
             url = EXCLUDED.url"
    )
    .bind(payload.run_number)
    .bind(payload.run_start_date)
    .bind(run_end_date)
    .bind(state)
    .bind(url)
    .execute(pool.as_ref())
    .await
    {
        Ok(_) => (StatusCode::OK, Json("Run created".to_string())).into_response(),
        Err(error) => {
            eprintln!("Failed to create run: {}", error);
            (StatusCode::INTERNAL_SERVER_ERROR, Json("Failed to create run".to_string())).into_response()
        }
    }
}

async fn update_run_state(
    State(pool): State<Arc<PgPool>>,
    headers: HeaderMap,
    Path(run_number): Path<i32>,
    ExtractJson(payload): ExtractJson<UpdateRunStatePayload>,
) -> impl IntoResponse {
    if let Err((status, msg)) = check_auth_scope(&headers, "file_import").await {
        return (status, Json(msg)).into_response();
    }

    match sqlx::query("UPDATE runs SET state = $1::workflow_state WHERE run_number = $2")
        .bind(payload.new_state)
        .bind(run_number)
        .execute(pool.as_ref())
        .await
    {
        Ok(result) if result.rows_affected() > 0 => {
            (StatusCode::OK, Json("Updated".to_string())).into_response()
        }
        Ok(_) => (StatusCode::NOT_FOUND, Json("Run not found".to_string())).into_response(),
        Err(error) => {
            eprintln!("Failed to update run state: {}", error);
            (StatusCode::INTERNAL_SERVER_ERROR, Json("Failed to update run state".to_string())).into_response()
        }
    }
}

async fn update_step(
    State(pool): State<Arc<PgPool>>,
    headers: HeaderMap,
    ExtractJson(payload): ExtractJson<UpdateStepPayload>,
) -> impl IntoResponse {
    if let Err((status, msg)) = check_auth_scope(&headers, "file_import").await {
        return (status, Json(msg)).into_response();
    }

    let stage = match parse_stage(&payload.stage) {
        Ok(stage) => stage,
        Err(response) => return response,
    };

    match sqlx::query(
        "INSERT INTO processing_steps (id, run_number, stage, started_date, end_date, site, location)
         VALUES ($1, $2, $3::stage, $4, $5, $6, $7)
         ON CONFLICT (run_number, stage) DO UPDATE
         SET started_date = EXCLUDED.started_date,
             end_date = EXCLUDED.end_date,
             site = EXCLUDED.site,
             location = EXCLUDED.location,
             updated_at = CURRENT_TIMESTAMP"
    )
    .bind(Uuid::new_v4())
    .bind(payload.run_number)
    .bind(stage)
    .bind(payload.started_date)
    .bind(payload.end_date)
    .bind(payload.site)
    .bind(payload.location)
    .execute(pool.as_ref())
    .await
    {
        Ok(_) => (StatusCode::OK, Json("Step updated".to_string())).into_response(),
        Err(error) => {
            eprintln!("Failed to update step: {}", error);
            (StatusCode::INTERNAL_SERVER_ERROR, Json("Failed to update step".to_string())).into_response()
        }
    }
}

async fn upload_files(
    State(pool): State<Arc<PgPool>>,
    headers: HeaderMap,
    ExtractJson(payload): ExtractJson<UploadFilesPayload>,
) -> impl IntoResponse {
    if let Err((status, msg)) = check_auth_scope(&headers, "file_import").await {
        return (status, Json(msg)).into_response();
    }

    let stage = match parse_stage(&payload.stage) {
        Ok(stage) => stage,
        Err(response) => return response,
    };

    let part_re = Regex::new(r"_(\d+)\.").unwrap();
    let mut processed = 0;
    let mut inserted = 0;
    let updated = 0;
    let mut failed = 0;

    for file in payload.files {
        processed += 1;
        let file_name = FilePath::new(&file.file_path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&file.file_path)
            .to_string();

        let part_number = file.part_number.or_else(|| {
            part_re
                .captures(&file_name)
                .and_then(|captures| captures.get(1))
                .and_then(|value| value.as_str().parse::<i32>().ok())
        });

        let Some(part_number) = part_number else {
            failed += 1;
            continue;
        };

        let uuid = Uuid::new_v5(&Uuid::NAMESPACE_URL, file.sha512.as_bytes());
        match insert_file(&pool, uuid, payload.run_number, part_number, stage, &file_name, &file.sha512).await {
            Ok(_) => inserted += 1,
            Err(error) => {
                eprintln!("Failed to upload file {}: {}", file_name, error);
                failed += 1;
            }
        }
    }

    (StatusCode::OK, Json(ImportResponse {
        processed,
        inserted,
        updated,
        failed,
        message: "File upload completed".to_string(),
    })).into_response()
}

async fn check_auth_scope(headers: &HeaderMap, required_scope: &str) -> Result<(), (StatusCode, String)> {
    let token = extract_bearer_token(headers)?;

    let claims = validate_oidc_token(token)
        .await
        .map_err(|error| (StatusCode::UNAUTHORIZED, format!("Token validation failed: {}", error)))?;

    if !required_scope.is_empty() && !claims.has_scope(required_scope) {
        return Err((
            StatusCode::FORBIDDEN,
            format!("Missing required scope: {}", required_scope),
        ));
    }

    Ok(())
}

fn extract_bearer_token(headers: &HeaderMap) -> Result<&str, (StatusCode, String)> {
    headers
        .get("Authorization")
        .and_then(|header| header.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or((StatusCode::UNAUTHORIZED, "Missing or invalid Authorization header".to_string()))
}

async fn validate_oidc_token(token: &str) -> Result<OidcClaims, String> {
    let header = decode_header(token).map_err(|error| error.to_string())?;
    let kid = header.kid.clone();
    let algorithm = header.alg;

    if !matches!(algorithm, Algorithm::RS256 | Algorithm::RS384 | Algorithm::RS512) {
        return Err(format!("Unsupported JWT algorithm: {:?}", algorithm));
    }

    let cache = get_oidc_cache(false).await?;
    let key = match find_decoding_key(&cache.jwks, kid.as_deref()) {
        Some(key) => key,
        None => {
            let refreshed = get_oidc_cache(true).await?;
            find_decoding_key(&refreshed.jwks, kid.as_deref())
                .ok_or_else(|| format!("No matching JWKS key found for kid {:?}", kid))?
        }
    };

    let mut validation = Validation::new(algorithm);
    validation.validate_aud = false;
    validation.set_issuer(&[cache.issuer.clone()]);

    let token_data = decode::<OidcClaims>(token, &key, &validation)
        .map_err(|error| error.to_string())?;

    if let Some(expected_audience) = cache.audience.as_deref() {
        if !token_data.claims.has_audience(expected_audience) {
            return Err(format!("Missing expected audience: {}", expected_audience));
        }
    }

    Ok(token_data.claims)
}

async fn get_oidc_cache(force_refresh: bool) -> Result<OidcCacheEntry, String> {
    let issuer_url = env::var("OIDC_ISSUER_URL")
        .map_err(|_| "OIDC_ISSUER_URL is not configured".to_string())?;
    let audience = env::var("OIDC_AUDIENCE").ok();

    if !force_refresh {
        if let Some(cached) = OIDC_CACHE.lock().map_err(|_| "OIDC cache poisoned".to_string())?.clone() {
            if cached.issuer_url == issuer_url && cached.audience == audience {
                return Ok(cached);
            }
        }
    }

    let discovery_url = format!(
        "{}/.well-known/openid-configuration",
        issuer_url.trim_end_matches('/')
    );
    let discovery = reqwest::get(&discovery_url)
        .await
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.to_string())?
        .json::<OidcDiscoveryDocument>()
        .await
        .map_err(|error| error.to_string())?;

    let jwks_url = env::var("OIDC_JWKS_URL").unwrap_or(discovery.jwks_uri.clone());
    let jwks = reqwest::get(&jwks_url)
        .await
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.to_string())?
        .json::<JwkSet>()
        .await
        .map_err(|error| error.to_string())?;

    let cache = OidcCacheEntry {
        issuer_url,
        audience,
        issuer: discovery.issuer,
        jwks,
    };

    *OIDC_CACHE.lock().map_err(|_| "OIDC cache poisoned".to_string())? = Some(cache.clone());
    Ok(cache)
}

fn find_decoding_key(jwks: &JwkSet, kid: Option<&str>) -> Option<DecodingKey> {
    let jwk = if let Some(kid) = kid {
        jwks.keys.iter().find(|jwk| jwk.kid.as_deref() == Some(kid))?
    } else {
        jwks.keys.iter().find(|jwk| jwk.kty == "RSA")?
    };

    if jwk.kty != "RSA" {
        return None;
    }

    if let Some(algorithm) = &jwk.alg {
        if !matches!(algorithm.as_str(), "RS256" | "RS384" | "RS512") {
            return None;
        }
    }

    let n = jwk.n.as_deref()?;
    let e = jwk.e.as_deref()?;
    DecodingKey::from_rsa_components(n, e).ok()
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
    let updated = 0;
    let mut failed = 0;

    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        processed += 1;

        let record: NdJsonFileRecord = match serde_json::from_str(line) {
            Ok(record) => record,
            Err(_) => {
                failed += 1;
                continue;
            }
        };

        if record.processing_level != "PFRaw" {
            continue;
        }

        let file_name = FilePath::new(&record.logical_name)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&record.logical_name)
            .to_string();

        match insert_file(
            &pool,
            record.uuid,
            record.run.run_number,
            record.run.part_number,
            Stage::RawData,
            &file_name,
            &record.checksum.sha512,
        )
        .await
        {
            Ok(_) => inserted += 1,
            Err(error) => {
                eprintln!("PFRaw import error: {}", error);
                failed += 1;
            }
        }
    }

    (StatusCode::OK, Json(ImportResponse {
        processed,
        inserted,
        updated,
        failed,
        message: "PFRaw import completed".to_string(),
    })).into_response()
}

async fn import_step1(
    State(pool): State<Arc<PgPool>>,
    headers: HeaderMap,
    ExtractJson(payload): ExtractJson<HashMap<String, Vec<Step1FileRecord>>>,
) -> impl IntoResponse {
    if let Err((status, msg)) = check_auth_scope(&headers, "file_import").await {
        return (status, Json(msg)).into_response();
    }

    let re = Regex::new(r"Run(\d+)_Subrun\d+_(\d+)\.").unwrap();

    let mut processed = 0;
    let mut inserted = 0;
    let updated = 0;
    let mut failed = 0;

    for (_key, records) in payload {
        for record in records {
            processed += 1;

            let file_name = FilePath::new(&record.logical_name)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(&record.logical_name)
                .to_string();

            let Some(captures) = re.captures(&file_name) else {
                failed += 1;
                continue;
            };

            let run_number = captures[1].parse::<i32>().unwrap_or(0);
            let part_number = captures[2].parse::<i32>().unwrap_or(0);
            let uuid = Uuid::new_v5(&Uuid::NAMESPACE_URL, record.checksum.sha512.as_bytes());

            match insert_file(
                &pool,
                uuid,
                run_number,
                part_number,
                Stage::Step1,
                &file_name,
                &record.checksum.sha512,
            )
            .await
            {
                Ok(_) => inserted += 1,
                Err(error) => {
                    eprintln!("Step1 import error: {}", error);
                    failed += 1;
                }
            }
        }
    }

    (StatusCode::OK, Json(ImportResponse {
        processed,
        inserted,
        updated,
        failed,
        message: "Step 1 import completed".to_string(),
    })).into_response()
}

async fn update_run_details(
    State(pool): State<Arc<PgPool>>,
    headers: HeaderMap,
    ExtractJson(payload): ExtractJson<UpdateRunRequest>,
) -> impl IntoResponse {
    if let Err((status, msg)) = check_auth_scope(&headers, "file_import").await {
        return (status, Json(msg)).into_response();
    }

    if let Some(new_state) = payload.status {
        let _ = sqlx::query("UPDATE runs SET state = $1::workflow_state WHERE run_number = $2")
            .bind(new_state)
            .bind(payload.run_number)
            .execute(pool.as_ref())
            .await;
    }

    if let Some(new_date) = payload.date {
        let _ = sqlx::query("UPDATE runs SET run_start_date = $1 WHERE run_number = $2")
            .bind(new_date)
            .bind(payload.run_number)
            .execute(pool.as_ref())
            .await;
    }

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

    (StatusCode::OK, Json("Run updated".to_string())).into_response()
}

async fn report_broken_file(
    State(pool): State<Arc<PgPool>>,
    headers: HeaderMap,
    ExtractJson(payload): ExtractJson<BrokenFileRequest>,
) -> impl IntoResponse {
    if let Err((status, msg)) = check_auth_scope(&headers, "file_import").await {
        return (status, Json(msg)).into_response();
    }

    let stage = match parse_stage(&payload.stage) {
        Ok(stage) => stage,
        Err(response) => return response,
    };

    match sqlx::query(
        "INSERT INTO broken_files (id, run_number, part_number, stage, file_path)
         VALUES ($1, $2, $3, $4::stage, $5)"
    )
    .bind(Uuid::new_v4())
    .bind(payload.run_number)
    .bind(payload.part_number)
    .bind(stage)
    .bind(payload.file_path)
    .execute(pool.as_ref())
    .await
    {
        Ok(_) => (StatusCode::OK, Json("Broken file reported".to_string())).into_response(),
        Err(error) => {
            eprintln!("Broken file insert failed: {}", error);
            (StatusCode::INTERNAL_SERVER_ERROR, Json("Failed to report broken file".to_string())).into_response()
        }
    }
}

async fn register_gcd_file(
    State(pool): State<Arc<PgPool>>,
    headers: HeaderMap,
    ExtractJson(payload): ExtractJson<GcdFileRequest>,
) -> impl IntoResponse {
    if let Err((status, msg)) = check_auth_scope(&headers, "file_import").await {
        return (status, Json(msg)).into_response();
    }

    let stage = match parse_stage(&payload.stage) {
        Ok(stage) => stage,
        Err(response) => return response,
    };

    match sqlx::query(
        "INSERT INTO gcd_files (id, run_number, stage, location, sha512)
         VALUES ($1, $2, $3::stage, $4, $5)
         ON CONFLICT (run_number, stage) DO UPDATE
         SET location = EXCLUDED.location,
             sha512 = EXCLUDED.sha512,
             updated_at = CURRENT_TIMESTAMP"
    )
    .bind(Uuid::new_v4())
    .bind(payload.run_number)
    .bind(stage)
    .bind(payload.location)
    .bind(payload.sha512)
    .execute(pool.as_ref())
    .await
    {
        Ok(_) => (StatusCode::OK, Json("GCD file registered".to_string())).into_response(),
        Err(error) => {
            eprintln!("GCD file registration failed: {}", error);
            (StatusCode::INTERNAL_SERVER_ERROR, Json("Failed to register GCD file".to_string())).into_response()
        }
    }
}

fn parse_stage(stage: &str) -> Result<Stage, axum::response::Response> {
    match stage {
        "Raw Data" | "raw" => Ok(Stage::RawData),
        "Step 1" | "step1" => Ok(Stage::Step1),
        "Step 2" | "step2" => Ok(Stage::Step2),
        _ => Err((StatusCode::BAD_REQUEST, Json("Invalid stage".to_string())).into_response()),
    }
}

async fn migrate_json_to_db(pool: &PgPool) {
    if let Ok(count) = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM runs")
        .fetch_one(pool)
        .await
    {
        if count > 0 {
            println!("Database already has {} runs, skipping JSON migration", count);
            return;
        }
    }

    println!("Database initialized and ready for runs and processing steps");
}