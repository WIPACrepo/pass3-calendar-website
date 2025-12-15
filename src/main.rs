use axum::{
    routing::{get, post},
    Router,
    response::{Json, IntoResponse},
    extract::Json as ExtractJson,
    http::StatusCode,
};
use std::{fs, net::SocketAddr, env};
use serde::{Deserialize, Serialize};
use tower_http::services::ServeFile;
use base64::{Engine as _, engine::general_purpose};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};

#[tokio::main]
async fn main() {
    // FIX 1: Removed semicolon after the first route so the chain continues
    let app = Router::new()
        .route_service("/", ServeFile::new("index.html"))
        .route("/api/events", get(get_events).post(update_event)) 
        .route("/api/login", post(login_handler)); 

    let addr = SocketAddr::from(([0, 0, 0, 0], 80));
    println!("Listening on {}", addr);

    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}

// --- DATA STRUCTURES ---

#[derive(Serialize, Deserialize, Clone)]
struct Event {
    title: String,
    date: String,
    url: String,
    status: String,
    description: String,
}

#[derive(Deserialize)]
struct UpdatePayload {
    title: String,
    new_status: String,
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

async fn get_events() -> Json<Vec<Event>> {
    let data = fs::read_to_string("events.json").unwrap_or_else(|_| "[]".to_string());
    let events: Vec<Event> = serde_json::from_str(&data).unwrap_or(vec![]);
    Json(events)
}

async fn login_handler(
    jar: CookieJar, 
    ExtractJson(payload): ExtractJson<LoginPayload>
) -> impl IntoResponse {
    if payload.password == "secret123" {
        // FIX 2: Passed arguments separately: "session", "admin_authorized"
        // FIX 3: Used .finish() instead of .build()
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

async fn update_event(
    jar: CookieJar, 
    ExtractJson(payload): ExtractJson<UpdatePayload>
) -> impl IntoResponse {
    
    // Check for cookie
    if jar.get("session").map(|c| c.value()) != Some("admin_authorized") {
        return (StatusCode::UNAUTHORIZED, Json("Please Log In First".to_string()));
    }

    let path = "events.json";
    let data = fs::read_to_string(path).unwrap_or_else(|_| "[]".to_string());
    let mut events: Vec<Event> = serde_json::from_str(&data).unwrap_or(vec![]);

    let mut updated = false;
    for event in &mut events {
        if event.title == payload.title {
            event.status = payload.new_status.clone();
            updated = true;
        }
    }

    if updated {
        let new_json = serde_json::to_string_pretty(&events).unwrap();
        fs::write(path, &new_json).expect("Failed to write local file");

        tokio::spawn(async move {
            if let Err(e) = push_to_github(new_json).await {
                eprintln!("Failed to sync with GitHub: {}", e);
            }
        });
    }

    (StatusCode::OK, Json("Updated".to_string()))
}

async fn push_to_github(json_content: String) -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let token = env::var("GITHUB_TOKEN")?; 
    let owner = env::var("REPO_OWNER")?; 
    let repo = env::var("REPO_NAME")?;   
    let file_path = "events.json";       
    let url = format!("https://api.github.com/repos/{}/{}/contents/{}", owner, repo, file_path);

    let resp = client.get(&url)
        .header("User-Agent", "rust-app")
        .header("Authorization", format!("Bearer {}", token))
        .send().await?.json::<GitHubFileResponse>().await?;

    let encoded_content = general_purpose::STANDARD.encode(json_content);

    let body = GitHubUpdatePayload {
        message: "Update status via Web Dashboard".to_string(),
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