use std::{net::SocketAddr, sync::Arc};

// StudioCommand engine (v0)
//
// This service intentionally stays small at first:
//   - Serve the browser UI (static files)
//   - Provide a few JSON endpoints for system status
//   - Run behind a reverse proxy (nginx) for HTTPS and internet exposure


use axum::{
    extract::State,
            routing::get,
    Json, Router,
};
use serde::Serialize;
use sysinfo::System;
use tracing::{info, warn};

#[derive(Clone)]
struct AppState {
    version: String,
    sys: Arc<tokio::sync::Mutex<System>>,
    playout: Arc<tokio::sync::RwLock<PlayoutState>>,
}

#[derive(Clone, Serialize)]
struct LogItem {
    tag: String,
    time: String,
    title: String,
    artist: String,
    state: String, // "playing" | "next" | "queued"
    dur: String,   // "3:45"
    cart: String,
}

#[derive(Clone, Serialize)]
struct NowPlaying {
    title: String,
    artist: String,
    dur: u32, // seconds
    pos: u32, // seconds
}

#[derive(Clone, Serialize)]
struct ProducerStatus {
    name: String,
    role: String,
    connected: bool,
    onAir: bool,
    camOn: bool,
    jitter: String,
    loss: String,
    level: f32,
}

#[derive(Clone)]
struct PlayoutState {
    now: NowPlaying,
    log: Vec<LogItem>,
    producers: Vec<ProducerStatus>,
}

#[derive(Serialize)]
struct StatusResponse {
    version: String,
    now: NowPlaying,
    log: Vec<LogItem>,
    producers: Vec<ProducerStatus>,
    system: SystemInfo,
}



/// Root endpoint: UI is served by nginx; the engine focuses on API/WebSocket.
async fn root() -> &'static str {
    "StudioCommand engine is running. UI is served by nginx. Try /api/v1/status"
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();

    let version = env!("CARGO_PKG_VERSION").to_string();

    let sys = System::new_all();

// Demo playout state (v0): the UI now pulls this via /api/v1/status.
// In later versions this becomes the real automation engine state.
let playout = PlayoutState {
    now: NowPlaying { title: "Neutron Dance".into(), artist: "Pointer Sisters".into(), dur: 242, pos: 0 },
    log: demo_log(),
    producers: demo_producers(),
};

let state = AppState {
    version: version.clone(),
    sys: Arc::new(tokio::sync::Mutex::new(sys)),
    playout: Arc::new(tokio::sync::RwLock::new(playout)),
};

// Background tick: advances the demo queue once per second.
tokio::spawn(playout_tick(state.playout.clone()));


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
    Router::new()
        .route("/", get(root))
        .route("/health", get(|| async { "OK" }))
        .route("/api/v1/status", get(status))
        .route("/api/v1/system/info", get(system_info))
        .route("/admin/api/v1/update/status", get(update_status))
        .with_state(state)
}



fn demo_log() -> Vec<LogItem> {
    vec![
        LogItem{ tag:"MUS".into(), time:"Now".into(), title:"Neutron Dance".into(), artist:"Pointer Sisters".into(), state:"playing".into(), dur:"4:02".into(), cart:"080-0861".into() },
        LogItem{ tag:"MUS".into(), time:"+0:00".into(), title:"Super Freak (Part 1)".into(), artist:"Rick James".into(), state:"next".into(), dur:"3:14".into(), cart:"080-1588".into() },
        LogItem{ tag:"MUS".into(), time:"+3:14".into(), title:"Bette Davis Eyes".into(), artist:"Kim Carnes".into(), state:"queued".into(), dur:"3:30".into(), cart:"080-6250".into() },
        LogItem{ tag:"MUS".into(), time:"+6:44".into(), title:"Jessie's Girl".into(), artist:"Rick Springfield".into(), state:"queued".into(), dur:"3:07".into(), cart:"080-1591".into() },
    ]
}

fn demo_producers() -> Vec<ProducerStatus> {
    vec![
        ProducerStatus{ name:"Sarah".into(), role:"Producer".into(), connected:true, onAir:true, camOn:false, jitter:"8–20ms".into(), loss:"0.1–0.9%".into(), level:0.72 },
        ProducerStatus{ name:"Emily".into(), role:"Producer".into(), connected:true, onAir:false, camOn:false, jitter:"8–20ms".into(), loss:"0.1–0.9%".into(), level:0.44 },
        ProducerStatus{ name:"Michael".into(), role:"Producer".into(), connected:true, onAir:false, camOn:false, jitter:"8–20ms".into(), loss:"0.1–0.9%".into(), level:0.51 },
    ]
}

async fn playout_tick(playout: Arc<tokio::sync::RwLock<PlayoutState>>) {
    use tokio::time::{sleep, Duration};

    loop {
        sleep(Duration::from_secs(1)).await;

        let mut p = playout.write().await;
        p.now.pos = p.now.pos.saturating_add(1);

        // When the current item finishes, drop it from the log and promote the next item.
        if p.now.pos >= p.now.dur {
            p.now.pos = 0;

            if !p.log.is_empty() {
                // Remove the playing item (top of log).
                p.log.remove(0);
            }

            // Promote new playing item from top of log.
            if let Some(first) = p.log.get_mut(0) {
                first.state = "playing".into();
                p.now.title = first.title.clone();
                p.now.artist = first.artist.clone();
                // crude parse of M:SS
                if let Some((m,s)) = first.dur.split_once(":") {
                    if let (Ok(m), Ok(s)) = (m.parse::<u32>(), s.parse::<u32>()) {
                        p.now.dur = m*60 + s;
                    }
                }
            }

            // Ensure there's a "next" item
            if let Some(second) = p.log.get_mut(1) {
                second.state = "next".into();
            }

            // Keep a few queued items; in real engine this comes from scheduler.
            while p.log.len() < 8 {
                let n = p.log.len();
                p.log.push(LogItem{
                    tag:"MUS".into(),
                    time:format!("+{}", n),
                    title:format!("Queued Track {}", n),
                    artist:"Various".into(),
                    state:"queued".into(),
                    dur:"3:30".into(),
                    cart:format!("080-{:04}", 9000+n as i32),
                });
            }
        }
    }
}

async fn status(State(state): State<AppState>) -> Json<StatusResponse> {
    // Refresh system snapshot
    let system = (system_info(State(state.clone())).await).0;

    let p = state.playout.read().await;
    Json(StatusResponse {
        version: state.version.clone(),
        now: p.now.clone(),
        log: p.log.clone(),
        producers: p.producers.clone(),
        system,
    })
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
