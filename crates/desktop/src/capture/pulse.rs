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
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use newfoundsync_core::config::FRAME_DURATION_MS;
use psimple::Simple;
use pulse::callbacks::ListResult;
use pulse::context::{Context, FlagSet as ContextFlagSet, State as ContextState};
use pulse::mainloop::standard::{IterateResult, Mainloop};
use pulse::operation::{Operation, State as OpState};
use pulse::sample::{Format, Spec};
use pulse::stream::{
    Direction, FlagSet as StreamFlagSet, PeekResult, State as StreamState, Stream,
};

/// 20 ms of 48 kHz stereo in BYTES (960 frames x 2 channels x 2 bytes). Requested as the
/// PulseAudio fragment size AND used as the read buffer, so the two cannot drift apart.
const FRAME_BYTES: usize = FRAME_SAMPLES * 2;

/// ASK FOR A 20 ms FRAGMENT. Passing a null buffer attr (the server's default) is what made Linux
/// audio stutter: PulseAudio chose a ~340 ms fragment for the monitor source, so our 20 ms reads
/// returned SEVENTEEN AT A TIME with no delay and then blocked for a third of a second.
///
/// The frame count stayed perfect — 51 frames/s, zero errors, /health looked healthy — but the
/// cadence was a burst. That matters because the audio PTS is stamped when a frame is published, so
/// seventeen frames read within a millisecond all claim nearly the SAME presentation time. The client
/// is then told to play 340 ms of audio at one instant, plays one frame and discards the rest, and you
/// hear audio cutting in and out roughly twice a second. Measured at the client: 94% of frames
/// arriving <1 ms apart, p99 gap 340 ms.
///
/// Windows never showed this because WASAPI loopback delivers on a steady ~10-20 ms cadence, which is
/// the regular delivery a publish-time PTS quietly assumes.
///
/// SHARED by both capture paths on purpose. The first fix passed this to `Simple::new` only, and the
/// per-app path (`start_app`, a different libpulse API) silently kept the ~340 ms default — the same
/// bug, still shipping, in the source you reach by picking a single application.
const FRAGMENT_ATTR: pulse::def::BufferAttr = pulse::def::BufferAttr {
    // The only one that matters for a RECORD stream: how many bytes the server hands over at a time.
    fragsize: FRAME_BYTES as u32,
    // u32::MAX means "server default"; maxlength is a cap, and tlength/prebuf/minreq are playback-only.
    maxlength: u32::MAX,
    tlength: u32::MAX,
    prebuf: u32::MAX,
    minreq: u32::MAX,
};

/// 20 ms of 48 kHz stereo, in i16 samples (960 frames × 2 channels).
///
/// Both capture paths hand `on_frame` exactly this many samples, so the Opus encoder sees a uniform
/// frame and the two paths stay behaviourally interchangeable.
///
/// A fixed READ SIZE does not by itself buy a fixed cadence — that was the assumption behind the
/// Linux stutter. `pa_simple` returns as soon as the server has data, so with a large server-side
/// fragment it returns many reads instantly and then blocks. The cadence comes from asking for a
/// matching `fragsize` (see `BufferAttr` in `start`), not from the size of this buffer.
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
    /// Keeps the "Newfoundsync" output device alive while this capture records it (`None` for the
    /// other sources). Declared LAST on purpose: fields drop in declaration order, after our own
    /// `Drop` has stopped the reader thread, so the device is removed only once nothing reads it.
    _virtual_sink: Option<VirtualSinkHandle>,
}

impl PulseCapture {
    /// Start recording the default sink's monitor at 48 kHz stereo, delivering interleaved i16
    /// frames to `on_frame`. Errors (rather than falling back to the mic) if there is no monitor.
    pub fn start<F>(on_frame: F) -> Result<Self>
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
        // Open the record stream now so start() fails fast if the monitor can't be opened.
        let simple = open_monitor(&monitor)?;
        tracing::info!("[capture] Linux monitor capture active: {monitor}");
        // The GUI renders this verbatim as "Serving: …". For the dummy sink say so in words rather
        // than leaving "auto_null.monitor" to be decoded by the reader.
        let device_name = if dummy {
            format!("{monitor}  {}", super::DUMMY_TAG)
        } else {
            monitor
        };
        Self::spawn_reader(simple, device_name, None, on_frame)
    }

    /// Create the "Newfoundsync" output device (a virtual sink) and record ONLY what applications
    /// play into it.
    ///
    /// This is the source for "stream this app, and keep it quiet on this machine". An app whose
    /// output is set to the device is heard by the clients and by nobody here — which is the point:
    /// the direct path plays `buffer_ms` EARLIER than every synced client, so leaving it audible
    /// doubles the audio. To listen on this machine, open the web client here like any other device.
    ///
    /// Why this does not contradict the note on [`start_app`](Self::start_app), which rejects the
    /// null-sink approach: that rejection is about MOVING a stream the operator never asked us to
    /// touch. Here we move nothing. We publish a device and the operator points apps at it, the
    /// same way they would pick a Bluetooth speaker. See [`VirtualSinkHandle`] for the lifetime.
    pub fn start_virtual_sink<F>(on_frame: F) -> Result<Self>
    where
        F: FnMut(&[i16]) + Send + 'static,
    {
        let sink = VirtualSinkHandle::acquire().context("create the Newfoundsync output device")?;
        let monitor = format!("{VIRTUAL_SINK_NAME}.monitor");
        // The module load is acknowledged before the session manager has necessarily finished
        // publishing the new sink's monitor source, so the first open can lose that race. Retry
        // briefly rather than failing a start that would have worked 100 ms later.
        let mut attempt = 0;
        let simple = loop {
            match open_monitor(&monitor) {
                Ok(s) => break s,
                Err(e) if attempt >= 20 => return Err(e), // `sink` drops here and removes the device
                Err(_) => {
                    attempt += 1;
                    thread::sleep(Duration::from_millis(100));
                }
            }
        };
        tracing::info!(
            "[capture] Newfoundsync output device active: set an application's output to \
             \"{VIRTUAL_SINK_DESCRIPTION}\" to stream it (it is silent on this machine)"
        );
        let mut device_name =
            format!("\"{VIRTUAL_SINK_DESCRIPTION}\" output device — only apps sent to it");
        // If the device is the DEFAULT output, everything plays into it — including a browser on
        // this machine listening to the stream, whose output we would capture and send round again,
        // one buffer later, forever. Nothing else can detect that, so say it where it will be read.
        if default_sink_is_virtual() {
            tracing::warn!(
                "the Newfoundsync output device is this machine's DEFAULT output. Everything plays \
                 into it, so a browser listening on this machine would be re-captured and echo. \
                 Pick your real speakers as the default and send only chosen apps to Newfoundsync."
            );
            device_name.push_str("  ⚠ it is the DEFAULT output: listening on this PC will echo");
        }
        Self::spawn_reader(simple, device_name, Some(sink), on_frame)
    }

    /// The blocking-read capture loop shared by every whole-monitor source.
    fn spawn_reader<F>(
        simple: Simple,
        device_name: String,
        virtual_sink: Option<VirtualSinkHandle>,
        mut on_frame: F,
    ) -> Result<Self>
    where
        F: FnMut(&[i16]) + Send + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let thread = thread::Builder::new()
            .name("pulse-capture".into())
            .spawn(move || {
                // Same size we asked the server for, via the shared constant.
                let mut buf = [0u8; FRAME_BYTES];
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
        Ok(PulseCapture {
            stop,
            thread: Some(thread),
            done_rx,
            device_name,
            _virtual_sink: virtual_sink,
        })
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
                    // Same 20 ms fragment the whole-system path asks for. Without it this path gets
                    // the server default (~340 ms on a monitor source) and the burst below re-blocks
                    // it into ~17 frames emitted back-to-back, all claiming nearly one instant.
                    stream
                        .connect_record(
                            Some(&monitor),
                            Some(&FRAGMENT_ATTR),
                            StreamFlagSet::ADJUST_LATENCY,
                        )
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
                // KEEP THE TIMELINE ALIVE WHILE THE APP IS QUIET.
                //
                // A per-app tap is attached to one sink-input, and a sink-input that is not playing
                // delivers NOTHING — not zeros. Pause a video, switch tabs, let a track end, and
                // PulseAudio simply stops handing us data. Without this the whole server stops
                // publishing: every connected client starves at the same instant, the stall watchdog
                // fires ("AUDIO HAS STOPPED"), and the clients do not recover on their own — they
                // need a page reload, which is exactly what was reported from three devices at once.
                //
                // The whole-system monitor never shows this, because an idle sink still renders
                // zeros. So the per-app path has to synthesise what the monitor gets for free: after
                // a short grace period, emit silent frames on the normal 20 ms cadence until real
                // audio comes back. This mirrors the Windows process-capture path, which silence-pads
                // on underrun for the same reason.
                let silence = [0i16; FRAME_SAMPLES];
                let frame_dur = Duration::from_millis(FRAME_DURATION_MS as u64);
                // Wait this long after the last real frame before synthesising. Generous enough that
                // ordinary delivery jitter never injects a spurious silent frame between two real
                // ones (which would be an audible click and an extra 20 ms of timeline).
                let quiet_grace = frame_dur * 3;
                let mut next_due = Instant::now() + quiet_grace;
                let mut padding = false;
                while !stop_t.load(Ordering::Relaxed) {
                    // NON-blocking: a blocking iterate parks here until PulseAudio has something to
                    // say, and a corked sink-input says nothing — so the padding below would never
                    // get to run, which is the whole point of it.
                    match conn.mainloop.iterate(false) {
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
                    let had_data = !scratch.is_empty();
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
                    if had_data {
                        // Real audio resets the clock; padding only starts after a fresh grace period.
                        if padding {
                            tracing::info!("[capture] per-app: audio resumed, stopped padding silence");
                            padding = false;
                        }
                        next_due = Instant::now() + quiet_grace;
                    } else if Instant::now() >= next_due {
                        // Never try to CATCH UP a long gap. `next_due += frame_dur` advances by one
                        // frame per pass, so after a stall (thread descheduled, machine suspended,
                        // a slow encode blocking on_frame) the loop would run flat out emitting one
                        // silent frame per iteration until it caught up — a burst. Every frame in
                        // that burst is stamped from `mono_now()` when it is PUBLISHED, so they all
                        // claim nearly the same instant, and the client is handed a second of audio
                        // to play at one moment. That is exactly the PTS collapse the 20 ms fragment
                        // request was added to cure; re-creating it here would be a bad joke.
                        //
                        // So on a gap of more than a few frames, abandon the old schedule and resume
                        // from now. The same guard the Windows process-capture path uses.
                        let now = Instant::now();
                        if now > next_due + quiet_grace {
                            next_due = now;
                        }
                        if !padding {
                            // INFO, not debug: this is the difference between "the app is paused" and
                            // "capture is broken", and it is the first thing to check when a listener
                            // says the sound went away.
                            tracing::info!(
                                "[capture] per-app: the app went quiet — sending silence to keep \
                                 clients in sync (audio returns on its own when it plays again)"
                            );
                            padding = true;
                        }
                        on_frame(&silence);
                        next_due += frame_dur;
                    }
                    if !had_data {
                        // Non-blocking iterate would otherwise spin a core. Well under one frame, so
                        // the padding cadence above stays accurate.
                        thread::sleep(Duration::from_millis(2));
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
        Ok(PulseCapture { stop, thread: Some(thread), done_rx, device_name, _virtual_sink: None })
    }
}

/// Open a 48 kHz stereo record stream on `monitor` with the shared 20 ms fragment.
fn open_monitor(monitor: &str) -> Result<Simple> {
    let spec = Spec { format: Format::S16le, channels: 2, rate: 48_000 };
    if !spec.is_valid() {
        bail!("invalid PulseAudio sample spec");
    }
    Simple::new(
        None,           // default server
        "Newfoundsync", // application name
        Direction::Record,
        Some(monitor),  // record the sink's monitor (never the mic)
        "system audio", // stream description
        &spec,
        None,                 // default channel map
        Some(&FRAGMENT_ATTR), // 20 ms fragments — see the constant
    )
    .map_err(|e| anyhow!("open monitor source '{monitor}': {e:?}"))
}

// ---- The "Newfoundsync" output device -----------------------------------------------------------

/// The sink's machine name — what `pactl` and `<name>.monitor` use.
pub const VIRTUAL_SINK_NAME: &str = "newfoundsync";
/// What the desktop's output-device picker shows. No spaces: it is passed inside a module argument
/// string, where a space ends the property.
pub const VIRTUAL_SINK_DESCRIPTION: &str = "Newfoundsync";

/// Arguments for `module-null-sink`. 48 kHz stereo matches the capture, so nothing is resampled.
fn virtual_sink_args() -> String {
    format!(
        "sink_name={VIRTUAL_SINK_NAME} channels=2 rate=48000 \
         sink_properties=device.description={VIRTUAL_SINK_DESCRIPTION}"
    )
}

/// Is this loaded module our output device? Matched on the exact `sink_name=` token, so somebody
/// else's null sink (`sink_name=newfoundsync_test`, or one that merely mentions us in a description)
/// is never adopted — and therefore never unloaded — by us.
fn is_virtual_sink_module(name: &str, argument: &str) -> bool {
    let want = format!("sink_name={VIRTUAL_SINK_NAME}");
    name == "module-null-sink" && argument.split_whitespace().any(|a| a == want)
}

/// The one device this process owns: its module index, and how many captures are recording it.
struct VirtualSinkState {
    module: u32,
    users: usize,
}

/// Process-wide, because the device is: one sink, however many `PulseCapture`s come and go.
static VIRTUAL_SINK: Mutex<Option<VirtualSinkState>> = Mutex::new(None);

fn virtual_sink_state() -> std::sync::MutexGuard<'static, Option<VirtualSinkState>> {
    // A panic elsewhere while holding this must not stop us REMOVING the device on the way out.
    VIRTUAL_SINK.lock().unwrap_or_else(|e| e.into_inner())
}

/// A claim on the "Newfoundsync" output device. The device exists while at least one is alive.
///
/// COUNTED rather than owned outright, because switching settings starts the new capture BEFORE the
/// old one is dropped (so the stream never goes dark between them). With plain ownership, pressing
/// Apply while this source is selected would have the outgoing capture remove the device the
/// incoming one had just started recording — and every app routed to it would fall back to the
/// speakers, undoing the reason the operator chose this source.
///
/// THE DEVICE OUTLIVES A CRASH. The sound server owns a loaded module, not the client that asked
/// for it, so nothing removes it if this process dies without running the code below — and an app
/// routed to it then plays to nobody. Three things bound that:
/// * a clean stop, source switch, window close or SIGINT/SIGTERM/SIGHUP removes it (see
///   [`remove_virtual_sink`] and `install_exit_cleanup`);
/// * the next start ADOPTS a leftover device instead of creating a second one, so restarting the
///   server heals it and apps already routed there simply resume;
/// * it is never made the default output, so only apps the operator deliberately sent are affected.
///
/// SIGKILL and power loss are not covered. `pactl unload-module module-null-sink` clears it by hand.
pub struct VirtualSinkHandle(());

impl VirtualSinkHandle {
    fn acquire() -> Result<Self> {
        let mut state = virtual_sink_state();
        if let Some(s) = state.as_mut() {
            s.users += 1;
            return Ok(VirtualSinkHandle(()));
        }
        let mut conn = connect_pulse("Newfoundsync-sink")?;
        let module = match find_virtual_sink_module(&mut conn)? {
            Some(idx) => {
                tracing::info!(
                    module = idx,
                    "reusing the Newfoundsync output device left by a previous run"
                );
                idx
            }
            None => load_virtual_sink_module(&mut conn)?,
        };
        *state = Some(VirtualSinkState { module, users: 1 });
        install_exit_cleanup();
        Ok(VirtualSinkHandle(()))
    }
}

impl Drop for VirtualSinkHandle {
    fn drop(&mut self) {
        let mut state = virtual_sink_state();
        // `None` here means remove_virtual_sink() already took it down (process exit).
        let Some(s) = state.as_mut() else { return };
        s.users = s.users.saturating_sub(1);
        if s.users == 0 {
            let module = s.module;
            *state = None;
            // Still under the lock: a concurrent acquire() must not adopt the module we are
            // half-way through unloading.
            unload_virtual_sink_module(module);
        }
    }
}

/// Remove the output device NOW, whoever still holds a claim. For process exit only: neither a
/// closed GUI window nor a signal runs `Drop` on the live capture, and the device would otherwise
/// be left behind with apps still playing into it. A no-op if the device was never created.
pub fn remove_virtual_sink() {
    let mut state = virtual_sink_state();
    if let Some(s) = state.take() {
        unload_virtual_sink_module(s.module);
    }
}

fn find_virtual_sink_module(conn: &mut PulseConn) -> Result<Option<u32>> {
    let found: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));
    let found_cb = found.clone();
    let op = conn.context.introspect().get_module_info_list(move |res| {
        if let ListResult::Item(m) = res {
            let name = m.name.as_deref().unwrap_or_default();
            let argument = m.argument.as_deref().unwrap_or_default();
            if is_virtual_sink_module(name, argument) {
                // Keep the FIRST: if a crash ever left two, they are equivalent to us.
                found_cb.lock().unwrap().get_or_insert(m.index);
            }
        }
    });
    conn.wait_for(&op, "listing modules")?;
    let idx = *found.lock().unwrap();
    Ok(idx)
}

fn load_virtual_sink_module(conn: &mut PulseConn) -> Result<u32> {
    let loaded: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));
    let loaded_cb = loaded.clone();
    let args = virtual_sink_args();
    let op = conn.context.introspect().load_module("module-null-sink", &args, move |idx| {
        *loaded_cb.lock().unwrap() = Some(idx);
    });
    conn.wait_for(&op, "creating the Newfoundsync output device")?;
    let idx = *loaded.lock().unwrap();
    match idx {
        // PA_INVALID_INDEX is how the server says "refused".
        Some(idx) if idx != u32::MAX => {
            tracing::info!(module = idx, "created the Newfoundsync output device");
            Ok(idx)
        }
        _ => bail!(
            "the sound server refused to create a virtual output device (module-null-sink). \
             Workaround: `pactl load-module module-null-sink {args}`, then start this source again"
        ),
    }
}

/// Best effort by design — this runs on teardown and at exit, where there is nobody to return an
/// error to and nothing useful to do with one except say so.
fn unload_virtual_sink_module(module: u32) {
    let result = (|| -> Result<bool> {
        let mut conn = connect_pulse("Newfoundsync-sink")?;
        let ok: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
        let ok_cb = ok.clone();
        let op = conn.context.introspect().unload_module(module, move |success| {
            *ok_cb.lock().unwrap() = success;
        });
        conn.wait_for(&op, "removing the Newfoundsync output device")?;
        let ok = *ok.lock().unwrap();
        Ok(ok)
    })();
    match result {
        Ok(true) => tracing::info!(module, "removed the Newfoundsync output device"),
        Ok(false) => tracing::warn!(
            module,
            "the sound server would not remove the Newfoundsync output device \
             (already gone?) — `pactl unload-module module-null-sink` clears a leftover"
        ),
        Err(e) => tracing::warn!(
            module,
            "could not remove the Newfoundsync output device: {e:#} — \
             `pactl unload-module module-null-sink` clears a leftover"
        ),
    }
}

/// Remove the device when the process is told to stop.
///
/// Installed only once a device exists, so every other source keeps the default signal behaviour
/// it always had. A signal's default action kills the process without unwinding, which is exactly
/// the path that would strand the device; this turns the three polite ones into "clean up, then
/// exit".
fn install_exit_cleanup() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let spawned = thread::Builder::new().name("sink-cleanup".into()).spawn(|| {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_io().build() {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::warn!("no signal cleanup for the output device (runtime: {e})");
                    return;
                }
            };
            let code = rt.block_on(async {
                use tokio::signal::unix::{signal, SignalKind};
                let (mut int, mut term, mut hup) = match (
                    signal(SignalKind::interrupt()),
                    signal(SignalKind::terminate()),
                    signal(SignalKind::hangup()),
                ) {
                    (Ok(i), Ok(t), Ok(h)) => (i, t, h),
                    _ => return None,
                };
                // 130 is what a shell reports for Ctrl-C. The other two are requests to stop that
                // we honour cleanly, so they exit 0 — a service manager must not read it as a crash.
                Some(tokio::select! {
                    _ = int.recv() => 130,
                    _ = term.recv() => 0,
                    _ = hup.recv() => 0,
                })
            });
            let Some(code) = code else {
                tracing::warn!("no signal cleanup for the output device (could not hook signals)");
                return;
            };
            remove_virtual_sink();
            std::process::exit(code);
        });
        if let Err(e) = spawned {
            tracing::warn!("no signal cleanup for the output device (thread: {e})");
        }
    });
}

/// Is the output device currently this machine's default output? Best effort: `false` on any
/// failure, because the answer only decides whether to print a warning.
fn default_sink_is_virtual() -> bool {
    let name = (|| -> Result<Option<String>> {
        let mut conn = connect_pulse("Newfoundsync-probe")?;
        default_sink_name(&mut conn)
    })();
    matches!(name, Ok(Some(n)) if n == VIRTUAL_SINK_NAME)
}

/// The server's default sink, if it reports one.
fn default_sink_name(conn: &mut PulseConn) -> Result<Option<String>> {
    let sink: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let sink_cb = sink.clone();
    let op = conn.context.introspect().get_server_info(move |info| {
        if let Some(name) = info.default_sink_name.as_ref() {
            *sink_cb.lock().unwrap() = Some(name.to_string());
        }
    });
    conn.wait_for(&op, "querying server info")?;
    let name = sink.lock().unwrap().clone();
    Ok(name)
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
    connect_pulse_with(app_name, false)
}

/// Open a connection that is allowed to MOVE other applications' streams, even from a sandbox.
///
/// Inside a Flatpak the session manager gives a client read-and-call access only: creating our own
/// device is allowed, but moving somebody else's stream is refused ("Access denied" — measured on
/// WirePlumber 0.5.19, and it is what the first build of the app list ran into). WirePlumber's stock
/// policy lifts that for a sandboxed client that declares `media.category = Manager`; it is the
/// route sandboxed mixers and patchbays take. Outside a sandbox the property changes nothing.
///
/// Used for the routing connection ONLY. Every other connection stays an ordinary client, so the
/// wider access exists for the fraction of a second a routing pass takes and nowhere else.
fn connect_pulse_as_manager(app_name: &str) -> Result<PulseConn> {
    connect_pulse_with(app_name, true)
}

fn connect_pulse_with(app_name: &str, manager: bool) -> Result<PulseConn> {
    let mut mainloop = Mainloop::new().ok_or_else(|| anyhow!("PulseAudio mainloop alloc failed"))?;
    let context = if manager {
        let mut props = pulse::proplist::Proplist::new()
            .ok_or_else(|| anyhow!("PulseAudio proplist alloc failed"))?;
        props
            .set_str("media.category", "Manager")
            .map_err(|()| anyhow!("could not declare the routing connection as a manager"))?;
        Context::new_with_proplist(&mainloop, app_name, &props)
    } else {
        Context::new(&mainloop, app_name)
    };
    let mut context = context.ok_or_else(|| anyhow!("PulseAudio context alloc failed"))?;
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

// ---- Sending apps to the output device ----------------------------------------------------------
//
// Most players have no output-device setting of their own (Finamp does not), and the desktop's
// volume mixer hides the control two menus deep under a name the operator may not recognise —
// Finamp's stream is called "mpv". So the server window lists what is playing and moves it itself.

/// One playback stream, reduced to what routing needs.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PlaybackStream {
    index: u32,
    sink: u32,
    /// See [`route_key`].
    key: String,
    /// `media.name` — what the stream says it is playing. Often the only place the application the
    /// operator recognises shows up: Finamp's stream is `application.name = mpv`,
    /// `media.name = Finamp`.
    media: String,
}

/// The identity a routing choice is remembered under: `application.name`, else the binary.
///
/// Deliberately NOT the PID the capture picker uses. Sandboxed (Flatpak) and native-PipeWire clients
/// report no `application.process.id` at all — Finamp is both — and a PID would not survive a
/// restart of the app anyway, which is exactly when a remembered choice has to be re-applied.
/// Coarser than a stream on purpose, too: the sound server's own memory of where an app plays is
/// keyed the same way, so two keys could not be made to disagree for long.
fn route_key(app_name: &str, binary: &str) -> Option<String> {
    [app_name, binary].iter().map(|s| s.trim()).find(|s| !s.is_empty()).map(str::to_string)
}

/// One application in the "send to the output device" list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutedApp {
    /// What the choice is remembered under — see [`route_key`].
    pub key: String,
    /// What to show: the key, plus what its streams say they are playing when that adds anything.
    pub label: String,
    /// Every one of its streams plays into the output device right now.
    pub on_device: bool,
}

/// What [`sync_routes`] found, after doing what it was asked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RouteSnapshot {
    /// The output device exists. `false` means its source is not live, so nothing can be sent yet.
    pub device_live: bool,
    /// Every application playing audio, sorted by name.
    pub apps: Vec<RoutedApp>,
    /// Set when a move was refused, worded for the operator.
    pub problem: Option<String>,
}

/// Media names that say nothing about which application this is.
const GENERIC_MEDIA_NAMES: [&str; 4] = ["playback", "playback stream", "audiostream", "audio stream"];

/// A media name cut down to something a checkbox label can carry. Command-line players report the
/// whole path of the file they are playing; the file's own name is the part that identifies it.
fn short_media_name(media: &str) -> String {
    const MAX_CHARS: usize = 40;
    let m = media.trim();
    let m = if m.starts_with('/') { m.rsplit('/').next().unwrap_or(m) } else { m };
    if m.chars().count() <= MAX_CHARS {
        return m.to_string();
    }
    let head: String = m.chars().take(MAX_CHARS - 1).collect();
    format!("{}…", head.trim_end())
}

fn group_routed_apps(streams: &[PlaybackStream], device: Option<u32>) -> Vec<RoutedApp> {
    let mut keys: Vec<&str> = streams.iter().map(|s| s.key.as_str()).collect();
    keys.sort_by_key(|k| k.to_lowercase());
    keys.dedup();
    keys.into_iter()
        .map(|key| {
            let mine: Vec<&PlaybackStream> = streams.iter().filter(|s| s.key == key).collect();
            let mut media: Vec<String> = Vec::new();
            for s in &mine {
                let m = short_media_name(&s.media);
                let says_something = !m.is_empty()
                    && !m.eq_ignore_ascii_case(key)
                    && !GENERIC_MEDIA_NAMES.iter().any(|g| m.eq_ignore_ascii_case(g));
                if says_something && !media.contains(&m) {
                    media.push(m);
                }
            }
            let label = match media.len() {
                0 => key.to_string(),
                1 | 2 => format!("{key}  —  {}", media.join(", ")),
                // A browser with a dozen noisy tabs: name two, admit to the rest.
                n => format!("{key}  —  {}, {} and {} more", media[0], media[1], n - 2),
            };
            let on_device = device.is_some_and(|d| mine.iter().all(|s| s.sink == d));
            RoutedApp { key: key.to_string(), label, on_device }
        })
        .collect()
}

/// A stream to move: onto the output device, or back off it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RouteMove {
    index: u32,
    to_device: bool,
}

/// Which streams have to move so that `wanted` apps play into the device and `release`d ones do not.
/// An app in both is released — the operator's latest word was "no".
fn plan_route_moves(
    streams: &[PlaybackStream],
    device: u32,
    wanted: &[String],
    release: &[String],
) -> Vec<RouteMove> {
    streams
        .iter()
        .filter_map(|s| {
            let on_device = s.sink == device;
            if release.contains(&s.key) {
                on_device.then_some(RouteMove { index: s.index, to_device: false })
            } else if wanted.contains(&s.key) {
                (!on_device).then_some(RouteMove { index: s.index, to_device: true })
            } else {
                None
            }
        })
        .collect()
}

/// Make the sound server match the operator's choices, and report what is playing.
///
/// Apps in `wanted` are moved onto the output device; apps in `release` are moved back to the
/// default output. Safe to call repeatedly — it only moves what is in the wrong place — and it HAS
/// to be called repeatedly: the sound server remembers where an app plays for streams the app opens
/// later, but a stream that was already open when the device (re)appeared stays on the speakers
/// until someone moves it. Measured on PipeWire 1.6 / WirePlumber 0.5: the device was removed and
/// re-created under a routed stream, and the stream stayed on the fallback sink.
///
/// With the device absent nothing is moved, and `device_live` says so.
pub fn sync_routes(wanted: &[String], release: &[String]) -> Result<RouteSnapshot> {
    let mut conn = connect_pulse_as_manager("Newfoundsync-routes")?;
    let device = sink_index_by_name(&mut conn, VIRTUAL_SINK_NAME)?;
    let mut streams = list_playback_streams(&mut conn)?;
    let Some(device) = device else {
        return Ok(RouteSnapshot {
            device_live: false,
            apps: group_routed_apps(&streams, None),
            problem: None,
        });
    };

    let mut refused: Vec<String> = Vec::new();
    let mut problem: Option<String> = None;
    let mut away: Option<String> = None;
    for mv in plan_route_moves(&streams, device, wanted, release) {
        let target = if mv.to_device {
            VIRTUAL_SINK_NAME.to_string()
        } else {
            if away.is_none() {
                match release_target(&mut conn) {
                    Ok(t) => away = Some(t),
                    Err(e) => {
                        problem = Some(format!("{e:#}"));
                        break;
                    }
                }
            }
            away.clone().unwrap_or_default()
        };
        let moved = move_stream(&mut conn, mv.index, &target)?;
        let Some(s) = streams.iter_mut().find(|s| s.index == mv.index) else { continue };
        if moved {
            // Recorded by hand rather than re-listed: the server acknowledges a move before the
            // stream is actually relinked, so an immediate re-list still shows the old sink.
            // PA_INVALID_INDEX stands for "somewhere that is not the device".
            s.sink = if mv.to_device { device } else { u32::MAX };
        } else if !refused.contains(&s.key) {
            // Usually a stream that ended between the listing and the move; the next pass shows the
            // truth either way.
            refused.push(s.key.clone());
        }
    }
    if problem.is_none() && !refused.is_empty() {
        problem = Some(format!(
            "the sound server would not move: {} — set its output to \"{VIRTUAL_SINK_DESCRIPTION}\" \
             in your system volume mixer instead",
            refused.join(", ")
        ));
    }
    Ok(RouteSnapshot { device_live: true, apps: group_routed_apps(&streams, Some(device)), problem })
}

/// Where a released stream goes: the default output — unless the operator made the output device
/// itself the default, in which case "default" would send it straight back.
fn release_target(conn: &mut PulseConn) -> Result<String> {
    let default = default_sink_name(conn)?;
    if default.as_deref() != Some(VIRTUAL_SINK_NAME) {
        // By alias, not by name: PipeWire reads a move to the alias as "follow the default again",
        // where a move to the named sink would pin the app to that one device for good.
        return Ok("@DEFAULT_SINK@".to_string());
    }
    sink_names(conn)?
        .into_iter()
        .find(|s| s != VIRTUAL_SINK_NAME && !is_dummy_sink_monitor(s))
        .ok_or_else(|| {
            anyhow!(
                "\"{VIRTUAL_SINK_DESCRIPTION}\" is the default output and there is no other device \
                 to send the app back to"
            )
        })
}

fn move_stream(conn: &mut PulseConn, index: u32, sink_name: &str) -> Result<bool> {
    let ok: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    let ok_cb = ok.clone();
    let op = conn.context.introspect().move_sink_input_by_name(
        index,
        sink_name,
        Some(Box::new(move |success| {
            *ok_cb.lock().unwrap() = success;
        })),
    );
    conn.wait_for(&op, "moving an app's audio")?;
    let ok = *ok.lock().unwrap();
    Ok(ok)
}

/// The index of the sink called `name`, or `None` if there is no such sink.
fn sink_index_by_name(conn: &mut PulseConn, name: &str) -> Result<Option<u32>> {
    let found: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));
    let cb = found.clone();
    let op = conn.context.introspect().get_sink_info_by_name(name, move |res| {
        if let ListResult::Item(info) = res {
            *cb.lock().unwrap() = Some(info.index);
        }
    });
    conn.wait_for(&op, "looking for the Newfoundsync output device")?;
    let idx = *found.lock().unwrap();
    Ok(idx)
}

fn sink_names(conn: &mut PulseConn) -> Result<Vec<String>> {
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
    Ok(all)
}

fn list_playback_streams(conn: &mut PulseConn) -> Result<Vec<PlaybackStream>> {
    let found: Arc<Mutex<Vec<PlaybackStream>>> = Arc::new(Mutex::new(Vec::new()));
    let cb = found.clone();
    let op = conn.context.introspect().get_sink_input_info_list(move |res| {
        if let ListResult::Item(info) = res {
            let p = &info.proplist;
            let name = p.get_str("application.name").unwrap_or_default();
            let binary = p.get_str("application.process.binary").unwrap_or_default();
            // A stream with neither cannot be remembered or told apart, so it is not offered.
            let Some(key) = route_key(&name, &binary) else { return };
            cb.lock().unwrap().push(PlaybackStream {
                index: info.index,
                sink: info.sink,
                key,
                media: info.name.as_deref().unwrap_or_default().to_string(),
            });
        }
    });
    conn.wait_for(&op, "listing what is playing")?;
    let out = found.lock().unwrap().clone();
    Ok(out)
}

/// Connect to PulseAudio/PipeWire, read the default sink name, and return `<sink>.monitor`.
fn default_sink_monitor() -> Result<String> {
    // Kept as a whole `PulseConn` rather than destructured: the two fields must drop in that
    // struct's declared order (context, then mainloop), and separate locals would drop in reverse.
    let mut conn = connect_pulse("Newfoundsync-probe")?;

    let sink = default_sink_name(&mut conn)?
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
        let all = sink_names(&mut conn)?;
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
    use super::{
        group_routed_apps, is_dummy_sink_monitor, is_virtual_sink_module, plan_route_moves,
        route_key, virtual_sink_args, PlaybackStream, RouteMove,
    };

    const DEVICE: u32 = 700;
    const SPEAKERS: u32 = 62;

    fn stream(index: u32, sink: u32, key: &str, media: &str) -> PlaybackStream {
        PlaybackStream { index, sink, key: key.into(), media: media.into() }
    }

    #[test]
    fn an_app_is_remembered_by_name_not_by_process() {
        // Measured: Finamp (a Flatpak, playing through libmpv) reports application.name = "mpv" and
        // no process id or binary at all — a PID-keyed list cannot even show it.
        assert_eq!(route_key("mpv", ""), Some("mpv".to_string()));
        assert_eq!(route_key("  ", "pacat"), Some("pacat".to_string()));
        // Nothing to call it: it cannot be remembered, so it is not offered.
        assert_eq!(route_key("", " "), None);
    }

    #[test]
    fn the_list_names_what_the_operator_would_recognise() {
        let apps = group_routed_apps(
            &[
                stream(1, SPEAKERS, "mpv", "Finamp"),
                stream(2, SPEAKERS, "Google Chrome", "Playback"),
                stream(3, DEVICE, "Google Chrome", "Playback"),
            ],
            Some(DEVICE),
        );
        let labels: Vec<&str> = apps.iter().map(|a| a.label.as_str()).collect();
        // Sorted, one row per app; "Playback" says nothing, "Finamp" is the whole point.
        assert_eq!(labels, ["Google Chrome", "mpv  —  Finamp"]);
        // Only half of Chrome is on the device, so it is not "on" it.
        assert!(apps.iter().all(|a| !a.on_device));
    }

    #[test]
    fn a_long_media_name_does_not_take_over_the_window() {
        // Seen in the window: `paplay` names its stream after the full path of the file.
        let apps = group_routed_apps(
            &[stream(1, SPEAKERS, "paplay", "/tmp/some/very/deep/scratch/directory/silence.wav")],
            None,
        );
        assert_eq!(apps[0].label, "paplay  —  silence.wav");
        let title = "x".repeat(200);
        let apps = group_routed_apps(&[stream(1, SPEAKERS, "Firefox", &title)], None);
        assert!(apps[0].label.chars().count() < 60, "{}", apps[0].label);
        assert!(apps[0].label.ends_with('…'));
    }

    #[test]
    fn nothing_is_on_a_device_that_does_not_exist() {
        let apps = group_routed_apps(&[stream(1, DEVICE, "mpv", "Finamp")], None);
        assert!(!apps[0].on_device);
        let apps = group_routed_apps(&[stream(1, DEVICE, "mpv", "Finamp")], Some(DEVICE));
        assert!(apps[0].on_device);
    }

    #[test]
    fn moves_only_what_is_in_the_wrong_place() {
        let streams = [
            stream(1, SPEAKERS, "mpv", "Finamp"),     // wanted, not there yet
            stream(2, DEVICE, "mpv", "Finamp"),       // wanted, already there
            stream(3, DEVICE, "Firefox", "a video"),  // released, still there
            stream(4, SPEAKERS, "Firefox", "a song"), // released, already gone
            stream(5, DEVICE, "Spotify", ""),         // sent by the system mixer: not ours to touch
        ];
        let moves =
            plan_route_moves(&streams, DEVICE, &["mpv".to_string()], &["Firefox".to_string()]);
        assert_eq!(
            moves,
            [RouteMove { index: 1, to_device: true }, RouteMove { index: 3, to_device: false }]
        );
        // The second pass has nothing left to do — this runs on a timer, so it must settle.
        let settled = [stream(1, DEVICE, "mpv", "Finamp"), stream(3, SPEAKERS, "Firefox", "")];
        assert!(plan_route_moves(&settled, DEVICE, &["mpv".to_string()], &[]).is_empty());
    }

    #[test]
    fn unticking_beats_a_stale_tick() {
        let streams = [stream(1, DEVICE, "mpv", "Finamp")];
        let both = ["mpv".to_string()];
        assert_eq!(
            plan_route_moves(&streams, DEVICE, &both, &both),
            [RouteMove { index: 1, to_device: false }]
        );
    }

    #[test]
    fn recognises_only_our_own_output_device() {
        // Whatever we load, we must be able to find again — that is what makes a restart adopt a
        // leftover device instead of stacking a second one next to it.
        assert!(is_virtual_sink_module("module-null-sink", &virtual_sink_args()));
        assert!(is_virtual_sink_module("module-null-sink", "sink_name=newfoundsync"));
        // Adopting means we will UNLOAD it later, so near-misses must not match.
        assert!(!is_virtual_sink_module("module-null-sink", "sink_name=newfoundsync_test"));
        assert!(!is_virtual_sink_module("module-null-sink", "sink_name=other"));
        assert!(!is_virtual_sink_module(
            "module-null-sink",
            "sink_name=other sink_properties=device.description=newfoundsync"
        ));
        assert!(!is_virtual_sink_module("module-loopback", "sink_name=newfoundsync"));
        assert!(!is_virtual_sink_module("module-null-sink", ""));
    }

    #[test]
    fn our_output_device_is_never_mistaken_for_the_dummy() {
        // The dummy banner means "this machine has no real output". Our device is a deliberate
        // choice, and tagging it as a fault would send the operator hunting for a missing sound card.
        assert!(!is_dummy_sink_monitor("newfoundsync.monitor"));
    }

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
