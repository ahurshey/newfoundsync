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

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use axum_server::tls_rustls::RustlsConfig;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, watch};

use newfoundsync_core::config::mono_now;

use crate::media::{Frame, Media};

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const SERVICE_WORKER: &str = include_str!("../web/sw.js");
const MANIFEST: &str = include_str!("../web/manifest.webmanifest");
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

/// The currently-served stream. Swapped atomically via the `watch` channel when
/// the source changes.
#[derive(Clone)]
pub struct StreamState {
    pub config_json: String,
    pub audio_tx: broadcast::Sender<Frame>,
    pub video_tx: broadcast::Sender<Frame>,
}

impl StreamState {
    pub fn from_media(media: &Media) -> StreamState {
        StreamState {
            config_json: media.config.to_json(),
            audio_tx: media.audio_tx.clone(),
            video_tx: media.video_tx.clone(),
        }
    }
}

struct AppState {
    stream: watch::Receiver<Arc<StreamState>>,
    clients: Arc<AtomicUsize>,
}

/// Run the web server until shutdown. `stream` carries the active capture/stream
/// (swappable live); `clients` tracks the number of connected browsers. When
/// `use_tls` is true (the default) it serves HTTPS so browsers grant a secure
/// context (required for WebCodecs); plain HTTP is only for localhost/reverse-proxy.
pub async fn run(
    stream: watch::Receiver<Arc<StreamState>>,
    clients: Arc<AtomicUsize>,
    addr: SocketAddr,
    use_tls: bool,
) -> Result<()> {
    let state = Arc::new(AppState { stream, clients });

    let app = Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
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
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], INDEX_HTML)
}

async fn app_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        APP_JS,
    )
}

async fn service_worker() -> impl IntoResponse {
    // no-cache so the browser always revalidates the SW script and picks up updates.
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        SERVICE_WORKER,
    )
}

async fn manifest() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/manifest+json; charset=utf-8")],
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
    ws.on_upgrade(move |socket| ws_client(socket, state))
}

/// Decrements the connected-client count on drop, so the count stays correct
/// even if the serve task panics mid-flight.
struct ClientGuard(Arc<AtomicUsize>);
impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn ws_client(socket: WebSocket, state: Arc<AppState>) {
    state.clients.fetch_add(1, Ordering::Relaxed);
    let _guard = ClientGuard(state.clients.clone());
    serve(socket, &state).await;
}

async fn serve(socket: WebSocket, state: &AppState) {
    let (mut sender, mut receiver) = socket.split();

    // Snapshot the active stream at connect time.
    let mut stream_rx = state.stream.clone();
    let active = stream_rx.borrow_and_update().clone();

    if sender
        .send(Message::Text(active.config_json.clone()))
        .await
        .is_err()
    {
        return;
    }

    let mut arx = active.audio_tx.subscribe();
    let mut vrx = active.video_tx.subscribe();

    loop {
        tokio::select! {
            // The source was swapped (or the server is shutting down) — drop this
            // client so the browser reconnects and picks up the new stream.
            _ = stream_rx.changed() => break,
            incoming = receiver.next() => {
                match incoming {
                    Some(Ok(Message::Binary(b))) if b.first() == Some(&MSG_CLOCK_REQ) => {
                        let mut r = Vec::with_capacity(9);
                        r.push(MSG_CLOCK_RSP);
                        r.extend_from_slice(&mono_now().to_be_bytes());
                        if sender.send(Message::Binary(r)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {} // ping/pong/text — ignore
                }
            }
            audio = arx.recv() => {
                match audio {
                    Ok(frame) => {
                        if sender.send(Message::Binary((*frame).clone())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {} // fell behind; skip
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            video = vrx.recv() => {
                match video {
                    Ok(frame) => {
                        if sender.send(Message::Binary((*frame).clone())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}
