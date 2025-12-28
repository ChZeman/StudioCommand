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
use rusqlite::{Connection, params};
use sysinfo::System;
use tracing::{info, warn};
use tokio::io::AsyncWriteExt;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::io::{AsyncBufReadExt, BufReader};
use std::collections::VecDeque;

#[derive(Clone)]
struct AppState {
    version: String,
    sys: Arc<tokio::sync::Mutex<System>>,
    playout: Arc<tokio::sync::RwLock<PlayoutState>>,
    topup: Arc<tokio::sync::Mutex<TopUpConfig>>,
    output: Arc<tokio::sync::Mutex<OutputRuntime>>,
}

// --- Streaming output (Icecast) -----------------------------------------

#[derive(Clone, Serialize, Deserialize, Default)]
struct StreamOutputConfig {
    r#type: String,      // "icecast" (future: "shoutcast")
    host: String,
    port: u16,
    mount: String,
    username: String,
    password: String,
    codec: String,       // "mp3" | "aac"
    bitrate_kbps: u16,   // 64..320
    enabled: bool,
    name: Option<String>,
    genre: Option<String>,
    description: Option<String>,
    public: Option<bool>,
}

#[derive(Clone, Serialize, Deserialize, Default)]
struct TopUpConfig {
    enabled: bool,
    dir: String,
    min_queue: u16,
    batch: u16,
}


#[derive(Clone, Serialize, Deserialize)]
struct StreamOutputStatus {
    state: String, // stopped | starting | connected | error
    uptime_sec: u64,
    last_error: Option<String>,
    codec: Option<String>,
    bitrate_kbps: Option<u16>,
}

struct OutputRuntime {
    config: StreamOutputConfig,
    status: StreamOutputStatus,
    ffmpeg_child: Option<tokio::process::Child>,
    writer_task: Option<tokio::task::JoinHandle<()>>,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
    stderr_tail: VecDeque<String>,
    started_at: Option<std::time::Instant>,
}

impl OutputRuntime {
    fn new(config: StreamOutputConfig) -> Self {
        Self {
            status: StreamOutputStatus {
                state: "stopped".into(),
                uptime_sec: 0,
                last_error: None,
                codec: None,
                bitrate_kbps: None,
            },
            config,
            ffmpeg_child: None,
            writer_task: None,
            stderr_task: None,
            stderr_tail: VecDeque::with_capacity(80),
            started_at: None,
        }
    }
}

// --- Persistence (SQLite) -------------------------------------------------
//
// Why SQLite?
// - Crash-safe: updates happen inside transactions.
// - Concurrent-safe: UI reorder, future ingest, and engine ops can all share one DB.
// - Operationally simple: a single file, but with the safety properties of a database.
//
// We keep the DB schema intentionally small and stable. The HTTP API remains the main
// integration surface; future third-party file ingest can translate inputs into API/commands.
//
// DB location:
// - Can be overridden with STUDIOCOMMAND_DB_PATH
// - Defaults to /opt/studiocommand/shared/studiocommand.db (installer-managed persistent dir)
//
// Note: rusqlite is synchronous. We call it via spawn_blocking to avoid blocking tokio.
fn db_path() -> String {
    std::env::var("STUDIOCOMMAND_DB_PATH")
        .unwrap_or_else(|_| "/opt/studiocommand/shared/studiocommand.db".to_string())
}

fn db_init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;
        PRAGMA foreign_keys = ON;

        CREATE TABLE IF NOT EXISTS queue_items (
            id       TEXT PRIMARY KEY,
            position INTEGER NOT NULL,
            tag      TEXT NOT NULL,
            time     TEXT NOT NULL,
            title    TEXT NOT NULL,
            artist   TEXT NOT NULL,
            state    TEXT NOT NULL,
            dur      TEXT NOT NULL,
            cart     TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_queue_items_position ON queue_items(position);

         CREATE TABLE IF NOT EXISTS stream_output_config (
            id            INTEGER PRIMARY KEY CHECK (id = 1),
            type          TEXT NOT NULL,
            host          TEXT NOT NULL,
            port          INTEGER NOT NULL,
            mount         TEXT NOT NULL,
            username      TEXT NOT NULL,
            password      TEXT NOT NULL,
            codec         TEXT NOT NULL,
            bitrate_kbps  INTEGER NOT NULL,
            enabled       INTEGER NOT NULL,
            name          TEXT,
            genre         TEXT,
            description   TEXT,
            public        INTEGER
        );

        CREATE TABLE IF NOT EXISTS top_up_config (
            id            INTEGER PRIMARY KEY CHECK (id = 1),
            enabled       INTEGER NOT NULL,
            dir           TEXT NOT NULL,
            min_queue     INTEGER NOT NULL,
            batch         INTEGER NOT NULL
        );
        "#,
    )?;
    Ok(())
}

fn db_load_queue(conn: &Connection) -> anyhow::Result<Option<Vec<LogItem>>> {
    db_init(conn)?;

    let count: i64 = conn.query_row("SELECT COUNT(*) FROM queue_items", [], |row| row.get(0))?;
    if count == 0 {
        return Ok(None);
    }

    let mut stmt = conn.prepare(
        "SELECT id, tag, time, title, artist, state, dur, cart FROM queue_items ORDER BY position ASC",
    )?;
    let mut rows = stmt.query([])?;

    let mut out: Vec<LogItem> = Vec::new();
    while let Some(row) = rows.next()? {
        let id_str: String = row.get(0)?;
        let id = Uuid::parse_str(&id_str)
            .map_err(|e| anyhow::anyhow!("invalid UUID in DB (id={id_str}): {e}"))?;

        out.push(LogItem {
            id,
            tag: row.get(1)?,
            time: row.get(2)?,
            title: row.get(3)?,
            artist: row.get(4)?,
            state: row.get(5)?,
            dur: row.get(6)?,
            cart: row.get(7)?,
        });
    }

    // Normalize state markers so the UI is consistent even if the DB contains older data.
    // Note: we only normalize the *log* markers here; NowPlaying is derived from the
    // in-memory PlayoutState and is handled separately.
    normalize_log_markers(&mut out);

    Ok(Some(out))
}

fn db_save_queue(conn: &mut Connection, log: &[LogItem]) -> anyhow::Result<()> {
    db_init(conn)?;

    let tx = conn.transaction()?;

    // Simple + safe approach: rewrite the table in one transaction.
    // This keeps ordering consistent and avoids partial updates on crash.
    tx.execute("DELETE FROM queue_items", [])?;

    let mut position: i64 = 0;
    for item in log {
        tx.execute(
            "INSERT INTO queue_items (id, position, tag, time, title, artist, state, dur, cart)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                item.id.to_string(),
                position,
                item.tag,
                item.time,
                item.title,
                item.artist,
                item.state,
                item.dur,
                item.cart
            ],
        )?;
        position += 1;
    }

    tx.commit()?;
    Ok(())
}

async fn load_queue_from_db_or_demo() -> Vec<LogItem> {
    let path = db_path();
    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Vec<LogItem>>> {
        let conn = Connection::open(path)?;
        db_load_queue(&conn)
    })
    .await;

    // Helper: in the stub/demo engine we always want a few items in the log so
    // the UI has something to work with immediately after startup.
    //
    // In a real automation engine, the scheduler would be responsible for
    // keeping the queue filled. Here we do a small amount of padding to make
    // restarts less surprising (e.g. if the persisted DB contains only a few
    // rows).
    fn pad_demo_items(mut log: Vec<LogItem>) -> Vec<LogItem> {
        // Keep the current items, but ensure at least 8 total so UI testing is
        // consistent.
        while log.len() < 8 {
            let n = log.len();
            log.push(LogItem {
                id: Uuid::new_v4(),
                tag: "MUS".into(),
                time: format!("+{}", n),
                title: format!("Queued Track {}", n),
                artist: "Various".into(),
                state: "queued".into(),
                dur: "3:30".into(),
                cart: format!("080-{:04}", 9000 + n as i32),
            });
        }
        normalize_log_markers(&mut log);
        log
    }

    match res {
        Ok(Ok(Some(log))) => pad_demo_items(log),
        Ok(Ok(None)) => demo_log(),
        Ok(Err(e)) => {
            tracing::warn!("failed to load queue from sqlite, using demo queue: {e}");
            demo_log()
        }
        Err(e) => {
            tracing::warn!("failed to join sqlite load task, using demo queue: {e}");
            demo_log()
        }
    }
}

fn default_output_config() -> StreamOutputConfig {
    StreamOutputConfig {
        r#type: "icecast".into(),
        host: "seahorse.juststreamwith.us".into(),
        port: 8006,
        mount: "/studiocommand".into(),
        username: "source".into(),
        password: "".into(),
        codec: "mp3".into(),
        bitrate_kbps: 128,
        enabled: false,
        name: Some("StudioCommand".into()),
        genre: None,
        description: None,
        public: Some(false),
    }
}

fn default_topup_config() -> TopUpConfig {
    TopUpConfig { enabled: false, dir: "".into(), min_queue: 5, batch: 5 }
}

fn db_load_topup_config(conn: &Connection) -> anyhow::Result<TopUpConfig> {
    db_init(conn)?;

    let row_opt = conn.query_row(
        "SELECT enabled, dir, min_queue, batch FROM top_up_config WHERE id = 1",
        [],
        |row| {
            Ok(TopUpConfig {
                enabled: row.get::<_, i64>(0)? != 0,
                dir: row.get::<_, String>(1)?,
                min_queue: row.get::<_, i64>(2)? as u16,
                batch: row.get::<_, i64>(3)? as u16,
            })
        },
    );

    match row_opt {
        Ok(cfg) => Ok(cfg),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(default_topup_config()),
        Err(e) => Err(e.into()),
    }
}

fn db_save_topup_config(conn: &mut Connection, cfg: &TopUpConfig) -> anyhow::Result<()> {
    db_init(conn)?;
    conn.execute(
        "INSERT INTO top_up_config (id, enabled, dir, min_queue, batch)
         VALUES (1, ?1, ?2, ?3, ?4)
         ON CONFLICT(id) DO UPDATE SET
           enabled=excluded.enabled,
           dir=excluded.dir,
           min_queue=excluded.min_queue,
           batch=excluded.batch",
        params![
            if cfg.enabled { 1 } else { 0 },
            cfg.dir,
            cfg.min_queue as i64,
            cfg.batch as i64,
        ],
    )?;
    Ok(())
}

async fn load_topup_config_from_db_or_default() -> TopUpConfig {
    let path = db_path();
    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<TopUpConfig> {
        let conn = Connection::open(path)?;
        db_load_topup_config(&conn)
    })
    .await;

    match res {
        Ok(Ok(cfg)) => cfg,
        Ok(Err(e)) => {
            tracing::warn!("failed to load top-up config, using defaults: {e}");
            default_topup_config()
        }
        Err(e) => {
            tracing::warn!("failed to join top-up load task, using defaults: {e}");
            default_topup_config()
        }
    }
}

fn db_load_output_config(conn: &Connection) -> anyhow::Result<StreamOutputConfig> {
    db_init(conn)?;

    let row_opt = conn.query_row(
        "SELECT type, host, port, mount, username, password, codec, bitrate_kbps, enabled, name, genre, description, public FROM stream_output_config WHERE id = 1",
        [],
        |row| {
            Ok(StreamOutputConfig {
                r#type: row.get::<_, String>(0)?,
                host: row.get::<_, String>(1)?,
                port: row.get::<_, i64>(2)? as u16,
                mount: row.get::<_, String>(3)?,
                username: row.get::<_, String>(4)?,
                password: row.get::<_, String>(5)?,
                codec: row.get::<_, String>(6)?,
                bitrate_kbps: row.get::<_, i64>(7)? as u16,
                enabled: row.get::<_, i64>(8)? != 0,
                name: row.get::<_, Option<String>>(9)?,
                genre: row.get::<_, Option<String>>(10)?,
                description: row.get::<_, Option<String>>(11)?,
                public: match row.get::<_, Option<i64>>(12)? {
                    Some(v) => Some(v != 0),
                    None => None,
                },
            })
        },
    );

    match row_opt {
        Ok(cfg) => Ok(cfg),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(default_output_config()),
        Err(e) => Err(e.into()),
    }
}

fn db_save_output_config(conn: &mut Connection, cfg: &StreamOutputConfig) -> anyhow::Result<()> {
    db_init(conn)?;
    conn.execute(
        "INSERT INTO stream_output_config (id, type, host, port, mount, username, password, codec, bitrate_kbps, enabled, name, genre, description, public)
         VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(id) DO UPDATE SET
           type=excluded.type,
           host=excluded.host,
           port=excluded.port,
           mount=excluded.mount,
           username=excluded.username,
           password=excluded.password,
           codec=excluded.codec,
           bitrate_kbps=excluded.bitrate_kbps,
           enabled=excluded.enabled,
           name=excluded.name,
           genre=excluded.genre,
           description=excluded.description,
           public=excluded.public",
        params![
            cfg.r#type,
            cfg.host,
            cfg.port as i64,
            cfg.mount,
            cfg.username,
            cfg.password,
            cfg.codec,
            cfg.bitrate_kbps as i64,
            if cfg.enabled { 1 } else { 0 },
            cfg.name,
            cfg.genre,
            cfg.description,
            cfg.public.map(|v| if v { 1 } else { 0 }),
        ],
    )?;
    Ok(())
}

async fn load_output_config_from_db_or_default() -> StreamOutputConfig {
    let path = db_path();
    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<StreamOutputConfig> {
        let conn = Connection::open(path)?;
        db_load_output_config(&conn)
    })
    .await;

    match res {
        Ok(Ok(cfg)) => cfg,
        Ok(Err(e)) => {
            tracing::warn!("failed to load stream output config, using defaults: {e}");
            default_output_config()
        }
        Err(e) => {
            tracing::warn!("failed to join stream output load task, using defaults: {e}");
            default_output_config()
        }
    }
}

async fn persist_queue(log: Vec<LogItem>) {
    let path = db_path();
    let _ = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let mut conn = Connection::open(path)?;
        db_save_queue(&mut conn, &log)?;
        Ok(())
    })
    .await
    .map_err(|e| anyhow::anyhow!(e))
    .and_then(|x| x)
    .map_err(|e| tracing::warn!("failed to persist queue to sqlite: {e}"));
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
let log = load_queue_from_db_or_demo().await;

// Load streaming output config (Icecast) from SQLite (or defaults).
let output_cfg = load_output_config_from_db_or_default().await;

// Load playout top-up config (random folder filler) from SQLite (or defaults).
let topup_cfg = load_topup_config_from_db_or_default().await;

// Ensure the current queue is persisted so restarts are deterministic.
// This is cheap (single transaction) and makes initial installs predictable.
persist_queue(log.clone()).await;

let playout = PlayoutState {
    now: NowPlaying { title: "Neutron Dance".into(), artist: "Pointer Sisters".into(), dur: 242, pos: 0 },
    // Load the queue from SQLite if present; otherwise fall back to a demo queue.
    log: log.clone(),
    producers: demo_producers(),
};

let state = AppState {
    version: version.clone(),
    sys: Arc::new(tokio::sync::Mutex::new(sys)),
    playout: Arc::new(tokio::sync::RwLock::new(playout)),
    topup: Arc::new(tokio::sync::Mutex::new(topup_cfg)),
    output: Arc::new(tokio::sync::Mutex::new(OutputRuntime::new(output_cfg))),
};

// Optional: auto-start streaming output if config says enabled.
// (If ffmpeg isn't installed or creds are wrong, status will surface the error.)
{
    let out = state.output.clone();
    let pl = state.playout.clone();
    let tu = state.topup.clone();
    let enabled = out.lock().await.config.enabled;
    if enabled {
        tokio::spawn(async move {
            let _ = output_start_internal(out, pl, tu).await;
        });
    }
}

// Background tick: advances the demo queue once per second.
// tokio::spawn(playout_tick(state.playout.clone()));


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
        .route("/api/v1/output", get(api_output_get))
        .route("/api/v1/output/config", post(api_output_set_config))
        .route("/api/v1/output/start", post(api_output_start))
        .route("/api/v1/output/stop", post(api_output_stop))
        .route("/api/v1/playout/topup", get(api_topup_get))
        .route("/api/v1/playout/topup/config", post(api_topup_set_config))
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
        //
        // NOTE: This stub engine mutates the queue over time (removing the playing
        // item and padding demo items). To keep SQLite persistence intuitive during
        // development/testing, we also persist the updated queue whenever the
        // "track ends" event occurs.
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

            // Persist the updated queue, but do it *after* releasing the write lock.
            // We intentionally clone the log to keep the lock hold-time short.
            let snapshot = p.log.clone();
            drop(p);
            persist_queue(snapshot).await;
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

// --- Output API (Icecast) -------------------------------------------------

fn sanitize_ffmpeg_line(line: &str, password: &str) -> String {
    // Best-effort redaction. We never want to leak credentials into UI/logs.
    // ffmpeg typically doesn't echo full URLs at loglevel=error, but it can.
    let mut s = line.to_string();
    if !password.is_empty() {
        s = s.replace(password, "****");
    }
    // Also redact any Basic auth header content if it appears.
    if s.to_ascii_lowercase().contains("authorization:") {
        return "Authorization: ****".to_string();
    }
    s
}

fn push_stderr_tail(o: &mut OutputRuntime, line: String) {
    const MAX: usize = 80;
    if o.stderr_tail.len() >= MAX {
        o.stderr_tail.pop_front();
    }
    o.stderr_tail.push_back(line.clone());

    // If ffmpeg emits a clear HTTP/auth/config error, surface it immediately.
    let lc = line.to_ascii_lowercase();
    if lc.contains("unauthorized") || lc.contains("forbidden") || lc.contains("not found") || lc.contains("server returned") {
        o.status.state = "error".into();
        o.status.last_error = Some(line);
    }
}

fn last_stderr_summary(tail: &VecDeque<String>) -> Option<String> {
    // Prefer the last non-empty, non-noisy line.
    for line in tail.iter().rev() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        // Skip repetitive/low-signal lines.
        let lc = t.to_ascii_lowercase();
        if lc.contains("broken pipe") {
            continue;
        }
        if lc.contains("conversion failed") {
            continue;
        }
        return Some(t.to_string());
    }
    // Fall back to the last line if that's all we have.
    tail.back().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

#[derive(Serialize)]
struct OutputGetResponse {
    config: StreamOutputConfig,
    status: StreamOutputStatus,
}

async fn api_output_get(State(state): State<AppState>) -> Json<OutputGetResponse> {
    let mut o = state.output.lock().await;

    // If ffmpeg exited since last poll, update status.
    if let Some(child) = o.ffmpeg_child.as_mut() {
        match child.try_wait() {
            Ok(Some(es)) => {
                o.ffmpeg_child = None;
                o.started_at = None;
                if let Some(task) = o.stderr_task.take() {
                    task.abort();
                }
                o.status.uptime_sec = 0;
                if es.success() {
                    o.status.state = "stopped".into();
                } else {
                    o.status.state = "error".into();
                    // Prefer the last meaningful stderr line for operator visibility.
                    if let Some(tail) = last_stderr_summary(&o.stderr_tail) {
                        o.status.last_error = Some(tail);
                    } else {
                        o.status.last_error = Some(format!("ffmpeg exited: {es}"));
                    }
                }
            }
            Ok(None) => {}
            Err(e) => {
                o.status.state = "error".into();
                o.status.last_error = Some(format!("ffmpeg try_wait error: {e}"));
            }
        }
    }
    // Refresh uptime
    if let Some(started) = o.started_at {
        o.status.uptime_sec = started.elapsed().as_secs();
    } else {
        o.status.uptime_sec = 0;
    }
    Json(OutputGetResponse {
        config: o.config.clone(),
        status: o.status.clone(),
    })
}

async fn api_output_set_config(
    State(state): State<AppState>,
    Json(mut cfg): Json<StreamOutputConfig>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // Normalize a few inputs for operator convenience.
    if !cfg.mount.starts_with('/') {
        cfg.mount = format!("/{}", cfg.mount);
    }
    if cfg.codec != "mp3" && cfg.codec != "aac" {
        return Err(StatusCode::BAD_REQUEST);
    }
    if cfg.bitrate_kbps < 32 || cfg.bitrate_kbps > 320 {
        return Err(StatusCode::BAD_REQUEST);
    }

    // Persist to SQLite.
    let path = db_path();
    let cfg_clone = cfg.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let mut conn = Connection::open(path)?;
        db_save_output_config(&mut conn, &cfg_clone)?;
        Ok(())
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Update in-memory config.
    let mut o = state.output.lock().await;
    o.config = cfg;

    Ok(Json(json!({"ok": true})))
}

async fn api_output_start(State(state): State<AppState>) -> Result<Json<serde_json::Value>, StatusCode> {
    output_start_internal(state.output.clone(), state.playout.clone(), state.topup.clone()).await?;
    Ok(Json(json!({"ok": true})))
}

async fn api_output_stop(State(state): State<AppState>) -> Result<Json<serde_json::Value>, StatusCode> {
    output_stop_internal(state.output.clone()).await;
    Ok(Json(json!({"ok": true})))
}

async fn output_start_internal(output: Arc<tokio::sync::Mutex<OutputRuntime>>, playout: Arc<tokio::sync::RwLock<PlayoutState>>, topup: Arc<tokio::sync::Mutex<TopUpConfig>>) -> Result<(), StatusCode> {
    let mut o = output.lock().await;
    if o.ffmpeg_child.is_some() {
        return Err(StatusCode::CONFLICT);
    }

    // Basic validation
    if o.config.password.trim().is_empty() {
        o.status.state = "error".into();
        o.status.last_error = Some("Icecast password is empty".into());
        return Err(StatusCode::BAD_REQUEST);
    }

    // Spawn ffmpeg and a simple audio generator to prove end-to-end streaming.
    let (mut child, stdin, stderr) = spawn_ffmpeg_icecast(&o.config).await.map_err(|e| {
        o.status.state = "error".into();
        o.status.last_error = Some(e.to_string());
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    o.status.state = "starting".into();
    o.status.last_error = None;
    o.status.codec = Some(o.config.codec.clone());
    o.status.bitrate_kbps = Some(o.config.bitrate_kbps);
    o.started_at = Some(std::time::Instant::now());

    let output_for_writer = output.clone();
    let writer_task = tokio::spawn(async move {
        if let Err(e) = writer_playout(stdin, playout, topup).await {
            let mut o = output_for_writer.lock().await;
            o.status.state = "error".into();
            o.status.last_error = Some(format!("audio writer: {e}"));
        }
    });

    // Capture ffmpeg stderr so the UI can show actionable errors (e.g. 401 Unauthorized)
    // without exposing secrets.
    let output_for_stderr = output.clone();
    let password = o.config.password.clone();
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let sanitized = sanitize_ffmpeg_line(&line, &password);
            if sanitized.trim().is_empty() {
                continue;
            }
            let mut o = output_for_stderr.lock().await;
            push_stderr_tail(&mut o, sanitized);
        }
    });

    // Put child + task into runtime.
    o.ffmpeg_child = Some(child);
    o.writer_task = Some(writer_task);
    o.stderr_task = Some(stderr_task);

    // Optimistically mark connected after a short grace period if ffmpeg is still alive.
    drop(o);
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    let mut o = output.lock().await;
    if o.ffmpeg_child.is_some() && o.status.state == "starting" {
        o.status.state = "connected".into();
    }

    Ok(())
}

async fn output_stop_internal(output: Arc<tokio::sync::Mutex<OutputRuntime>>) {
    let mut o = output.lock().await;

    if let Some(mut child) = o.ffmpeg_child.take() {
        // Try graceful shutdown first.
        let _ = child.kill().await;
    }

    if let Some(task) = o.writer_task.take() {
        task.abort();
    }

    if let Some(task) = o.stderr_task.take() {
        task.abort();
    }

    o.started_at = None;
    o.status.uptime_sec = 0;
    o.status.state = "stopped".into();
}

async fn spawn_ffmpeg_icecast(cfg: &StreamOutputConfig) -> anyhow::Result<(tokio::process::Child, tokio::process::ChildStdin, tokio::process::ChildStderr)> {
    let ffmpeg = std::env::var("STUDIOCOMMAND_FFMPEG").unwrap_or_else(|_| "ffmpeg".to_string());

    // Important: never log the password.
    // Note: Icecast source passwords are usually ASCII and safe to embed.
    // If you need full URL-encoding later, we can add it, but we avoid pulling
    // in extra deps for the MVP.
    let url = format!(
        "icecast://{}:{}@{}:{}{}",
        cfg.username,
        cfg.password,
        cfg.host,
        cfg.port,
        cfg.mount
    );

    let mut cmd = Command::new(ffmpeg);
    cmd.arg("-hide_banner");
    cmd.arg("-loglevel").arg("error");
    cmd.arg("-re");
    cmd.arg("-f").arg("s16le");
    cmd.arg("-ar").arg("44100");
    cmd.arg("-ac").arg("2");
    cmd.arg("-i").arg("pipe:0");

    match cfg.codec.as_str() {
        "mp3" => {
            cmd.arg("-c:a").arg("libmp3lame");
            cmd.arg("-b:a").arg(format!("{}k", cfg.bitrate_kbps));
            cmd.arg("-content_type").arg("audio/mpeg");
            cmd.arg("-f").arg("mp3");
        }
        "aac" => {
            cmd.arg("-c:a").arg("aac");
            cmd.arg("-b:a").arg(format!("{}k", cfg.bitrate_kbps));
            cmd.arg("-content_type").arg("audio/aac");
            cmd.arg("-f").arg("adts");
        }
        _ => anyhow::bail!("unsupported codec: {}", cfg.codec),
    }

    cmd.arg(url);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn()?;
    let stdin = child.stdin.take().ok_or_else(|| anyhow::anyhow!("ffmpeg stdin unavailable"))?;
    let stderr = child.stderr.take().ok_or_else(|| anyhow::anyhow!("ffmpeg stderr unavailable"))?;
    Ok((child, stdin, stderr))
}

async fn writer_sine_wave(mut stdin: tokio::process::ChildStdin) -> anyhow::Result<()> {
    // 1k frames per chunk (~23ms @ 44.1kHz)
    const SR: f32 = 44100.0;
    const FRAMES: usize = 1024;
    const FREQ: f32 = 440.0;
    let mut phase: f32 = 0.0;
    let step = (std::f32::consts::TAU * FREQ) / SR;

    let mut interval = tokio::time::interval(std::time::Duration::from_millis(20));
    loop {
        interval.tick().await;
        let mut buf = Vec::with_capacity(FRAMES * 2 * 2);
        for _ in 0..FRAMES {
            let v = (phase.sin() * 0.12 * i16::MAX as f32) as i16;
            phase += step;
            if phase > std::f32::consts::TAU {
                phase -= std::f32::consts::TAU;
            }
            // stereo interleaved s16le
            buf.extend_from_slice(&v.to_le_bytes());
            buf.extend_from_slice(&v.to_le_bytes());
        }
        stdin.write_all(&buf).await?;
    }
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

    // Persist the updated queue so restarts keep the same order.
    persist_queue(p.log.clone()).await;
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

    // Persist the updated queue so restarts keep the same order.
    persist_queue(p.log.clone()).await;
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

    // Persist the updated queue so restarts keep the same order.
    persist_queue(p.log.clone()).await;

    Ok(Json(json!({"ok": true})))
}

async fn api_queue_insert(
    State(state): State<AppState>,
    Json(req): Json<QueueInsertReq>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // Insert a cart after a given index (e.g., after "next" => after=1).
    let mut p = state.playout.write().await;
    // Handle truly-empty queues: inserting at index 1 would panic.
    // In that case, the first inserted item becomes "playing".
    if p.log.is_empty() {
        let ins = LogItem {
            id: Uuid::new_v4(),
            tag: req.item.tag,
            time: "--:--".into(),
            title: req.item.title,
            artist: req.item.artist,
            state: "playing".into(),
            dur: req.item.dur,
            cart: req.item.cart,
        };
        p.log.push(ins);
    } else {
        let after = req.after.min(p.log.len().saturating_sub(1));
        let ins = LogItem {
            id: Uuid::new_v4(),
            tag: req.item.tag,
            time: "--:--".into(),
            title: req.item.title,
            artist: req.item.artist,
            state: "queued".into(),
            dur: req.item.dur,
            cart: req.item.cart,
        };
        p.log.insert(after + 1, ins);
    }
    normalize_log_state(&mut p);

    // Persist the updated queue so restarts keep the same order.
    persist_queue(p.log.clone()).await;
    Ok(Json(json!({"ok": true})))
}

fn normalize_log_markers(log: &mut [LogItem]) {
    // Keep queue marker semantics deterministic:
    //   - index 0 is always "playing"
    //   - index 1 (if present) is always "next"
    //   - everything after that is "queued"
    //
    // We centralize this logic so it can be applied both to the in-memory queue
    // and to DB-loaded queues (which may contain legacy/incorrect markers).
    if let Some(first) = log.get_mut(0) {
        first.state = "playing".into();
    }
    if log.len() > 1 {
        log[1].state = "next".into();
    }
    for i in 2..log.len() {
        log[i].state = "queued".into();
    }
}

fn normalize_log_state(p: &mut PlayoutState){
    // Ensure we always have deterministic "playing/next/queued" markers,
    // and keep Now Playing in sync with the first item in the log.
    normalize_log_markers(&mut p.log);

    if let Some(first) = p.log.get(0) {
        p.now.title = first.title.clone();
        p.now.artist = first.artist.clone();
        p.now.dur = parse_dur_to_sec(&first.dur);
        // keep current position, but clamp to duration
        if p.now.pos > p.now.dur { p.now.pos = 0; }
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

// --- Playout top-up (random folder filler) -------------------------------


#[derive(Serialize)]
struct TopUpGetResponse {
    config: TopUpConfig,
}

async fn api_topup_get(State(state): State<AppState>) -> Json<TopUpGetResponse> {
    let cfg = state.topup.lock().await.clone();
    Json(TopUpGetResponse { config: cfg })
}

async fn api_topup_set_config(
    State(state): State<AppState>,
    Json(mut cfg): Json<TopUpConfig>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // Basic validation / normalization
    cfg.dir = cfg.dir.trim().to_string();
    if cfg.min_queue == 0 || cfg.min_queue > 100 {
        return Err(StatusCode::BAD_REQUEST);
    }
    if cfg.batch == 0 || cfg.batch > 100 {
        return Err(StatusCode::BAD_REQUEST);
    }

    let path = db_path();
    let cfg_clone = cfg.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let mut conn = Connection::open(path)?;
        db_save_topup_config(&mut conn, &cfg_clone)?;
        Ok(())
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut cur = state.topup.lock().await;
    *cur = cfg;

    Ok(Json(json!({"ok": true})))
}

// --- Real playout writer --------------------------------------------------

fn resolve_cart_to_path(cart: &str) -> Option<String> {
    use std::path::Path;

    let cart = cart.trim();
    if cart.is_empty() {
        return None;
    }

    // Absolute path
    if cart.starts_with('/') && Path::new(cart).exists() {
        return Some(cart.to_string());
    }

    // Shared carts folder lookup: /opt/studiocommand/shared/carts/<cart>.<ext>
    let base = "/opt/studiocommand/shared/carts";
    let exts = ["flac", "wav", "mp3", "m4a", "aac", "ogg", "opus"]; // decode via ffmpeg
    for ext in exts {
        let p = format!("{base}/{cart}.{ext}");
        if Path::new(&p).exists() {
            return Some(p);
        }
    }

    None
}

async fn spawn_ffmpeg_decoder(input: &str) -> anyhow::Result<(tokio::process::Child, tokio::process::ChildStdout)> {
    let ffmpeg = std::env::var("STUDIOCOMMAND_FFMPEG").unwrap_or_else(|_| "ffmpeg".to_string());

    let mut cmd = Command::new(ffmpeg);
    cmd.arg("-hide_banner")
        .arg("-loglevel").arg("error")
        .arg("-i").arg(input)
        .arg("-f").arg("s16le")
        .arg("-ar").arg("44100")
        .arg("-ac").arg("2")
        .arg("pipe:1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());

    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("decoder stdout unavailable"))?;
    Ok((child, stdout))
}

fn make_silence_chunk(frames: usize) -> Vec<u8> {
    // s16le stereo = 2 bytes * 2 channels
    vec![0u8; frames * 2 * 2]
}

fn parse_dur_seconds(dur: &str) -> Option<u32> {
    let dur = dur.trim();
    let (m, s) = dur.split_once(':')?;
    let m: u32 = m.parse().ok()?;
    let s: u32 = s.parse().ok()?;
    Some(m * 60 + s)
}

fn normalize_queue_states(log: &mut Vec<LogItem>) {
    normalize_log_markers(log);
    if let Some(first) = log.get_mut(0) {
        first.state = "playing".into();
    }
    if let Some(second) = log.get_mut(1) {
        second.state = "next".into();
    }
    for i in 2..log.len() {
        log[i].state = "queued".into();
    }
}

fn title_from_path(p: &str) -> String {
    use std::path::Path;
    Path::new(p)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Unknown")
        .replace('_', " ")
}

fn scan_audio_files_recursive(dir: &str) -> anyhow::Result<Vec<String>> {
    use std::path::Path;
    let mut out = Vec::new();
    let allowed = ["flac", "wav", "mp3", "m4a", "aac", "ogg", "opus"]; // decoder-supported

    fn walk(path: &Path, allowed: &[&str], out: &mut Vec<String>) {
        if let Ok(rd) = std::fs::read_dir(path) {
            for ent in rd.flatten() {
                let p = ent.path();
                if p.is_dir() {
                    walk(&p, allowed, out);
                } else if p.is_file() {
                    if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
                        let ext_lc = ext.to_ascii_lowercase();
                        if allowed.iter().any(|a| *a == ext_lc) {
                            if let Some(s) = p.to_str() {
                                out.push(s.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    let root = Path::new(dir);
    if !root.exists() {
        anyhow::bail!("top-up dir does not exist: {dir}");
    }
    walk(root, &allowed, &mut out);
    Ok(out)
}

// Returns true if the queue was modified (items appended).
async fn topup_if_needed(log: &mut Vec<LogItem>, cfg: &TopUpConfig) -> bool {
    if !cfg.enabled {
        return false;
    }
    if cfg.dir.trim().is_empty() {
        return false;
    }

    if log.len() as u16 >= cfg.min_queue {
        return false;
    }

    let dir = cfg.dir.clone();
    let batch = cfg.batch as usize;
    let files_res = tokio::task::spawn_blocking(move || scan_audio_files_recursive(&dir)).await;
    let files = match files_res {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            tracing::warn!("top-up scan failed: {e}");
            return false;
        }
        Err(e) => {
            tracing::warn!("top-up scan join failed: {e}");
            return false;
        }
    };

    if files.is_empty() {
        tracing::warn!("top-up dir had no audio files: {}", cfg.dir);
        return false;
    }

    // Pick random unique files.
    let mut picked = std::collections::HashSet::<usize>::new();
    let mut tries = 0usize;
    while picked.len() < batch && tries < batch * 20 {
        let i = fastrand::usize(..files.len());
        picked.insert(i);
        tries += 1;
    }

    for i in &picked {
        let path = &files[*i];
        log.push(LogItem {
            id: Uuid::new_v4(),
            tag: "MUS".into(),
            time: "".into(),
            title: title_from_path(path),
            artist: "TopUp".into(),
            state: "queued".into(),
            dur: "0:00".into(),
            cart: path.clone(), // absolute path
        });
    }

    normalize_queue_states(log);
    tracing::info!("top-up appended {} items from {}", picked.len(), cfg.dir);
    true
}

async fn writer_playout(
    mut stdin: tokio::process::ChildStdin,
    playout: Arc<tokio::sync::RwLock<PlayoutState>>,
    topup: Arc<tokio::sync::Mutex<TopUpConfig>>,
) -> anyhow::Result<()> {
    const SR: u32 = 44100;
    const FRAMES: usize = 1024;
    const BYTES_PER_FRAME: usize = 2 * 2; // s16le * stereo
    const CHUNK_BYTES: usize = FRAMES * BYTES_PER_FRAME;

    let silence = make_silence_chunk(FRAMES);
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(20));
    // Avoid hammering the filesystem when we're idling on silence.
    let mut last_topup_check = std::time::Instant::now() - std::time::Duration::from_secs(10);

    loop {
        // If output is running but the queue is empty/low, top-up must still run.
        // (In v0.1.42 it only ran after an end-of-track advance, so an empty queue
        // would idle on silence forever.)
        if last_topup_check.elapsed() >= std::time::Duration::from_secs(2) {
            last_topup_check = std::time::Instant::now();

            let cfg = topup.lock().await.clone();
            let mut snapshot_to_persist: Option<Vec<LogItem>> = None;
            {
                let mut p = playout.write().await;
                if topup_if_needed(&mut p.log, &cfg).await {
                    snapshot_to_persist = Some(p.log.clone());
                }
            }
            if let Some(log) = snapshot_to_persist {
                persist_queue(log).await;
            }
        }

        // Determine current track (log[0]) and resolve its path.
        let (id, title, artist, dur_s, path_opt) = {
            let mut p = playout.write().await;

            if p.log.is_empty() {
                // Nothing to play.
                (Uuid::nil(), "".into(), "".into(), 0u32, None)
            } else {
                normalize_queue_states(&mut p.log);

                let (first_id, title, artist, dur_s, cart) = {
                    let first = &p.log[0];
                    (
                        first.id,
                        first.title.clone(),
                        first.artist.clone(),
                        parse_dur_seconds(&first.dur).unwrap_or(0),
                        first.cart.clone(),
                    )
                };

                let path_opt = resolve_cart_to_path(&cart)
                    .or_else(|| if cart.starts_with('/') { Some(cart.clone()) } else { None });

                // Update now-playing.
                p.now.title = title.clone();
                p.now.artist = artist.clone();
                p.now.dur = dur_s;
                p.now.pos = 0;

                (first_id, title, artist, dur_s, path_opt)
            }
        };

        // If we don't have a playable path, write silence and retry.
        let Some(path) = path_opt else {
            interval.tick().await;
            stdin.write_all(&silence).await?;
            continue;
        };

        tracing::info!("playout start: {} - {} ({})", artist, title, path);

        // Start decoder and stream PCM to encoder stdin.
        let (_child, mut dec_stdout) = match spawn_ffmpeg_decoder(&path).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("decoder spawn failed for {path}: {e}");
                interval.tick().await;
                stdin.write_all(&silence).await?;
                continue;
            }
        };

        let mut buf = vec![0u8; CHUNK_BYTES];
        let mut frames_played: u64 = 0;

        loop {
            let n = dec_stdout.read(&mut buf).await?;
            if n == 0 {
                break;
            }

            interval.tick().await;
            stdin.write_all(&buf[..n]).await?;

            frames_played += (n as u64) / (BYTES_PER_FRAME as u64);
            let pos_s = (frames_played / (SR as u64)) as u32;

            // Update pos occasionally (cheap).
            if frames_played % (SR as u64) < (FRAMES as u64) {
                let mut p = playout.write().await;
                p.now.pos = pos_s;
            }
        }

        tracing::info!("playout end: {} - {}", artist, title);

        // Advance the queue if the currently playing id still matches log[0].
        let mut snapshot_to_persist: Option<Vec<LogItem>> = None;
        {
            let mut p = playout.write().await;
            if !p.log.is_empty() && p.log[0].id == id {
                p.log.remove(0);
                normalize_queue_states(&mut p.log);

                if let Some(first) = p.log.get(0) {
                    let (t, a, d) = (
                        first.title.clone(),
                        first.artist.clone(),
                        parse_dur_seconds(&first.dur).unwrap_or(0),
                    );
                    p.now.title = t;
                    p.now.artist = a;
                    p.now.dur = d;
                    p.now.pos = 0;
                } else {
                    p.now.title.clear();
                    p.now.artist.clear();
                    p.now.dur = 0;
                    p.now.pos = 0;
                }

                // Top-up if configured and queue is getting low.
                let cfg = topup.lock().await.clone();
                let _ = topup_if_needed(&mut p.log, &cfg).await;

                snapshot_to_persist = Some(p.log.clone());
            }
        }
        if let Some(log) = snapshot_to_persist {
            persist_queue(log).await;
        }

        // If the queue is empty after advancing, continue producing silence.
    }
}