// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Linux hardware AV1 through the Flatpak FFmpeg VA-API and NVENC encoders.
//!
//! PipeWire deliberately supplies CPU-mapped BGRA frames, so this wrapper writes frames to one
//! long-lived FFmpeg process. FFmpeg converts/uploads the frame to the selected hardware API and
//! emits an IVF stream. IVF gives each AV1 packet a length prefix over a byte pipe; the reader
//! removes that wrapper and returns the raw AV1 access unit expected by the WebCodecs transport.

use anyhow::{anyhow, bail, Context, Result};
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const MAX_AV1_PACKET_BYTES: usize = 16 * 1024 * 1024;

/// AMD and Intel publish AV1 encode through VA-API. NVIDIA uses NVENC and does not require a
/// VA-API driver, so both need independent probes.
#[derive(Clone)]
enum HardwareBackend {
    Vaapi(PathBuf),
    Nvenc,
}

impl HardwareBackend {
    fn label(&self) -> &'static str {
        match self {
            HardwareBackend::Vaapi(_) => "GPU AV1 (VA-API)",
            HardwareBackend::Nvenc => "GPU AV1 (NVENC)",
        }
    }

    fn describe(&self) -> String {
        match self {
            HardwareBackend::Vaapi(device) => format!("VA-API ({})", device.display()),
            HardwareBackend::Nvenc => "NVENC".to_string(),
        }
    }
}

/// Persistent hardware AV1 encoder. Creation submits a black frame through each available API,
/// so GPU-only fails at startup on a driver which exposes a device but has no AV1 encode support.
pub struct FfmpegAv1Encoder {
    child: Child,
    /// Latest frame awaiting the writer thread, with the capture time that belongs to it.
    /// Capacity ONE, overwrite-on-arrival — the same shape as the capture slot, and for the same
    /// reason: when the consumer is slow the right answer is to drop stale frames, not to queue them.
    ///
    /// This exists because the write CANNOT happen on the producer thread. A 1080p BGRA frame is
    /// 8 MB (33 MB at 4K) and a pipe holds 64 KB, so `write_all` parks until FFmpeg has drained
    /// almost the whole frame. Parked on the producer thread that meant: the capture slot went
    /// undrained (so `capture_frames` measured FFmpeg's throughput instead of capture liveness and
    /// inverted the signal that tells "capture died" from "encoder died"), `stop_t` was never
    /// observed (so shutdown detached the thread, `Drop` never ran, and FFmpeg was orphaned holding
    /// the render node), and — worst — a wedged driver never produced an `Err`, so the Auto->CPU
    /// fallback was unreachable in exactly the scenario it was written for.
    submit: Arc<(Mutex<Option<(Vec<u8>, i64)>>, Condvar)>,
    /// Capture times of frames handed to FFmpeg, oldest first. The reader pairs each emitted packet
    /// with the front entry, which is what lets a packet be published under the capture time of the
    /// picture it actually contains. Sound because output order equals input order here: `-bf 0` on
    /// both backends, so there are no reordered frames.
    pending: Arc<Mutex<std::collections::VecDeque<i64>>>,
    packets: mpsc::Receiver<Result<Vec<u8>, String>>,
    stop: Arc<AtomicBool>,
    writer_thread: Option<JoinHandle<()>>,
    stdout_thread: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
    stderr: Arc<Mutex<String>>,
    /// Frames submitted with nothing coming back yet, and when output was last seen. Together these
    /// turn "FFmpeg is alive but not encoding" into an `Err` the producer can act on.
    since_packet: u32,
    last_packet: Instant,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    backend: HardwareBackend,
}

/// How long a hardware encoder may accept frames without emitting any before it is declared broken.
/// Generous enough to cover a slow first packet and a keyframe hiccup; short enough that Auto's CPU
/// fallback happens while a viewer is still watching rather than after they have given up.
const OUTPUT_STALL: Duration = Duration::from_secs(2);

impl FfmpegAv1Encoder {
    pub fn new(width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> Result<Self> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            bail!("hardware AV1 requires non-zero, even dimensions ({width}x{height})");
        }
        let mut backends = render_nodes()
            .into_iter()
            .map(HardwareBackend::Vaapi)
            .collect::<Vec<_>>();
        // A system can have NVIDIA character devices without a DRM render node. Try NVENC after
        // VA-API so Intel, AMD, and NVIDIA work without GPU-name heuristics. Older NVIDIA cards
        // merely reject this one-frame AV1 probe and Auto then selects SVT-AV1.
        backends.push(HardwareBackend::Nvenc);

        let mut errors = Vec::new();
        for backend in backends {
            match Self::probe(width, height, fps, bitrate_kbps, &backend)
                .and_then(|()| Self::spawn(width, height, fps, bitrate_kbps, backend.clone()))
            {
                Ok(encoder) => return Ok(encoder),
                Err(e) => errors.push(format!("{}: {e:#}", backend.describe())),
            }
        }
        bail!("could not start a hardware AV1 encoder: {}", errors.join("; "))
    }

    /// Device nodes and FFmpeg's compiled encoder list are insufficient: AV1 encode support is
    /// model- and driver-specific. A real one-frame encode is the trustworthy capability test.
    fn probe(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
        backend: &HardwareBackend,
    ) -> Result<()> {
        let dimensions = format!("{width}x{height}");
        let rate = fps.max(1).to_string();
        let bitrate = format!("{bitrate_kbps}k");
        let source = format!("color=c=black:s={dimensions}:r={rate}");
        let mut command = Command::new(ffmpeg_binary());
        command.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
        add_device_args(&mut command, backend);
        command.args(["-f", "lavfi", "-i", &source, "-frames:v", "1", "-an"]);
        add_encoder_args(&mut command, backend);
        let output = command
            .args(["-b:v", &bitrate, "-f", "null", "-"])
            .stdout(Stdio::null())
            .output()
            .with_context(|| format!("start FFmpeg {} AV1 probe", backend.describe()))?;
        if output.status.success() {
            return Ok(());
        }
        let diagnostic = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(
            "FFmpeg {} AV1 probe exited ({}){}",
            backend.describe(),
            output.status,
            if diagnostic.is_empty() {
                String::new()
            } else {
                format!(": {diagnostic}")
            }
        );
    }

    fn spawn(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
        backend: HardwareBackend,
    ) -> Result<Self> {
        let dimensions = format!("{width}x{height}");
        let rate = fps.max(1).to_string();
        let bitrate = format!("{bitrate_kbps}k");
        let buffer = format!("{}k", bitrate_kbps.saturating_mul(2));
        let gop = fps.max(1).saturating_mul(2).to_string();
        let mut command = Command::new(ffmpeg_binary());
        command.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
        add_device_args(&mut command, &backend);
        command.args([
            "-f",
            "rawvideo",
            "-pixel_format",
            "bgra",
            "-video_size",
            &dimensions,
            "-framerate",
            &rate,
            "-i",
            "pipe:0",
            "-an",
        ]);
        add_encoder_args(&mut command, &backend);
        command
            .args([
                "-b:v",
                &bitrate,
                "-maxrate",
                &bitrate,
                "-bufsize",
                &buffer,
                "-g",
                &gop,
                "-flush_packets",
                "1",
                "-f",
                "ivf",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("start FFmpeg {} AV1 encoder", backend.describe()))?;
        let stdin = child.stdin.take().context("take FFmpeg video input")?;
        let stdout = child.stdout.take().context("take FFmpeg video output")?;
        let stderr = child.stderr.take().context("take FFmpeg diagnostics")?;
        let (packet_tx, packets) = mpsc::channel();
        let stderr_text = Arc::new(Mutex::new(String::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let submit: Arc<(Mutex<Option<(Vec<u8>, i64)>>, Condvar)> =
            Arc::new((Mutex::new(None), Condvar::new()));
        let pending = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        // WRITER: owns the blocking pipe write, so parking here costs a dropped frame instead of a
        // stalled producer. Records each frame's capture time as it goes out, in submit order.
        let writer_thread = {
            let submit = submit.clone();
            let pending = pending.clone();
            let stop = stop.clone();
            thread::Builder::new()
                .name("ffmpeg-av1-input".into())
                .spawn(move || write_frames(stdin, submit, pending, stop))
                .context("start FFmpeg AV1 input writer")?
        };
        let stdout_thread = thread::Builder::new()
            .name("ffmpeg-av1-output".into())
            .spawn(move || read_ivf_packets(stdout, packet_tx))
            .context("start FFmpeg AV1 output reader")?;
        let stderr_copy = stderr_text.clone();
        let stderr_thread = thread::Builder::new()
            .name("ffmpeg-av1-errors".into())
            .spawn(move || collect_stderr(stderr, stderr_copy))
            .context("start FFmpeg AV1 diagnostic reader")?;
        Ok(Self {
            child,
            submit,
            pending,
            packets,
            stop,
            writer_thread: Some(writer_thread),
            stdout_thread: Some(stdout_thread),
            stderr_thread: Some(stderr_thread),
            stderr: stderr_text,
            since_packet: 0,
            last_packet: Instant::now(),
            width,
            height,
            fps: fps.max(1),
            bitrate_kbps,
            backend,
        })
    }

    /// Submit a frame and return any packet FFmpeg has finished, WITH the capture time of the
    /// picture that packet actually contains.
    ///
    /// The returned capture time is the whole point. FFmpeg is a separate process with pipeline
    /// depth, so the packet coming back is an earlier frame than the one going in. Publishing those
    /// bits under the submitted frame's capture time — which is what happened before — stamps a
    /// picture as newer than it is and lands directly on lip-sync, the exact error the
    /// "every PTS is a content time" rule exists to prevent.
    pub fn encode_bgra(&mut self, bgra: &[u8], captured_ns: i64) -> Result<(Vec<u8>, i64)> {
        let expected = self.width as usize * self.height as usize * 4;
        if bgra.len() != expected {
            bail!("short BGRA frame: {} != {expected}", bgra.len());
        }
        if let Some(status) = self.child.try_wait().context("check FFmpeg hardware encoder")? {
            bail!(
                "FFmpeg {} encoder exited ({status}): {}",
                self.backend.describe(),
                self.diagnostics()
            );
        }
        // Hand the frame over without blocking: replace whatever the writer has not taken yet.
        // Dropping a stale frame is correct — the encoder is behind, and the newest picture is the
        // one worth sending.
        {
            let (lock, cv) = &*self.submit;
            let mut slot = lock.lock().map_err(|_| anyhow!("FFmpeg submit slot poisoned"))?;
            *slot = Some((bgra.to_vec(), captured_ns));
            cv.notify_one();
        }
        // Drain EVERYTHING ready, not one packet. Taking a single packet per submitted frame meant
        // any burst permanently became backlog, so the gap between a packet's content and the frame
        // being submitted could only grow.
        let mut newest: Option<Vec<u8>> = None;
        loop {
            match self.packets.try_recv() {
                Ok(Ok(packet)) => {
                    if let Some(dropped) = newest.replace(packet) {
                        // More than one packet ready in a single tick: publish the newest and let the
                        // older one go, rather than growing a queue we can never drain faster than
                        // one per tick.
                        tracing::debug!(bytes = dropped.len(), "ffmpeg: dropped a backlogged packet");
                        self.take_capture_time();
                    }
                }
                Ok(Err(e)) => {
                    return Err(anyhow!(
                        "read FFmpeg {} output: {e}: {}",
                        self.backend.describe(),
                        self.diagnostics()
                    ))
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    bail!(
                        "FFmpeg {} output closed: {}",
                        self.backend.describe(),
                        self.diagnostics()
                    )
                }
            }
        }
        match newest {
            Some(packet) => {
                self.since_packet = 0;
                self.last_packet = Instant::now();
                // Pair with the oldest un-answered submission. If the mapping has somehow run dry,
                // fall back to the submitted frame's own time rather than inventing one.
                let content_ns = self.take_capture_time().unwrap_or(captured_ns);
                Ok((packet, content_ns))
            }
            None => {
                self.since_packet = self.since_packet.saturating_add(1);
                // ALIVE BUT NOT ENCODING. Without this the producer would feed a wedged GPU for
                // ever: `try_wait` only catches an FFmpeg that has already exited, and the write no
                // longer fails because it happens on another thread. Returning Err is what makes
                // Auto's CPU fallback reachable on a driver wedge, and what makes GPU-only report a
                // real failure instead of freezing on its last picture.
                if self.since_packet > self.fps.max(1)
                    && self.last_packet.elapsed() > OUTPUT_STALL
                {
                    bail!(
                        "FFmpeg {} accepted {} frames over {:?} without encoding any: {}",
                        self.backend.describe(),
                        self.since_packet,
                        self.last_packet.elapsed(),
                        self.diagnostics()
                    );
                }
                Ok((Vec::new(), captured_ns))
            }
        }
    }

    /// Pop the oldest recorded submission time.
    fn take_capture_time(&self) -> Option<i64> {
        self.pending.lock().ok().and_then(|mut q| q.pop_front())
    }

    /// The pipe-driven FFmpeg backends use the two-second GOP configured at launch; neither has a
    /// reliable per-frame keyframe request through this interface.
    pub fn force_keyframe(&mut self) {}

    pub fn cpu_fallback_config(&self) -> (u32, u32, u32, u32) {
        (self.width, self.height, self.fps, self.bitrate_kbps)
    }

    pub fn backend_label(&self) -> &'static str {
        self.backend.label()
    }

    fn diagnostics(&self) -> String {
        self.stderr
            .lock()
            .map(|text| if text.is_empty() { "no FFmpeg diagnostic".to_string() } else { text.clone() })
            .unwrap_or_else(|_| "FFmpeg diagnostic lock poisoned".to_string())
    }
}

impl Drop for FfmpegAv1Encoder {
    fn drop(&mut self) {
        // ORDER IS LOAD-BEARING. Kill the child FIRST, then join.
        //
        // The writer thread may be parked in an 8 MB pipe write that FFmpeg has stopped reading.
        // Joining before killing would block teardown on exactly the wedge this is meant to survive;
        // killing first closes the pipe, so the parked write fails immediately and the thread exits.
        // (Reaching Drop at all is now guaranteed, because the producer thread no longer parks on the
        // write and therefore still observes its stop flag.)
        self.stop.store(true, Ordering::Relaxed);
        let (_, cv) = &*self.submit;
        cv.notify_all();
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        for thread in [self.writer_thread.take(), self.stdout_thread.take(), self.stderr_thread.take()]
        {
            if let Some(thread) = thread {
                let _ = thread.join();
            }
        }
    }
}

fn add_device_args(command: &mut Command, backend: &HardwareBackend) {
    if let HardwareBackend::Vaapi(device) = backend {
        command.args(["-vaapi_device"]).arg(device);
    }
}

fn add_encoder_args(command: &mut Command, backend: &HardwareBackend) {
    match backend {
        HardwareBackend::Vaapi(_) => {
            // -bf 0 on BOTH arms: the packet/capture-time pairing is a FIFO, which is only sound
            // while output order equals input order. B-frames reorder output, so leaving them to the
            // driver's default would silently mis-pair every packet with another frame's timestamp.
            command.args(["-vf", "format=nv12,hwupload", "-c:v", "av1_vaapi", "-bf", "0"]);
        }
        HardwareBackend::Nvenc => {
            command.args(["-c:v", "av1_nvenc", "-pix_fmt", "yuv420p", "-bf", "0"]);
        }
    }
}

fn ffmpeg_binary() -> OsString {
    std::env::var_os("NEWFOUNDSYNC_FFMPEG").unwrap_or_else(|| {
        // The screencast Flatpak supplies the NVENC-capable build in /app. Native development
        // builds retain a PATH fallback, which lets a distro FFmpeg provide either backend.
        if Path::new("/app/bin/ffmpeg").is_file() {
            OsString::from("/app/bin/ffmpeg")
        } else {
            OsString::from("ffmpeg")
        }
    })
}

fn render_nodes() -> Vec<PathBuf> {
    let mut nodes = fs::read_dir("/dev/dri")
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("renderD"))
        })
        .collect::<Vec<_>>();
    nodes.sort();
    nodes
}

/// Blocking pipe writes, on their own thread.
///
/// Takes the latest submitted frame and writes it to FFmpeg, recording its capture time in submit
/// order so the reader side can pair packets with the picture they contain. Parking here is fine —
/// that is the entire point of the thread — and a slow FFmpeg simply means frames get overwritten in
/// the slot before they are ever written, which is the correct way to shed load.
fn write_frames(
    mut stdin: ChildStdin,
    submit: Arc<(Mutex<Option<(Vec<u8>, i64)>>, Condvar)>,
    pending: Arc<Mutex<std::collections::VecDeque<i64>>>,
    stop: Arc<AtomicBool>,
) {
    let (lock, cv) = &*submit;
    loop {
        let frame = {
            let mut slot = match lock.lock() {
                Ok(slot) => slot,
                Err(_) => return,
            };
            while slot.is_none() {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                // Timed wait so `stop` is still observed if no frame ever arrives.
                let (next, _) = match cv.wait_timeout(slot, Duration::from_millis(200)) {
                    Ok(v) => v,
                    Err(_) => return,
                };
                slot = next;
            }
            slot.take()
        };
        let Some((bgra, captured_ns)) = frame else { continue };
        if stop.load(Ordering::Relaxed) {
            return;
        }
        // Record BEFORE writing: a packet can only exist once the bytes are in, and if the write
        // fails we drop the entry again so the mapping cannot drift.
        if let Ok(mut q) = pending.lock() {
            q.push_back(captured_ns);
            // The queue can only grow if FFmpeg stops emitting, which encode_bgra already turns into
            // an error — this is a belt-and-braces cap so a wedge cannot grow it without bound.
            while q.len() > 240 {
                q.pop_front();
            }
        }
        if stdin.write_all(&bgra).is_err() {
            // FFmpeg is gone or the pipe closed (including our own Drop killing the child). Undo the
            // record and stop; encode_bgra surfaces the failure via try_wait/stall detection.
            if let Ok(mut q) = pending.lock() {
                q.pop_back();
            }
            return;
        }
    }
}

fn read_ivf_packets(mut stdout: impl Read, packets: mpsc::Sender<Result<Vec<u8>, String>>) {
    let result = (|| -> Result<()> {
        let mut header = [0u8; 32];
        stdout.read_exact(&mut header).context("read IVF header")?;
        if &header[..4] != b"DKIF" {
            bail!("FFmpeg wrote a non-IVF stream");
        }
        loop {
            let mut packet_header = [0u8; 12];
            match stdout.read_exact(&mut packet_header) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(e).context("read IVF packet header"),
            }
            let len = u32::from_le_bytes(packet_header[..4].try_into().unwrap()) as usize;
            if len > MAX_AV1_PACKET_BYTES {
                bail!("FFmpeg produced an implausibly large AV1 packet ({len} bytes)");
            }
            let mut packet = vec![0u8; len];
            stdout.read_exact(&mut packet).context("read IVF AV1 packet")?;
            if packets.send(Ok(packet)).is_err() {
                return Ok(());
            }
        }
    })();
    if let Err(e) = result {
        let _ = packets.send(Err(format!("{e:#}")));
    }
}

fn collect_stderr(stderr: impl Read, latest: Arc<Mutex<String>>) {
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    while reader.read_line(&mut line).unwrap_or(0) != 0 {
        if let Ok(mut text) = latest.lock() {
            *text = line.trim().to_string();
        }
        line.clear();
    }
}
