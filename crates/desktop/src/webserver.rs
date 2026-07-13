// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! HTTP + WebSocket server (axum). Serves the embedded web client and a
//! WebSocket that hands each browser the stream config, answers NTP-style
//! clock-sync requests against the server's monotonic clock, and forwards the
//! audio/video frames published on the broadcast channels. The browser buffers,
//! syncs, and decodes — so every client plays in lock-step.
//!
//! The active stream is delivered via a `watch` channel so the GUI can swap the
//! capture source (or toggle video) live: it publishes a new [`StreamState`], the
//! old `Media`'s channels close, connected browsers see the close and reconnect,
//! and pick up the new stream automatically.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use axum_server::tls_rustls::RustlsConfig;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc, watch};

use newfoundsync_core::config::mono_now;

use crate::media::{Frame, Media};

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const SERVICE_WORKER: &str = include_str!("../web/sw.js");
const MANIFEST: &str = include_str!("../web/manifest.webmanifest");

/// A content build tag — FNV-1a hash of the served shell (app.js + index.html + sw.js + manifest).
/// It changes whenever ANY of those change, so a browser running a STALE (service-worker-cached)
/// shell can detect the mismatch against `/version` and self-heal (drop the SW + caches, reload to
/// fresh code). sw.js and the manifest are folded in so a change touching only one of them still
/// rotates the tag/bucket and triggers the client's `<head>` self-heal. Computed once.
fn build_tag() -> &'static str {
    static TAG: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TAG.get_or_init(|| {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a offset basis
        for &b in APP_JS
            .as_bytes()
            .iter()
            .chain(INDEX_HTML.as_bytes())
            .chain(SERVICE_WORKER.as_bytes())
            .chain(MANIFEST.as_bytes())
        {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3); // FNV-1a prime
        }
        format!("{h:016x}")
    })
    .as_str()
}

/// The shell files with the `__NFS_BUILD__` placeholder stamped with the current build tag, so each
/// carries the version its client-side self-heal check compares against. Substituted once at startup.
fn index_body() -> &'static str {
    static S: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    S.get_or_init(|| INDEX_HTML.replace("__NFS_BUILD__", build_tag())).as_str()
}
fn app_js_body() -> &'static str {
    static S: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    S.get_or_init(|| APP_JS.replace("__NFS_BUILD__", build_tag())).as_str()
}
/// The service worker with `__NFS_BUILD__` stamped, so its cache name (`nfs-shell-<tag>`) rotates
/// with each build and `activate()` purges the stale bucket — no more hand-bumping the version.
fn service_worker_body() -> &'static str {
    static S: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    S.get_or_init(|| SERVICE_WORKER.replace("__NFS_BUILD__", build_tag())).as_str()
}
// Branding (the Newfoundland badge) — shared with the exe/GUI icon.
const FAVICON_PNG: &[u8] = include_bytes!("../../../branding/icon-32.png");
const ICON_128_PNG: &[u8] = include_bytes!("../../../branding/icon-128.png");
const ICON_256_PNG: &[u8] = include_bytes!("../../../branding/icon-256.png");
const ICON_512_PNG: &[u8] = include_bytes!("../../../branding/icon-512.png");
const ICON_512_MASKABLE_PNG: &[u8] = include_bytes!("../../../branding/icon-512-maskable.png");

/// Client→server: NTP-style clock request (first byte).
const MSG_CLOCK_REQ: u8 = 0x10;
/// Server→client: clock reply, then the server's monotonic ns (i64 BE).
const MSG_CLOCK_RSP: u8 = 0x11;

/// Server→client: set this client's server-controlled ("remote") volume.
/// Payload after the tag byte: an `f32` (little-endian) gain multiplier (≥ 0).
const MSG_SET_VOLUME: u8 = 0x20;
/// Client→server: HELLO — identify with a stable id (persists across reconnects,
/// from the browser's `localStorage`) plus a friendly display name.
/// Payload: `[id_len: u8][stable_id utf8][name utf8 …]`.
const MSG_HELLO: u8 = 0x21;
/// Server↔client calibration orchestration (Phase B: "Calibrate all"). The byte after the tag
/// is a sub-type: server→client ROLE (1) assigns reference/follower + code seeds + TDMA slot;
/// client→server STATUS (2) carries a short UTF-8 progress string for the GUI.
const MSG_CALIB_CTRL: u8 = 0x22;
const CALIB_SUB_ROLE: u8 = 1; // server→client: [0x22][1][role:u8][refSeed u32 LE][selfSeed u32 LE][slot u8]
const CALIB_SUB_STATUS: u8 = 2; // client→server: [0x22][2][utf8 status text]
/// Role byte in a ROLE message. 0 = idle/stop, 1 = reference (emit), 2 = follower (listen+align).
const CALIB_ROLE_IDLE: u8 = 0;
/// Server→client: set this client's server-controlled sync offset (a playout nudge
/// that ADDS to the device's own trim). Payload after the tag: an `i32` (LE) of
/// milliseconds — positive = play later, negative = earlier.
const MSG_SET_TRIM: u8 = 0x23;

/// Client→server: the client reports its *actual* effective sync offset (its own local
/// trim from calibration/its slider PLUS our [`MSG_SET_TRIM`] offset). Payload after the
/// tag: an `i32` (LE) of milliseconds. Lets the GUI show each device's real sync instead
/// of only the value the server commanded (which is 0 until the operator touches it).
const MSG_CLIENT_SYNC: u8 = 0x24;

// --- Web-client cast (uplink relay) — only meaningful when the active source is WebUplink. ---
/// C→S: a casting client's Opus packet. `[0x30][opus bytes]` (server stamps PTS, re-broadcasts).
const MSG_UP_AUDIO: u8 = 0x30;
/// C→S: a casting client's H.264 access unit. `[0x31][key u8][Annex-B bytes]` (Phase 2).
const MSG_UP_VIDEO: u8 = 0x31;
/// C→S: claim the single caster slot. `[0x32]`.
const MSG_CAST_REQUEST: u8 = 0x32;
/// S→C: grant/deny + the server's encode targets the caster must use. Fixed 21-byte layout:
/// `[0x33][grant u8][videoOn u8][w u16 LE][h u16 LE][fps u8][vKbps u32 LE][aBps u32 LE][sampleRate u32 LE][channels u8]`.
const MSG_CAST_GRANT: u8 = 0x33;
/// C↔S: stop casting (caster requests, or operator stops it via the clients panel). `[0x34]`.
const MSG_CAST_STOP: u8 = 0x34;

// --- Cast / uplink safety limits ----------------------------------------------------------------
/// Bounded per-client outbound queue, drained by a dedicated write task so a slow reader can never
/// block the serve() loop (which must stay live to free the caster slot + registry entry on drop).
const OUT_QUEUE: usize = 256;
/// Per-socket write deadline: a wedged / half-open peer is evicted within this, not held forever.
const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Max caster uplink payload (AFTER the tag byte). Opus stays well under 8 KiB even at 510 kbps; 4 MiB
/// covers a pathological H.264 keyframe while staying under the WS frame cap.
const MAX_UP_AUDIO_BYTES: usize = 8 * 1024;
const MAX_UP_VIDEO_BYTES: usize = 4 * 1024 * 1024;
/// Floor for the per-connection caster upload budget (covers audio-only casts + overhead). The ACTUAL
/// budget is derived per-connection from the server-dictated bitrate in serve() (see uplink_rate_exceeded),
/// so no operator quality preset — up to the 80 Mbps video clamp — can false-trip the limiter.
const UPLINK_MIN_BUDGET_BYTES: usize = 2 * 1024 * 1024;

/// The connection id of the single active caster (web-uplink source), or `None`. Shared between
/// the GUI (operator "Stop cast") and the per-client serve() tasks (claim/relay/release).
pub type CastState = Arc<Mutex<Option<u64>>>;

/// The currently-served stream. Swapped atomically via the `watch` channel when
/// the source changes.
#[derive(Clone)]
pub struct StreamState {
    pub config_json: String,
    pub audio_tx: broadcast::Sender<Frame>,
    pub video_tx: broadcast::Sender<Frame>,
    /// Present iff the active source is a web-client cast: the relay the serve() task pushes a
    /// caster's uploaded frames into. `Some` also signals to serve() that casting is allowed.
    pub cast_relay: Option<Arc<crate::media::CastRelay>>,
}

impl StreamState {
    pub fn from_media(media: &Media) -> StreamState {
        StreamState {
            config_json: media.config.to_json(),
            audio_tx: media.audio_tx.clone(),
            video_tx: media.video_tx.clone(),
            cast_relay: media.cast_relay.clone(),
        }
    }
}

/// A connected web client the server can address individually — for per-client
/// volume today, calibration orchestration (Phase B) later. Lives in a
/// [`ClientRegistry`] keyed by the ephemeral connection id.
pub struct ClientEntry {
    /// Ephemeral per-connection id (the registry key); a fresh one each connect.
    pub conn_id: u64,
    /// Stable id the browser persists in `localStorage` and sends in HELLO. Lets
    /// the GUI keep a client's volume across reconnects. Empty until HELLO arrives.
    pub stable_id: String,
    /// Friendly display name from HELLO. Empty until HELLO arrives.
    pub name: String,
    /// Push channel: bytes queued here are delivered to this client as a binary WS
    /// message by its `serve()` task (e.g. a [`MSG_SET_VOLUME`] frame from the GUI).
    pub ctrl_tx: mpsc::UnboundedSender<Vec<u8>>,
    /// Last effective remote volume pushed to this client (perClient × master).
    pub volume: f32,
    /// Last server-controlled sync offset (ms) pushed to this client.
    pub trim_ms: i32,
    /// The client's *actual* effective sync offset (ms), as it last reported via
    /// [`MSG_CLIENT_SYNC`] — its own local trim (calibration / its slider) plus our pushed
    /// offset. `None` until the first report. The GUI shows this so calibrated devices read
    /// their real, differing offsets instead of the commanded 0.
    pub reported_trim_ms: Option<i32>,
    /// Latest calibration progress this client reported (CALIB_CTRL STATUS), for the GUI.
    pub calib_status: String,
    /// True once HELLO has been received (so `stable_id`/`name` are meaningful).
    pub identified: bool,
}

/// All currently-connected clients, keyed by ephemeral connection id. Shared
/// between the web server (insert on connect, remove on drop, fill in on HELLO)
/// and the GUI (render the list + push per-client volume via each `ctrl_tx`).
pub type ClientRegistry = Arc<Mutex<HashMap<u64, ClientEntry>>>;

/// Build a server→client [`MSG_SET_VOLUME`] frame carrying `gain` (f32 LE).
/// Exposed so the GUI can push volume without re-encoding the wire format.
pub fn set_volume_msg(gain: f32) -> Vec<u8> {
    let mut m = Vec::with_capacity(5);
    m.push(MSG_SET_VOLUME);
    m.extend_from_slice(&gain.to_le_bytes());
    m
}

/// Build a server→client [`MSG_SET_TRIM`] frame carrying `ms` (i32 LE).
/// Exposed so the GUI can push per-client sync without re-encoding the wire format.
pub fn set_trim_msg(ms: i32) -> Vec<u8> {
    let mut m = Vec::with_capacity(5);
    m.push(MSG_SET_TRIM);
    m.extend_from_slice(&ms.to_le_bytes());
    m
}

/// Build a server→client [`MSG_CAST_GRANT`]. On grant, carries the server's encode targets the
/// caster must use (so all receivers get the operator's quality). Fixed 21-byte layout; on deny,
/// the param bytes are zero. `relay` supplies the targets when granting.
fn cast_grant_msg(grant: bool, relay: Option<&crate::media::CastRelay>) -> Vec<u8> {
    let mut m = Vec::with_capacity(21);
    m.push(MSG_CAST_GRANT);
    m.push(grant as u8);
    match relay {
        Some(r) if grant => {
            m.push(r.video_on as u8);
            m.extend_from_slice(&r.width.to_le_bytes());
            m.extend_from_slice(&r.height.to_le_bytes());
            m.push(r.fps);
            m.extend_from_slice(&r.video_kbps.to_le_bytes());
            m.extend_from_slice(&r.audio_bps.to_le_bytes());
            m.extend_from_slice(&r.sample_rate.to_le_bytes());
            m.push(r.channels);
        }
        _ => m.extend_from_slice(&[0u8; 19]), // denied: pad to the fixed length
    }
    m
}

/// Build a [`MSG_CAST_STOP`] frame (operator stops the active cast). Exposed for the GUI.
pub fn cast_stop_msg() -> Vec<u8> {
    vec![MSG_CAST_STOP]
}

/// Build a CALIB_CTRL ROLE frame: assign this client a calibration role + code seeds + TDMA slot.
/// `role`: 0 = idle/stop, 1 = reference, 2 = follower. Exposed for the GUI's "Calibrate all".
pub fn calib_role_msg(role: u8, ref_seed: u32, self_seed: u32, slot: u8) -> Vec<u8> {
    let mut m = Vec::with_capacity(12);
    m.push(MSG_CALIB_CTRL);
    m.push(CALIB_SUB_ROLE);
    m.push(role);
    m.extend_from_slice(&ref_seed.to_le_bytes());
    m.extend_from_slice(&self_seed.to_le_bytes());
    m.push(slot);
    m
}

/// Build a CALIB_CTRL "stop" frame (ROLE = idle) to abort calibration on a client.
pub fn calib_stop_msg() -> Vec<u8> {
    calib_role_msg(CALIB_ROLE_IDLE, 0, 0, 0)
}

/// Parse a HELLO payload `[0x21][id_len: u8][stable_id][name …]` → (stable_id, name).
/// Returns `None` if it's too short to hold the declared id.
fn parse_hello(b: &[u8]) -> Option<(String, String)> {
    if b.len() < 2 {
        return None;
    }
    let id_len = b[1] as usize;
    if b.len() < 2 + id_len {
        return None;
    }
    let stable_id = String::from_utf8_lossy(&b[2..2 + id_len]).into_owned();
    let name = String::from_utf8_lossy(&b[2 + id_len..]).into_owned();
    Some((stable_id, name))
}

struct AppState {
    stream: watch::Receiver<Arc<StreamState>>,
    clients: Arc<AtomicUsize>,
    clients_reg: ClientRegistry,
    next_id: AtomicU64,
    /// The single active caster's conn_id (web-uplink source), or None. Shared with the GUI.
    cast: CastState,
}

/// Run the web server until shutdown. `stream` carries the active capture/stream
/// (swappable live); `clients` tracks the number of connected browsers. When
/// `use_tls` is true (the default) it serves HTTPS so browsers grant a secure
/// context (required for WebCodecs); plain HTTP is only for localhost/reverse-proxy.
pub async fn run(
    stream: watch::Receiver<Arc<StreamState>>,
    clients: Arc<AtomicUsize>,
    clients_reg: ClientRegistry,
    cast: CastState,
    addr: SocketAddr,
    use_tls: bool,
) -> Result<()> {
    let state = Arc::new(AppState {
        stream,
        clients,
        clients_reg,
        next_id: AtomicU64::new(1),
        cast,
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/status", get(status)) // headless-friendly live view of connected clients
        .route("/app.js", get(app_js))
        .route("/version", get(version)) // content build tag — the client self-heals a stale cached shell
        .route("/sw.js", get(service_worker))
        .route("/manifest.webmanifest", get(manifest))
        .route("/favicon.png", get(favicon_png))
        .route("/icon-128.png", get(icon_128_png))
        .route("/icon-256.png", get(icon_256_png))
        .route("/icon-512.png", get(icon_512_png))
        .route("/icon-512-maskable.png", get(icon_512_maskable_png))
        .route("/ws", get(ws_upgrade))
        .with_state(state);

    if !use_tls {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("bind HTTP server on {addr}"))?;
        tracing::warn!(%addr, "serving plain HTTP — WebCodecs only works via localhost or a TLS proxy");
        axum::serve(listener, app.into_make_service())
            .await
            .context("web server error")?;
        return Ok(());
    }

    // Two crypto providers are compiled in (ring + aws-lc); pick one as the process
    // default before axum-server builds its ServerConfig.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (cert_pem, key_pem) = crate::tls::load_or_create_cert().context("TLS certificate")?;
    let config = RustlsConfig::from_pem(cert_pem, key_pem)
        .await
        .context("load TLS config")?;

    tracing::info!(
        %addr,
        "HTTPS server listening — open https://<this-pc>:{} (accept the one-time self-signed cert)",
        addr.port()
    );
    axum_server::bind_rustls(addr, config)
        .serve(app.into_make_service())
        .await
        .context("web server error")?;
    Ok(())
}

async fn index() -> impl IntoResponse {
    // no-cache so a rebuilt shell is never masked by browser/proxy heuristic caching
    // (the service worker is network-first, but the very first load predates it).
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        index_body(),
    )
}

async fn app_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        app_js_body(),
    )
}

/// The current content build tag (plain text). The client fetches this on load and, if it differs
/// from the tag stamped into the running app.js/index.html, drops the SW + caches and reloads.
async fn version() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        build_tag(),
    )
}

const STATUS_HEAD: &str = "<!doctype html><html lang=en><head><meta charset=utf-8>\
<title>Newfoundsync — clients</title><meta http-equiv=refresh content=2>\
<meta name=viewport content=\"width=device-width, initial-scale=1\"><meta name=color-scheme content=dark>\
<style>body{background:#0b0f15;color:#e8eef5;font:14px system-ui,'Segoe UI',sans-serif;margin:0;padding:18px}\
h1{font-size:18px;margin:0 0 12px}.dim{color:#94a1b2}\
table{border-collapse:collapse;width:100%;max-width:780px}\
th,td{text-align:left;padding:8px 12px;border-bottom:1px solid #2a3340}\
th{color:#94a1b2;font-weight:600;font-size:12px}td{font-variant-numeric:tabular-nums}</style></head><body>";

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Headless-friendly server-side view of connected clients (the GUI mixer hangs on some
/// machines, so this gives the same visibility from any browser at `/status`). Read-only;
/// auto-refreshes every 2 s. Lists each device's name, connection state, the sync offset it
/// reported, its effective remote volume, and any calibration status.
async fn status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut rows = String::new();
    let mut n = 0usize;
    if let Ok(reg) = state.clients_reg.lock() {
        let mut entries: Vec<&ClientEntry> = reg.values().collect();
        entries.sort_by_key(|e| e.conn_id);
        n = entries.len();
        for e in entries {
            let name = if e.name.trim().is_empty() {
                format!("Client {}", e.conn_id)
            } else {
                html_escape(&e.name)
            };
            let status_txt = if e.identified { "connected" } else { "connecting…" };
            let sync = match e.reported_trim_ms {
                Some(ms) => format!("{ms:+} ms"),
                None => format!("{:+} ms (cmd)", e.trim_ms),
            };
            let vol = format!("{}%", (e.volume * 100.0).round() as i64);
            let calib = html_escape(&e.calib_status);
            rows.push_str("<tr><td>");
            rows.push_str(&name);
            rows.push_str("</td><td>");
            rows.push_str(status_txt);
            rows.push_str("</td><td>");
            rows.push_str(&sync);
            rows.push_str("</td><td>");
            rows.push_str(&vol);
            rows.push_str("</td><td class=dim>");
            rows.push_str(&calib);
            rows.push_str("</td></tr>");
        }
    }
    if n == 0 {
        rows.push_str("<tr><td colspan=5 class=dim>No clients connected yet.</td></tr>");
    }
    let mut body = String::from(STATUS_HEAD);
    body.push_str("<h1>Connected clients <span class=dim>(");
    body.push_str(&n.to_string());
    body.push_str(")</span></h1><table><thead><tr><th>Device</th><th>Status</th><th>Sync</th><th>Volume</th><th>Calibration</th></tr></thead><tbody>");
    body.push_str(&rows);
    body.push_str("</tbody></table><p class=dim>Auto-refreshes every 2 s · server-side view (works headless)</p></body></html>");
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
}

async fn service_worker() -> impl IntoResponse {
    // no-cache so the browser always revalidates the SW script and picks up updates.
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        service_worker_body(),
    )
}

async fn manifest() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/manifest+json; charset=utf-8"),
            // no-cache: it's part of build_tag now, so a rebuilt manifest must not be masked by
            // heuristic/HTTP caching (every other shell route is no-cache too).
            (header::CACHE_CONTROL, "no-cache"),
        ],
        MANIFEST,
    )
}

fn png(bytes: &'static [u8]) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        bytes,
    )
}
async fn favicon_png() -> impl IntoResponse {
    png(FAVICON_PNG)
}
async fn icon_128_png() -> impl IntoResponse {
    png(ICON_128_PNG)
}
async fn icon_256_png() -> impl IntoResponse {
    png(ICON_256_PNG)
}
async fn icon_512_png() -> impl IntoResponse {
    png(ICON_512_PNG)
}
async fn icon_512_maskable_png() -> impl IntoResponse {
    png(ICON_512_MASKABLE_PNG)
}

async fn ws_upgrade(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // Cap WS message/frame size well below tungstenite's 64 MiB default so a malicious/buggy caster
    // can't force huge per-message allocations (each fanned out to every receiver). Above any legit
    // payload (see MAX_UP_* — largest is a 4 MiB video AU).
    ws.max_message_size(8 * 1024 * 1024)
        .max_frame_size(8 * 1024 * 1024)
        .on_upgrade(move |socket| ws_client(socket, state))
}

/// Removes this client from the registry and decrements the connected-client
/// count on drop — so both stay correct even if the serve task panics mid-flight.
struct ClientGuard {
    clients: Arc<AtomicUsize>,
    reg: ClientRegistry,
    cast: CastState,
    conn_id: u64,
}
impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.clients.fetch_sub(1, Ordering::Relaxed);
        if let Ok(mut reg) = self.reg.lock() {
            reg.remove(&self.conn_id);
        }
        // If this was the active caster, free the slot so another client can claim it.
        if let Ok(mut slot) = self.cast.lock() {
            if *slot == Some(self.conn_id) {
                *slot = None;
            }
        }
    }
}

async fn ws_client(socket: WebSocket, state: Arc<AppState>) {
    state.clients.fetch_add(1, Ordering::Relaxed);
    let conn_id = state.next_id.fetch_add(1, Ordering::Relaxed);
    // Per-client push channel: the GUI sends control frames (e.g. SET_VOLUME) here,
    // and this client's serve() loop forwards them over the socket.
    let (ctrl_tx, ctrl_rx) = mpsc::unbounded_channel();
    if let Ok(mut reg) = state.clients_reg.lock() {
        reg.insert(
            conn_id,
            ClientEntry {
                conn_id,
                stable_id: String::new(),
                name: String::new(),
                ctrl_tx,
                volume: 1.0,
                trim_ms: 0,
                reported_trim_ms: None,
                calib_status: String::new(),
                identified: false,
            },
        );
    }
    let _guard = ClientGuard {
        clients: state.clients.clone(),
        reg: state.clients_reg.clone(),
        cast: state.cast.clone(),
        conn_id,
    };
    serve(socket, &state, conn_id, ctrl_rx).await;
}

/// Per-connection sliding ~1 s byte-rate limiter for caster uplinks. Returns true once the caster has
/// exceeded `budget` bytes in the current window (→ evict the abuser). `budget` is derived per-connection
/// from the server-dictated bitrate (see serve()), so legitimate high-quality casts don't false-trip.
fn uplink_rate_exceeded(
    win_start: &mut std::time::Instant,
    win_bytes: &mut usize,
    add: usize,
    budget: usize,
) -> bool {
    if win_start.elapsed().as_millis() >= 1000 {
        *win_start = std::time::Instant::now();
        *win_bytes = 0;
    }
    *win_bytes += add;
    *win_bytes > budget
}

async fn serve(
    socket: WebSocket,
    state: &AppState,
    conn_id: u64,
    mut ctrl_rx: mpsc::UnboundedReceiver<Vec<u8>>,
) {
    let (sender, mut receiver) = socket.split();

    // Snapshot the active stream at connect time.
    let mut stream_rx = state.stream.clone();
    let active = stream_rx.borrow_and_update().clone();

    // All outbound frames go through a BOUNDED channel drained by a dedicated write task, so a slow /
    // non-reading client can never block this loop — it must stay live to observe disconnects and free
    // the caster slot + registry entry (via ClientGuard) on return. Each socket write is timeout-bounded,
    // so a wedged / half-open peer is evicted within WRITE_TIMEOUT rather than holding the slot forever.
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(OUT_QUEUE);
    let mut write_handle = tokio::spawn(async move {
        let mut sender = sender;
        while let Some(msg) = out_rx.recv().await {
            match tokio::time::timeout(WRITE_TIMEOUT, sender.send(msg)).await {
                Ok(Ok(())) => {}
                _ => break, // write error OR deadline exceeded → peer is dead (drops sender → socket closes)
            }
        }
    });

    // Config must be the client's first frame; the queue is empty so it lands immediately.
    if out_tx
        .send(Message::Text(active.config_json.clone()))
        .await
        .is_err()
    {
        write_handle.abort();
        return;
    }
    let mut arx = active.audio_tx.subscribe();
    let mut vrx = active.video_tx.subscribe();

    // Per-connection uplink rate window + budget (caster-abuse guard, see uplink_rate_exceeded). The
    // budget is derived from the bitrate the SERVER dictates to this caster (video_kbps + audio_bps) with
    // ~3x keyframe-burst headroom, floored for audio-only casts — so no operator quality preset (up to the
    // 80 Mbps video clamp) false-trips it, while a genuinely abusive flood is still cut off.
    let mut win_start = std::time::Instant::now();
    let mut win_bytes: usize = 0;
    let uplink_budget = active
        .cast_relay
        .as_deref()
        .map(|r| ((r.video_kbps as usize * 1000 + r.audio_bps as usize) / 8) * 3)
        .unwrap_or(0)
        .max(UPLINK_MIN_BUDGET_BYTES);

    loop {
        tokio::select! {
            // The write task ended (write error or timeout) → the peer is gone; tear down.
            _ = &mut write_handle => break,
            // The source was swapped (or the server is shutting down) — drop this
            // client so the browser reconnects and picks up the new stream.
            _ = stream_rx.changed() => break,
            // GUI → this client: forward a server-pushed control frame (e.g. SET_VOLUME).
            Some(msg) = ctrl_rx.recv() => {
                if out_tx.try_send(Message::Binary(msg)).is_err() { break; } // queue full (too far behind) or closed → evict
            }
            incoming = receiver.next() => {
                match incoming {
                    Some(Ok(Message::Binary(b))) if b.first() == Some(&MSG_CLOCK_REQ) => {
                        // True 4-timestamp NTP: t2 = the instant we dequeued the request, t3 = the
                        // instant just before we send the reply. The client cancels (t3 - t2) server
                        // dwell out of BOTH its offset and its RTT, removing the per-device DC bias the
                        // old single-stamp scheme baked in. Reply: [tag][t2 i64 BE][t3 i64 BE].
                        let t2 = mono_now();
                        let mut r = Vec::with_capacity(17);
                        r.push(MSG_CLOCK_RSP);
                        r.extend_from_slice(&t2.to_be_bytes());
                        let t3 = mono_now();
                        r.extend_from_slice(&t3.to_be_bytes());
                        if out_tx.try_send(Message::Binary(r)).is_err() { break; }
                    }
                    // Calibration progress report (CALIB_CTRL STATUS) → store for the GUI.
                    Some(Ok(Message::Binary(b)))
                        if b.first() == Some(&MSG_CALIB_CTRL) && b.get(1) == Some(&CALIB_SUB_STATUS) =>
                    {
                        let text = String::from_utf8_lossy(&b[2..]).into_owned();
                        if let Ok(mut reg) = state.clients_reg.lock() {
                            if let Some(e) = reg.get_mut(&conn_id) {
                                e.calib_status = text;
                            }
                        }
                    }
                    // Client → server: report its actual effective sync offset (i32 LE ms),
                    // so the GUI can show each device's real sync rather than the commanded 0.
                    Some(Ok(Message::Binary(b)))
                        if b.first() == Some(&MSG_CLIENT_SYNC) && b.len() >= 5 =>
                    {
                        let ms = i32::from_le_bytes([b[1], b[2], b[3], b[4]]);
                        if let Ok(mut reg) = state.clients_reg.lock() {
                            if let Some(e) = reg.get_mut(&conn_id) {
                                e.reported_trim_ms = Some(ms);
                            }
                        }
                    }
                    // Identify this connection so the GUI can name it and remember
                    // its volume across reconnects (matched by stable_id).
                    Some(Ok(Message::Binary(b))) if b.first() == Some(&MSG_HELLO) => {
                        if let Some((stable_id, name)) = parse_hello(&b) {
                            if let Ok(mut reg) = state.clients_reg.lock() {
                                if let Some(e) = reg.get_mut(&conn_id) {
                                    e.name = if name.trim().is_empty() {
                                        format!("Client {conn_id}")
                                    } else {
                                        name
                                    };
                                    e.stable_id = stable_id;
                                    e.identified = true;
                                }
                            }
                        }
                    }
                    // Web cast: a client claims the single caster slot. Granted only when the active
                    // source is a web uplink (cast_relay present) AND the slot is free (or already ours).
                    Some(Ok(Message::Binary(b))) if b.first() == Some(&MSG_CAST_REQUEST) => {
                        let granted = active.cast_relay.is_some()
                            && state
                                .cast
                                .lock()
                                .map(|mut slot| match *slot {
                                    None => {
                                        *slot = Some(conn_id);
                                        true
                                    }
                                    Some(c) => c == conn_id, // re-request is idempotent; else taken
                                })
                                .unwrap_or(false);
                        let msg = cast_grant_msg(granted, active.cast_relay.as_deref());
                        if out_tx.try_send(Message::Binary(msg)).is_err() { break; }
                    }
                    // Web cast: the active caster's uploaded Opus packet → re-stamp + fan out.
                    Some(Ok(Message::Binary(b))) if b.first() == Some(&MSG_UP_AUDIO) && b.len() > 1 => {
                        let is_caster = state.cast.lock().map(|s| *s == Some(conn_id)).unwrap_or(false);
                        if is_caster {
                            if b.len() - 1 > MAX_UP_AUDIO_BYTES { break; } // oversize payload → protocol abuse, evict
                            if uplink_rate_exceeded(&mut win_start, &mut win_bytes, b.len(), uplink_budget) { break; }
                            if let Some(relay) = active.cast_relay.as_deref() {
                                relay.push_audio(&b[1..]);
                            }
                        }
                    }
                    // Web cast: the active caster's uploaded H.264 access unit → re-stamp + fan out (Phase 2).
                    Some(Ok(Message::Binary(b))) if b.first() == Some(&MSG_UP_VIDEO) && b.len() > 2 => {
                        let is_caster = state.cast.lock().map(|s| *s == Some(conn_id)).unwrap_or(false);
                        if is_caster {
                            if let Some(relay) = active.cast_relay.as_deref() {
                                if !relay.video_on { break; } // audio-only stream must never relay video → evict
                                if b.len() - 2 > MAX_UP_VIDEO_BYTES { break; } // oversize access unit → evict
                                if uplink_rate_exceeded(&mut win_start, &mut win_bytes, b.len(), uplink_budget) { break; }
                                relay.push_video(&b[2..]); // key flag re-derived server-side, not trusted from b[1]
                            }
                        }
                    }
                    // Web cast: client stops casting → free the slot for the next claimant.
                    Some(Ok(Message::Binary(b))) if b.first() == Some(&MSG_CAST_STOP) => {
                        if let Ok(mut slot) = state.cast.lock() {
                            if *slot == Some(conn_id) {
                                *slot = None;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {} // ping/pong/text — ignore
                }
            }
            audio = arx.recv() => {
                match audio {
                    Ok(frame) => match out_tx.try_send(Message::Binary((*frame).clone())) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(_)) => {} // client behind; drop this frame (like Lagged)
                        Err(mpsc::error::TrySendError::Closed(_)) => break,
                    },
                    Err(broadcast::error::RecvError::Lagged(_)) => {} // fell behind; skip
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            video = vrx.recv() => {
                match video {
                    Ok(frame) => match out_tx.try_send(Message::Binary((*frame).clone())) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(_)) => {} // client behind; drop this frame
                        Err(mpsc::error::TrySendError::Closed(_)) => break,
                    },
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    write_handle.abort(); // stop the write task; out_tx drops here too, closing the channel
}
