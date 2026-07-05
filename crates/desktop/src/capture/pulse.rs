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
use pulse::context::{Context, FlagSet as ContextFlagSet, State as ContextState};
use pulse::mainloop::standard::{IterateResult, Mainloop};
use pulse::operation::State as OpState;
use pulse::sample::{Format, Spec};
use pulse::stream::Direction;

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
        Ok(PulseCapture { stop, thread: Some(thread), done_rx, device_name: monitor })
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

/// Connect to PulseAudio/PipeWire, read the default sink name, and return `<sink>.monitor`.
fn default_sink_monitor() -> Result<String> {
    let mut mainloop = Mainloop::new().ok_or_else(|| anyhow!("PulseAudio mainloop alloc failed"))?;
    let mut context = Context::new(&mainloop, "Newfoundsync-probe")
        .ok_or_else(|| anyhow!("PulseAudio context alloc failed"))?;
    context
        .connect(None, ContextFlagSet::NOFLAGS, None)
        .map_err(|e| anyhow!("connect to PulseAudio/PipeWire: {e:?}"))?;

    // Pump the mainloop until the context is ready (or fails).
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

    let sink: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let sink_cb = sink.clone();
    let op = context.introspect().get_server_info(move |info| {
        if let Some(name) = info.default_sink_name.as_ref() {
            *sink_cb.lock().unwrap() = Some(name.to_string());
        }
    });
    loop {
        match mainloop.iterate(true) {
            IterateResult::Success(_) => {}
            IterateResult::Quit(_) => bail!("PulseAudio mainloop quit querying server info"),
            IterateResult::Err(e) => bail!("PulseAudio mainloop error: {e:?}"),
        }
        match op.get_state() {
            OpState::Done => break,
            OpState::Cancelled => bail!("PulseAudio server-info query cancelled"),
            OpState::Running => {}
        }
    }

    let sink = sink
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| anyhow!("no default sink reported — is any audio output active?"))?;
    Ok(format!("{sink}.monitor"))
}
