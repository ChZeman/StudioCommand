use serde_json::json;
use axum::http::StatusCode;
use std::{net::SocketAddr, sync::Arc};

// StudioCommand engine (v0)
//
// This service intentionally stays small at first:
//   - Serve the browser UI (static files)
//   - Provide a few JSON endpoints for system status
//   - Run behind a reverse proxy (nginx) for HTTPS and internet exposure


use axum::{
    extract::State,
            routing::{get, post},
    Json, Router,
};
use serde::{Serialize, Deserialize};
use uuid::Uuid;
use sysinfo::System;
use tracing::{info, warn};

#[derive(Clone)]
struct AppState {
    version: String,
    sys: Arc<tokio::sync::Mutex<System>>,
    playout: Arc<tokio::sync::RwLock<PlayoutState>>,
}

#[derive(Clone, Serialize, Deserialize)]
struct LogItem {
    id: Uuid,
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
        .route("/api/v1/transport/skip", post(api_transport_skip))
        .route("/api/v1/transport/dump", post(api_transport_dump))
        .route("/api/v1/transport/reload", post(api_transport_reload))
        .route("/api/v1/queue/remove", post(api_queue_remove))
        .route("/api/v1/queue/move", post(api_queue_move))
        .route("/api/v1/queue/reorder", post(api_queue_reorder))
        .route("/api/v1/queue/insert", post(api_queue_insert))
        .route("/", get(root))
        .route("/health", get(|| async { "OK" }))
        .route("/api/v1/status", get(status))
        .route("/api/v1/ping", get(ping))
        .route("/api/v1/system/info", get(system_info))
        .route("/admin/api/v1/update/status", get(update_status))
        .with_state(state)
}



fn demo_log() -> Vec<LogItem> {
    vec![
        LogItem{ id: Uuid::new_v4(), tag:"MUS".into(), time:"Now".into(), title:"Neutron Dance".into(), artist:"Pointer Sisters".into(), state:"playing".into(), dur:"4:02".into(), cart:"080-0861".into() },
        LogItem{ id: Uuid::new_v4(), tag:"MUS".into(), time:"+0:00".into(), title:"Super Freak (Part 1)".into(), artist:"Rick James".into(), state:"next".into(), dur:"3:14".into(), cart:"080-1588".into() },
        LogItem{ id: Uuid::new_v4(), tag:"MUS".into(), time:"+3:14".into(), title:"Bette Davis Eyes".into(), artist:"Kim Carnes".into(), state:"queued".into(), dur:"3:30".into(), cart:"080-6250".into() },
        LogItem{ id: Uuid::new_v4(), tag:"MUS".into(), time:"+6:44".into(), title:"Jessie's Girl".into(), artist:"Rick Springfield".into(), state:"queued".into(), dur:"3:07".into(), cart:"080-1591".into() },
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
                // Mark the first log item as playing. We must avoid holding a mutable
                // borrow of `first` while also mutating `p.now` (Rust borrow rules).
                first.state = "playing".into();

                // Clone the fields we need *while* we have access to `first`...
                let title = first.title.clone();
                let artist = first.artist.clone();
                let dur = first.dur.clone();

                // ...then explicitly end the `first` borrow before touching `p.now`.
                drop(first);

                p.now.title = title;
                p.now.artist = artist;

                // crude parse of M:SS
                if let Some((m,s)) = dur.split_once(":") {
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
                p.log.push(LogItem{ id: Uuid::new_v4(),
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



async fn ping(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({
        "ok": true,
        "version": state.version,
        "features": ["status", "transport"]
    }))
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



async fn api_transport_skip(State(state): State<AppState>) -> Json<serde_json::Value> {
    // "Skip" advances immediately to the next item in the playout log.
    let mut p = state.playout.write().await;
    advance_to_next(&mut p, Some("skipped"));
    Json(json!({"ok": true}))
}

async fn api_transport_dump(State(state): State<AppState>) -> Json<serde_json::Value> {
    // "Dump" is an operator action to instantly remove the current playing item.
    // In this stub engine, we treat it as "skip with reason=dumped".
    let mut p = state.playout.write().await;
    advance_to_next(&mut p, Some("dumped"));
    Json(json!({"ok": true}))
}

async fn api_transport_reload(State(state): State<AppState>) -> Json<serde_json::Value> {
    // "Reload" repopulates the in-memory demo log.
    let mut p = state.playout.write().await;
    reset_demo_playout(&mut p);
    Json(json!({"ok": true}))
}



#[derive(serde::Deserialize)]
struct QueueRemoveReq { index: usize }

#[derive(serde::Deserialize)]
struct QueueMoveReq { from: usize, to: usize }

#[derive(serde::Deserialize)]
struct QueueReorderReq { order: Vec<Uuid> }


#[derive(serde::Deserialize)]
struct QueueInsertReq { after: usize, item: QueueInsertItem }

#[derive(serde::Deserialize)]
struct QueueInsertItem {
    tag: String,
    title: String,
    artist: String,
    dur: String,
    cart: String,
}

async fn api_queue_remove(
    State(state): State<AppState>,
    Json(req): Json<QueueRemoveReq>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // Remove an upcoming item from the queue. Index 0 is "playing" and cannot be removed.
    let mut p = state.playout.write().await;
    if req.index == 0 || req.index >= p.log.len() {
        return Err(StatusCode::BAD_REQUEST);
    }
    p.log.remove(req.index);
    normalize_log_state(&mut p);
    Ok(Json(json!({"ok": true})))
}

async fn api_queue_move(
    State(state): State<AppState>,
    Json(req): Json<QueueMoveReq>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // Move an upcoming item within the queue. Index 0 is "playing" and stays put.
    let mut p = state.playout.write().await;
    if req.from == 0 || req.to == 0 || req.from >= p.log.len() || req.to >= p.log.len() {
        return Err(StatusCode::BAD_REQUEST);
    }
    if req.from == req.to {
        return Ok(Json(json!({"ok": true})));
    }
    let item = p.log.remove(req.from);
    p.log.insert(req.to, item);
    normalize_log_state(&mut p);
    Ok(Json(json!({"ok": true})))
}


async fn api_queue_reorder(
    State(state): State<AppState>,
    Json(req): Json<QueueReorderReq>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // Reorder upcoming items in the queue using stable item IDs.
    // Index 0 is "playing" and is pinned.
    let mut p = state.playout.write().await;

    if p.log.len() <= 1 {
        return Ok(Json(json!({"ok": true})));
    }

    // We reorder only the upcoming items (everything after the playing item).
    // Require a full list for determinism.
    let upcoming_len = p.log.len() - 1;
    if req.order.len() != upcoming_len {
        return Err(StatusCode::BAD_REQUEST);
    }

    // Build a lookup for upcoming items.
    use std::collections::{HashMap, HashSet};
    let mut by_id: HashMap<Uuid, LogItem> = HashMap::with_capacity(upcoming_len);
    for item in p.log.drain(1..) {
        by_id.insert(item.id, item);
    }

    // Validate: no duplicates and all IDs exist.
    let mut seen: HashSet<Uuid> = HashSet::with_capacity(req.order.len());
    let mut reordered: Vec<LogItem> = Vec::with_capacity(upcoming_len);

    for id in &req.order {
        if !seen.insert(*id) {
            return Err(StatusCode::BAD_REQUEST);
        }
        let item = by_id.remove(id).ok_or(StatusCode::BAD_REQUEST)?;
        reordered.push(item);
    }

    // Defensive: append any stragglers (should be none due to strict length check).
    reordered.extend(by_id.into_values());

    // Put the playing item back at the front and normalize state markers.
    // (We drained from index 1.. above, so p.log currently has exactly the playing item.)
    p.log.extend(reordered);
    normalize_log_state(&mut p);

    Ok(Json(json!({"ok": true})))
}

async fn api_queue_insert(
    State(state): State<AppState>,
    Json(req): Json<QueueInsertReq>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // Insert a cart after a given index (e.g., after "next" => after=1).
    let mut p = state.playout.write().await;
    let after = req.after.min(p.log.len().saturating_sub(1));
    let ins = LogItem{ id: Uuid::new_v4(),
        tag: req.item.tag,
        time: "--:--".into(),
        title: req.item.title,
        artist: req.item.artist,
        state: "queued".into(),
        dur: req.item.dur,
        cart: req.item.cart,
    };
    p.log.insert(after+1, ins);
    normalize_log_state(&mut p);
    Ok(Json(json!({"ok": true})))
}

fn normalize_log_state(p: &mut PlayoutState){
    // Ensure we always have exactly one playing + one next marker,
    // and keep Now Playing in sync with the first item in the log.
    if let Some(first) = p.log.get_mut(0) {
        first.state = "playing".into();
        p.now.title = first.title.clone();
        p.now.artist = first.artist.clone();
        p.now.dur = parse_dur_to_sec(&first.dur);
        // keep current position, but clamp to duration
        if p.now.pos > p.now.dur { p.now.pos = 0; }
    }
    if p.log.len() > 1 {
        p.log[1].state = "next".into();
    }
    for i in 2..p.log.len() {
        p.log[i].state = "queued".into();
    }
}

fn reset_demo_playout(p: &mut PlayoutState) {
    // Keep this deterministic so the UI is predictable while we build real scheduling.
    p.now.title = "Lean On Me".into();
    p.now.artist = "Club Nouveau".into();
    p.now.dur = 3*60 + 48;
    p.now.pos = 0;

    p.log = vec![
        LogItem{ id: Uuid::new_v4(), tag:"MUS".into(), time:"15:33".into(), title:"Lean On Me".into(), artist:"Club Nouveau".into(), state:"playing".into(), dur:"3:48".into(), cart:"080-0599".into() },
        LogItem{ id: Uuid::new_v4(), tag:"MUS".into(), time:"15:37".into(), title:"Bette Davis Eyes".into(), artist:"Kim Carnes".into(), state:"queued".into(), dur:"3:30".into(), cart:"080-6250".into() },
        LogItem{ id: Uuid::new_v4(), tag:"MUS".into(), time:"15:41".into(), title:"Talk Dirty To Me".into(), artist:"Poison".into(), state:"queued".into(), dur:"3:42".into(), cart:"080-4577".into() },
        LogItem{ id: Uuid::new_v4(), tag:"EVT".into(), time:"15:45".into(), title:"TOH Legal ID".into(), artist:"".into(), state:"locked".into(), dur:"0:10".into(), cart:"ID-TOH".into() },
        LogItem{ id: Uuid::new_v4(), tag:"MUS".into(), time:"15:46".into(), title:"Jessie's Girl".into(), artist:"Rick Springfield".into(), state:"queued".into(), dur:"3:07".into(), cart:"080-1591".into() },
    ];

    // Ensure "next" is marked consistently.
    if p.log.len() > 1 {
        p.log[1].state = "next".into();
    }
}

fn parse_dur_to_sec(d: &str) -> u32 {
    if let Some((m,s)) = d.split_once(":") {
        if let (Ok(m), Ok(s)) = (m.parse::<u32>(), s.parse::<u32>()) {
            return m*60 + s;
        }
    }
    0
}

fn advance_to_next(p: &mut PlayoutState, reason: Option<&str>) {
    // Mark and remove the current playing item, then promote the next queued item.
    if !p.log.is_empty() {
        // remove the first item (assumed playing)
        let mut removed = p.log.remove(0);
        if let Some(r) = reason {
            removed.state = r.into();
        } else {
            removed.state = "played".into();
        }
    }

    // Promote new first item
    if let Some(first) = p.log.get_mut(0) {
        first.state = "playing".into();
        p.now.title = first.title.clone();
        p.now.artist = first.artist.clone();
        p.now.dur = parse_dur_to_sec(&first.dur);
        p.now.pos = 0;
    } else {
        // Empty log: clear now
        p.now.title = "".into();
        p.now.artist = "".into();
        p.now.dur = 0;
        p.now.pos = 0;
    }

    // Maintain "next" marker
    if p.log.len() > 1 {
        p.log[1].state = "next".into();
        for i in 2..p.log.len() {
            if p.log[i].state == "next" {
                p.log[i].state = "queued".into();
            }
        }
    }
}