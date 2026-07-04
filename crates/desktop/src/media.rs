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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::sync::broadcast;

use newfoundsync_core::codec::{CodecKind, Encoder};
use newfoundsync_core::config::mono_now;
use newfoundsync_core::video::{EncoderBackend, VideoConfig};

use crate::capture::system::SystemCapture;

/// Message tags (first byte of each broadcast/WS frame).
pub const MSG_AUDIO: u8 = 0x01;
pub const MSG_VIDEO: u8 = 0x02;

/// One ready-to-send WebSocket binary frame.
pub type Frame = Arc<Vec<u8>>;

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
    #[cfg(target_os = "windows")]
    _video: Option<VideoProducer>,
    pub capture_device: String,
    /// Present only for [`CaptureSource::WebUplink`]: the web layer pushes a casting
    /// client's already-encoded frames here, which get re-stamped + fanned out like a
    /// local capture. `None` for local sources.
    pub cast_relay: Option<Arc<CastRelay>>,
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
    }

    /// Wrap+fan-out one H.264 access unit (Annex-B) uploaded by the caster (Phase 2). The keyframe
    /// flag is RE-DERIVED from the bitstream (never trusted from the caster's wire byte) so a buggy
    /// or hostile caster can't mislabel frames and strand receivers on a black frame — matching the
    /// local capture path (which also scans via `is_keyframe`). Web casts are always H.264 (avc1).
    pub fn push_video(&self, h264: &[u8]) {
        let key = crate::video::relay::annexb_has_h264_idr(h264);
        let pts = mono_now() + self.lead_ns;
        let mut msg = Vec::with_capacity(10 + h264.len());
        msg.push(MSG_VIDEO);
        msg.extend_from_slice(&pts.to_be_bytes());
        msg.push(if key { 1 } else { 0 });
        msg.extend_from_slice(h264);
        let _ = self.video_tx.send(Arc::new(msg));
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
}

/// Start capture + encode, returning the broadcast channels the web server fans
/// out to browser WebSocket clients.
pub fn start(opts: MediaOptions) -> Result<Media> {
    let lead_ns = opts.lead_ms.max(0) * 1_000_000;
    // Bounded ring; each WS client task forwards immediately, so it only needs to
    // cover momentary scheduling jitter (the *browser* holds the big buffer).
    let (audio_tx, _) = broadcast::channel::<Frame>(512);
    let (video_tx, _) = broadcast::channel::<Frame>(256);

    // Web-uplink source = no local capture; a casting web client's already-encoded
    // frames arrive over the WebSocket and are relayed onto these same channels.
    let web_uplink = matches!(opts.capture_source, CaptureSource::WebUplink);

    // --- audio producer (skipped for web uplink) ------------------------
    let (audio_capture, capture_device) = if web_uplink {
        (AudioCapture::None, "Web client cast".to_string())
    } else {
        let mut encoder = Encoder::new(opts.codec, opts.bitrate).context("build audio encoder")?;
        let audio_pub = audio_tx.clone();
        let on_frame = move |frame: &[i16]| {
            // FFI callback (cpal/WGC) — trap panics so they can't unwind across C.
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| match encoder.encode(frame) {
                Ok(payload) => {
                    let pts = mono_now() + lead_ns;
                    let mut msg = Vec::with_capacity(9 + payload.len());
                    msg.push(MSG_AUDIO);
                    msg.extend_from_slice(&pts.to_be_bytes());
                    msg.extend_from_slice(&payload);
                    let _ = audio_pub.send(Arc::new(msg)); // Err only if no clients
                }
                Err(e) => tracing::debug!("audio encode: {e}"),
            }));
        };
        AudioCapture::start(opts.capture_source, on_frame)?
    };

    // --- video producer (Windows; skipped for web uplink — Phase 1 is audio-only) ---
    #[cfg(target_os = "windows")]
    let video = if web_uplink {
        None
    } else {
        match opts.video {
            Some(vcfg) => Some(
                VideoProducer::start(vcfg, opts.video_target, opts.encoder, lead_ns, video_tx.clone())
                    .context("start video producer")?,
            ),
            None => None,
        }
    };
    #[cfg(not(target_os = "windows"))]
    if opts.video.is_some() && !web_uplink {
        tracing::warn!("video capture is Windows-only for now; serving audio only");
    }

    // Video is on for a local Windows capture with a VideoConfig, OR for a web uplink whose
    // operator enabled video (Phase 2): the caster H.264-encodes to the targets below and the
    // server relays it without decoding. A web uplink isn't gated on the host OS (no local capture).
    let video_on = if web_uplink {
        opts.video.is_some()
    } else {
        cfg!(target_os = "windows") && opts.video.is_some()
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
        }))
    } else {
        None
    };

    let config = MediaConfig {
        name: opts.name,
        sample_rate: newfoundsync_core::config::SAMPLE_RATE,
        channels: newfoundsync_core::config::CHANNELS as u16,
        audio_codec: match opts.codec {
            CodecKind::Opus => "opus",
            CodecKind::Pcm => "pcm",
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
        #[cfg(target_os = "windows")]
        _video: video,
        capture_device,
        cast_relay,
    })
}

/// Holds whichever audio capture is running (stops it on drop).
enum AudioCapture {
    System(SystemCapture),
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
            #[cfg(not(target_os = "windows"))]
            CaptureSource::AllExceptSelf | CaptureSource::App { .. } => {
                tracing::warn!("process-loopback is Windows-only; using system loopback");
                let c = SystemCapture::start(on_frame).context("start system capture")?;
                let name = c.device_name.clone();
                Ok((AudioCapture::System(c), name))
            }
        }
    }
}

/// Windows screen-capture → AV1/VP9 encode → broadcast WS video frames.
#[cfg(target_os = "windows")]
struct VideoProducer {
    stop: Arc<AtomicBool>,
    _capture: crate::video::capture::ScreenCapture,
    thread: Option<JoinHandle<()>>,
}

#[cfg(target_os = "windows")]
impl VideoProducer {
    fn start(
        cfg: VideoConfig,
        target: VideoTarget,
        encoder_backend: EncoderBackend,
        lead_ns: i64,
        tx: broadcast::Sender<Frame>,
    ) -> Result<VideoProducer> {
        use crate::video::capture::{CapturedFrame, ScreenCapture};
        use crate::video::codec::VideoEncoder;
        use rayon::prelude::*;

        const KEYFRAME_SECS: u64 = 2;

        let (dw, dh) = cfg.resolution.dims();
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

                while !stop_t.load(Ordering::Relaxed) {
                    let tick = Instant::now();
                    if let Some(f) = slot.lock().unwrap().take() {
                        last = Some(f);
                        got_any = true;
                    }
                    if !got_any && !warned_no_frame && started_at.elapsed() > Duration::from_secs(3) {
                        // All video now flows through this capture slot (the HEVC GPU fast-lane was
                        // removed), so 3s of silence means the source is idle — a minimized/occluded
                        // window, or simply nothing changing on screen.
                        tracing::debug!("video-producer: no captured frame in 3s (source idle/occluded?)");
                        warned_no_frame = true;
                    }
                    // Only encode when at least one browser is watching.
                    let rx = tx.receiver_count();
                    if rx > 0 && !encoder_failed {
                        if let Some(frame) = &last {
                            // Lazily build the system-memory encoder on the first slot frame.
                            if encoder.is_none() {
                                match VideoEncoder::new(encoder_backend, dw, dh, fps, bitrate) {
                                    Ok(e) => {
                                        tracing::info!(backend = e.backend_label(), "video encoder ready");
                                        encoder = Some(e);
                                    }
                                    Err(e) => {
                                        tracing::error!("video encoder init failed: {e:#}");
                                        encoder_failed = true;
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
                                        let pts = mono_now() + lead_ns;
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
                                    }
                                    Ok(_) => {}
                                    Err(e) => tracing::debug!("video encode: {e}"),
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
        })
    }
}

#[cfg(target_os = "windows")]
impl Drop for VideoProducer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
