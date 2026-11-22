// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Searches for Cast devices and runs one launch session per device, on a thread of its own so the
//! server window only ever reads a snapshot.
//!
//! A session keeps the control channel open for as long as the page plays: the device expects a
//! heartbeat from a connected sender, and the receiver status that arrives on it is how the window
//! learns that someone switched the TV to another app.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};

use super::v2::{self, CastMessage, Connection, ReceiverStatus};
use super::{CastDevice, CastDirectory};

/// How long a search listens for answers.
const SEARCH_WINDOW: Duration = Duration::from_millis(2500);
/// How long a launch may take before it is reported as failed. A TV that has to wake its screen and
/// load the page from the network can take a while.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);
const HEARTBEAT: Duration = Duration::from_secs(5);

/// What one device is doing, as the window shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionState {
    /// Connecting and asking the device to open the page.
    Starting,
    /// The page is open on the device.
    Playing,
    /// Asked to close it.
    Stopping,
    /// It ended on its own or was stopped; the text says how, in plain words.
    Ended(String),
    /// It never started; the text says why and what to check.
    Failed(String),
}

impl SessionState {
    pub fn is_active(&self) -> bool {
        matches!(self, SessionState::Starting | SessionState::Playing | SessionState::Stopping)
    }
}

/// A copy of everything the window shows, taken under the lock.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub devices: Vec<CastDevice>,
    pub searching: bool,
    /// When the last search finished, if one has.
    pub searched_at: Option<Instant>,
    /// Why the last search could not run (no network, a blocked socket), if it could not.
    pub search_error: Option<String>,
    /// By device id. A device with no entry has not been played on since the server started.
    pub sessions: HashMap<String, SessionState>,
}

#[derive(Default)]
struct Shared {
    snap: Snapshot,
}

enum Cmd {
    Search,
    Play(CastDevice, String),
    Stop(String),
}

/// The handle the window holds. Cheap to clone.
#[derive(Clone)]
pub struct CastManager {
    shared: Arc<Mutex<Shared>>,
    cmd: mpsc::UnboundedSender<Cmd>,
    directory: CastDirectory,
}

impl CastManager {
    /// Start the manager's thread and run a first search. `directory` is filled with every device
    /// found, for the web server's `/cast/whoami`.
    pub fn start(directory: CastDirectory) -> CastManager {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (cmd, rx) = mpsc::unbounded_channel();
        let m = CastManager { shared: shared.clone(), cmd, directory: directory.clone() };
        let spawned = std::thread::Builder::new().name("google-cast".into()).spawn(move || {
            match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt.block_on(run(rx, shared, directory)),
                Err(e) => tracing::error!("Google Cast: no runtime ({e})"),
            }
        });
        if let Err(e) = spawned {
            tracing::error!("Google Cast: could not start its thread ({e})");
        }
        m.search();
        m
    }

    pub fn search(&self) {
        if let Ok(mut s) = self.shared.lock() {
            s.snap.searching = true;
        }
        let _ = self.cmd.send(Cmd::Search);
    }

    /// Open the page on `dev` with the Cast app `app_id`.
    pub fn play(&self, dev: &CastDevice, app_id: &str) {
        if let Ok(mut s) = self.shared.lock() {
            s.snap.sessions.insert(dev.id.clone(), SessionState::Starting);
        }
        // The page asks "which TV am I?" as soon as it loads, so make sure the answer is there even if
        // this device came from an earlier search whose results have since been replaced.
        if let Ok(mut d) = self.directory.lock() {
            d.insert(dev.ip, dev.clone());
        }
        let _ = self.cmd.send(Cmd::Play(dev.clone(), app_id.to_string()));
    }

    pub fn stop(&self, device_id: &str) {
        if let Ok(mut s) = self.shared.lock() {
            if let Some(st) = s.snap.sessions.get_mut(device_id) {
                if st.is_active() {
                    *st = SessionState::Stopping;
                }
            }
        }
        let _ = self.cmd.send(Cmd::Stop(device_id.to_string()));
    }

    pub fn snapshot(&self) -> Snapshot {
        self.shared.lock().map(|s| s.snap.clone()).unwrap_or_default()
    }
}

async fn run(mut rx: mpsc::UnboundedReceiver<Cmd>, shared: Arc<Mutex<Shared>>, directory: CastDirectory) {
    // Each running session's stop switch, by device id.
    let mut stops: HashMap<String, watch::Sender<bool>> = HashMap::new();
    while let Some(cmd) = rx.recv().await {
        stops.retain(|_, tx| !tx.is_closed());
        match cmd {
            Cmd::Search => {
                let shared = shared.clone();
                let directory = directory.clone();
                tokio::spawn(async move {
                    // Both at once: mDNS is quick and gives the model, the subnet scan gets through
                    // networks that drop multicast. Merged by device id, mDNS first.
                    let lan = newfoundsync_core::net::primary_lan_ipv4();
                    let (by_mdns, by_scan) = tokio::join!(
                        tokio::task::spawn_blocking(|| super::mdns::discover(SEARCH_WINDOW)),
                        async move {
                            match lan {
                                Some(ip) => super::scan::scan(ip).await,
                                None => Vec::new(),
                            }
                        }
                    );
                    let mdns_err = match &by_mdns {
                        Ok(Err(e)) => Some(format!("{e}")),
                        Err(e) => Some(format!("{e}")),
                        Ok(Ok(_)) => None,
                    };
                    let mut merged: Vec<CastDevice> = by_mdns.ok().and_then(|r| r.ok()).unwrap_or_default();
                    for dev in by_scan {
                        if !merged.iter().any(|d| d.id == dev.id || d.ip == dev.ip) {
                            merged.push(dev);
                        }
                    }
                    merged.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                    let err = match (&mdns_err, lan, merged.is_empty()) {
                        (Some(e), None, true) => Some(format!("Could not search the network: {e}")),
                        (_, None, true) => Some("This computer does not seem to be on a network.".to_string()),
                        _ => None,
                    };
                    if let Some(e) = &mdns_err {
                        tracing::info!("Google Cast: mDNS search failed ({e}); the subnet scan still ran");
                    }
                    let devices = Some(merged);
                    if let Some(devs) = &devices {
                        tracing::info!(found = devs.len(), "Google Cast search finished");
                        if let Ok(mut d) = directory.lock() {
                            for dev in devs {
                                d.insert(dev.ip, dev.clone());
                            }
                        }
                    }
                    if let Ok(mut s) = shared.lock() {
                        s.snap.searching = false;
                        s.snap.searched_at = Some(Instant::now());
                        s.snap.search_error = err;
                        if let Some(devs) = devices {
                            s.snap.devices = devs;
                        }
                    }
                });
            }
            Cmd::Play(dev, app_id) => {
                if let Some(old) = stops.remove(&dev.id) {
                    let _ = old.send(true);
                }
                let (stop_tx, stop_rx) = watch::channel(false);
                stops.insert(dev.id.clone(), stop_tx);
                let shared = shared.clone();
                tokio::spawn(async move {
                    let id = dev.id.clone();
                    let end = session(&dev, &app_id, stop_rx, &shared).await;
                    tracing::info!(device = %dev.name, outcome = ?end, "Google Cast session ended");
                    if let Ok(mut s) = shared.lock() {
                        // A newer session for the same device may already have replaced this one.
                        if s.snap.sessions.get(&id).is_some_and(|st| st.is_active()) {
                            s.snap.sessions.insert(id, end);
                        }
                    }
                });
            }
            Cmd::Stop(id) => {
                if let Some(tx) = stops.remove(&id) {
                    let _ = tx.send(true);
                } else if let Ok(mut s) = shared.lock() {
                    s.snap.sessions.insert(id, SessionState::Ended("Stopped.".into()));
                }
            }
        }
    }
}

fn set(shared: &Arc<Mutex<Shared>>, id: &str, st: SessionState) {
    if let Ok(mut s) = shared.lock() {
        s.snap.sessions.insert(id.to_string(), st);
    }
}

/// Launch the app on `dev` and stay connected until it ends. Returns the final state.
async fn session(
    dev: &CastDevice,
    app_id: &str,
    mut stop: watch::Receiver<bool>,
    shared: &Arc<Mutex<Shared>>,
) -> SessionState {
    let addr = SocketAddr::new(dev.ip, dev.port);
    let mut conn = match Connection::open(addr).await {
        Ok(c) => c,
        Err(e) => return SessionState::Failed(format!("Could not reach {}: {e:#}.", dev.name)),
    };
    let mut req = 1u32;
    let launch = format!(r#"{{"type":"LAUNCH","appId":"{app_id}","requestId":{req}}}"#);
    if let Err(e) = conn.send(&CastMessage::new(v2::SENDER_ID, v2::RECEIVER_ID, v2::NS_RECEIVER, launch)).await {
        return SessionState::Failed(format!("{e:#}"));
    }
    tracing::info!(device = %dev.name, %app_id, "Google Cast: asked the device to open the page");

    let started = Instant::now();
    let mut session_id: Option<String> = None;
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = stop.changed() => {
                if let Some(sid) = &session_id {
                    req += 1;
                    let msg = format!(r#"{{"type":"STOP","sessionId":"{sid}","requestId":{req}}}"#);
                    let _ = conn.send(&CastMessage::new(v2::SENDER_ID, v2::RECEIVER_ID, v2::NS_RECEIVER, msg)).await;
                    // Give the device a moment to act before the connection drops.
                    let _ = tokio::time::timeout(Duration::from_millis(800), conn.recv()).await;
                }
                return SessionState::Ended("Stopped.".into());
            }
            _ = heartbeat.tick() => {
                if session_id.is_none() && started.elapsed() > LAUNCH_TIMEOUT {
                    return SessionState::Failed(format!(
                        "{} did not open the page within {} seconds. Check that the receiver address in \
                         the Cast console is right and that the server window is reachable from the TV.",
                        dev.name,
                        LAUNCH_TIMEOUT.as_secs()
                    ));
                }
                let ping = CastMessage::new(v2::SENDER_ID, v2::RECEIVER_ID, v2::NS_HEARTBEAT, r#"{"type":"PING"}"#);
                if let Err(e) = conn.send(&ping).await {
                    return lost(dev, session_id.is_some(), e);
                }
            }
            msg = conn.recv() => {
                let msg = match msg {
                    Ok(m) => m,
                    Err(e) => return lost(dev, session_id.is_some(), e),
                };
                let json: serde_json::Value = serde_json::from_str(&msg.payload).unwrap_or_default();
                let kind = json.get("type").and_then(|t| t.as_str()).unwrap_or_default();
                match (msg.namespace.as_str(), kind) {
                    (v2::NS_HEARTBEAT, "PING") => {
                        let pong = CastMessage::new(v2::SENDER_ID, &msg.source, v2::NS_HEARTBEAT, r#"{"type":"PONG"}"#);
                        if let Err(e) = conn.send(&pong).await {
                            return lost(dev, session_id.is_some(), e);
                        }
                    }
                    (v2::NS_CONNECTION, "CLOSE") if msg.source == v2::RECEIVER_ID => {
                        return lost(dev, session_id.is_some(), anyhow::anyhow!("the device closed the connection"));
                    }
                    (v2::NS_RECEIVER, "RECEIVER_STATUS") => {
                        let st = ReceiverStatus::parse(json.get("status").unwrap_or(&serde_json::Value::Null));
                        match (st.app(app_id), &session_id) {
                            (Some(app), None) => {
                                session_id = Some(app.session_id.clone());
                                // Join the app's own channel too: a sender that is connected to it is
                                // what keeps a receiver from closing as abandoned.
                                let join = CastMessage::new(v2::SENDER_ID, &app.transport_id, v2::NS_CONNECTION, r#"{"type":"CONNECT"}"#);
                                let _ = conn.send(&join).await;
                                tracing::info!(device = %dev.name, "Google Cast: the page is open");
                                set(shared, &dev.id, SessionState::Playing);
                            }
                            (None, Some(_)) => {
                                return SessionState::Ended(match st.other_app(app_id) {
                                    Some(other) if !other.display_name.is_empty() => {
                                        format!("{} switched to {}.", dev.name, other.display_name)
                                    }
                                    Some(_) => format!("{} switched to another app.", dev.name),
                                    None => format!("{} closed the page.", dev.name),
                                });
                            }
                            _ => {}
                        }
                    }
                    (v2::NS_RECEIVER, "LAUNCH_ERROR") | (v2::NS_RECEIVER, "INVALID_REQUEST") => {
                        let reason = json.get("reason").and_then(|r| r.as_str()).unwrap_or("no reason given");
                        return SessionState::Failed(format!(
                            "{} refused to open the app ({reason}). Check the app ID, that this device's \
                             serial number is registered for testing in the Cast console, and that the \
                             device was restarted after registering it.",
                            dev.name
                        ));
                    }
                    _ => {}
                }
            }
        }
    }
}

fn lost(dev: &CastDevice, was_playing: bool, e: anyhow::Error) -> SessionState {
    if was_playing {
        SessionState::Ended(format!("Lost the connection to {} ({e:#}).", dev.name))
    } else {
        SessionState::Failed(format!("Lost the connection to {} before the page opened ({e:#}).", dev.name))
    }
}
