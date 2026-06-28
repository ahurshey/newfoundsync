//! Media producer: drives the existing capture + encode pipeline and publishes
//! ready-to-send WebSocket frames onto broadcast channels. Each WebSocket client
//! task subscribes and forwards; the browser does the buffering, clock-sync, and
//! decode. This keeps the heavy sync work (cpal/WGC callbacks, encoders) on
//! dedicated threads, bridged to the async web server via `tokio::broadcast`.
//!
//! Wire frames (binary, server→browser):
//!   audio: [0x01][pts i64 BE][Opus bytes]
//!   video: [0x02][pts i64 BE][flags u8][HEVC (H.265) Annex-B bytes]

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
    pub video_codec: &'static str,
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

    // --- audio producer -------------------------------------------------
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
    let (audio_capture, capture_device) = AudioCapture::start(opts.capture_source, on_frame)?;

    // --- video producer (Windows) --------------------------------------
    #[cfg(target_os = "windows")]
    let video = match opts.video {
        Some(vcfg) => Some(
            VideoProducer::start(vcfg, opts.video_target, opts.encoder, lead_ns, video_tx.clone())
                .context("start video producer")?,
        ),
        None => None,
    };
    #[cfg(not(target_os = "windows"))]
    if opts.video.is_some() {
        tracing::warn!("video capture is Windows-only for now; serving audio only");
    }

    let video_on = cfg!(target_os = "windows") && opts.video.is_some();
    let (fw, fps) = match opts.video {
        Some(v) => (v.resolution, v.fps.value()),
        None => (newfoundsync_core::video::Resolution::P1080, 30),
    };
    let _ = fw;

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
        // GPU MF emits Main-profile HEVC (H.265). The level here (5.1) just needs to be
        // >= the stream's real level (covers up to 4K); the decoder reads the exact
        // params from the in-band VPS/SPS/PPS. "hev1" = parameter sets are in-band.
        video_codec: "hev1.1.6.L153.B0",
    };

    Ok(Media {
        config,
        audio_tx,
        video_tx,
        _audio_capture: audio_capture,
        #[cfg(target_os = "windows")]
        _video: video,
        capture_device,
    })
}

/// Holds whichever audio capture is running (stops it on drop).
enum AudioCapture {
    System(SystemCapture),
    #[cfg(target_os = "windows")]
    Process(crate::capture::process::ProcessCapture),
}

impl AudioCapture {
    fn start<F>(source: CaptureSource, on_frame: F) -> Result<(AudioCapture, String)>
    where
        F: FnMut(&[i16]) + Send + 'static,
    {
        match source {
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

/// Windows screen-capture → H.264 encode → broadcast WS video frames.
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
        use crate::video::capture::{annexb_has_keyframe, CapturedFrame, GpuParams, ScreenCapture};
        use crate::video::codec::VideoEncoder;
        use rayon::prelude::*;

        const KEYFRAME_SECS: u64 = 2;

        let (dw, dh) = cfg.resolution.dims();
        let fps = cfg.fps.value();
        let bitrate = cfg.suggested_bitrate_kbps();

        // Try the GPU zero-copy fast-lane unless the user forced CPU. It's built inside the
        // capture callback (the only place the WGC device/context are valid); if it can't init
        // there it silently degrades to the CPU slot path below — which is why we still spawn
        // the producer thread and create its system-memory encoder LAZILY (only if a frame ever
        // reaches the slot, i.e. only when the GPU lane is NOT handling frames).
        let gpu = if encoder_backend == EncoderBackend::Cpu {
            None
        } else {
            Some(GpuParams { tx: tx.clone(), lead_ns, dw, dh, fps, bitrate_kbps: bitrate })
        };

        let capture = match target {
            VideoTarget::PrimaryMonitor => {
                ScreenCapture::start_primary(gpu).context("start screen capture")?
            }
            VideoTarget::Window { hwnd } => {
                ScreenCapture::start_window(hwnd, gpu).context("start window capture")?
            }
        };
        let slot = capture.slot.clone();
        let stop = Arc::new(AtomicBool::new(false));

        let stop_t = stop.clone();
        let thread = thread::Builder::new()
            .name("video-producer".into())
            .spawn(move || {
                let frame_dur = Duration::from_nanos(1_000_000_000 / fps as u64);
                let key_interval = (fps as u64 * KEYFRAME_SECS).max(1);
                let mut frame_count: u64 = 0;
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
                        // The GPU fast-lane bypasses this slot, so silence here doesn't mean "no
                        // video" — only that the CPU path is idle (GPU active, or source occluded).
                        tracing::debug!("video-producer: no CPU-path frame in 3s (GPU lane may be active)");
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
                                        tracing::info!(backend = e.backend_label(), "video encoder ready (CPU path)");
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
                                let periodic = frame_count % key_interval == 0;
                                // Emit a keyframe on the periodic cadence AND whenever a new
                                // client subscribes (reconnect / source swap).
                                let new_subscriber = rx > prev_rx;
                                if periodic || new_subscriber {
                                    enc.force_keyframe(); // a REQUEST; the GPU may honor it on its own GOP cadence
                                }
                                match enc.encode_bgra(&scaled) {
                                    Ok(bits) if !bits.is_empty() => {
                                        let pts = mono_now() + lead_ns;
                                        // Flag the keyframe from the ACTUAL emitted bitstream (scan for
                                        // an IRAP NAL), not the request — force_keyframe is a no-op on
                                        // the GPU MFT (GOP-driven), so the request would mislabel IDRs
                                        // and the client (which discards non-key chunks until a real
                                        // keyframe) would stay black.
                                        let is_key = annexb_has_keyframe(&bits);
                                        let mut msg = Vec::with_capacity(10 + bits.len());
                                        msg.push(MSG_VIDEO);
                                        msg.extend_from_slice(&pts.to_be_bytes());
                                        msg.push(if is_key { 1 } else { 0 });
                                        msg.extend_from_slice(&bits);
                                        let _ = tx.send(Arc::new(msg));
                                        frame_count += 1;
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
