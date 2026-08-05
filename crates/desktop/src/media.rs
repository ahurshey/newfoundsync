// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Media producer: drives the existing capture + encode pipeline and publishes
//! ready-to-send WebSocket frames onto broadcast channels. Each WebSocket client
//! task subscribes and forwards; the browser does the buffering, clock-sync, and
//! decode. This keeps the heavy sync work (cpal/WGC callbacks, encoders) on
//! dedicated threads, bridged to the async web server via `tokio::broadcast`.
//!
//! Wire frames (binary, server→browser):
//!   audio: [0x01][pts i64 BE][Opus bytes]
//!   video: [0x02][pts i64 BE][flags u8][codec bytes — AV1 OBU or VP9 for a native capture,
//!          H.264 Annex-B for a web-uplink cast; the exact codec is advertised in MediaConfig.video_codec]

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::sync::broadcast;

use newfoundsync_core::codec::{CodecKind, Encoder};
use newfoundsync_core::config::{mono_now, CHANNELS, SAMPLE_RATE};
use newfoundsync_core::video::{EncodeDevice, EncoderBackend, VideoConfig};

#[cfg(not(target_os = "linux"))]
use crate::capture::system::SystemCapture;

/// Message tags (first byte of each broadcast/WS frame).
pub const MSG_AUDIO: u8 = 0x01;
pub const MSG_VIDEO: u8 = 0x02;

/// One ready-to-send WebSocket binary frame.
pub type Frame = Arc<Vec<u8>>;

/// How often a repeating hot-path failure may log. Encode errors recur per frame (30–60/s), so an
/// unthrottled `warn!` would flood the log — but logging them at `debug!` (which the default `info`
/// filter drops) is what made a failing encoder produce silence with NOTHING in the log.
const HOT_LOG_EVERY: Duration = Duration::from_secs(5);

/// Rate-limiter for a hot-path log line: emits the first occurrence immediately, then at most one per
/// [`HOT_LOG_EVERY`], reporting how many were suppressed in between (so the log shows the true rate).
struct LogThrottle {
    next: Option<Instant>,
    suppressed: u64,
}

impl LogThrottle {
    const fn new() -> LogThrottle {
        LogThrottle { next: None, suppressed: 0 }
    }

    /// `Some(suppressed_since_last_line)` when the caller should log now; `None` to stay quiet.
    fn tick(&mut self) -> Option<u64> {
        let now = Instant::now();
        match self.next {
            Some(t) if now < t => {
                self.suppressed += 1;
                None
            }
            _ => {
                let n = std::mem::take(&mut self.suppressed);
                self.next = Some(now + HOT_LOG_EVERY);
                Some(n)
            }
        }
    }
}

/// Liveness + failure counters for the running pipeline, shared with the web layer (via
/// [`crate::webserver::StreamState`]) so `/health` can answer "is this thing actually producing
/// media?" without a human standing in front of a speaker.
///
/// Acted on by the web layer's stall watchdog (see `webserver::spawn_stall_watchdog`), which logs a
/// stopped pipeline and surfaces it on `/status` and `/health`.
///
/// One thing deliberately NOT done: withdrawing the advertised `video: true` when
/// `video_encoder_failed` is set. It looked necessary, but the client already handles it — if no frame
/// arrives within 6 s it swaps the dead video stage for the audio visualizer and swaps back
/// automatically if video ever appears (`noVideoFallbackTimer` in app.js). So the user-visible symptom
/// is already handled, and the operator now learns about it from the `error!` log and `/health`.
/// Republishing a whole new `StreamState` to correct one boolean would reconnect every client for no
/// gain.
///
/// # What these counters do and do NOT prove
///
/// Be precise here: a diagnostic that overstates what it knows is worse than none, because you trust
/// it while it misleads you.
/// * `audio_frames` counts frames the pipeline PUBLISHED, not audible sound. The default
///   `--capture allapps` source pads silence to keep a steady 20 ms cadence, so this keeps climbing
///   through a silent or dead device. It proves the pipeline is turning, not that anyone can hear it.
///   (A peak-level field would settle that; it belongs with the reliability phase.)
/// * `video_frames` counts ENCODER output. The producer re-encodes its last captured frame when the
///   capture goes idle, so this also climbs while the picture is frozen — which is why
///   `capture_frames` is tracked separately: it only advances when a genuinely NEW frame arrives from
///   the OS capture, so `video_frames` climbing while `capture_frames` is flat means capture died,
///   not the encoder.
#[derive(Debug, Default)]
pub struct MediaHealth {
    /// Encoded audio frames successfully published.
    pub audio_frames: AtomicU64,
    /// Encoded video frames successfully published (see the caveat on the struct).
    pub video_frames: AtomicU64,
    /// NEW frames taken from the OS capture slot — the honest measure of capture liveness, since
    /// `video_frames` keeps climbing off a stale frame after capture stops.
    pub capture_frames: AtomicU64,
    /// `mono_now()` when the last NEW capture frame arrived; 0 = none yet.
    pub last_capture_ns: AtomicI64,
    /// Audio encode failures since start.
    pub audio_errors: AtomicU64,
    /// Video encode failures since start.
    pub video_errors: AtomicU64,
    /// `mono_now()` at the moment the last audio frame was published; 0 = none yet.
    pub last_audio_ns: AtomicI64,
    /// `mono_now()` at the moment the last video frame was published; 0 = none yet.
    pub last_video_ns: AtomicI64,
    /// Set once the video encoder failed to initialize — video is then off for the life of this
    /// stream even though clients were told `video: true`.
    pub video_encoder_failed: AtomicBool,
    /// Set when Windows CLOSED the screen-capture session (the shared window/display went away or
    /// access was revoked). Mirrors `ScreenCapture::closed`.
    ///
    /// This is the authoritative capture-death signal, and the reason `capture_frames` must NOT be used
    /// to infer one: WGC delivery is change-driven, so a static screen legitimately produces no frames
    /// at all for an unbounded time. A still slide is the most ordinary state a screen-share has, and a
    /// silence-based check reports it as a fault.
    pub capture_closed: AtomicBool,
    /// True when this stream is expected to be producing video at all (video on, encoder alive). Lets
    /// the watchdog tell "video is off" from "video died".
    pub video_expected: AtomicBool,
}

impl MediaHealth {
    /// Note a published audio frame. Takes NO timestamp on purpose: these stamps must be the
    /// *publish instant*, and the caller's nearest value is the wire PTS — which is
    /// `mono_now() + lead_ns`, i.e. ~50 ms in the FUTURE. Storing that made `/health` report a
    /// negative age for a perfectly healthy stream, and could even land on the `-1` "no frames yet"
    /// sentinel. Reading the clock here makes that mistake unrepresentable.
    fn note_audio(&self) {
        self.audio_frames.fetch_add(1, Ordering::Relaxed);
        self.last_audio_ns.store(mono_now(), Ordering::Relaxed);
    }

    /// Note a published video frame. Same no-timestamp rule as [`MediaHealth::note_audio`].
    fn note_video(&self) {
        self.video_frames.fetch_add(1, Ordering::Relaxed);
        self.last_video_ns.store(mono_now(), Ordering::Relaxed);
    }

    /// Note a NEW frame arriving from the OS capture (not an encode).
    fn note_capture(&self) {
        self.capture_frames.fetch_add(1, Ordering::Relaxed);
        self.last_capture_ns.store(mono_now(), Ordering::Relaxed);
    }

    /// Milliseconds since the last published AUDIO frame, or `None` if none has ever been published.
    ///
    /// The `None` case is load-bearing, not a nicety: "never produced a frame" is *waiting* (a web-cast
    /// source with no caster yet, or the first moments after Apply) and must not be reported as a
    /// fault. A stall is specifically "it WAS producing and stopped".
    pub fn audio_stall_ms(&self) -> Option<i64> {
        Self::age(self.last_audio_ns.load(Ordering::Relaxed))
    }

    /// Milliseconds since the last published VIDEO frame, or `None` if none ever was. Note that video
    /// legitimately produces nothing when it's switched off, which is why `None` must stay distinct.
    pub fn video_stall_ms(&self) -> Option<i64> {
        Self::age(self.last_video_ns.load(Ordering::Relaxed))
    }

    /// Milliseconds since the last NEW frame arrived from the OS capture. Compare against
    /// [`MediaHealth::video_stall_ms`]: video fresh + capture stale = the screen is frozen and the
    /// encoder is dutifully re-encoding one stale frame.
    pub fn capture_stall_ms(&self) -> Option<i64> {
        Self::age(self.last_capture_ns.load(Ordering::Relaxed))
    }

    fn age(last_ns: i64) -> Option<i64> {
        if last_ns == 0 {
            return None; // never produced — waiting, not stalled
        }
        Some((mono_now() - last_ns).max(0) / 1_000_000)
    }
}

/// Where the shared audio comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureSource {
    /// Default output device's mix via endpoint loopback. Goes silent when the
    /// Windows output is muted.
    System,
    /// Every app except this one, via process loopback. Survives output mute.
    AllExceptSelf,
    /// A single application (and its child processes), via process loopback.
    /// Survives output mute.
    App { pid: u32 },
    /// No local capture — the audio/video is cast UP from a web client over the
    /// WebSocket and relayed (via [`CastRelay`]) onto the same broadcast channels.
    WebUplink,
}

/// What the screen-video capture grabs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoTarget {
    /// The whole primary monitor (default).
    PrimaryMonitor,
    /// A single window, identified by its raw `HWND` value (from [`CaptureSource`]'s picker).
    Window { hwnd: isize },
}

/// Static config the server hands each browser on connect (as JSON).
#[derive(Clone, Debug)]
pub struct MediaConfig {
    pub name: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub audio_codec: &'static str,
    pub video: bool,
    pub frame_rate: u32,
    pub buffer_ms: i64,
    pub video_codec: String,
}

impl MediaConfig {
    pub fn to_json(&self) -> String {
        format!(
            "{{\"name\":\"{}\",\"sampleRate\":{},\"channels\":{},\"audioCodec\":\"{}\",\"video\":{},\"frameRate\":{},\"bufferMs\":{},\"videoCodec\":\"{}\"}}",
            self.name.replace('\\', "\\\\").replace('"', "'"), // escape backslash first → valid JSON
            self.sample_rate,
            self.channels,
            self.audio_codec,
            self.video,
            self.frame_rate,
            self.buffer_ms,
            self.video_codec,
        )
    }
}

/// Keeps the capture + encode threads alive; channels handed to the web server.
pub struct Media {
    pub config: MediaConfig,
    pub audio_tx: broadcast::Sender<Frame>,
    pub video_tx: broadcast::Sender<Frame>,
    _audio_capture: AudioCapture,
    #[cfg(any(target_os = "windows", all(target_os = "linux", feature = "linux-capture"), all(target_os = "macos", feature = "mac-capture")))]
    _video: Option<VideoProducer>,
    pub capture_device: String,
    /// Present only for [`CaptureSource::WebUplink`]: the web layer pushes a casting
    /// client's already-encoded frames here, which get re-stamped + fanned out like a
    /// local capture. `None` for local sources.
    pub cast_relay: Option<Arc<CastRelay>>,
    /// Liveness/failure counters for this stream, surfaced by `/health`.
    pub health: Arc<MediaHealth>,
}

/// Relays a casting web client's ALREADY-ENCODED frames onto the broadcast channels,
/// re-stamping each with a fresh server-clock PTS so they're indistinguishable from
/// a local capture (receivers' clock-sync/buffer/decode need no changes). The client
/// did the encoding to the server-dictated quality; the server never decodes.
pub struct CastRelay {
    audio_tx: broadcast::Sender<Frame>,
    video_tx: broadcast::Sender<Frame>,
    lead_ns: i64,
    // Encode targets handed to the caster in the CAST_GRANT (server-dictated quality, so all
    // receivers get the operator's settings regardless of the caster's hardware).
    pub audio_bps: u32,
    pub sample_rate: u32,
    pub channels: u8,
    pub video_on: bool,
    pub width: u16,
    pub height: u16,
    pub fps: u8,
    pub video_kbps: u32,
    /// Same counters as the local capture path, so `/health` reports a relayed cast identically.
    health: Arc<MediaHealth>,
}

impl CastRelay {
    /// Wrap+fan-out one Opus packet uploaded by the caster. Mirrors the local audio path.
    pub fn push_audio(&self, opus: &[u8]) {
        let pts = mono_now() + self.lead_ns;
        let mut msg = Vec::with_capacity(9 + opus.len());
        msg.push(MSG_AUDIO);
        msg.extend_from_slice(&pts.to_be_bytes());
        msg.extend_from_slice(opus);
        let _ = self.audio_tx.send(Arc::new(msg)); // Err only if no clients
        self.health.note_audio();
    }

    /// Wrap+fan-out one H.264 access unit (Annex-B) uploaded by the caster (Phase 2). The keyframe
    /// flag is RE-DERIVED from the bitstream (never trusted from the caster's wire byte) so a buggy
    /// or hostile caster can't mislabel frames and strand receivers on a black frame — matching the
    /// local capture path (which also scans via `is_keyframe`). Web casts are always H.264 (avc1).
    pub fn push_video(&self, h264: &[u8]) {
        if !self.video_on {
            return; // audio-only relay must never emit video (defense-in-depth; webserver also gates this)
        }
        let key = crate::video::relay::annexb_has_h264_idr(h264);
        let pts = mono_now() + self.lead_ns;
        let mut msg = Vec::with_capacity(10 + h264.len());
        msg.push(MSG_VIDEO);
        msg.extend_from_slice(&pts.to_be_bytes());
        msg.push(if key { 1 } else { 0 });
        msg.extend_from_slice(h264);
        let _ = self.video_tx.send(Arc::new(msg));
        self.health.note_video();
    }
}

/// Settings for starting the media pipeline.
pub struct MediaOptions {
    pub name: String,
    pub codec: CodecKind,
    pub bitrate: i32,
    pub lead_ms: i64,
    pub buffer_ms: i64,
    pub capture_source: CaptureSource,
    pub video: Option<VideoConfig>,
    /// What the screen-video capture grabs (whole monitor or a single window).
    pub video_target: VideoTarget,
    pub encoder: EncoderBackend,
    /// Where video encodes (GPU / CPU / Auto). Audio is unaffected.
    pub encode_device: EncodeDevice,
    /// A/V offset in NANOSECONDS, added to every audio PTS. Positive = audio plays LATER.
    ///
    /// Shared and read per frame so the operator can tune it by ear while the stream runs — an
    /// Apply-and-restart cycle makes a control like this unusable, because judging lip-sync needs
    /// continuous playback.
    ///
    /// It exists because the audio and video we capture do not describe the same instant, and the
    /// gap is not derivable. A player lip-syncs its own output, so the frame on screen at time T
    /// matches the sound HEARD at T — which was written to the sink earlier, at T - L. The monitor
    /// hands us that audio at write time, so our audio runs L ahead of our video, and L depends on
    /// the sound card, the playing app's buffering and the compositor. PipeWire reports 0 usec for
    /// every latency here, so there is nothing honest to read it from. Screen-capture tools all
    /// solve this the same way — OBS calls it "audio sync offset".
    pub av_offset_ns: Arc<AtomicI64>,
}

/// Resolve the requested encoder against what this BUILD can actually do, once, before anything
/// consumes it.
///
/// VP9 links libvpx and is behind the (non-default) `vp9` feature, so a build without it cannot honor
/// `--encoder vp9`. It must be resolved here rather than at the encoder, because two independent
/// consumers depend on the answer: the encoder we construct, and the `videoCodec` string we ADVERTISE
/// to clients. If those disagree the server emits AV1 while telling browsers to decode `vp09` — every
/// client then fails to decode, which is far worse than the wrong codec. Resolving once makes the
/// mismatch unrepresentable.
/// The presentation timestamp for one encoded video frame.
///
/// A FRESH frame is stamped from its own capture time, so the PTS states when the picture existed
/// rather than when the encoder finished with it (the poll wait + scale + encode are tens of ms, and
/// vary per frame — that difference lands directly on lip-sync).
///
/// A REPEAT — the re-encode of `last` that happens when the source delivered nothing this tick —
/// extends the LAST published stamp by the real time that has actually elapsed. That telescopes to
/// `captured + elapsed + lead`, which keeps a repeat on the SAME reference as the fresh frames around
/// it while staying anchored to the real clock.
///
/// Both halves of that matter, and three earlier attempts each got one of them wrong:
///   * Stepping by one NOMINAL frame duration per repeat. Real ticks are slower than nominal whenever
///     encoding is (measured: 38 ms actual against a 33 ms nominal), so on an idle screen the PTS fell
///     ~5 ms further into the past every frame and ran away without bound — 7 s behind inside one 18 s
///     run, after which the client drops everything as past-due.
///   * Seeding that timeline from zero, which stamped the first frame ~56 years in the past.
///   * Stamping a repeat at plain `now`. Anchored, but on a DIFFERENT reference than fresh frames: a
///     repeat then sits one capture latency ahead of what the next fresh frame claims, so that fresh
///     frame steps back BEHIND an already-queued repeat. The client's `videoStep` only inspects
///     `vq[0]`, so a non-monotonic queue does not simply cost one past-due frame — the genuinely new
///     picture is drained together with the later-stamped repeat and shown a frame late.
///
/// Elapsed-time extension has none of those: it cannot drift (it tracks the real clock however slow
/// the ticks get), it needs no estimate of the capture latency (the anchor carries it), and it stays
/// monotonic — so the client's queue stays ordered.
fn video_pts(
    fresh: bool,
    captured_ns: i64,
    now_ns: i64,
    lead_ns: i64,
    prev: Option<(i64, i64)>,
) -> i64 {
    if fresh {
        return captured_ns + lead_ns;
    }
    match prev {
        Some((prev_pts, prev_now)) => prev_pts + (now_ns - prev_now),
        // Nothing published yet: no reference to extend, and the held frame's capture time could be
        // seconds stale. The screen still looks like that, so `now` is the honest stamp.
        None => now_ns + lead_ns,
    }
}

fn resolve_encoder(requested: EncoderBackend) -> EncoderBackend {
    #[cfg(not(feature = "vp9"))]
    if matches!(requested, EncoderBackend::Vp9) {
        tracing::warn!(
            "VP9 was requested but this build has no VP9 support (built without the `vp9` feature, \
             which links libvpx); using AV1 — also royalty-free — and advertising av01 to clients. \
             Rebuild with `--features vp9` for VP9."
        );
        return EncoderBackend::Av1;
    }
    requested
}

/// Start capture + encode, returning the broadcast channels the web server fans
/// out to browser WebSocket clients.
pub fn start(opts: MediaOptions) -> Result<Media> {
    // Resolve BEFORE building the encoder or the advertised codec string (see resolve_encoder).
    let opts = MediaOptions { encoder: resolve_encoder(opts.encoder), ..opts };
    let lead_ns = opts.lead_ms.max(0) * 1_000_000;
    // Bounded ring; each WS client task forwards immediately, so it only needs to
    // cover momentary scheduling jitter (the *browser* holds the big buffer).
    let (audio_tx, _) = broadcast::channel::<Frame>(512);
    let (video_tx, _) = broadcast::channel::<Frame>(256);

    // Web-uplink source = no local capture; a casting web client's already-encoded
    // frames arrive over the WebSocket and are relayed onto these same channels.
    let web_uplink = matches!(opts.capture_source, CaptureSource::WebUplink);

    let health = Arc::new(MediaHealth::default());

    // --- audio producer (skipped for web uplink) ------------------------
    let (audio_capture, capture_device) = if web_uplink {
        (AudioCapture::None, "Web client cast".to_string())
    } else {
        let mut encoder = Encoder::new(opts.codec, opts.bitrate).context("build audio encoder")?;
        let audio_pub = audio_tx.clone();
        let audio_health = health.clone();
        let av_offset = opts.av_offset_ns.clone();
        let mut enc_err = LogThrottle::new();
        let on_frame = move |frame: &[i16]| {
            // When the FIRST sample in this frame was captured. The frame spans the 20 ms BEFORE the
            // callback fires — the buffer had to fill before we were handed it — and the client
            // schedules a frame's playback to START at its PTS. So stamping `mono_now()` here
            // described the frame's LAST sample and played the whole frame one frame-time late:
            // systematic, and it lands entirely on A/V sync as 20 ms of audio lag against video that
            // has no matching delay.
            //
            // Derived from the frame's own length rather than FRAME_DURATION_MS so a short frame
            // (source shutting down, a resampler flush) is stamped for what it actually contains.
            //
            // KNOWN FLOOR, not an exact age. It is exact for the Linux monitor path, where a read
            // returns one fragment of just-captured audio. It is a LOWER BOUND for
            // `capture::process` (the Windows `allapps`/`app` source, and the CLI default): that one
            // drains WASAPI packets into a VecDeque and pops frames off the FRONT on a wall-clock
            // schedule, bounding occupancy at four frames — so the true age is
            // `span + current occupancy`, up to 80 ms more than claimed, and the residual sits
            // wherever the producer/consumer clock mismatch parks that queue. Closing it properly
            // means each backend stamping its own capture time and passing it through `on_frame`, the
            // way the video path now does with `captured_ns`.
            let span_ns = (frame.len() / CHANNELS) as i64 * 1_000_000_000 / SAMPLE_RATE as i64;
            let capture_ns = mono_now() - span_ns;
            // FFI callback (cpal/WGC) — trap panics so they can't unwind across C.
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| match encoder.encode(frame) {
                Ok(payload) => {
                    // + the operator's A/V offset (see MediaOptions::av_offset_ns). Read per frame,
                    // so dragging the slider re-times the stream immediately.
                    let pts = capture_ns + lead_ns + av_offset.load(Ordering::Relaxed);
                    let mut msg = Vec::with_capacity(9 + payload.len());
                    msg.push(MSG_AUDIO);
                    msg.extend_from_slice(&pts.to_be_bytes());
                    msg.extend_from_slice(&payload);
                    let _ = audio_pub.send(Arc::new(msg)); // Err only if no clients
                    audio_health.note_audio();
                }
                Err(e) => {
                    // WARN, not debug: a persistently failing encoder makes the whole room silent, and
                    // at debug! this was invisible under the default `info` filter — the console a user
                    // sends back had nothing in it. Throttled so a per-frame failure can't flood.
                    audio_health.audio_errors.fetch_add(1, Ordering::Relaxed);
                    if let Some(suppressed) = enc_err.tick() {
                        tracing::warn!(suppressed, "audio encode failed — stream is silent: {e}");
                    }
                }
            }));
        };
        AudioCapture::start(opts.capture_source, on_frame)?
    };

    // --- local video producer (skipped for a web uplink, which relays the caster's own frames) ---
    #[cfg(any(target_os = "windows", all(target_os = "linux", feature = "linux-capture"), all(target_os = "macos", feature = "mac-capture")))]
    let video = if web_uplink {
        None
    } else {
        match opts.video {
            Some(vcfg) => Some(
                VideoProducer::start(
                    vcfg,
                    opts.video_target,
                    opts.encoder,
                    opts.encode_device,
                    lead_ns,
                    video_tx.clone(),
                    health.clone(),
                )
                .context("start video producer")?,
            ),
            None => None,
        }
    };
    // No local capture backend in THIS build. The CLI refuses --video up front and the GUI greys
    // the option out, so reaching here means something bypassed both -- keep the warning as a
    // backstop rather than silently serving audio.
    #[cfg(not(any(target_os = "windows", all(target_os = "linux", feature = "linux-capture"), all(target_os = "macos", feature = "mac-capture"))))]
    if opts.video.is_some() && !web_uplink {
        tracing::warn!(
            "this build has no local screen-capture backend; serving audio only (Linux needs \n             --features linux-capture, macOS --features mac-capture)"
        );
    }

    // Video is on for a local Windows capture with a VideoConfig, OR for a web uplink whose
    // operator enabled video (Phase 2): the caster H.264-encodes to the targets below and the
    // server relays it without decoding. A web uplink isn't gated on the host OS (no local capture).
    let video_on = if web_uplink {
        opts.video.is_some()
    } else {
        cfg!(any(target_os = "windows", all(target_os = "linux", feature = "linux-capture"), all(target_os = "macos", feature = "mac-capture")))
            && opts.video.is_some()
    };
    let (fw, fps) = match opts.video {
        Some(v) => (v.resolution, v.fps.value()),
        None => (newfoundsync_core::video::Resolution::P1080, 30),
    };
    // Encode targets dictated to a web caster in the CAST_GRANT, so all receivers get the
    // operator's chosen quality regardless of the caster's hardware. Zero when this isn't a
    // web-uplink video source.
    let (cast_w, cast_h, cast_fps, cast_kbps) = match opts.video {
        Some(v) if web_uplink => {
            let (w, h) = v.resolution.dims();
            (w as u16, h as u16, v.fps.value() as u8, v.suggested_bitrate_kbps())
        }
        _ => (0u16, 0u16, 0u8, 0u32),
    };

    // For a web-uplink source, hand the web layer a relay it pushes the caster's frames into.
    let cast_relay = if web_uplink {
        Some(Arc::new(CastRelay {
            audio_tx: audio_tx.clone(),
            video_tx: video_tx.clone(),
            lead_ns,
            audio_bps: opts.bitrate.max(0) as u32,
            sample_rate: newfoundsync_core::config::SAMPLE_RATE,
            channels: newfoundsync_core::config::CHANNELS as u8,
            video_on,
            width: cast_w,
            height: cast_h,
            fps: cast_fps,
            video_kbps: cast_kbps,
            health: health.clone(),
        }))
    } else {
        None
    };

    let config = MediaConfig {
        name: opts.name,
        sample_rate: newfoundsync_core::config::SAMPLE_RATE,
        channels: newfoundsync_core::config::CHANNELS as u16,
        // A web-uplink caster ALWAYS Opus-encodes its uplink (the browser's WebCodecs AudioEncoder;
        // there is no local encoder here for --codec to drive), and browsers can't PCM-decode, so
        // advertise "opus" regardless of opts.codec — mirroring the video_codec web_uplink case below.
        // Otherwise honor the operator's codec for a native local source.
        audio_codec: if web_uplink {
            "opus"
        } else {
            match opts.codec {
                CodecKind::Opus => "opus",
                CodecKind::Pcm => "pcm",
            }
        },
        video: video_on,
        frame_rate: fps,
        buffer_ms: opts.buffer_ms,
        // Codec advertised to clients (they pick the matching WebCodecs decoder). A *web-uplink*
        // caster sends H.264 ("avc1"; browsers H.264-encode far more reliably); every native
        // server source is AV1 ("av01") or the VP9 fallback — both royalty-free. The level in the
        // string is derived from the resolution/fps so it never understates the stream.
        video_codec: if web_uplink {
            "avc1.42E01F".to_string()
        } else if matches!(opts.encoder, EncoderBackend::Vp9) {
            newfoundsync_core::video::vp9_codec_string(fw, fps)
        } else {
            newfoundsync_core::video::av1_codec_string(fw, fps)
        },
    };

    Ok(Media {
        config,
        audio_tx,
        video_tx,
        _audio_capture: audio_capture,
        #[cfg(any(target_os = "windows", all(target_os = "linux", feature = "linux-capture"), all(target_os = "macos", feature = "mac-capture")))]
        _video: video,
        capture_device,
        cast_relay,
        health,
    })
}

/// Holds whichever audio capture is running (stops it on drop).
enum AudioCapture {
    #[cfg(not(target_os = "linux"))]
    System(SystemCapture),
    #[cfg(target_os = "linux")]
    Pulse(crate::capture::pulse::PulseCapture),
    #[cfg(target_os = "windows")]
    Process(crate::capture::process::ProcessCapture),
    /// No local capture (web-uplink source — frames arrive over the WebSocket).
    None,
}

impl AudioCapture {
    fn start<F>(source: CaptureSource, on_frame: F) -> Result<(AudioCapture, String)>
    where
        F: FnMut(&[i16]) + Send + 'static,
    {
        match source {
            // The web-uplink source never reaches here — start() handles it without local capture.
            CaptureSource::WebUplink => unreachable!("WebUplink has no local capture"),

            // Linux: capture the default sink's MONITOR via PulseAudio/PipeWire (the system output,
            // never the mic). System and all-apps are the same capture here — PulseAudio has no
            // "everything except me" filter, and we produce no output of our own to exclude.
            #[cfg(target_os = "linux")]
            CaptureSource::System | CaptureSource::AllExceptSelf => {
                let c = crate::capture::pulse::PulseCapture::start(on_frame)
                    .context("start PulseAudio/PipeWire monitor capture")?;
                let name = c.device_name.clone();
                Ok((AudioCapture::Pulse(c), name))
            }
            // Linux per-app: narrow a monitor capture to one application's stream. Until this arm
            // existed, App{..} fell into the whole-system arm above and the pid was DISCARDED —
            // capture silently broadened to everything, while the UI still named the chosen app.
            // If per-app cannot be honoured we now fail loudly instead, because the alternative is
            // broadcasting a machine's entire audio to the LAN under a label that says otherwise.
            #[cfg(target_os = "linux")]
            CaptureSource::App { pid } => {
                tracing::info!("[capture] starting audio source = SINGLE APP: pid={pid} (PulseAudio sink-input tap)");
                let c = crate::capture::pulse::PulseCapture::start_app(pid, on_frame)
                    .context("start per-application PulseAudio/PipeWire capture")?;
                let name = c.device_name.clone();
                Ok((AudioCapture::Pulse(c), name))
            }

            #[cfg(not(target_os = "linux"))]
            CaptureSource::System => {
                tracing::info!("[capture] starting audio source = SYSTEM endpoint loopback (cpal)");
                let c = SystemCapture::start(on_frame).context("start system capture")?;
                let name = c.device_name.clone();
                Ok((AudioCapture::System(c), name))
            }
            #[cfg(target_os = "windows")]
            CaptureSource::AllExceptSelf => {
                // NOTE: this mode is "everything EXCEPT us" by design — it captures every
                // other app AND general Windows system sounds. If the operator expects a
                // single app but hears everything, the wrong source is likely active here.
                tracing::info!(
                    "[capture] starting audio source = ALL APPS EXCEPT SELF \
                     (process-loopback EXCLUDE-self; by design = all other apps + system sounds)"
                );
                let c = crate::capture::process::ProcessCapture::start_exclude_current(on_frame)
                    .context("start process-loopback capture")?;
                Ok((AudioCapture::Process(c), "All apps (survives mute)".to_string()))
            }
            #[cfg(target_os = "windows")]
            CaptureSource::App { pid } => {
                // The picked PID is often a WINDOW process whose audio is rendered by a different
                // process (a browser's audio-service child, or a UWP app under ApplicationFrameHost).
                // Resolve to the process that actually owns an audio render session so INCLUDE
                // captures THAT app, not the wrong tree / whole mix.
                let render_pid = crate::capture::sessions::resolve_render_pid(pid);
                tracing::info!(
                    "[capture] starting audio source = SINGLE APP: picked pid={pid} -> capturing render pid={render_pid} \
                     (process-loopback INCLUDE; if audio from other apps STILL leaks, this Windows build is not honoring the per-PID filter)"
                );
                let c = crate::capture::process::ProcessCapture::start_include(render_pid, on_frame)
                    .context("start per-app process-loopback capture")?;
                Ok((AudioCapture::Process(c), format!("App (PID {render_pid}, survives mute)")))
            }
            #[cfg(not(any(target_os = "windows", target_os = "linux")))]
            CaptureSource::AllExceptSelf | CaptureSource::App { .. } => {
                tracing::warn!("process-loopback is Windows-only; using system loopback");
                let c = SystemCapture::start(on_frame).context("start system capture")?;
                let name = c.device_name.clone();
                Ok((AudioCapture::System(c), name))
            }
        }
    }
}

/// Screen capture -> AV1/VP9 encode -> broadcast WS video frames.
///
/// Platform-agnostic: `crate::video::capture` resolves to the WGC backend on Windows and the
/// portal/PipeWire one on Linux, and both expose the same frame slot + closed latch.
#[cfg(any(target_os = "windows", all(target_os = "linux", feature = "linux-capture"), all(target_os = "macos", feature = "mac-capture")))]
struct VideoProducer {
    stop: Arc<AtomicBool>,
    _capture: crate::video::capture::ScreenCapture,
    thread: Option<JoinHandle<()>>,
    /// Signalled by the producer thread just before it exits, so `Drop` can wait a BOUNDED time for a
    /// clean exit instead of joining unconditionally (see the `Drop` impl).
    done_rx: std::sync::mpsc::Receiver<()>,
}

#[cfg(any(target_os = "windows", all(target_os = "linux", feature = "linux-capture"), all(target_os = "macos", feature = "mac-capture")))]
impl VideoProducer {
    fn start(
        cfg: VideoConfig,
        target: VideoTarget,
        encoder_backend: EncoderBackend,
        encode_device: EncodeDevice,
        lead_ns: i64,
        tx: broadcast::Sender<Frame>,
        health: Arc<MediaHealth>,
    ) -> Result<VideoProducer> {
        use crate::video::capture::{CapturedFrame, ScreenCapture};
        use crate::video::codec::VideoEncoder;
        use rayon::prelude::*;

        const KEYFRAME_SECS: u64 = 2;

        // Provisional only. The real target keeps the SOURCE's shape and is derived from the first
        // captured frame (see fit_dims at the encoder build below) — the preset alone is always 16:9,
        // and using it directly is what squashed ultrawide desktops into ellipses.
        let (mut dw, mut dh) = cfg.resolution.dims();
        let fps = cfg.fps.value();
        let bitrate = cfg.suggested_bitrate_kbps();

        // All video now encodes from system-memory BGRA (AV1 via SVT-AV1, or the GPU's AV1 MFT
        // internally). The HEVC-only GPU zero-copy fast-lane was removed with HEVC.
        let capture = match target {
            VideoTarget::PrimaryMonitor => {
                ScreenCapture::start_primary().context("start screen capture")?
            }
            VideoTarget::Window { hwnd } => {
                ScreenCapture::start_window(hwnd).context("start window capture")?
            }
        };
        let slot = capture.slot.clone();
        let stop = Arc::new(AtomicBool::new(false));
        // Clean-exit signal for a BOUNDED teardown — see VideoProducer's Drop impl.
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        // The capture's authoritative "Windows closed the session" flag, mirrored into health below.
        let capture_closed = capture.closed.clone();
        // This stream IS meant to be producing video, so a video stall is a real fault (rather than
        // "video is simply off"). Set before the thread starts so the watchdog can't race it.
        health.video_expected.store(true, Ordering::Relaxed);

        let stop_t = stop.clone();
        let thread = thread::Builder::new()
            .name("video-producer".into())
            .spawn(move || {
                let frame_dur = Duration::from_nanos(1_000_000_000 / fps as u64);
                let mut last_key_req = Instant::now();
                let mut scaled = Vec::new();
                let mut last: Option<CapturedFrame> = None;
                let mut prev_rx: usize = 0;
                let mut encoder: Option<VideoEncoder> = None;
                let mut encoder_failed = false;
                let started_at = Instant::now();
                let mut got_any = false;
                let mut warned_no_frame = false;
                let mut enc_err = LogThrottle::new();
                // Counts only frames published from a NEW capture, so the latency log below can't
                // sample a repeat.
                let mut fresh_published: u64 = 0;
                // (pts, mono_now) of the last published frame — the reference a repeat extends.
                let mut prev_pub: Option<(i64, i64)> = None;

                while !stop_t.load(Ordering::Relaxed) {
                    let tick = Instant::now();
                    // Did the OS hand us a NEW picture this tick? Drives the PTS: a new frame is
                    // stamped from its own capture time, a re-encode of `last` cannot be.
                    let mut fresh = false;
                    // Mirror the authoritative capture-death signal into health so the web layer can
                    // report it (see MediaHealth::capture_closed for why frame silence must NOT be
                    // used for this).
                    if capture_closed.load(Ordering::Relaxed)
                        && !health.capture_closed.load(Ordering::Relaxed)
                    {
                        health.capture_closed.store(true, Ordering::Relaxed);
                    }
                    if let Some(f) = slot.lock().unwrap().take() {
                        last = Some(f);
                        got_any = true;
                        fresh = true;
                        // Counted separately from encoded output: the loop below happily re-encodes
                        // `last` forever after capture stops, so video_frames alone can't tell a live
                        // screen from a frozen one. This only moves on a genuinely NEW OS frame.
                        health.note_capture();
                    }
                    if !got_any && !warned_no_frame && started_at.elapsed() > Duration::from_secs(3) {
                        // All video now flows through this capture slot (the HEVC GPU fast-lane was
                        // removed), so 3s of silence means the source is idle — a minimized/occluded
                        // window, or simply nothing changing on screen. One-shot, so `info` can't
                        // flood; at `debug` it was invisible, and this is the only signal that
                        // distinguishes "capture never delivered" from "encoder broken".
                        tracing::info!("video-producer: no captured frame in 3s (source idle/occluded?)");
                        warned_no_frame = true;
                    }
                    // Only encode when at least one browser is watching.
                    let rx = tx.receiver_count();
                    if rx > 0 && !encoder_failed {
                        if let Some(frame) = &last {
                            // Lazily build the system-memory encoder on the first slot frame.
                            if encoder.is_none() {
                                // NOW the source shape is known, so pick a target that matches it
                                // rather than the preset's fixed 16:9. Done once, here, because the
                                // encoder is built once: changing dimensions later would mean
                                // rebuilding it and re-keyframing every client.
                                let (fw, fh) = newfoundsync_core::video::fit_dims(
                                    frame.width,
                                    frame.height,
                                    cfg.resolution,
                                );
                                if (fw, fh) != (dw, dh) {
                                    tracing::info!(
                                        src_w = frame.width,
                                        src_h = frame.height,
                                        enc_w = fw,
                                        enc_h = fh,
                                        "video: encoding at the source's aspect ratio"
                                    );
                                    dw = fw;
                                    dh = fh;
                                }
                                match VideoEncoder::new(encoder_backend, encode_device, dw, dh, fps, bitrate) {
                                    Ok(e) => {
                                        tracing::info!(backend = e.backend_label(), "video encoder ready");
                                        encoder = Some(e);
                                    }
                                    Err(e) => {
                                        // Latched: video stays off for the life of this stream even
                                        // though clients were already told `video: true`, so they show
                                        // a stage that never receives a frame. Recorded in health so
                                        // /health can report the mismatch.
                                        tracing::error!("video encoder init failed — video is off for this stream: {e:#}");
                                        encoder_failed = true;
                                        health.video_encoder_failed.store(true, Ordering::Relaxed);
                                    }
                                }
                            }
                            if let Some(enc) = encoder.as_mut() {
                                scale_bgra(
                                    &frame.bgra,
                                    frame.width as usize,
                                    frame.height as usize,
                                    dw as usize,
                                    dh as usize,
                                    &mut scaled,
                                );
                                // ~2 s cadence driven by wall-clock, not the emitted-output count
                                // (which stalls during encoder ramp-up / dropped frames).
                                let periodic = last_key_req.elapsed() >= Duration::from_secs(KEYFRAME_SECS);
                                // Emit a keyframe on the periodic cadence AND whenever a new
                                // client subscribes (reconnect / source swap).
                                let new_subscriber = rx > prev_rx;
                                if periodic || new_subscriber {
                                    enc.force_keyframe(); // a REQUEST; the GPU may honor it on its own GOP cadence
                                    last_key_req = Instant::now();
                                }
                                match enc.encode_bgra(&scaled) {
                                    Ok(bits) if !bits.is_empty() => {
                                        // PTS describes when the PICTURE EXISTED, not when the
                                        // encoder finished with it. Stamping `mono_now()` here folded
                                        // the poll wait + scale + AV1 encode into the timestamp —
                                        // tens of ms, and a different amount every frame. The client
                                        // deadline-schedules video against the same master clock as
                                        // audio, so that went straight out as lip-sync error that
                                        // wobbled frame to frame and no fixed trim could remove.
                                        let now_ns = mono_now();
                                        let pts = video_pts(
                                            fresh,
                                            frame.captured_ns,
                                            now_ns,
                                            lead_ns,
                                            prev_pub,
                                        );
                                        prev_pub = Some((pts, now_ns));
                                        // FRESH frames only. Measuring a repeat here reports the age
                                        // of a deliberately frozen picture as though it were pipeline
                                        // latency — on an idle screen that printed 9118 ms, which
                                        // reads as a catastrophe and means nothing.
                                        if fresh {
                                            fresh_published += 1;
                                            if fresh_published == 60 {
                                                // Past the encoder's cold start. This is exactly what
                                                // the capture stamp removes from the PTS; if it is
                                                // large, video is arriving late and the client is
                                                // likely dropping frames as past-due.
                                                tracing::info!(
                                                    ms = (now_ns - frame.captured_ns) / 1_000_000,
                                                    "video capture→publish latency"
                                                );
                                            }
                                        }
                                        // Flag the keyframe from the ACTUAL emitted bitstream (an AV1
                                        // Sequence-Header OBU or the VP9 keyframe bit), not the request —
                                        // force_keyframe is a no-op on the GPU MFT (GOP-driven), so the
                                        // request would mislabel frames and the client (which discards
                                        // non-key chunks until a real keyframe) would stay black.
                                        // Codec-aware (AV1 sequence-header OBU / VP9 keyframe bit).
                                        let is_key = enc.is_keyframe(&bits);
                                        let mut msg = Vec::with_capacity(10 + bits.len());
                                        msg.push(MSG_VIDEO);
                                        msg.extend_from_slice(&pts.to_be_bytes());
                                        msg.push(if is_key { 1 } else { 0 });
                                        msg.extend_from_slice(&bits);
                                        let _ = tx.send(Arc::new(msg));
                                        health.note_video();
                                    }
                                    Ok(_) => {}
                                    Err(e) => {
                                        // WARN, not debug: a per-frame failure (e.g. after a GPU driver
                                        // reset) freezes the client picture on its last frame while
                                        // everything still reports healthy. Throttled — this fires at
                                        // frame rate.
                                        health.video_errors.fetch_add(1, Ordering::Relaxed);
                                        // A hardware encoder can fail after its initial capability probe
                                        // (for example after a driver reset). Auto promises a usable
                                        // stream, not merely a successful probe, so replace it with
                                        // SVT-AV1 here and keyframe the next frame. GPU-only keeps
                                        // reporting the real hardware failure instead of spending CPU.
                                        if encode_device == EncodeDevice::Auto {
                                            match enc.fall_back_to_cpu() {
                                                Ok(true) => {
                                                    tracing::warn!(
                                                        "video: GPU AV1 failed ({e:#}); switched to CPU SVT-AV1"
                                                    );
                                                    enc.force_keyframe();
                                                    last_key_req = Instant::now();
                                                    continue;
                                                }
                                                Ok(false) => {}
                                                Err(fallback) => tracing::error!(
                                                    "video: GPU AV1 failed ({e:#}); CPU fallback also failed: {fallback:#}"
                                                ),
                                            }
                                        }
                                        if let Some(suppressed) = enc_err.tick() {
                                            tracing::warn!(suppressed, "video encode failed — picture is frozen: {e}");
                                        }
                                    }
                                }
                            }
                        }
                    }
                    prev_rx = rx;
                    let el = tick.elapsed();
                    if el < frame_dur {
                        thread::sleep(frame_dur - el);
                    }
                }
                // Drop the encoder BEFORE signalling. MfEncoder's own Drop talks to the GPU driver, so
                // it can hang too — signalling first would tell the waiting Drop "I'm clean", and it
                // would then join and block forever inside the encoder teardown: exactly the wedge this
                // bounded handshake exists to prevent, just moved one frame later.
                drop(encoder);
                // Observed `stop`, left the loop, and released the encoder — Drop can join safely.
                // (Err = the receiver is already gone, which is fine.)
                let _ = done_tx.send(());

                /// Nearest-neighbor BGRA scale, parallel by row.
                fn scale_bgra(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize, out: &mut Vec<u8>) {
                    out.resize(dw * dh * 4, 0);
                    if sw == dw && sh == dh && src.len() >= dw * dh * 4 {
                        out.copy_from_slice(&src[..dw * dh * 4]);
                        return;
                    }
                    if sw == 0 || sh == 0 {
                        return;
                    }
                    out.par_chunks_mut(dw * 4).enumerate().for_each(|(dy, orow)| {
                        let sy = (dy * sh / dh).min(sh - 1);
                        for dx in 0..dw {
                            let sx = (dx * sw / dw).min(sw - 1);
                            let si = (sy * sw + sx) * 4;
                            if si + 4 <= src.len() {
                                orow[dx * 4..dx * 4 + 4].copy_from_slice(&src[si..si + 4]);
                            }
                        }
                    });
                }
            })
            .context("spawn video producer thread")?;

        Ok(VideoProducer {
            stop,
            _capture: capture,
            thread: Some(thread),
            done_rx,
        })
    }
}

#[cfg(any(target_os = "windows", all(target_os = "linux", feature = "linux-capture"), all(target_os = "macos", feature = "mac-capture")))]
impl Drop for VideoProducer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // BOUNDED teardown, mirroring PulseCapture::drop. This runs on every source switch, Apply, and
        // shutdown, so it must never hang: the producer thread can be parked inside a GPU encoder call
        // that isn't observing `stop` yet. Wait briefly for the clean-exit signal and join (so a panic
        // payload is still reported); otherwise DETACH and say so. The deadline is comfortably longer
        // than one encode attempt (see mf_encoder's PUMP_DEADLINE) so a clean exit is the normal path.
        const TEARDOWN_WAIT: Duration = Duration::from_millis(1_500);
        // Three outcomes, and they are NOT interchangeable:
        //   Ok           — clean exit; join is instant.
        //   Disconnected — the sender was dropped WITHOUT signalling, i.e. the thread unwound. This
        //                  returns immediately (not after the timeout), and the thread has finished, so
        //                  joining is safe and is the only way to recover the panic payload. Treating
        //                  this as "wedged" would both lie in the log and destroy the evidence.
        //   Timeout      — still running and not observing `stop`, most likely parked in a GPU driver
        //                  call. Do NOT join: that is exactly what turned an encoder wedge into a
        //                  permanently unusable Apply button.
        match self.done_rx.recv_timeout(TEARDOWN_WAIT) {
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if let Some(t) = self.thread.take() {
                    if let Err(panic) = t.join() {
                        let why = panic
                            .downcast_ref::<&str>()
                            .map(|s| (*s).to_string())
                            .or_else(|| panic.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "unknown panic payload".to_string());
                        tracing::error!("video producer thread panicked: {why}");
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                tracing::warn!(
                    "video producer did not stop within {TEARDOWN_WAIT:?} — detaching it so teardown \
                     can finish (it is probably stuck in a GPU encoder call)"
                );
                let _ = self.thread.take(); // drop the JoinHandle without joining
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEAD: i64 = 100_000_000; //  100 ms
    const NOMINAL: i64 = 33_333_333; // ~1/30 s, the tick the producer AIMS for
    const ACTUAL: i64 = 38_000_000; //  what a tick really costs when encoding is slow

    const LATENCY: i64 = 40_000_000; // 40 ms of poll wait + scale + encode, as measured

    #[test]
    fn a_fresh_frame_is_stamped_from_its_own_capture_time() {
        // Not from "now": the poll wait + scale + encode must not leak into the timestamp, or video is
        // presented that much after the moment it depicts.
        let captured = 1_700_000_000_000_000_000i64;
        let published_later = captured + LATENCY;
        assert_eq!(video_pts(true, captured, published_later, LEAD, None), captured + LEAD);
        // A prior publish must not change a fresh frame's answer — its own capture time is the truth.
        let pts = video_pts(true, captured, published_later, LEAD, Some((999, published_later)));
        assert_eq!(pts, captured + LEAD);
    }

    #[test]
    fn a_repeat_with_no_prior_publish_is_stamped_now() {
        // There is no reference to extend, and the held frame can be seconds stale. Reusing that stale
        // capture time would emit a duplicate and then past-due timestamps; a bare 0 timeline (an
        // earlier attempt) stamped it ~56 years in the past.
        let captured = 1_700_000_000_000_000_000i64;
        let now = captured + 9_000_000_000; // 9 s of an idle screen
        assert_eq!(video_pts(false, captured, now, LEAD, None), now + LEAD);
    }

    #[test]
    fn a_repeat_inherits_the_back_dating_of_the_frame_it_extends() {
        // The property that keeps the queue ordered: a repeat must claim the same content age as the
        // fresh frames around it, NOT publish time. Stamping a repeat at plain `now` put it one whole
        // capture latency ahead, and the next fresh frame then sorted behind it.
        let captured = 1_000_000_000i64;
        let published = captured + LATENCY;
        let fresh = video_pts(true, captured, published, LEAD, None);
        let later = published + NOMINAL;
        let repeat = video_pts(false, captured, later, LEAD, Some((fresh, published)));
        assert_eq!(repeat, later - LATENCY + LEAD, "a repeat carries the SAME {LATENCY} ns back-dating");
        assert!(repeat > fresh, "and is still strictly increasing");
    }

    #[test]
    fn a_fresh_frame_after_repeats_does_not_step_back() {
        // The ordering violation this rule exists to prevent. The client's videoStep only inspects
        // vq[0], so a fresh frame stamped BEHIND an already-queued repeat is not simply "one past-due
        // frame" — it is drained together with that repeat and the new picture is shown a frame late.
        let mut now = 1_000_000_000i64;
        let mut prev = Some((video_pts(true, now - LATENCY, now, LEAD, None), now));
        for _ in 0..10 {
            now += ACTUAL;
            prev = Some((video_pts(false, now - LATENCY, now, LEAD, prev), now));
        }
        now += ACTUAL;
        let fresh = video_pts(true, now - LATENCY, now, LEAD, prev);
        assert!(fresh > prev.unwrap().0, "a fresh frame must not sort behind the repeats before it");
    }

    #[test]
    fn repeats_cannot_drift_when_ticks_run_slower_than_nominal() {
        // The regression that made this a function. Advancing by NOMINAL per repeat while each real
        // tick costs ACTUAL put the PTS ~5 ms further into the past every frame — 7 s behind inside one
        // 18 s run, and unbounded after that. Extending by REAL elapsed time cannot drift: after 500
        // slow ticks the stamp still sits exactly one capture latency behind the clock.
        let start = 1_000_000_000i64;
        let mut now = start;
        let mut prev = Some((video_pts(true, now - LATENCY, now, LEAD, None), now));
        for i in 1..=500 {
            now += ACTUAL;
            let pts = video_pts(false, 42, now, LEAD, prev);
            assert_eq!(pts - prev.unwrap().0, ACTUAL, "repeat {i} tracks the real clock, not nominal fps");
            assert_eq!(pts, now - LATENCY + LEAD, "and never accumulates error");
            prev = Some((pts, now));
        }
        assert!(NOMINAL < ACTUAL, "the drift only existed because nominal < actual");
    }

    #[test]
    fn a_repeat_after_an_unsubscribed_gap_is_not_past_due() {
        // Nothing publishes while no client is watching, so `prev` can be minutes old. The elapsed
        // term covers the whole gap, so the stamp lands at ~now — not at the pre-gap timeline, which
        // would be instantly discarded as past-due by the client that just connected.
        let now = 1_000_000_000i64;
        let prev = Some((video_pts(true, now - LATENCY, now, LEAD, None), now));
        let after_gap = now + 120_000_000_000; // 2 minutes with no subscribers
        let pts = video_pts(false, now - LATENCY, after_gap, LEAD, prev);
        assert_eq!(pts, after_gap - LATENCY + LEAD, "the gap is absorbed, not inherited");
    }

    /// The distinction the stall watchdog is built on: "never produced" must NOT look like "stopped".
    /// A web-cast source with no caster yet, or the first moments after Apply, legitimately has zero
    /// frames — reporting that as a fault would cry wolf on every startup.
    #[test]
    fn stall_helpers_distinguish_never_started_from_stopped() {
        let h = MediaHealth::default();
        assert_eq!(h.audio_stall_ms(), None, "no frames yet must be None, not a stall");
        assert_eq!(h.video_stall_ms(), None);
        assert_eq!(h.capture_stall_ms(), None);

        h.note_audio();
        let age = h.audio_stall_ms().expect("a published frame must produce Some(age)");
        assert!((0..1_000).contains(&age), "age right after a frame should be ~0ms, got {age}");
        assert_eq!(h.audio_frames.load(Ordering::Relaxed), 1);
        // Publishing audio must not make video look alive.
        assert_eq!(h.video_stall_ms(), None, "audio frames must not affect the video age");
    }

    /// An age far in the past reads as a stall; the threshold comparison is the watchdog's whole
    /// decision, so pin it rather than trusting it by inspection.
    #[test]
    fn a_stale_timestamp_reads_as_stalled() {
        let h = MediaHealth::default();
        // 10s ago on the same monotonic clock the helpers read.
        h.last_audio_ns.store(mono_now() - 10_000_000_000, Ordering::Relaxed);
        let ms = h.audio_stall_ms().expect("a set timestamp must yield Some");
        assert!(ms >= 9_000, "expected ~10000ms of staleness, got {ms}");
        assert!(ms > 2_000, "must exceed the watchdog's 2s threshold");
    }

    /// Frozen picture: the encoder keeps emitting from a stale frame, so video looks fresh while
    /// capture has stopped. That pairing is the ONLY way to tell a frozen screen from a dead encoder.
    #[test]
    fn fresh_video_with_stale_capture_is_a_frozen_picture() {
        let h = MediaHealth::default();
        h.note_video(); // encoder still producing
        h.last_capture_ns.store(mono_now() - 5_000_000_000, Ordering::Relaxed); // capture died 5s ago
        assert!(h.video_stall_ms().unwrap() < 2_000, "video should look fresh");
        assert!(h.capture_stall_ms().unwrap() > 2_000, "capture should look stale");
    }

    /// The log throttle must emit the FIRST occurrence immediately — an encoder that fails once and
    /// recovers should still leave a trace — and then suppress, counting what it swallowed.
    #[test]
    fn log_throttle_emits_first_then_suppresses_with_a_count() {
        let mut t = LogThrottle::new();
        assert_eq!(t.tick(), Some(0), "the first occurrence must log immediately");
        assert_eq!(t.tick(), None, "an immediate repeat must be suppressed");
        assert_eq!(t.tick(), None);
        assert_eq!(t.suppressed, 2, "suppressed occurrences must be counted for the next line");
    }
}
