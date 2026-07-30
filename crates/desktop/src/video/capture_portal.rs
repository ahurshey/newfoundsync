// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Linux screen capture: xdg-desktop-portal ScreenCast → PipeWire → tightly-packed BGRA.
//!
//! Presents the SAME surface as the Windows WGC backend (`capture.rs`) — `start_primary()`, a
//! `slot` holding the latest frame, and a `closed` death latch — so `VideoProducer` consumes either
//! without knowing which it has. `video/mod.rs` picks one as `crate::video::capture`.
//!
//! # Two things about this platform that shape the design
//!
//! **Capture requires a human, once.** The portal always shows a picker ("share which screen?") on
//! the first Start, and there is no API to pre-approve it — `restore_token` can suppress the dialog
//! on *later* runs, but only after somebody approved interactively at least once. So
//! `start_primary()` blocks until the dialog is answered, BOUNDED by [`CONSENT_TIMEOUT`]: an
//! unanswered prompt would otherwise wedge `media::start` and the web server would never bind its
//! port, killing audio and the UI too. It must also never be called from the GUI thread;
//! `media::start` already runs on the media-control thread, which keeps the window responsive while
//! the prompt is up.
//!
//! **A headless box cannot do this at all.** The ScreenCast implementation lives in a
//! desktop-environment backend (`xdg-desktop-portal-kde`, `-gnome`, …) that needs a running
//! compositor. On a bare systemd server the portal call fails, and it fails for architectural
//! reasons rather than configuration ones — no amount of packages fixes it. The error path says so
//! rather than leaving the operator to guess.
//!
//! # Why "whole screen" only
//!
//! There is deliberately no `start_window(id)`. On Windows a window is addressable by `HWND`, so the
//! app can enumerate windows and pick one itself. Wayland exposes no such handle to an unprivileged
//! client — choosing *what* to share is the portal dialog's job, and the caller gets whatever the
//! user picked. Offering a window list we could not honour would repeat the exact mistake this
//! platform's video support started with.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use pipewire as pw;
use pw::{properties::properties, spa};

/// How long to wait for a human to answer the portal's screen-share dialog.
///
/// Generous on purpose -- somebody may be walking back to the machine -- but finite, because
/// an unanswered prompt otherwise blocks `media::start` and the server never binds its port.
const CONSENT_TIMEOUT: Duration = Duration::from_secs(45);

/// One captured frame: tightly-packed BGRA (width*height*4). Byte-identical contract to the
/// Windows backend's frame, because the encoder's `bgra_to_i420` reads b,g,r at p, p+1, p+2 and
/// ignores the fourth byte — so a BGRx source needs no alpha fixup, only de-striding.
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// Latest-frame slot, overwrite-on-arrival. Never a queue: the encoder pulls at its own fps and
/// stale frames should be dropped, not backlogged.
pub type FrameSlot = Arc<Mutex<Option<CapturedFrame>>>;

/// A live portal ScreenCast session plus its PipeWire stream. Stops on drop.
pub struct ScreenCapture {
    /// Set when the session ends for real (stream error, portal session closed, output gone).
    ///
    /// NEVER set on a frame drought. PipeWire, like WGC, is change-driven: a static desktop
    /// legitimately produces no frames for an unbounded time, so silence is the most ordinary state
    /// a screen share has and must not be read as death.
    pub closed: Arc<AtomicBool>,
    pub slot: FrameSlot,
    /// Wakes the PipeWire loop so it can quit from another thread. `MainLoop::quit()` is not safe
    /// to call across threads, so teardown is a message the loop processes itself.
    quit: Option<pw::channel::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl ScreenCapture {
    /// Ask the user (via the portal dialog) for a screen to share, then stream it.
    ///
    /// Blocks until the dialog is answered — see the module docs.
    pub fn start_primary() -> Result<ScreenCapture> {
        let slot: FrameSlot = Arc::new(Mutex::new(None));
        let closed = Arc::new(AtomicBool::new(false));
        let (quit_tx, quit_rx) = pw::channel::channel::<()>();
        // The worker reports whether the portal handshake + stream connect succeeded, so
        // start_primary() can fail fast the way the Windows backend does instead of returning a
        // live-looking object that will never produce a frame.
        let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<(), String>>();

        let slot_t = slot.clone();
        let closed_t = closed.clone();
        let thread = thread::Builder::new()
            .name("portal-capture".into())
            .spawn(move || {
                let r = run_capture(slot_t, closed_t.clone(), quit_rx, &ready_tx);
                // Whatever happened, the session is over: latch it so the producer and /health stop
                // claiming video is expected.
                closed_t.store(true, Ordering::Relaxed);
                if let Err(e) = r {
                    tracing::error!("portal screen capture ended: {e:#}");
                    // If setup never reported, report now — otherwise start_primary() would hang.
                    let _ = ready_tx.send(Err(format!("{e:#}")));
                }
            })
            .context("spawn portal-capture thread")?;

        tracing::info!(
            "[capture] waiting for the desktop screen-share dialog (up to {}s)",
            CONSENT_TIMEOUT.as_secs()
        );
        // BOUNDED wait, and this is the whole reason: the portal shows a dialog and blocks until a
        // human answers it. Waiting forever wedges `media::start`, which means the web server never
        // binds — no UI, no audio, nothing — because someone walked away from a prompt. Observed
        // exactly that on a box whose screen had blanked. Windows capture never blocks, so nothing
        // upstream of here is built to tolerate it; failing after a generous timeout keeps the rest
        // of the server alive and audio-only, which is strictly better than dead.
        match ready_rx.recv_timeout(CONSENT_TIMEOUT) {
            Ok(Ok(())) => Ok(ScreenCapture { closed, slot, quit: Some(quit_tx), thread: Some(thread) }),
            Ok(Err(e)) => bail!("{e}"),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("portal capture thread died during setup")
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // The worker is still parked in the portal call and cannot be cancelled from here,
                // so it is deliberately DETACHED rather than joined — teardown must not inherit the
                // hang we are escaping. It owns everything it touches and exits once the dialog is
                // answered or dismissed, finding no receiver.
                bail!(
                    "the screen-share dialog was not answered within {}s. Screen capture on Linux \
                     needs someone to approve it at the machine; there is no way to pre-authorise \
                     it. Start video from the GUI (where you will see the prompt), or relay a \
                     browser's cast with --capture web --video instead",
                    CONSENT_TIMEOUT.as_secs()
                )
            }
        }
    }

    /// Not available on this platform — Wayland gives an unprivileged client no window handle to
    /// address. The portal dialog is what selects the source.
    pub fn start_window(_id: isize) -> Result<ScreenCapture> {
        bail!(
            "capturing a single window is Windows-only. On Linux the desktop portal chooses the \
             source, so pick \"Whole screen\" and select the window in the portal dialog, or relay \
             a browser cast with --capture web --video"
        )
    }
}

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        // Ask the loop to quit, then join. Unlike the audio path there is no uncancelable blocking
        // read here — the PipeWire loop processes our message and returns from run() — so a bounded
        // join is safe and we do not need to detach.
        if let Some(q) = self.quit.take() {
            let _ = q.send(());
        }
        if let Some(t) = self.thread.take() {
            if let Err(e) = t.join() {
                tracing::warn!("portal capture thread panicked on teardown: {e:?}");
            }
        }
    }
}

/// Portal handshake + PipeWire stream, all on the capture thread.
///
/// Everything PipeWire-side is created and destroyed here on purpose: its loop, context and stream
/// are not `Send`, so none of them can be built by the caller and moved in.
fn run_capture(
    slot: FrameSlot,
    closed: Arc<AtomicBool>,
    quit_rx: pw::channel::Receiver<()>,
    ready_tx: &mpsc::Sender<std::result::Result<(), String>>,
) -> Result<()> {
    // ashpd is async (zbus); the rest of this thread is a blocking loop. A current-thread runtime
    // keeps the portal handshake local rather than dragging in the server's worker runtime.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build the portal runtime")?;
    let (node_id, fd) = rt.block_on(open_portal())?;
    tracing::info!("[capture] portal ScreenCast granted; PipeWire node {node_id}");

    pw::init();
    // MainLoopRc, not MainLoopBox: the quit message below needs a handle the callback can own, and
    // only the Rc flavour is Clone (MainLoopBox is single-owner). Both deref to MainLoop::quit().
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| anyhow!("pw mainloop: {e}"))?;
    // "Creation failed" here is almost always a MISSING CLIENT CONFIG rather than anything to do
    // with the daemon: pw_context_new reads /usr/share/pipewire/client.conf, and a distro's
    // libpipewire *-dev* package ships the shared object and the SPA modules WITHOUT that file. Cost
    // an hour of looking at the portal handshake, which was fine. Say it here so nobody repeats it.
    let context = pw::context::ContextBox::new(mainloop.loop_(), None).map_err(|e| {
        anyhow!(
            "could not create a PipeWire client context ({e}). If the log above mentions              client.conf, PipeWire's client configuration is missing — install the full `pipewire`              package, not just its -dev headers"
        )
    })?;
    let core = context.connect_fd(fd, None).map_err(|e| anyhow!("pw connect_fd: {e}"))?;

    // Teardown message: the loop quits itself when Drop sends.
    let _quit = quit_rx.attach(mainloop.loop_(), {
        let ml = mainloop.clone();
        move |_| ml.quit()
    });

    let stream = pw::stream::StreamBox::new(
        &core,
        "newfoundsync-screen",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| anyhow!("pw stream: {e}"))?;

    let _listener = stream
        .add_local_listener_with_user_data(StreamState::default())
        .state_changed({
            let closed = closed.clone();
            move |_, _, old, new| {
                tracing::debug!("portal capture stream: {old:?} -> {new:?}");
                if matches!(new, pw::stream::StreamState::Error(_)) {
                    closed.store(true, Ordering::Relaxed);
                }
            }
        })
        .param_changed(|_, state, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = spa::param::format_utils::parse_format(param)
            else {
                return;
            };
            if media_type != spa::param::format::MediaType::Video
                || media_subtype != spa::param::format::MediaSubtype::Raw
            {
                return;
            }
            if state.format.parse(param).is_err() {
                tracing::warn!("portal capture: could not parse the negotiated video format");
                return;
            }
            let size = state.format.size();
            tracing::info!(
                "[capture] portal video format: {:?} {}x{} @ {}/{}",
                state.format.format(),
                size.width,
                size.height,
                state.format.framerate().num,
                state.format.framerate().denom
            );
        })
        .process({
            let slot = slot.clone();
            move |stream, state| {
                let Some(mut buffer) = stream.dequeue_buffer() else { return };
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }
                let size = state.format.size();
                let (w, h) = (size.width, size.height);
                if w == 0 || h == 0 {
                    return; // format not negotiated yet
                }
                let chunk_stride = datas[0].chunk().stride().max(0) as usize;
                let Some(src) = datas[0].data() else { return };
                let row = w as usize * 4;
                // PipeWire rows are stride-padded; the encoder wants them tightly packed. A stride
                // of 0 means "no padding" rather than "empty row".
                let stride = if chunk_stride == 0 { row } else { chunk_stride };
                if src.len() < stride * (h as usize - 1) + row {
                    return; // short buffer — skip rather than read out of bounds
                }
                let mut bgra = vec![0u8; row * h as usize];
                for y in 0..h as usize {
                    bgra[y * row..(y + 1) * row]
                        .copy_from_slice(&src[y * stride..y * stride + row]);
                }
                if let Ok(mut guard) = slot.lock() {
                    *guard = Some(CapturedFrame { width: w, height: h, bgra });
                }
            }
        })
        .register()
        .map_err(|e| anyhow!("register pw listener: {e}"))?;

    let mut params = format_params();
    let mut pods = [spa::pod::Pod::from_bytes(&params)
        .ok_or_else(|| anyhow!("build the EnumFormat pod"))?];
    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut pods,
        )
        .map_err(|e| anyhow!("connect the PipeWire stream: {e}"))?;
    params.clear();

    // Setup is done and frames are on their way — release start_primary().
    let _ = ready_tx.send(Ok(()));
    mainloop.run();
    Ok(())
}

/// Negotiated format state, shared with the PipeWire callbacks.
#[derive(Default)]
struct StreamState {
    format: spa::param::video::VideoInfoRaw,
}

/// The formats we accept, deliberately narrow: only the two 32-bit BGR layouts.
///
/// The upstream example advertises RGB/RGBA/RGBx/BGRx/YUY2/I420, which means the compositor may pick
/// any of them and the client owes a conversion for each. Asking only for BGRx/BGRA makes the frame
/// path a de-stride memcpy — the exact bytes `bgra_to_i420` already expects — and turns "the
/// compositor chose a format we half-support" from a runtime pixel bug into a negotiation failure at
/// connect time, which is far easier to diagnose. No DMA-BUF modifiers are advertised either, so
/// PipeWire mmaps the buffers and `data()` is plain CPU-readable memory.
fn format_params() -> Vec<u8> {
    let obj = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRA
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle { width: 1920, height: 1080 },
            spa::utils::Rectangle { width: 1, height: 1 },
            spa::utils::Rectangle { width: 7680, height: 4320 }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 60, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 240, denom: 1 }
        ),
    );
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .expect("serializing a fixed, statically-known pod cannot fail")
    .0
    .into_inner()
}

/// The portal half: CreateSession → SelectSources → Start → OpenPipeWireRemote.
async fn open_portal() -> Result<(u32, std::os::fd::OwnedFd)> {
    use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
    use ashpd::desktop::PersistMode;

    let proxy = Screencast::new().await.map_err(|e| {
        anyhow!(
            "no desktop portal available ({e}). Screen capture needs a running desktop session — a \
             headless server cannot do it at all, because the portal's ScreenCast backend lives in \
             the desktop environment. Use --capture web --video to relay a browser cast instead"
        )
    })?;
    let session = proxy.create_session(Default::default()).await.map_err(|e| {
        anyhow!("the desktop portal refused a ScreenCast session ({e}) — is xdg-desktop-portal-kde \
                 (or -gnome, matching your desktop) installed?")
    })?;
    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                // Show the pointer: a screen share without a cursor is much harder to follow.
                .set_cursor_mode(CursorMode::Embedded)
                .set_sources(SourceType::Monitor | SourceType::Window)
                .set_multiple(false)
                .set_restore_token(None)
                .set_persist_mode(PersistMode::DoNot),
        )
        .await
        .map_err(|e| anyhow!("portal SelectSources failed: {e}"))?;

    let response = proxy
        .start(&session, None, Default::default())
        .await
        .map_err(|e| anyhow!("portal Start failed: {e}"))?
        .response()
        .map_err(|e| {
            anyhow!("screen sharing was not granted ({e}) — the portal dialog was dismissed or denied")
        })?;
    let stream = response
        .streams()
        .first()
        .ok_or_else(|| anyhow!("the portal granted the session but selected no stream"))?
        .to_owned();

    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .map_err(|e| anyhow!("portal OpenPipeWireRemote failed: {e}"))?;
    Ok((stream.pipe_wire_node_id(), fd))
}
