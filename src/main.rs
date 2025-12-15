use axum::{
    routing::{get},
    Router,
    response::Json,
    extract::Json as ExtractJson,
};
use std::{fs, net::SocketAddr, env}; // Added env
use serde::{Deserialize, Serialize};
use tower_http::services::ServeFile;
use base64::{Engine as _, engine::general_purpose};

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route_service("/", ServeFile::new("index.html"))
        .route("/api/events", get(get_events).post(update_event)); // Add POST handler

    let addr = SocketAddr::from(([0, 0, 0, 0], 80));
    println!("Listening on {}", addr);

    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}

// Define the structure of your event to ensure type safety
#[derive(Serialize, Deserialize)]
struct Event {
    title: String,
    date: String,
    url: String,
    status: String,      // New field
    description: String, // New field
}

// NEW: Structure for GitHub API response
#[derive(Deserialize, Debug)]
struct GitHubFileResponse {
    sha: String,
}

// NEW: Structure for sending file to GitHub
#[derive(Serialize)]
struct GitHubUpdatePayload {
    message: String,
    content: String, // Base64 encoded content
    sha: String,
}

#[derive(Deserialize)]
struct UpdatePayload {
    title: String,   // We use Title to find the right event
    new_status: String,
}

// The handler to read the file and return JSON
async fn get_events() -> Json<Vec<Event>> {
    // In a real high-perf scenario, you might cache this data.
    // For GitOps, reading from disk allows the file to change without restarting.
    let data = fs::read_to_string("events.json").unwrap_or_else(|_| "[]".to_string());
    let events: Vec<Event> = serde_json::from_str(&data).unwrap_or(vec![]);
    Json(events)
}


async fn update_event(ExtractJson(payload): ExtractJson<UpdatePayload>) -> Json<String> {
    // 1. OPTIMISTIC UPDATE (Local Disk)
    // We update the local file so the UI works instantly while waiting for K8s deployment
    let path = "events.json";
    let data = fs::read_to_string(path).unwrap_or_else(|_| "[]".to_string());
    let mut events: Vec<Event> = serde_json::from_str(&data).unwrap_or(vec![]);

    // Find and update
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

        // 2. BACKGROUND SYNC (Push to GitHub)
        // We spawn a thread so the user doesn't have to wait for the API call
        tokio::spawn(async move {
            if let Err(e) = push_to_github(new_json).await {
                eprintln!("Failed to sync with GitHub: {}", e);
            }
        });
    }

    Json("Updating...".to_string())
}

// THE MAGIC FUNCTION
async fn push_to_github(json_content: String) -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    
    // Get config from Env Vars (Set these in K8s!)
    let token = env::var("GITHUB_TOKEN")?; 
    let owner = env::var("REPO_OWNER")?; // e.g. "yourname"
    let repo = env::var("REPO_NAME")?;   // e.g. "calendar-app"
    let file_path = "events.json";       // Path in repo
    
    let url = format!("https://api.github.com/repos/{}/{}/contents/{}", owner, repo, file_path);

    // Step A: Get the current file SHA (Required by GitHub API to prove we aren't overwriting blindly)
    let resp = client.get(&url)
        .header("User-Agent", "rust-app")
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await?
        .json::<GitHubFileResponse>()
        .await?;

    // Step B: Commit the change
    // GitHub API requires content to be Base64 encoded
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
        .send()
        .await?;

    println!("Successfully pushed changes to GitHub!");
    Ok(())
}