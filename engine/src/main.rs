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
    topup_stats: Arc<tokio::sync::Mutex<TopUpStats>>,
    output: Arc<tokio::sync::Mutex<OutputRuntime>>,

    // Broadcast of real-time PCM chunks (s16le stereo @ 48 kHz).
    //
    // This is the *single source of truth* for:
    //   - Icecast encoding (ffmpeg stdin)
    //   - UI meters/progress (derived from PCM)
    //   - WebRTC \"Listen Live\" monitor (Opus)
    //
    // We keep it as a broadcast channel so multiple WebRTC listeners can
    // subscribe without changing the core audio pipeline.
    pcm_tx: tokio::sync::broadcast::Sender<Vec<u8>>,

    // Active WebRTC "Listen Live" session (if any).
    //
    // We intentionally keep *at most one* active session for now because this
    // feature is primarily a low-latency *operator monitor* rather than a
    // public listener endpoint. This also keeps the signaling simple: the UI
    // can POST ICE candidates to `/api/v1/webrtc/candidate` without needing a
    // session id.
    //
    // If/when you want multiple concurrent listeners, we can evolve this into
    // a map keyed by a session UUID returned from the `/offer` response.
    webrtc: Arc<tokio::sync::Mutex<Option<WebRtcRuntime>>>,
}



// --- WebRTC "Listen Live" ---------------------------------------------------
//
// The UI uses a minimal HTTP signaling flow:
//   1) POST /api/v1/webrtc/offer      (send SDP offer, receive SDP answer)
//   2) POST /api/v1/webrtc/candidate  (send browser ICE candidates)
//
// Why we need the /candidate endpoint:
//   WebRTC ICE negotiation is bi-directional. Even if the server includes its
//   own host/srflx candidates in the SDP answer, the server still needs the
//   browser's candidates (from `RTCPeerConnection.onicecandidate`) to
//   establish a working ICE pair. Without those, ICE tends to get stuck at
//   `checking` and the browser eventually tears the connection down.
//
// For now, StudioCommand supports a single active listen-live session at a
// time (operator monitor). This keeps signaling dead-simple and avoids
// accumulating idle peer connections on a small box.
//
// Future: multi-listener can be implemented by storing sessions in a HashMap
// keyed by a UUID returned from `/offer`.
struct WebRtcRuntime {
    /// The active WebRTC PeerConnection for the operator "Listen Live" monitor.
    ///
    /// The `webrtc` crate exposes this type at `webrtc::peer_connection::RTCPeerConnection`.
    /// (Earlier iterations accidentally referenced a non-existent nested module
    /// path: `peer_connection::peer_connection::RTCPeerConnection`.)
    pc: std::sync::Arc<webrtc::peer_connection::RTCPeerConnection>,
    stopped: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Clone, Deserialize)]
struct WebRtcCandidate {
    // The browser sends an `RTCIceCandidate` which is compatible with
    // `RTCIceCandidateInit` (candidate string + mid/mline_index).
    candidate: webrtc::ice_transport::ice_candidate::RTCIceCandidateInit,
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

/// Runtime visibility for top-up.
///
/// Top-up is an automation feature and when it fails (missing directory,
/// permission issues, unsupported formats, empty folder, etc.) it can leave the
/// playout queue empty with no obvious UI indication.
///
/// We keep small, operator-friendly telemetry so we can surface it via API and
/// (later) the UI.
#[derive(Clone, Serialize, Default)]
struct TopUpStats {
    /// Unix millis of the last scan attempt.
    last_scan_ms: Option<u64>,
    /// The directory that was scanned (may be a fallback).
    last_dir: Option<String>,
    /// How many candidate audio files were discovered.
    last_files_found: Option<u32>,
    /// How many items were appended.
    last_appended: Option<u32>,
    /// Human-friendly last error string.
    last_error: Option<String>,

    /// If the last periodic tick *did not* scan because the queue was already
    /// at/above `min_queue`, we record a short reason here.
    ///
    /// Why this exists:
    /// We continuously publish top-up telemetry so operators can see whether
    /// the automation is healthy. If we overwrite `last_files_found` with 0
    /// every time we *skip* scanning (because the queue is already full), it
    /// looks like top-up is broken even when it previously appended items.
    last_skip_reason: Option<String>,
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

    match res {
        Ok(Ok(Some(mut log))) => {
            // In earlier versions we padded the queue with "Queued Track N" demo
            // items to keep the UI busy. Operators asked that we stop doing
            // this: an empty queue should remain empty.
            //
            // One more safety net: some installs may still have those old demo
            // rows persisted in SQLite. If they remain, they can block Top-Up
            // from refilling the real queue (because they count toward
            // `min_queue`). We strip them on load so the station always prefers
            // real audio.
            log.retain(|it| {
                let is_demo_title = it.title.starts_with("Queued Track");
                let is_demo_artist = it.artist == "Various";
                let has_no_path = it.cart.trim().is_empty();
                !(is_demo_title && is_demo_artist) && !has_no_path
            });
            normalize_log_markers(&mut log);
            log
        }
        Ok(Ok(None)) => Vec::new(),
        Ok(Err(e)) => {
            tracing::warn!("failed to load queue from sqlite, starting with empty queue: {e}");
            Vec::new()
        }
        Err(e) => {
            tracing::warn!("failed to join sqlite load task, starting with empty queue: {e}");
            Vec::new()
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
    // Default behavior: keep the station playing without requiring manual
    // DB configuration on first install. The installer creates
    // /opt/studiocommand/shared/data for persistent audio content.
    // If you prefer a fully manual queue, set top_up_config.enabled = false
    // via the API (or by inserting the row in SQLite).
    TopUpConfig { enabled: true, dir: "/opt/studiocommand/shared/data".into(), min_queue: 5, batch: 5 }
}

/// Returns true if the stored top-up config looks like an *uninitialized* legacy row.
///
/// Why this exists:
/// - Older StudioCommand versions created a `top_up_config` row with placeholder values
///   (e.g., `enabled = 0`, empty dir, or zeros for min_queue/batch).
/// - Newer versions default to a sensible, "keep the station playing" setup by
///   topping up from `/opt/studiocommand/shared/data`.
///
/// If we always trust the presence of the row, a legacy placeholder would "win" and
/// the engine would idle on silence forever even though audio exists.
fn topup_config_needs_migration(cfg: &TopUpConfig) -> bool {
    cfg.dir.trim().is_empty() || cfg.min_queue == 0 || cfg.batch == 0
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
        Ok(Ok(cfg)) => {
            // If a legacy install already has a `top_up_config` row, it may contain
            // placeholder values that effectively disable top-up forever.
            //
            // We treat that specific shape as "uninitialized" and migrate it to
            // the new, safe defaults (shared data folder).
            if topup_config_needs_migration(&cfg) {
                let migrated = default_topup_config();

                // Log before we move/clone any values so we never accidentally
                // keep a legacy install silent.
                tracing::warn!(
                    "top-up config looked uninitialized; migrated to defaults (dir={})",
                    migrated.dir
                );

                // We'll persist in the background, but we must not move `migrated`
                // into the closure because we still return it below.
                let migrated_for_save = migrated.clone();

                // Best-effort persist; if this fails we still return the migrated
                // config for this run so the station plays.
                let path = db_path();
                let _ = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                    let mut conn = Connection::open(path)?;
                    db_save_topup_config(&mut conn, &migrated_for_save)?;
                    Ok(())
                })
                .await;
                migrated
            } else {
                cfg
            }
        }
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
    dur: u32,   // seconds
    pos: u32,   // whole seconds (legacy/compat)
    pos_f: f64, // seconds with fractions (for smooth UI)
}

#[derive(Clone, Serialize, Default)]
struct VuLevels {
    rms_l: f32,
    rms_r: f32,
    peak_l: f32,
    peak_r: f32,
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

    // Internal timing/meters derived from the real PCM stream.
    track_started_at: Option<std::time::Instant>,
    vu: VuLevels,
}

#[derive(Serialize)]
struct StatusResponse {
    version: String,
    now: NowPlaying,
    vu: VuLevels,
    /// Back-compat alias for the UI.
    ///
    /// The UI historically used `queue` while the engine used `log`.
    /// Some UI builds treat a missing `queue` as a fatal parse error and
    /// fall back to DEMO mode.
    ///
    /// We now serve both fields, pointing to the same underlying vector.
    queue: Vec<LogItem>,
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
    now: NowPlaying { title: "Neutron Dance".into(), artist: "Pointer Sisters".into(), dur: 242, pos: 0, pos_f: 0.0 },
    // Load the queue from SQLite if present; otherwise fall back to a demo queue.
    log: log.clone(),
    producers: demo_producers(),
    track_started_at: None,
    vu: VuLevels::default(),
};

    // WebRTC Listen Live needs access to the real PCM stream.
    // We expose it internally as a broadcast channel so each peer can subscribe.
    let (pcm_tx, _pcm_rx) = tokio::sync::broadcast::channel::<Vec<u8>>(64);

let state = AppState {
    version: version.clone(),
    sys: Arc::new(tokio::sync::Mutex::new(sys)),
    playout: Arc::new(tokio::sync::RwLock::new(playout)),
    topup: Arc::new(tokio::sync::Mutex::new(topup_cfg)),
    topup_stats: Arc::new(tokio::sync::Mutex::new(TopUpStats::default())),
    output: Arc::new(tokio::sync::Mutex::new(OutputRuntime::new(output_cfg))),
    pcm_tx,
    webrtc: Arc::new(tokio::sync::Mutex::new(None)),
};

// Optional: auto-start streaming output if config says enabled.
// (If ffmpeg isn't installed or creds are wrong, status will surface the error.)
{
    let out = state.output.clone();
    let pl = state.playout.clone();
    let tu = state.topup.clone();
			let pcm_tx = state.pcm_tx.clone();
			let tu_stats = state.topup_stats.clone();
    let enabled = out.lock().await.config.enabled;
    if enabled {
        tokio::spawn(async move {
				let _ = output_start_internal(out, pl, tu, tu_stats, pcm_tx).await;
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
        .route("/api/v1/webrtc/offer", post(api_webrtc_offer))
        .route("/api/v1/webrtc/candidate", post(api_webrtc_candidate))
        .route("/api/v1/queue/move", post(api_queue_move))
        .route("/api/v1/queue/reorder", post(api_queue_reorder))
        .route("/api/v1/queue/insert", post(api_queue_insert))
        .route("/", get(root))
        .route("/health", get(|| async { "OK" }))
        .route("/api/v1/status", get(status))
        // Lightweight endpoint for high-rate meter polling.
        .route("/api/v1/meters", get(meters))
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
        p.now.pos_f = p.now.pos as f64;

        // When the current item finishes, drop it from the log and promote the next item.
        //
        // NOTE: This stub engine mutates the queue over time (removing the playing
        // item and padding demo items). To keep SQLite persistence intuitive during
        // development/testing, we also persist the updated queue whenever the
        // "track ends" event occurs.
        // Update playing position from monotonic clock.
        if let Some(started) = p.track_started_at {
            let mut pos_f = started.elapsed().as_secs_f64();
            if p.now.dur > 0 {
                pos_f = pos_f.min(p.now.dur as f64);
            }
            p.now.pos_f = pos_f;
            p.now.pos = pos_f.floor() as u32;
        }

        if p.now.pos >= p.now.dur {
            p.now.pos = 0;
    p.now.pos_f = 0.0;
    p.track_started_at = Some(std::time::Instant::now());
    p.vu = VuLevels::default();

            if !p.log.is_empty() {
                // Remove the playing item (top of log).
                p.log.remove(0);
            }

            // Promote new playing item from top of log.
            // Anchor timing for UI/progress and any dur-based logic.
            p.track_started_at = Some(std::time::Instant::now());
            p.vu = VuLevels::default();
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

            // Earlier versions padded the queue with demo tracks ("Queued Track N").
            // That behavior was convenient for UI screenshots, but surprising in
            // production. We now leave the queue exactly as the operator/scheduler
            // set it.

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

    // now.pos/now.pos_f are maintained in the playout loop using a monotonic clock.
    let now = p.now.clone();

    Json(StatusResponse {
        version: state.version.clone(),
        now,
        vu: p.vu.clone(),
        // Back-compat: serve both `queue` and `log`.
        queue: p.log.clone(),
        log: p.log.clone(),
        producers: p.producers.clone(),
        system,
    })
}

// High-rate meter polling endpoint. Keep it tiny so it stays responsive even
// over higher-latency connections.
async fn meters(State(state): State<AppState>) -> Json<VuLevels> {
    let p = state.playout.read().await;
    Json(p.vu.clone())
}


// --- WebRTC "Listen Live" monitor ---------------------------------------
//
// This implements a simple single-endpoint signaling flow:
//   Browser:  POST /api/v1/webrtc/offer  { sdp, type:"offer" }
//   Engine :  200 OK                    { sdp, type:"answer" }
//
// The media source is the same PCM pipeline used for Icecast + meters.
// We encode Opus frames in-process and publish them via a single WebRTC
// peer connection per listener.
//
// Design notes:
// - We *do not* create a new audio source per listener. Instead, we tap the
//   existing PCM broadcast channel (`AppState.pcm_tx`) and encode Opus for
//   each listener independently. (If CPU becomes a concern, we can evolve to a
//   single shared Opus encoder + RTP fan-out later.)
// - We standardize internal PCM to 48 kHz stereo so we can feed Opus/WebRTC
//   without resampling.
//
// Browser support: all modern browsers support Opus in WebRTC.
// Docs: https://docs.rs/webrtc (crate webrtc, WebRTC.rs stack).
//
// Security: this endpoint is intended for same-origin use behind your existing
// TLS terminator (Caddy/Nginx). If you expose it publicly, treat it like any
// other authenticated monitor endpoint.

#[derive(Debug, Clone, Deserialize)]
struct WebRtcOffer {
    sdp: String,
    #[serde(rename = "type")]
    r#type: String,
}

#[derive(Debug, Clone, Serialize)]
struct WebRtcAnswer {
    sdp: String,
    #[serde(rename = "type")]
    r#type: String, // always "answer"
}

async fn api_webrtc_offer(
    State(state): State<AppState>,
    Json(offer): Json<WebRtcOffer>,
) -> Result<Json<WebRtcAnswer>, StatusCode> {
    use std::sync::atomic::{AtomicBool, Ordering};

    use bytes::Bytes;
    use opus::{Application as OpusApplication, Channels as OpusChannels, Encoder as OpusEncoder};
    use webrtc::api::APIBuilder;
    use webrtc::api::media_engine::MediaEngine;
    use webrtc::api::interceptor_registry::register_default_interceptors;
    use webrtc::ice_transport::ice_server::RTCIceServer;
    use webrtc::peer_connection::configuration::RTCConfiguration;
    use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
    use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
    use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
    use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
    use webrtc::media::Sample;
    use webrtc::data_channel::data_channel_init::RTCDataChannelInit;

    // Basic validation: browsers send {type:"offer"}.
    if offer.r#type.to_lowercase() != "offer" {
        tracing::warn!("webrtc offer rejected: type was {}", offer.r#type);
        return Err(StatusCode::BAD_REQUEST);
    }

    // --- Build WebRTC API stack (codecs + interceptors) -------------------
    //
    // MediaEngine: codec registry (Opus etc).
    // Interceptors: RTCP, NACK, TWCC, etc. Default set is fine for audio-only.
    let mut m = MediaEngine::default();
    m.register_default_codecs()
        .map_err(|e| {
            tracing::warn!("webrtc: register_default_codecs failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let mut registry = webrtc::interceptor::registry::Registry::new();

    // NOTE: In webrtc-rs, `register_default_interceptors(...)` is *synchronous* and returns
    // `Result<Registry, webrtc::Error>`.
    //
    // Earlier drafts of this feature assumed an async API and incorrectly used `.await`.
    // That fails to compile with:
    //   "Result<...> is not a future"
    //
    // Keeping this explicit (and documented) helps future upgrades if the upstream API changes.
    registry = register_default_interceptors(registry, &mut m).map_err(|e| {
        tracing::warn!("webrtc: register_default_interceptors failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_interceptor_registry(registry)
        .build();

    // ICE servers: default to Google's public STUN unless overridden.
    // This matters if you ever want to listen from outside the LAN.
    let stun = std::env::var("STUDIOCOMMAND_WEBRTC_STUN")
        .unwrap_or_else(|_| "stun:stun.l.google.com:19302".to_string());

    let config = RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls: vec![stun],
            ..Default::default()
        }],
        ..Default::default()
    };

    let pc = std::sync::Arc::new(api.new_peer_connection(config).await.map_err(|e| {
        tracing::warn!("webrtc: new_peer_connection failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?);
    // A shared stop flag used by background tasks (silence keepalive, PCM pump).
    let stopped = std::sync::Arc::new(AtomicBool::new(false));

    // Replace any existing session (if the operator clicks Start repeatedly).
    //
    // We proactively stop the previous PeerConnection to avoid leaving idle
    // DTLS/SRTP tasks running on small machines.
    {
        let mut guard = state.webrtc.lock().await;
        if let Some(prev) = guard.take() {
            prev.stopped.store(true, Ordering::SeqCst);
            // Close is best-effort; we don't fail the new session if it errors.
            if let Err(e) = prev.pc.close().await {
                tracing::warn!("webrtc: closing previous PeerConnection failed: {e}");
            }
        }

        *guard = Some(WebRtcRuntime {
            pc: pc.clone(),
            stopped: stopped.clone(),
        });
    }



    // Track: Opus audio.
    let track = std::sync::Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: "audio/opus".to_string(),
            clock_rate: 48_000,
            channels: 2,
            sdp_fmtp_line: "minptime=10;useinbandfec=1".to_string(),
            rtcp_feedback: vec![],
        },
        "audio".to_string(),
        "studiocommand".to_string(),
    ));

    pc.add_track(track.clone()).await.map_err(|e| {
        tracing::warn!("webrtc: add_track failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // ---------------------------------------------------------------------
    // WebRTC data channel: meter alignment with what you *hear*
    //
    // Problem:
    //   Once we added WebRTC audio monitoring, operators may notice that the
    //   on-screen VU meters lag slightly behind what they hear.
    //
    // Why:
    //   - Audio playout in the browser runs through a jitter buffer and audio
    //     output scheduling.
    //   - The existing meters are delivered over HTTP polling (/api/v1/meters)
    //     and intentionally apply smoothing/ballistics.
    //   - Those two clocks will never be perfectly phase-aligned.
    //
    // Fix:
    //   When "Listen Live" is active, we also send meter snapshots over a
    //   WebRTC *data channel* in the same PeerConnection.
    //
    //   This gives the UI a low-latency meter stream that shares the same
    //   transport timing and RTT dynamics as the audio you are monitoring.
    //
    // Notes:
    //   - This is purely an *operator experience* feature.
    //   - If the data channel fails for any reason, the UI will fall back to
    //     the existing HTTP polling path.
    // ---------------------------------------------------------------------
    let dc = pc
        .create_data_channel(
            "meters",
            Some(RTCDataChannelInit {
                // Ordered delivery is fine; these are tiny.
                ordered: Some(true),
                ..Default::default()
            }),
        )
        .await
        .map_err(|e| {
            tracing::warn!("webrtc: create_data_channel(meters) failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // Start a background meter sender when the channel opens.
    // We intentionally send at ~50 Hz (20 ms) to match the Opus frame cadence.
    {
        let playout = state.playout.clone();
        let stopped = stopped.clone();
        let dc_open = dc.clone();
        dc.on_open(Box::new(move || {
            let playout = playout.clone();
            let stopped = stopped.clone();
            let dc = dc_open.clone();
            Box::pin(async move {
                tracing::info!("webrtc: meters data channel open");
                tokio::spawn(async move {
                    use std::time::{Duration, Instant};
                    let t0 = Instant::now();
                    loop {
                        if stopped.load(Ordering::SeqCst) {
                            break;
                        }

                        // Snapshot the current meter state.
                        // We keep this lock scope tiny to avoid blocking audio work.
                        let vu = {
                            let p = playout.read().await;
                            p.vu.clone()
                        };

                        // Include a monotonic timestamp so the UI can detect staleness.
                        let payload = json!({
                            "t_ms": t0.elapsed().as_millis() as u64,
                            "rms_l": vu.rms_l,
                            "rms_r": vu.rms_r,
                            "peak_l": vu.peak_l,
                            "peak_r": vu.peak_r,
                        })
                        .to_string();

                        // Best-effort send.
                        // If the peer disconnects, `stopped` will flip and we exit.
                        let _ = dc.send_text(payload).await;

                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                });
            })
        }));
    }

// ---------------------------------------------------------------------
// WebRTC "keepalive" audio packets (Opus silence)
//
// Symptom this fixes:
//   The browser shows "Connecting..." for a while and then returns to "Stopped"
//   without ever reaching "Connected".
//
// Cause:
//   Some browsers will tear down a PeerConnection if no RTP media arrives soon
//   after ICE/DTLS completes. This is especially easy to trigger in broadcast
//   scenarios where the "real" audio pipeline might take a moment to start,
//   or when the server has not yet received any PCM frames.
//
// Fix:
//   Immediately begin sending tiny 20 ms Opus packets that decode to silence.
//   As soon as the real PCM->Opus pump successfully writes its first packet,
//   it flips `audio_started` to true and this silence task exits.
//
// Notes:
//   - This is a common WebRTC broadcasting practice.
//   - CPU cost is negligible.
//   - It dramatically improves connection reliability and debuggability.
// ---------------------------------------------------------------------
let audio_started = std::sync::Arc::new(AtomicBool::new(false));
{
    let track_for_silence = track.clone();
    let stopped = stopped.clone();
    let audio_started = audio_started.clone();

    tokio::spawn(async move {
        use std::time::Duration;

        // A dedicated Opus encoder for the silence stream.
        // We encode 20 ms of all-zero PCM (stereo, 48 kHz).
        let mut enc = match OpusEncoder::new(48_000, OpusChannels::Stereo, OpusApplication::Audio) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("webrtc: failed to create Opus encoder for silence keepalive: {e}");
                return;
            }
        };

        // 20 ms @ 48 kHz => 960 samples/channel, stereo => 1920 samples total.
        const SILENCE_SAMPLES_TOTAL: usize = 960 * 2;
        let pcm_silence: Vec<i16> = vec![0; SILENCE_SAMPLES_TOTAL];

        // Opus packets are small; 4000 bytes is plenty for 20 ms.
        let mut out = vec![0u8; 4000];

        while !stopped.load(Ordering::SeqCst) && !audio_started.load(Ordering::SeqCst) {
            let n = match enc.encode(&pcm_silence, &mut out) {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!("webrtc: Opus silence encode failed: {e}");
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    continue;
                }
            };

            let sample = webrtc::media::Sample {
                data: Bytes::from(out[..n].to_vec()),
                duration: Duration::from_millis(20),
                ..Default::default()
            };

            // Ignore transient errors here; if the peer goes away, the state
            // callbacks will flip `stopped` and all tasks will exit naturally.
            let _ = track_for_silence.write_sample(&sample).await;

            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
}

    {
        let stopped = stopped.clone();
        pc.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            if matches!(
                s,
                RTCPeerConnectionState::Failed
                    | RTCPeerConnectionState::Closed
                    | RTCPeerConnectionState::Disconnected
            ) {
                stopped.store(true, Ordering::Relaxed);
            }
            Box::pin(async {})
        }));
    }

    // --- SDP handshake ----------------------------------------------------
    pc.set_remote_description(
        RTCSessionDescription::offer(offer.sdp)
            .map_err(|e| {
                tracing::warn!("webrtc: invalid offer SDP: {e}");
                StatusCode::BAD_REQUEST
            })?
    )
    .await
    .map_err(|e| {
        tracing::warn!("webrtc: set_remote_description failed: {e}");
        StatusCode::BAD_REQUEST
    })?;

    let answer = pc.create_answer(None).await.map_err(|e| {
        tracing::warn!("webrtc: create_answer failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    // IMPORTANT: We return a *non-trickle* SDP answer (all ICE candidates included in the SDP).
//
// In early WebRTC iterations we returned the SDP immediately after `set_local_description()`.
// That can produce an SDP answer with *zero* candidates in some environments, causing the browser to
// remain stuck in ICE state `new` (no remote candidates) and eventually give up.
//
// Full trickle ICE would require a candidate exchange endpoint and client-side event wiring.
// For StudioCommand’s "Listen Live" monitor, a simpler and robust approach is:
//   1) set the local description
//   2) wait *briefly* for ICE gathering to complete (bounded, so we never stall forever)
//   3) read the final local description (now containing candidates) and return it as the SDP answer
pc.set_local_description(answer).await.map_err(|e| {
    tracing::warn!("webrtc: set_local_description failed: {e}");
    StatusCode::INTERNAL_SERVER_ERROR
})?;

// Wait up to 2 seconds for ICE gathering to complete so the returned SDP includes candidates.
// If it times out, we still proceed (and the UI will show `new`/`checking`).
let mut gather_complete = pc.gathering_complete_promise().await;
let _ = tokio::time::timeout(std::time::Duration::from_secs(2), gather_complete.recv()).await;

    let local = pc.local_description().await.ok_or_else(|| {
        tracing::warn!("webrtc: local_description missing after set_local_description");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // --- Audio pump -------------------------------------------------------
    //
    // Subscribe to the PCM broadcast channel and encode 20 ms Opus packets.
    // PCM format: s16le stereo @ 48 kHz.
    // A 20 ms Opus frame = 960 samples per channel.
    let mut rx = state.pcm_tx.subscribe();
    let stopped_for_task = stopped.clone();
    let track_for_task = track.clone();

    tokio::spawn(async move {
        let audio_started = audio_started.clone();
        let mut wrote_first_packet = false;

        const SR: u32 = 48_000;
        const CHANNELS: usize = 2;
        const FRAME_SAMPLES_PER_CH: usize = 960; // 20 ms @ 48k
        const FRAME_SAMPLES_TOTAL: usize = FRAME_SAMPLES_PER_CH * CHANNELS;
        const FRAME_BYTES: usize = FRAME_SAMPLES_TOTAL * 2; // i16

        // Opus encoder: stereo, 48 kHz, general audio.
        let mut enc = match OpusEncoder::new(SR as u32, OpusChannels::Stereo, OpusApplication::Audio) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("webrtc: opus encoder init failed: {e}");
                return;
            }
        };

        // Buffer in case the PCM producer ever sends partial frames.
        let mut buf: Vec<u8> = Vec::with_capacity(FRAME_BYTES * 4);

        while !stopped_for_task.load(Ordering::Relaxed) {
            let chunk = match rx.recv().await {
                Ok(c) => c,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    // Listener fell behind; drop audio to catch up.
                    tracing::warn!("webrtc: pcm receiver lagged by {n} messages (dropping)");
                    continue;
                }
                Err(_) => break,
            };

            buf.extend_from_slice(&chunk);

            while buf.len() >= FRAME_BYTES {
                let frame = buf.drain(0..FRAME_BYTES).collect::<Vec<u8>>();

                // Convert bytes -> i16 samples.
                let mut samples: Vec<i16> = Vec::with_capacity(FRAME_SAMPLES_TOTAL);
                let mut i = 0usize;
                while i + 1 < frame.len() {
                    samples.push(i16::from_le_bytes([frame[i], frame[i + 1]]));
                    i += 2;
                }

                // Encode Opus.
                let mut out = vec![0u8; 4000];
                let n = match enc.encode(&samples, &mut out) {
                    Ok(n) => n,
                    Err(e) => {
                        tracing::warn!("webrtc: opus encode failed: {e}");
                        break;
                    }
                };
                out.truncate(n);

                // Ship as a media sample (WebRTC will packetize it as RTP).
                let sample = Sample {
                    data: Bytes::from(out),
                    duration: std::time::Duration::from_millis(20),
                    ..Default::default()
                };

                if let Err(e) = track_for_task.write_sample(&sample).await {
                    tracing::warn!("webrtc: write_sample failed (peer likely gone): {e}");
                    return;
                }
if !wrote_first_packet {
    wrote_first_packet = true;
    audio_started.store(true, Ordering::SeqCst);
    tracing::info!("webrtc: first audio packet sent (silence keepalive will stop)");
}
            }
        }
    });

    Ok(Json(WebRtcAnswer {
        sdp: local.sdp,
        r#type: "answer".to_string(),
    }))
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




/// Receive browser ICE candidates for the current WebRTC session.
///
/// WebRTC ICE negotiation is *bi-directional*: the server needs the browser's
/// candidates in order to find a valid candidate pair. Without this endpoint,
/// ICE commonly gets stuck at `checking` and the browser eventually closes the
/// connection (the UI reverts to "Stopped").
///
/// The UI calls this from `pc.onicecandidate` while a session is active.
///
/// For now there is only one active session at a time (operator monitor).
async fn api_webrtc_candidate(
    State(state): State<AppState>,
    Json(body): Json<WebRtcCandidate>,
) -> Result<StatusCode, StatusCode> {
    // Grab a snapshot of the current PeerConnection (if any) without holding
    // the mutex across an await on `add_ice_candidate`.
    let pc_opt = {
        let guard = state.webrtc.lock().await;
        guard.as_ref().map(|rt| rt.pc.clone())
    };

    let pc = match pc_opt {
        Some(pc) => pc,
        None => {
            // No active session. This can happen if the user hit Stop while
            // candidates were still trickling from the browser.
            return Err(StatusCode::CONFLICT);
        }
    };

    pc.add_ice_candidate(body.candidate).await.map_err(|e| {
        tracing::warn!("webrtc: add_ice_candidate failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(StatusCode::NO_CONTENT)
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
    output_start_internal(
        state.output.clone(),
        state.playout.clone(),
        state.topup.clone(),
        state.topup_stats.clone(),
        state.pcm_tx.clone(),
    ).await?;
    Ok(Json(json!({"ok": true})))
}

async fn api_output_stop(State(state): State<AppState>) -> Result<Json<serde_json::Value>, StatusCode> {
    output_stop_internal(state.output.clone()).await;
    Ok(Json(json!({"ok": true})))
}

async fn output_start_internal(
    output: Arc<tokio::sync::Mutex<OutputRuntime>>,
    playout: Arc<tokio::sync::RwLock<PlayoutState>>,
    topup: Arc<tokio::sync::Mutex<TopUpConfig>>,
    topup_stats: Arc<tokio::sync::Mutex<TopUpStats>>,
    pcm_tx: tokio::sync::broadcast::Sender<Vec<u8>>,
) -> Result<(), StatusCode> {
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
    let (child, stdin, stderr) = spawn_ffmpeg_icecast(&o.config).await.map_err(|e| {
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
        if let Err(e) = writer_playout(stdin, playout, topup, topup_stats, pcm_tx).await {
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
    cmd.arg("-ar").arg("48000");
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
        // Keep current position, but clamp only when duration is known.
        // If dur is 0 (unknown), do NOT reset pos; that makes the UI progress bar
        // creep forward and snap back to 0 every tick.
        if p.now.dur > 0 && p.now.pos > p.now.dur {
            p.now.pos = p.now.dur;
            p.now.pos_f = p.now.dur as f64;
        }
    }
}

fn reset_demo_playout(p: &mut PlayoutState) {
    // Keep this deterministic so the UI is predictable while we build real scheduling.
    p.now.title = "Lean On Me".into();
    p.now.artist = "Club Nouveau".into();
    p.now.dur = 3*60 + 48;
    p.now.pos = 0;
    p.now.pos_f = 0.0;
    p.track_started_at = Some(std::time::Instant::now());
    p.vu = VuLevels::default();

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
    p.now.pos_f = 0.0;
    p.track_started_at = Some(std::time::Instant::now());
    p.vu = VuLevels::default();
    } else {
        // Empty log: clear now
        p.now.title = "".into();
        p.now.artist = "".into();
        p.now.dur = 0;
        p.now.pos = 0;
    p.now.pos_f = 0.0;
    p.track_started_at = Some(std::time::Instant::now());
    p.vu = VuLevels::default();
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
    stats: TopUpStats,
}

async fn api_topup_get(State(state): State<AppState>) -> Json<TopUpGetResponse> {
    let cfg = state.topup.lock().await.clone();
    let stats = state.topup_stats.lock().await.clone();
    Json(TopUpGetResponse { config: cfg, stats })
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
        .arg("-ar").arg("48000")
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

fn clamp01_f32(x: f32) -> f32 { x.max(0.0).min(1.0) }

fn analyze_pcm_s16le_stereo(buf: &[u8]) -> VuLevels {
    // Interleaved stereo, little-endian i16.
    // Returns per-channel RMS and peak, normalized to [0,1].
    let mut sumsq_l: f64 = 0.0;
    let mut sumsq_r: f64 = 0.0;
    let mut peak_l: i32 = 0;
    let mut peak_r: i32 = 0;
    let mut nframes: u64 = 0;

    let mut i = 0usize;
    while i + 3 < buf.len() {
        let l = i16::from_le_bytes([buf[i], buf[i + 1]]) as i32;
        let r = i16::from_le_bytes([buf[i + 2], buf[i + 3]]) as i32;
        let al = l.abs();
        let ar = r.abs();
        if al > peak_l { peak_l = al; }
        if ar > peak_r { peak_r = ar; }
        sumsq_l += (l as f64) * (l as f64);
        sumsq_r += (r as f64) * (r as f64);
        nframes += 1;
        i += 4;
    }

    if nframes == 0 {
        return VuLevels::default();
    }

    let mean_l = sumsq_l / (nframes as f64);
    let mean_r = sumsq_r / (nframes as f64);

    let rms_l = (mean_l.sqrt() / 32768.0) as f32;
    let rms_r = (mean_r.sqrt() / 32768.0) as f32;
    let pk_l = (peak_l as f32) / 32768.0;
    let pk_r = (peak_r as f32) / 32768.0;

    VuLevels {
        rms_l: clamp01_f32(rms_l),
        rms_r: clamp01_f32(rms_r),
        peak_l: clamp01_f32(pk_l),
        peak_r: clamp01_f32(pk_r),
    }
}

fn smooth_level(current: f32, target: f32, attack: f32, release: f32) -> f32 {
    // attack/release are smoothing factors in (0,1]; higher = faster.
    if target >= current {
        current + (target - current) * attack
    } else {
        current + (target - current) * release
    }
}

fn parse_dur_seconds(dur: &str) -> Option<u32> {
    let dur = dur.trim();
    let (m, s) = dur.split_once(':')?;
    let m: u32 = m.parse().ok()?;
    let s: u32 = s.parse().ok()?;
    Some(m * 60 + s)
}

fn fmt_dur_mmss(total_s: u32) -> String {
    let m = total_s / 60;
    let s = total_s % 60;
    format!("{}:{:02}", m, s)
}

fn probe_duration_seconds(path: &str) -> Option<u32> {
    use std::process::Command;

    let ffprobe = std::env::var("STUDIOCOMMAND_FFPROBE")
        .unwrap_or_else(|_| "ffprobe".to_string());

    let out = Command::new(ffprobe)
        .arg("-v").arg("error")
        .arg("-show_entries").arg("format=duration")
        .arg("-of").arg("default=noprint_wrappers=1:nokey=1")
        .arg(path)
        .output()
        .ok()?;

    if !out.status.success() {
        return None;
    }

    let s = String::from_utf8_lossy(&out.stdout);
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    let secs_f: f64 = s.parse().ok()?;
    if !secs_f.is_finite() || secs_f <= 0.0 {
        return None;
    }

    Some(secs_f.round() as u32)
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

    // Decoder-supported file extensions.
    // Keep this list conservative — ffmpeg can decode more, but this is enough
    // for common station libraries.
    let allowed = ["flac", "wav", "mp3", "m4a", "aac", "ogg", "opus"];

    let root = Path::new(dir);
    if !root.exists() {
        anyhow::bail!("top-up dir does not exist: {dir}");
    }

    // IMPORTANT: do not silently ignore filesystem errors.
    // Earlier versions treated a failing `read_dir()` as "empty", which made
    // debugging impossible (e.g., permission denied / stale NAS mount).
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let rd = std::fs::read_dir(&path)
            .map_err(|e| anyhow::anyhow!("failed to read_dir({}): {e}", path.display()))?;
        for ent in rd {
            let ent = ent.map_err(|e| anyhow::anyhow!("failed to read_dir entry: {e}"))?;
            let p = ent.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if !p.is_file() {
                continue;
            }

            let Some(ext) = p.extension().and_then(|e| e.to_str()) else {
                continue;
            };
            let ext_lc = ext.to_ascii_lowercase();
            if !allowed.iter().any(|a| *a == ext_lc.as_str()) {
                continue;
            }

            // Paths on Linux are bytes; they are *usually* UTF-8, but not always.
            // `to_string_lossy()` lets us include non-UTF8 paths without crashing.
            out.push(p.to_string_lossy().to_string());
        }
    }

    Ok(out)
}

#[derive(Debug, Clone, Default)]
struct TopUpAttempt {
    /// True if we actually walked the filesystem to discover files.
    ///
    /// A periodic tick can also short-circuit early if the queue is already
    /// at/above `min_queue`. In that case we do *not* want to overwrite the
    /// last meaningful scan stats with zeros.
    scanned: bool,
    appended: u32,
    files_found: u32,
    error: Option<String>,

    /// If we didn't scan, record why.
    skip_reason: Option<String>,
}

/// Try to top-up a queue using the provided config.
///
/// This function never panics; it reports scan/probe errors via `error` so the
/// caller can decide whether to fallback to another directory.
async fn topup_try(log: &mut Vec<LogItem>, cfg: &TopUpConfig) -> TopUpAttempt {
    let mut out = TopUpAttempt::default();

    if !cfg.enabled {
        return out;
    }
    if cfg.dir.trim().is_empty() {
        out.error = Some("top-up dir is empty".into());
        return out;
    }
    // Only count *actually playable* items toward `min_queue`.
    //
    // Why this matters:
    // - Some UI modes keep played items visible, or older installs may still
    //   have placeholder/demo rows in SQLite.
    // - Those rows can make the queue look "full" even when there is nothing
    //   we can actually play, which would prevent Top-Up from refilling.
    //
    // We treat an item as "active" only if:
    // - it is not explicitly marked played, AND
    // - it has a non-empty `cart` path, AND
    // - that path exists on disk.
    let active_len = log
        .iter()
        .filter(|it| {
            it.state != "played"
                && !it.cart.trim().is_empty()
                && std::path::Path::new(it.cart.as_str()).exists()
        })
        .count() as u16;
    if active_len >= cfg.min_queue {
        out.skip_reason = Some(format!(
            "skipped: active queue {} >= min_queue {}",
            active_len, cfg.min_queue
        ));
        return out;
    }

    // From here onward we intend to actually scan.
    out.scanned = true;

    let dir = cfg.dir.clone();
    let batch = cfg.batch as usize;
    let files_res = tokio::task::spawn_blocking(move || scan_audio_files_recursive(&dir)).await;
    let files = match files_res {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            out.error = Some(format!("scan failed: {e}"));
            return out;
        }
        Err(e) => {
            out.error = Some(format!("scan join failed: {e}"));
            return out;
        }
    };

    out.files_found = files.len() as u32;
    if files.is_empty() {
        // Treat this as an operational error so the caller can fall back to a
        // known-good directory (e.g., /opt/studiocommand/shared/data) and so
        // operators can see what happened via /api/v1/playout/topup.
        out.error = Some("no eligible audio files found".into());
        return out;
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

        let dur_s = probe_duration_seconds(path).unwrap_or(0);
        let dur = if dur_s > 0 { fmt_dur_mmss(dur_s) } else { "0:00".into() };
        if dur_s == 0 {
            // Keep going, but record that probe was unhappy.
            out.error.get_or_insert_with(|| "ffprobe duration failed for one or more files".into());
        }

        log.push(LogItem {
            id: Uuid::new_v4(),
            tag: "MUS".into(),
            time: "".into(),
            title: title_from_path(path),
            artist: "TopUp".into(),
            state: "queued".into(),
            dur,
            cart: path.to_string(), // absolute path
        });
    }

    normalize_queue_states(log);
    out.appended = picked.len() as u32;
    out
}

async fn writer_playout(
    mut stdin: tokio::process::ChildStdin,
    playout: Arc<tokio::sync::RwLock<PlayoutState>>,
    topup: Arc<tokio::sync::Mutex<TopUpConfig>>,
    topup_stats: Arc<tokio::sync::Mutex<TopUpStats>>,
    pcm_tx: tokio::sync::broadcast::Sender<Vec<u8>>,
) -> anyhow::Result<()> {
    const SR: u32 = 48_000;
    // 20 ms @ 48 kHz = 960 frames. Keeping the chunk size aligned to 20 ms makes
    // WebRTC/Opus framing straightforward and keeps pacing accurate.
    const FRAMES: usize = 960;
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

            // Top-up config is persisted in SQLite and may point at external
            // storage (e.g., a NAS mount). If that mount disappears, the engine
            // would otherwise sit on silence forever.
            //
            // We treat a missing configured directory as a *runtime health* issue
            // and automatically fall back to the built-in shared data path
            // created by the installer.
            //
            // This keeps "it plays" behavior reliable while still allowing
            // operators to intentionally point top-up elsewhere.
            let mut cfg_guard = topup.lock().await;
            let cfg_default = default_topup_config();
            if cfg_guard.enabled {
                let configured = cfg_guard.dir.clone();
                let configured_exists = std::path::Path::new(&configured).exists();
                if !configured_exists {
                    let fallback = cfg_default.dir.clone();
                    if configured != fallback && std::path::Path::new(&fallback).exists() {
                        tracing::warn!(
                            "top-up dir missing ({}); falling back to {}",
                            configured,
                            fallback
                        );

                        // Adopt the fallback for this run (and persist best-effort).
                        cfg_guard.dir = fallback;

                        // If a legacy row had min/batch=0, fix that too.
                        if cfg_guard.min_queue == 0 {
                            cfg_guard.min_queue = cfg_default.min_queue;
                        }
                        if cfg_guard.batch == 0 {
                            cfg_guard.batch = cfg_default.batch;
                        }

                        let cfg_to_save = cfg_guard.clone();
                        let _ = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                            let mut conn = Connection::open(db_path())?;
                            db_save_topup_config(&mut conn, &cfg_to_save)?;
                            Ok(())
                        })
                        .await;
                    }
                }
            }

            let cfg = cfg_guard.clone();
            let mut used_dir = cfg.dir.clone();
            drop(cfg_guard);

            // Attempt a normal scan.
            let mut snapshot_to_persist: Option<Vec<LogItem>> = None;
            let mut attempt = TopUpAttempt::default();
            {
                let mut p = playout.write().await;
                attempt = topup_try(&mut p.log, &cfg).await;
                if attempt.appended > 0 {
                    snapshot_to_persist = Some(p.log.clone());
                }
            }

            // If the configured directory exists but is empty (or scan/probe
            // fails), automatically try the installer-managed shared data path.
            //
            // This is the common "it plays" expectation on fresh installs.
            if cfg.enabled && attempt.appended == 0 {
                let fallback = default_topup_config().dir;
                let should_try_fallback = (attempt.files_found == 0) || attempt.error.is_some();
                if should_try_fallback && cfg.dir != fallback && std::path::Path::new(&fallback).exists() {
                    let mut cfg2 = cfg.clone();
                    cfg2.dir = fallback.clone();

                    let mut attempt2 = TopUpAttempt::default();
                    {
                        let mut p = playout.write().await;
                        attempt2 = topup_try(&mut p.log, &cfg2).await;
                        if attempt2.appended > 0 {
                            snapshot_to_persist = Some(p.log.clone());
                        }
                    }

                    if attempt2.appended > 0 {
                        tracing::warn!(
                            "top-up from configured dir produced no items; falling back to {}",
                            fallback
                        );

                        // Adopt the fallback for subsequent runs and persist best-effort.
                        let mut cfg_guard = topup.lock().await;
                        cfg_guard.dir = fallback.clone();
                        let cfg_to_save = cfg_guard.clone();
                        drop(cfg_guard);
                        let _ = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                            let mut conn = Connection::open(db_path())?;
                            db_save_topup_config(&mut conn, &cfg_to_save)?;
                            Ok(())
                        }).await;

                        attempt = attempt2;
                        used_dir = fallback;
                    }
                }
            }

            // Publish top-up telemetry.
            {
                let mut s = topup_stats.lock().await;
                // Only overwrite scan results if we actually scanned.
                // Otherwise a healthy system (queue full) would constantly
                // clobber the last meaningful stats with zeros.
                if attempt.scanned {
                    s.last_scan_ms = Some(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64,
                    );
                    s.last_dir = Some(used_dir.clone());
                    s.last_files_found = Some(attempt.files_found);
                    s.last_appended = Some(attempt.appended);
                    s.last_error = attempt.error.clone();
                    s.last_skip_reason = None;
                } else {
                    s.last_skip_reason = attempt.skip_reason.clone();
                }
            }

            if let Some(log) = snapshot_to_persist {
                persist_queue(log).await;
            }
        }

        // Determine current track (log[0]) and resolve its path.
        let (id, title, artist, _dur_s, path_opt) = {
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

                // Update now-playing (anchor timing + reset meters/progress).
p.now.title = title.clone();
p.now.artist = artist.clone();
p.now.dur = dur_s;
p.now.pos = 0;
p.now.pos_f = 0.0;
p.track_started_at = Some(std::time::Instant::now());
p.vu = VuLevels::default();

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
        // IMPORTANT: we keep the Child handle so we can kill the decoder early
        // on operator actions like "skip" or "dump".
        let (mut child, mut dec_stdout) = match spawn_ffmpeg_decoder(&path).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("decoder spawn failed for {path}: {e}");
                interval.tick().await;
                stdin.write_all(&silence).await?;
                continue;
            }
        };

let mut buf = vec![0u8; CHUNK_BYTES];

// Progress derived from actual PCM that we successfully feed to the encoder.
// For s16le stereo, each frame is 4 bytes (2 bytes per channel).
let mut frames_written: u64 = 0;

// Meter + position updates (keep lock cadence modest).
let mut last_update = std::time::Instant::now() - std::time::Duration::from_secs(1);

// If an operator advances the queue while we're mid-track (Skip/Dump), we must
// stop emitting this track immediately. Otherwise the UI will jump to the next
// item while the previous track continues to play until EOF.
let mut interrupted = false;

loop {
    // Check for operator-driven queue advance.
    // We do this on every chunk (20ms) which is cheap and keeps stop latency low.
    {
        let p = playout.read().await;
        if p.log.is_empty() || p.log[0].id != id {
            interrupted = true;
        }
    }
    if interrupted {
        tracing::info!("playout interrupted (skip/dump): {} - {}", artist, title);
        break;
    }

    let n = dec_stdout.read(&mut buf).await?;
    if n == 0 {
        break;
    }

    // Analyze *before* writing so we can update meters even if the encoder blocks briefly.
    let inst = analyze_pcm_s16le_stereo(&buf[..n]);

    // Fan out the raw PCM to any WebRTC listeners.
    // If there are no receivers, broadcast::Sender::send returns an error; that's fine.
    let _ = pcm_tx.send(buf[..n].to_vec());


    // Pace writes to match real-time.
    interval.tick().await;
    stdin.write_all(&buf[..n]).await?;

    // Count frames actually delivered to the encoder.
    frames_written += (n / BYTES_PER_FRAME) as u64;

    // Update meters + position at ~30 Hz.
    if last_update.elapsed() >= std::time::Duration::from_millis(33) {
        last_update = std::time::Instant::now();

        let pos_f = frames_written as f64 / SR as f64;

        let mut p = playout.write().await;

        // Position (seconds). Clamp only when we have a known duration.
        p.now.pos_f = if p.now.dur > 0 {
            pos_f.min(p.now.dur as f64)
        } else {
            pos_f
        };
        p.now.pos = p.now.pos_f.floor() as u32;

        // Faster ballistics: snappy attack, moderate decay.
        p.vu.rms_l = smooth_level(p.vu.rms_l, inst.rms_l, 0.95, 0.55);
        p.vu.rms_r = smooth_level(p.vu.rms_r, inst.rms_r, 0.95, 0.55);
        p.vu.peak_l = smooth_level(p.vu.peak_l, inst.peak_l, 1.00, 0.65);
        p.vu.peak_r = smooth_level(p.vu.peak_r, inst.peak_r, 1.00, 0.65);
    }
}

        // If we broke out because the operator advanced the queue, kill ffmpeg
        // so the audio actually stops. Otherwise the child would keep decoding
        // in the background until it reaches EOF.
        if interrupted {
            let _ = child.kill().await;
            let _ = child.wait().await;
            tracing::info!("playout stop: {} - {}", artist, title);
        } else {
            tracing::info!("playout end: {} - {}", artist, title);
        }

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
                    p.now.pos_f = 0.0;
                    p.track_started_at = Some(std::time::Instant::now());
                    p.vu = VuLevels::default();
                } else {
                    p.now.title.clear();
                    p.now.artist.clear();
                    p.now.dur = 0;
                    p.now.pos = 0;
                    p.now.pos_f = 0.0;
                    p.track_started_at = None;
                    p.vu = VuLevels::default();
                }

                // Top-up if configured and queue is getting low.
                let cfg = topup.lock().await.clone();
                let attempt = topup_try(&mut p.log, &cfg).await;
                {
                    let mut s = topup_stats.lock().await;
                    s.last_scan_ms = Some(std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64);
                    s.last_dir = Some(cfg.dir.clone());
                    s.last_files_found = Some(attempt.files_found);
                    s.last_appended = Some(attempt.appended);
                    s.last_error = attempt.error;
                }

                snapshot_to_persist = Some(p.log.clone());
            }
        }
        if let Some(log) = snapshot_to_persist {
            persist_queue(log).await;
        }

        // If the queue is empty after advancing, continue producing silence.
    }
}
