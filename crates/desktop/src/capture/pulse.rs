// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Linux system-audio capture via PulseAudio / PipeWire.
//!
//! cpal talks to ALSA, and PipeWire's `<sink>.monitor` sources aren't exposed as ALSA devices —
//! so cpal can't capture "what's playing" on a PipeWire system. This module speaks the PulseAudio
//! protocol instead (which PipeWire implements, and which the Flatpak's `--socket=pulseaudio`
//! grants), finds the DEFAULT SINK, and records its `.monitor` source. It captures the system
//! output only — never the microphone.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use psimple::Simple;
use pulse::callbacks::ListResult;
use pulse::context::{Context, FlagSet as ContextFlagSet, State as ContextState};
use pulse::mainloop::standard::{IterateResult, Mainloop};
use pulse::operation::{Operation, State as OpState};
use pulse::sample::{Format, Spec};
use pulse::stream::{
    Direction, FlagSet as StreamFlagSet, PeekResult, State as StreamState, Stream,
};

/// 20 ms of 48 kHz stereo, in i16 samples (960 frames × 2 channels).
///
/// The whole-system path gets this cadence for free — `pa_simple` reads a fixed-size buffer. The
/// per-app path reads whatever PulseAudio hands it, so it re-blocks to this same size before
/// calling `on_frame`: downstream (the Opus encoder) is fed a uniform frame either way, and the two
/// capture paths stay behaviourally interchangeable.
const FRAME_SAMPLES: usize = 1920;

/// Records the default output sink's monitor. Stops the capture thread on drop.
pub struct PulseCapture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// The capture thread sends `()` here when its loop exits cleanly; Drop uses this to tell a
    /// clean exit from a thread still parked in an uncancelable read (see the Drop impl).
    done_rx: mpsc::Receiver<()>,
    /// The monitor source being recorded (e.g. `alsa_output.pci-….analog-stereo.monitor`).
    pub device_name: String,
}

impl PulseCapture {
    /// Start recording the default sink's monitor at 48 kHz stereo, delivering interleaved i16
    /// frames to `on_frame`. Errors (rather than falling back to the mic) if there is no monitor.
    pub fn start<F>(mut on_frame: F) -> Result<Self>
    where
        F: FnMut(&[i16]) + Send + 'static,
    {
        let monitor = default_sink_monitor().context("resolve the default sink's monitor source")?;
        // Capturing a dummy sink is indistinguishable from a working capture by every signal the
        // server has: the stream runs, frames arrive on a perfect 20 ms cadence, audioFrames climbs,
        // clients connect and report "playing". Observed in the field — a server showing
        // "Live · 1 listening" that nobody could hear, the only clue being the raw sink name leaking
        // into the status line. So name it for what it is, right where the operator is looking.
        //
        // Precisely what is wrong is worth stating carefully, because the obvious phrasing ("this
        // captures silence") is false: the dummy's monitor faithfully carries anything applications
        // play INTO it — verified by playing a tone to `auto_null` and reading it back off
        // `auto_null.monitor` at full amplitude. The real fault is that the machine has no output
        // device, so nobody there hears anything and the capture dries up the moment apps stop.
        let dummy = is_dummy_sink_monitor(&monitor);
        if dummy {
            tracing::error!(
                monitor = %monitor,
                "NO REAL AUDIO OUTPUT DEVICE — PipeWire/PulseAudio has fallen back to its Dummy \
                 Output. Nothing is audible on this machine, and this capture carries only what \
                 applications still play into the dummy — silence whenever nothing does. Either \
                 every sound-card profile is off, or the machine has no sound hardware (usual in a \
                 VM). Check `pactl list cards`: if a card is listed, set a real profile with \
                 `pactl set-card-profile <card> output:analog-stereo`. (Or use --capture web to \
                 relay a browser's audio instead of capturing this machine's.)"
            );
        }
        let spec = Spec { format: Format::S16le, channels: 2, rate: 48_000 };
        if !spec.is_valid() {
            bail!("invalid PulseAudio sample spec");
        }
        // Open the record stream now so start() fails fast if the monitor can't be opened.
        let simple = Simple::new(
            None,                   // default server
            "Newfoundsync",         // application name
            Direction::Record,
            Some(monitor.as_str()), // record the sink's monitor (system output)
            "system audio",         // stream description
            &spec,
            None, // default channel map
            None, // default buffering
        )
        .map_err(|e| anyhow!("open monitor source '{monitor}': {e:?}"))?;

        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let thread = thread::Builder::new()
            .name("pulse-capture".into())
            .spawn(move || {
                // ~20 ms of 48 kHz stereo S16 = 48000 * 2ch * 2B / 50 = 3840 bytes.
                let mut buf = [0u8; 3840];
                let mut samples = vec![0i16; buf.len() / 2];
                let mut reads: u64 = 0;
                let mut peak: i32 = 0;
                while !stop_t.load(Ordering::Relaxed) {
                    if let Err(e) = simple.read(&mut buf) {
                        tracing::error!("pulse monitor read failed: {e:?}");
                        break;
                    }
                    for (i, b) in buf.chunks_exact(2).enumerate() {
                        let s = i16::from_le_bytes([b[0], b[1]]);
                        samples[i] = s;
                        peak = peak.max((s as i32).abs());
                    }
                    // ~every 2 s, confirm frames are flowing and report the recent peak level — a quick
                    // "is the monitor actually carrying sound?" check (0 = silence, not a failure).
                    reads += 1;
                    if reads % 100 == 0 {
                        tracing::debug!("pulse monitor: {reads} reads ok, recent peak={peak}/32767");
                        peak = 0;
                    }
                    on_frame(&samples);
                }
                // Signal a clean exit so Drop can reap us promptly instead of detaching.
                let _ = done_tx.send(());
            })
            .context("spawn pulse-capture thread")?;

        tracing::info!("[capture] Linux monitor capture active: {monitor}");
        // The GUI renders this verbatim as "Serving: …". For the dummy sink say so in words rather
        // than leaving "auto_null.monitor" to be decoded by the reader.
        let device_name = if dummy {
            format!("{monitor}  {}", super::DUMMY_TAG)
        } else {
            monitor
        };
        Ok(PulseCapture { stop, thread: Some(thread), done_rx, device_name })
    }

    /// Start recording ONLY the audio produced by process `pid`, leaving that app's playback to the
    /// speakers untouched.
    ///
    /// This is a deliberately separate path from [`start`](Self::start) rather than a generalisation
    /// of it, for two reasons. First, the whole-system path uses the *blocking simple* API
    /// (`pa_simple`), which has no way to express a per-application filter — the filter lives on
    /// `pa_stream`, so per-app has to drive the async API by hand. Second, the system path is the
    /// default every user hits; folding both into one code path would put every per-app bug in its
    /// way for no gain.
    ///
    /// How the tap works: PulseAudio (and PipeWire's emulation of it) lets a record stream attached
    /// to a *sink monitor* be narrowed to a single sink input via `pa_stream_set_monitor_stream`.
    /// It is a filter on an otherwise ordinary monitor capture — the app keeps playing out the
    /// speakers, nothing is re-routed, and nothing is destroyed if we crash. That rules out the
    /// usual alternative (create a null sink, MOVE the app's stream onto it, loop it back), which
    /// hijacks the user's audio routing and strands it there if the process dies.
    pub fn start_app<F>(pid: u32, mut on_frame: F) -> Result<Self>
    where
        F: FnMut(&[i16]) + Send + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        // Everything PulseAudio-side has to be BUILT INSIDE the thread: `Mainloop`, `Context` and
        // `Stream` are all `!Send`, so they cannot be created here and moved in. That would cost us
        // start()'s fail-fast contract (the caller learns about "no such app" only later, from a
        // log line), so the worker hands back its setup result over this one-shot channel and we
        // block on it — failing right here, exactly like the system path does.
        let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<String, String>>();

        let thread = thread::Builder::new()
            .name("pulse-app-capture".into())
            .spawn(move || {
                let setup = (|| -> Result<(PulseConn, Stream, String)> {
                    let mut conn = connect_pulse("Newfoundsync")?;
                    let target = resolve_sink_input(&mut conn, pid)?;
                    let monitor = sink_monitor_name(&mut conn, target.sink)?;
                    let spec = Spec { format: Format::S16le, channels: 2, rate: 48_000 };
                    if !spec.is_valid() {
                        bail!("invalid PulseAudio sample spec");
                    }
                    let mut stream = Stream::new(&mut conn.context, "app audio", &spec, None)
                        .ok_or_else(|| anyhow!("allocate PulseAudio record stream"))?;
                    // ORDER IS LOAD-BEARING: libpulse requires set_monitor_stream() before
                    // connect_record(). Called after, it is ignored — and an ignored filter is the
                    // worst possible failure here, because capture still "works": the UI says the
                    // app's name, frames flow, listeners hear audio. It would just be everyone's
                    // audio, broadcast to the LAN, with nothing on screen to say so.
                    stream
                        .set_monitor_stream(target.index)
                        .map_err(|e| anyhow!("filter capture to sink-input {}: {e:?}", target.index))?;
                    stream
                        .connect_record(Some(&monitor), None, StreamFlagSet::NOFLAGS)
                        .map_err(|e| anyhow!("connect record stream to '{monitor}': {e:?}"))?;
                    loop {
                        match conn.mainloop.iterate(true) {
                            IterateResult::Success(_) => {}
                            IterateResult::Quit(_) => bail!("mainloop quit connecting the stream"),
                            IterateResult::Err(e) => bail!("mainloop error: {e:?}"),
                        }
                        match stream.get_state() {
                            StreamState::Ready => break,
                            StreamState::Failed => bail!("record stream failed to connect"),
                            StreamState::Terminated => bail!("record stream terminated"),
                            _ => {}
                        }
                    }
                    // Say the scope out loud in the status line. "Firefox" alone reads like a
                    // device name; the operator needs to be able to tell at a glance that the LAN
                    // is getting one app rather than the machine.
                    Ok((conn, stream, format!("{}  — this app only", target.label())))
                })();

                let (mut conn, mut stream, label) = match setup {
                    Ok(v) => {
                        let _ = ready_tx.send(Ok(v.2.clone()));
                        v
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("{e:#}")));
                        return;
                    }
                };
                tracing::info!("[capture] per-app capture active: {label}");

                let mut pending: Vec<i16> = Vec::with_capacity(FRAME_SAMPLES * 2);
                let mut scratch: Vec<i16> = Vec::with_capacity(FRAME_SAMPLES * 2);
                let mut reads: u64 = 0;
                let mut peak: i32 = 0;
                while !stop_t.load(Ordering::Relaxed) {
                    match conn.mainloop.iterate(true) {
                        IterateResult::Success(_) => {}
                        IterateResult::Quit(_) => break,
                        IterateResult::Err(e) => {
                            tracing::error!("pulse per-app mainloop error: {e:?}");
                            break;
                        }
                    }
                    scratch.clear();
                    // Empty must NOT be discarded (there is nothing to drop, and discarding an
                    // empty peek is an error); a Hole must be, or the read pointer never advances.
                    let mut consumed = true;
                    match stream.peek() {
                        Ok(PeekResult::Data(d)) => {
                            scratch.extend(
                                d.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])),
                            );
                        }
                        Ok(PeekResult::Hole(n)) => {
                            tracing::debug!("pulse per-app capture: {n}-byte hole (xrun)");
                        }
                        Ok(PeekResult::Empty) => consumed = false,
                        Err(e) => {
                            tracing::error!("pulse per-app peek failed: {e:?}");
                            break;
                        }
                    }
                    if consumed {
                        let _ = stream.discard();
                    }
                    for s in &scratch {
                        peak = peak.max((*s as i32).abs());
                    }
                    pending.extend_from_slice(&scratch);
                    while pending.len() >= FRAME_SAMPLES {
                        on_frame(&pending[..FRAME_SAMPLES]);
                        pending.drain(..FRAME_SAMPLES);
                        reads += 1;
                        if reads % 100 == 0 {
                            tracing::debug!(
                                "pulse per-app: {reads} frames ok, recent peak={peak}/32767"
                            );
                            peak = 0;
                        }
                    }
                }
                let _ = stream.disconnect();
                let _ = done_tx.send(());
            })
            .context("spawn pulse-app-capture thread")?;

        // Block until the worker reports its setup result (or dies trying).
        let device_name = match ready_rx.recv() {
            Ok(Ok(label)) => label,
            Ok(Err(e)) => bail!("{e}"),
            Err(_) => bail!("per-app capture thread died during setup"),
        };
        Ok(PulseCapture { stop, thread: Some(thread), done_rx, device_name })
    }
}

impl Drop for PulseCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // `pa_simple` read() is blocking and UNCANCELABLE: if the monitor's sink is suspended
        // (module-suspend-on-idle) it can deliver no data, parking the thread mid-read so it never
        // observes `stop`. Wait a bounded time for the clean-exit signal, then DETACH rather than
        // join — teardown (session stop / source switch / shutdown) must never hang. A detached
        // thread owns its pulse stream and exits on its next read (when the sink resumes) or at
        // process exit; its `on_frame` is `'static`, so outliving this struct is memory-safe.
        if self.done_rx.recv_timeout(Duration::from_millis(300)).is_ok() {
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        } else {
            tracing::warn!("pulse capture thread still parked in read() on stop — detaching");
            let _ = self.thread.take(); // drop the JoinHandle without joining
        }
    }
}

/// A live PulseAudio/PipeWire connection: a pumped mainloop plus a ready context.
///
/// Field order is load-bearing — struct fields drop in declaration order, and the context must be
/// torn down before the mainloop that services it.
struct PulseConn {
    context: Context,
    mainloop: Mainloop,
}

impl PulseConn {
    /// Pump the mainloop until `op` finishes. `what` names the query for error messages.
    fn wait_for<T: ?Sized>(&mut self, op: &Operation<T>, what: &str) -> Result<()> {
        loop {
            match self.mainloop.iterate(true) {
                IterateResult::Success(_) => {}
                IterateResult::Quit(_) => bail!("PulseAudio mainloop quit while {what}"),
                IterateResult::Err(e) => bail!("PulseAudio mainloop error while {what}: {e:?}"),
            }
            match op.get_state() {
                OpState::Done => return Ok(()),
                OpState::Cancelled => bail!("PulseAudio query cancelled while {what}"),
                OpState::Running => {}
            }
        }
    }
}

/// Open a connection and pump it until the context is Ready (or has definitively failed).
fn connect_pulse(app_name: &str) -> Result<PulseConn> {
    let mut mainloop = Mainloop::new().ok_or_else(|| anyhow!("PulseAudio mainloop alloc failed"))?;
    let mut context = Context::new(&mainloop, app_name)
        .ok_or_else(|| anyhow!("PulseAudio context alloc failed"))?;
    context
        .connect(None, ContextFlagSet::NOFLAGS, None)
        .map_err(|e| anyhow!("connect to PulseAudio/PipeWire: {e:?}"))?;
    loop {
        match mainloop.iterate(true) {
            IterateResult::Success(_) => {}
            IterateResult::Quit(_) => bail!("PulseAudio mainloop quit during connect"),
            IterateResult::Err(e) => bail!("PulseAudio mainloop error: {e:?}"),
        }
        match context.get_state() {
            ContextState::Ready => break,
            ContextState::Failed | ContextState::Terminated => {
                bail!("PulseAudio/PipeWire not available (context failed)")
            }
            _ => {}
        }
    }
    Ok(PulseConn { context, mainloop })
}

/// One application's playback stream — what PulseAudio calls a "sink input".
///
/// NOTE the lifetime of this concept, because it drives the whole UI design: a sink input exists
/// only while an application is actually *playing*. It is not a process listing. Pause the music
/// and the entry disappears; resume and it comes back with a NEW index. So an index is a handle
/// valid only right now, never something to persist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SinkInput {
    /// Ephemeral server-side id — the thing `set_monitor_stream` takes.
    pub index: u32,
    /// The sink it plays to; capture must attach to *this* sink's monitor, not the default one
    /// (an app can be routed to a different output than everything else).
    pub sink: u32,
    /// `application.process.id`, when the client bothered to set it.
    pub pid: Option<u32>,
    /// `application.name`, e.g. "Firefox".
    pub name: String,
    /// `application.process.binary`, e.g. "firefox". Frequently disagrees with `name` — a symlinked
    /// launcher reports `paplay` as the name and `pacat` as the binary — so neither alone is a
    /// trustworthy identity.
    pub binary: String,
}

impl SinkInput {
    /// Human label for the status line / picker. Falls back through the identity properties,
    /// because native-PipeWire and JACK clients routinely set none of them.
    pub fn label(&self) -> String {
        for candidate in [&self.name, &self.binary] {
            if !candidate.trim().is_empty() {
                return candidate.clone();
            }
        }
        format!("audio stream #{}", self.index)
    }
}

/// Every application currently producing audio.
pub fn list_sink_inputs() -> Result<Vec<SinkInput>> {
    let mut conn = connect_pulse("Newfoundsync-apps")?;
    let found: Arc<Mutex<Vec<SinkInput>>> = Arc::new(Mutex::new(Vec::new()));
    let cb = found.clone();
    let op = conn.context.introspect().get_sink_input_info_list(move |res| {
        if let ListResult::Item(info) = res {
            let p = &info.proplist;
            let name = p.get_str("application.name").unwrap_or_default();
            let binary = p.get_str("application.process.binary").unwrap_or_default();
            let pid = p.get_str("application.process.id").and_then(|s| s.parse::<u32>().ok());
            cb.lock().unwrap().push(SinkInput {
                index: info.index,
                sink: info.sink,
                pid,
                name,
                binary,
            });
        }
    });
    conn.wait_for(&op, "listing sink inputs")?;
    let out = found.lock().unwrap().clone();
    Ok(out)
}

/// Find the sink input belonging to `pid`.
///
/// One process can own several streams (a browser playing two tabs); we take the first, which is
/// the only choice available without asking the user to disambiguate streams they cannot see.
fn resolve_sink_input(conn: &mut PulseConn, pid: u32) -> Result<SinkInput> {
    let found: Arc<Mutex<Vec<SinkInput>>> = Arc::new(Mutex::new(Vec::new()));
    let cb = found.clone();
    let op = conn.context.introspect().get_sink_input_info_list(move |res| {
        if let ListResult::Item(info) = res {
            let p = &info.proplist;
            cb.lock().unwrap().push(SinkInput {
                index: info.index,
                sink: info.sink,
                pid: p.get_str("application.process.id").and_then(|s| s.parse::<u32>().ok()),
                name: p.get_str("application.name").unwrap_or_default(),
                binary: p.get_str("application.process.binary").unwrap_or_default(),
            });
        }
    });
    conn.wait_for(&op, "resolving the selected application")?;
    let all = found.lock().unwrap().clone();
    all.iter().find(|s| s.pid == Some(pid)).cloned().ok_or_else(|| {
        // Deliberately NOT falling back to whole-system capture. Someone who picked one app would
        // be silently switched to broadcasting everything — the exact privacy surprise the Windows
        // path already refuses to make (see poll_refresh in gui.rs).
        anyhow!(
            "no audio stream from PID {pid} — the app must be actively playing to be captured \
             (currently playing: {})",
            if all.is_empty() {
                "nothing".to_string()
            } else {
                all.iter().map(|s| s.label()).collect::<Vec<_>>().join(", ")
            }
        )
    })
}

/// The monitor source name of the sink with index `sink`.
fn sink_monitor_name(conn: &mut PulseConn, sink: u32) -> Result<String> {
    let found: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let cb = found.clone();
    let op = conn.context.introspect().get_sink_info_by_index(sink, move |res| {
        if let ListResult::Item(info) = res {
            if let Some(n) = info.monitor_source_name.as_ref() {
                *cb.lock().unwrap() = Some(n.to_string());
            }
        }
    });
    conn.wait_for(&op, "resolving the app's output sink")?;
    let name = found.lock().unwrap().clone();
    name.ok_or_else(|| anyhow!("sink {sink} reports no monitor source"))
}

/// Connect to PulseAudio/PipeWire, read the default sink name, and return `<sink>.monitor`.
fn default_sink_monitor() -> Result<String> {
    // Kept as a whole `PulseConn` rather than destructured: the two fields must drop in that
    // struct's declared order (context, then mainloop), and separate locals would drop in reverse.
    let mut conn = connect_pulse("Newfoundsync-probe")?;

    let sink: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let sink_cb = sink.clone();
    let op = conn.context.introspect().get_server_info(move |info| {
        if let Some(name) = info.default_sink_name.as_ref() {
            *sink_cb.lock().unwrap() = Some(name.to_string());
        }
    });
    conn.wait_for(&op, "querying server info")?;

    let sink = sink
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| anyhow!("no default sink reported — is any audio output active?"))?;

    // SELF-HEAL: if the default is the dummy sink, look for a real one and use that instead.
    //
    // The default being `auto_null` does NOT imply the machine has no audio. It happens whenever the
    // session manager couldn't pick a device at the moment it decided — a card that enumerated late, a
    // Bluetooth headset that hadn't connected yet, a fresh VM whose first sink appears seconds after
    // login, a profile that was off when the session started. In all of those a perfectly good sink is
    // present and simply isn't the default, and capturing the dummy would stream silence past a UI that
    // says "Live". So rather than trusting the default blindly, fall back to any real sink.
    if is_dummy_sink_monitor(&sink) {
        let sinks: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sinks_cb = sinks.clone();
        let op = conn.context.introspect().get_sink_info_list(move |res| {
            if let ListResult::Item(info) = res {
                if let Some(name) = info.name.as_ref() {
                    sinks_cb.lock().unwrap().push(name.to_string());
                }
            }
        });
        conn.wait_for(&op, "listing sinks")?;
        let all = sinks.lock().unwrap().clone();
        if let Some(real) = all.iter().find(|s| !is_dummy_sink_monitor(s)) {
            tracing::warn!(
                default_sink = %sink,
                using = %real,
                "the default output is the Dummy Output, but a real sink exists — capturing that \
                 instead so this doesn't silently stream silence"
            );
            return Ok(format!("{real}.monitor"));
        }
        // Nothing to fall back to: every card profile really is off. The caller warns loudly and
        // names it in the UI; we still return it so the operator can fix the profile live rather
        // than having the server refuse to start.
        tracing::warn!(sinks = ?all, "no real sink to fall back to — only the Dummy Output exists");
    }
    Ok(format!("{sink}.monitor"))
}

/// Is this monitor source the dummy/null sink's?
///
/// PipeWire and PulseAudio both synthesise a placeholder sink when no card offers a usable profile —
/// `auto_null` in practice, described as "Dummy Output". Capturing its monitor yields perfect silence,
/// which no operator ever wants, so it is worth calling out by name.
///
/// Matched narrowly on the known names rather than "contains null": a real device is unlikely but not
/// forbidden to have "null" in its name, and a false positive here would slander a working capture.
fn is_dummy_sink_monitor(monitor: &str) -> bool {
    let sink = monitor.strip_suffix(".monitor").unwrap_or(monitor);
    sink == "auto_null" || sink == "null" || sink.starts_with("auto_null.")
}

#[cfg(test)]
mod tests {
    use super::is_dummy_sink_monitor;

    #[test]
    fn detects_the_pipewire_pulse_dummy_sink() {
        // What the field failure actually looked like.
        assert!(is_dummy_sink_monitor("auto_null.monitor"));
        assert!(is_dummy_sink_monitor("auto_null"));
        assert!(is_dummy_sink_monitor("null"));
    }

    #[test]
    fn does_not_slander_a_real_device() {
        // The real sink from the same machine once its card profile was switched back on.
        assert!(!is_dummy_sink_monitor("alsa_output.pci-0000_00_1f.3.analog-stereo.monitor"));
        assert!(!is_dummy_sink_monitor("alsa_output.pci-0000_01_00.1.hdmi-stereo.monitor"));
        // A device that merely CONTAINS "null" is not the dummy.
        assert!(!is_dummy_sink_monitor("alsa_output.usb-Nullsoft_DAC.analog-stereo.monitor"));
        assert!(!is_dummy_sink_monitor("bluez_output.AC_BF_71_00_00_00.1.monitor"));
    }
}
