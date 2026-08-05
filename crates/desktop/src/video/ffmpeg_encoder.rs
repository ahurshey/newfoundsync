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
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};

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
    stdin: Option<ChildStdin>,
    packets: mpsc::Receiver<Result<Vec<u8>, String>>,
    stdout_thread: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
    stderr: Arc<Mutex<String>>,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    backend: HardwareBackend,
}

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
            stdin: Some(stdin),
            packets,
            stdout_thread: Some(stdout_thread),
            stderr_thread: Some(stderr_thread),
            stderr: stderr_text,
            width,
            height,
            fps: fps.max(1),
            bitrate_kbps,
            backend,
        })
    }

    pub fn encode_bgra(&mut self, bgra: &[u8]) -> Result<Vec<u8>> {
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
        let stdin = self.stdin.as_mut().context("FFmpeg hardware encoder input is closed")?;
        stdin.write_all(bgra).with_context(|| {
            format!(
                "write frame to FFmpeg {} encoder: {}",
                self.backend.describe(),
                self.diagnostics()
            )
        })?;
        match self.packets.try_recv() {
            Ok(Ok(packet)) => Ok(packet),
            Ok(Err(e)) => Err(anyhow!(
                "read FFmpeg {} output: {e}: {}",
                self.backend.describe(),
                self.diagnostics()
            )),
            Err(mpsc::TryRecvError::Empty) => Ok(Vec::new()),
            Err(mpsc::TryRecvError::Disconnected) => {
                bail!("FFmpeg {} output closed: {}", self.backend.describe(), self.diagnostics())
            }
        }
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
        // Closing stdin lets FFmpeg flush and exit. If a driver wedges, terminate only the child we
        // own so VideoProducer's bounded shutdown remains bounded.
        self.stdin.take();
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        if let Some(thread) = self.stdout_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
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
            command.args(["-vf", "format=nv12,hwupload", "-c:v", "av1_vaapi"]);
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
