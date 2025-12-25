use std::{net::SocketAddr, path::PathBuf, sync::Arc};

// StudioCommand engine (v0)
//
// This service intentionally stays small at first:
//   - Serve the browser UI (static files)
//   - Provide a few JSON endpoints for system status
//   - Run behind a reverse proxy (nginx) for HTTPS and internet exposure


use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{Html, IntoResponse},
    routing::get,
    Json, Router,
};
use serde::Serialize;
use sysinfo::System;
use tower_http::services::{ServeDir, ServeFile};
use tracing::{info, warn};

#[derive(Clone)]
struct AppState {
    version: String,
    web_root: PathBuf,
    sys: Arc<tokio::sync::Mutex<System>>,
}



/// Root endpoint: UI is served by nginx; the engine focuses on API/WebSocket.
async fn root() -> &'static str {
    "StudioCommand engine is running. UI is served by nginx."
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();

    let version = env!("CARGO_PKG_VERSION").to_string();

    let web_root = std::env::var("STUDIOCOMMAND_WEB_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("../web"));

    let sys = System::new_all();
let state = AppState {
        version,
        web_root: web_root.clone(),
        sys: Arc::new(tokio::sync::Mutex::new(sys)),
    };

    let app = build_router(state);

    // Bind loopback only; put Nginx/Caddy in front for LAN/Internet.
    let addr: SocketAddr = std::env::var("STUDIOCOMMAND_BIND")
        .unwrap_or_else(|_| "127.0.0.1:3000".to_string())
        .parse()?;

    info!("StudioCommand engine starting on http://{addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

fn build_router(state: AppState) -> Router {
    let index = state.web_root.join("index.html");
    // ServeDir serves static UI assets. not_found_service() provides SPA fallback so deep links work.
    let serve_dir = ServeDir::new(state.web_root.clone()).not_found_service(ServeFile::new(index));

    Router::new()
        .route("/health", get(|| async { "OK" }))
        .route("/api/v1/system/info", get(system_info))
        .route("/admin/api/v1/updates/status", get(update_status))
        // SPA entry points (same UI bundle in v0)
        .route("/", get(root))
        .route("/admin", get(spa_entry))
        .route("/remote", get(spa_entry))
        .fallback_service(serve_dir)
        .with_state(state)
}

async fn spa_entry() -> impl IntoResponse {
    // If web root is present, the fallback ServeDir will serve the real index.html.
    (StatusCode::OK, [(header::CONTENT_TYPE, "text/html")], Html(include_str!("../stub_index.html")))
}

#[derive(Serialize)]
struct SystemInfo {
    name: String,
    version: String,
    arch: String,
    cpu_model: String,
    cpu_cores: usize,
    load_1m: f32,
    load_5m: f32,
    load_15m: f32,
    temp_c: Option<f32>,
    hostname: Option<String>,
}

async fn system_info(State(st): State<AppState>) -> Json<SystemInfo> {
    let arch = std::env::consts::ARCH.to_string();
    let hostname = sysinfo::System::host_name();

    let mut sys = st.sys.lock().await;
    sys.refresh_all();

    let cpu_model = sys
        .cpus()
        .first()
        .map(|c| c.brand().to_string())
        .unwrap_or_else(|| "Unknown CPU".to_string());
    let cpu_cores = sys.cpus().len();

    let la = sysinfo::System::load_average();
    let temp_c = read_temp_c().ok().flatten();

    Json(SystemInfo {
        name: "StudioCommand Playout".to_string(),
        version: st.version.clone(),
        arch,
        cpu_model,
        cpu_cores,
        load_1m: la.one as f32,
        load_5m: la.five as f32,
        load_15m: la.fifteen as f32,
        temp_c,
        hostname,
    })
}

fn read_temp_c() -> anyhow::Result<Option<f32>> {
    let paths = [
        "/sys/class/thermal/thermal_zone0/temp",
        "/sys/class/hwmon/hwmon0/temp1_input",
    ];
    for p in paths {
        if let Ok(s) = std::fs::read_to_string(p) {
            if let Ok(v) = s.trim().parse::<f32>() {
                let c = if v > 1000.0 { v / 1000.0 } else { v };
                return Ok(Some(c));
            }
        }
    }
    Ok(None)
}

#[derive(Serialize)]
struct UpdateStatus {
    state: String,
    current: String,
    available: Option<String>,
    staged: Option<String>,
    last_result: Option<String>,
    progress: Option<u8>,
    arch: String,
}

async fn update_status(State(st): State<AppState>) -> Json<UpdateStatus> {
    Json(UpdateStatus {
        state: "idle".to_string(),
        current: st.version.clone(),
        available: None,
        staged: None,
        last_result: None,
        progress: None,
        arch: std::env::consts::ARCH.to_string(),
    })
}

async fn shutdown_signal() {
    let ctrl_c = async { tokio::signal::ctrl_c().await.ok(); };

    #[cfg(unix)]
    let term = async {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).expect("sigterm handler");
        sigterm.recv().await;
    };

    #[cfg(not(unix))]
    let term = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }

    warn!("Shutdown signal received.");
}
