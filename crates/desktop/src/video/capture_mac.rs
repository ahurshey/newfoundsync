// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! macOS screen capture via ScreenCaptureKit → BGRA frames.
//!
//! Presents the SAME surface as the Windows (WGC) and Linux (portal + PipeWire) backends —
//! `CapturedFrame` / `FrameSlot` / `ScreenCapture { closed, slot }` / `start_primary()` — under the
//! same module path, so `VideoProducer` consumes any of the three without knowing which it has.
//!
//! ScreenCaptureKit is the only supported way to do this on a modern macOS: the old
//! `CGDisplayStream` is deprecated, and screen recording is gated behind TCC either way. It needs
//! macOS 12.3+, which is far below anything this app runs on.
//!
//! # The permission is the whole user experience
//!
//! Screen Recording is a TCC permission, and macOS does not fail loudly when it is missing — the
//! stream starts and simply never delivers a frame, which is indistinguishable from a static
//! desktop. So `start_primary` treats "no frame within a grace period" as a hard error with an
//! explicit instruction, rather than handing back a live-looking object that will never produce
//! anything. This is the same failure shape as the macOS audio tap (a denied tap yields silence,
//! not an error), and it is worth the extra code both times.
//!
//! Note the prompt only appears for a process running in the user's GUI session. A binary launched
//! over SSH gets denied without a dialog; run the .app (or launch from a terminal in the GUI
//! session) the first time.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use newfoundsync_core::config::mono_now;
// `width`/`height`/`bytes_per_row`/`as_slice` are inherent methods on the lock guard itself — no
// extension trait to import (the crate's own doc example implies one; there isn't).
use screencapturekit::cv::CVPixelBufferLockFlags;
use screencapturekit::stream::delegate_trait::StreamCallbacks;
use screencapturekit::prelude::*;

/// How long to wait for the first frame before calling it a permission failure. ScreenCaptureKit
/// delivers the first frame almost immediately when it is allowed to, so this only has to be longer
/// than a slow first-frame; it is not a "maybe the screen is idle" timeout, because SCK sends an
/// initial frame even for a static desktop.
const FIRST_FRAME_GRACE: Duration = Duration::from_secs(5);

/// One captured frame: tightly-packed BGRA (`width*height*4`). Byte-identical contract to the other
/// two backends — the encoder's `bgra_to_i420` reads b,g,r at p, p+1, p+2 and ignores the fourth
/// byte, so a BGRX source needs no alpha fixup, only de-striding.
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
    /// `mono_now()` at the moment this picture arrived from ScreenCaptureKit. The video PTS is
    /// derived from THIS, not from when the encoder finished, so the timestamp describes the content
    /// and not the pipeline. Same contract as the other backends' field.
    pub captured_ns: i64,
}

/// Latest-frame slot, overwrite-on-arrival. Never a queue: the encoder pulls at its own fps and
/// stale frames should be dropped, not backlogged.
pub type FrameSlot = Arc<Mutex<Option<CapturedFrame>>>;

/// A live ScreenCaptureKit stream. Stops on drop.
pub struct ScreenCapture {
    /// Set when the capture ends for real (stream error, display gone).
    ///
    /// NEVER set on a frame drought. ScreenCaptureKit, like WGC and PipeWire, only sends frames when
    /// something changes, so a static desktop legitimately produces none for an unbounded time and
    /// silence must not be read as death.
    pub closed: Arc<AtomicBool>,
    pub slot: FrameSlot,
    stream: Option<SCStream>,
}

/// Receives frames on ScreenCaptureKit's dispatch queue and drops them into the shared slot.
///
/// Apple may invoke this from arbitrary threads (hence `Send + Sync`), which is exactly why the slot
/// is a Mutex rather than anything cleverer.
struct Handler {
    slot: FrameSlot,
    /// What we ASKED ScreenCaptureKit for, so the first delivered frame can be compared against it.
    asked: (u32, u32),
    logged: AtomicBool,
}

impl SCStreamOutputTrait for Handler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, _kind: SCStreamOutputType) {
        // Stamped on arrival, before the de-stride copy: the PTS has to describe when the picture
        // existed, not when the encoder was done with it.
        let captured_ns = mono_now();
        // A sample with no image buffer is normal — SCK emits status-only samples (e.g. "screen
        // unchanged") that carry no pixels. Skipping them is not an error.
        let Some(buffer) = sample.image_buffer() else { return };
        let Ok(guard) = buffer.lock(CVPixelBufferLockFlags::READ_ONLY) else { return };
        let (w, h) = (guard.width() as usize, guard.height() as usize);
        if w == 0 || h == 0 {
            return;
        }
        let stride = guard.bytes_per_row();
        let src = guard.as_slice();
        let row = w * 4;
        // Rows are stride-padded (SCK aligns to 64 bytes); the encoder wants them tightly packed.
        if stride < row || src.len() < stride * (h - 1) + row {
            return; // short buffer — skip rather than read out of bounds
        }
        let mut bgra = vec![0u8; row * h];
        for y in 0..h {
            bgra[y * row..(y + 1) * row].copy_from_slice(&src[y * stride..y * stride + row]);
        }
        // POINTS-vs-PIXELS, answered by the first real frame rather than guessed.
        //
        // `SCDisplay::width/height` are documented by Apple as POINTS, while SCStreamConfiguration's
        // width/height are PIXELS (the crate's own doc comment calls the display ones pixels, which
        // is why this is worth checking rather than trusting). If they are points, then on any
        // Retina Mac we are configuring the stream at half the panel's real resolution and every
        // 1080p+ stream is an upscale of a ~1512-wide capture.
        //
        // Blind-fixing it by multiplying by 2 would double the capture cost if the value was already
        // pixels, so instead: say what we asked for and what arrived, once. One line in the log
        // settles it on the first machine that ever runs this.
        if !self.logged.swap(true, Ordering::Relaxed) {
            tracing::info!(
                asked_w = self.asked.0,
                asked_h = self.asked.1,
                got_w = w,
                got_h = h,
                "[capture] macOS first frame — if 'got' is ~2x 'asked' this display is Retina and                  the configured size was in points; if they match, the capture is at the size we set"
            );
        }
        if let Ok(mut slot) = self.slot.lock() {
            *slot = Some(CapturedFrame {
                width: w as u32,
                height: h as u32,
                bgra,
                captured_ns,
            });
        }
        // Deliberately does NOT touch `closed`. That flag is a one-way death latch — the producer
        // mirrors it into health as "capture died" and never expects it to go back. Clearing it here
        // (which an earlier version did, on the reasoning that a frame proves liveness) would let a
        // single in-flight frame delivered after teardown un-report a real capture death, and cost an
        // atomic write per frame to do it.
    }
}

impl ScreenCapture {
    /// Capture the primary display.
    pub fn start_primary() -> Result<ScreenCapture> {
        // SCShareableContent is also the permission probe: without Screen Recording it either errors
        // or reports no displays, so a clear message here beats a stream that never delivers.
        let content = SCShareableContent::get().map_err(|e| {
            anyhow!(
                "macOS would not list the screen ({e:?}). Grant Screen Recording in System \
                 Settings → Privacy & Security → Screen & System Audio Recording, then start the \
                 stream again."
            )
        })?;
        let displays = content.displays();
        let display = displays.first().ok_or_else(|| {
            anyhow!(
                "macOS reported no displays to capture. This is what a missing Screen Recording \
                 permission looks like — grant it in System Settings → Privacy & Security → \
                 Screen & System Audio Recording."
            )
        })?;

        // Capture at the display's own size and let the producer scale: the same division of labour
        // as the other two backends, so one scaler serves all three.
        let (w, h) = (display.width(), display.height());
        let filter = SCContentFilter::create()
            .with_display(display)
            .with_excluding_windows(&[])
            .build();
        let config = SCStreamConfiguration::new()
            .with_width(w)
            .with_height(h)
            // BGRA so the frame needs de-striding and nothing else. Asking for YCbCr here would mean
            // converting twice, since the encoder wants I420 from BGRA.
            .with_pixel_format(PixelFormat::BGRA);

        let slot: FrameSlot = Arc::new(Mutex::new(None));
        let closed = Arc::new(AtomicBool::new(false));

        // A DELEGATE, so a stream that dies mid-session is REPORTED. Without one, `closed` could
        // only ever be set by our own Drop, and a capture killed by macOS (display unplugged,
        // permission revoked while running, stream error) would leave the producer happily
        // re-encoding its last frame forever while /health still said healthy — the exact failure
        // the other two backends have an authoritative signal for, and the reason `capture_frames`
        // is tracked separately from `video_frames`.
        let died = closed.clone();
        let delegate = StreamCallbacks::new()
            .on_stop(move |err| {
                match err {
                    Some(e) => tracing::error!("macOS screen capture stopped: {e}"),
                    // A stop with no error is also death as far as the producer is concerned: no
                    // further frames are coming, whatever the reason.
                    None => tracing::warn!("macOS screen capture stopped"),
                }
                died.store(true, Ordering::Relaxed);
            })
            .on_error(|e| tracing::error!("macOS screen capture error: {e}"));

        let mut stream = SCStream::new_with_delegate(&filter, &config, delegate);
        stream.add_output_handler(
            Handler { slot: slot.clone(), asked: (w, h), logged: AtomicBool::new(false) },
            SCStreamOutputType::Screen,
        );
        stream
            .start_capture()
            .map_err(|e| anyhow!("could not start the macOS screen capture: {e:?}"))?;

        // WAIT FOR PROOF. macOS does not report a denied Screen Recording permission as an error —
        // the stream starts and stays empty forever, which looks exactly like a healthy capture of a
        // static desktop. Requiring one real frame is what turns that silent denial into a message.
        let deadline = Instant::now() + FIRST_FRAME_GRACE;
        while Instant::now() < deadline {
            if slot.lock().map(|s| s.is_some()).unwrap_or(false) {
                tracing::info!(width = w, height = h, "[capture] macOS ScreenCaptureKit active");
                return Ok(ScreenCapture { closed, slot, stream: Some(stream) });
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = stream.stop_capture();
        bail!(
            "the screen capture started but produced no frame in {}s, which on macOS almost always \
             means Screen Recording is not granted (a denied capture is silent, not an error). \
             Grant it in System Settings → Privacy & Security → Screen & System Audio Recording, \
             then start the stream again. If this app was launched over SSH, macOS cannot show the \
             prompt — run it from the desktop once.",
            FIRST_FRAME_GRACE.as_secs()
        )
    }

    /// Not available on this platform — the picker UI that would select one window is macOS 14+ and
    /// interactive, and the per-window audio this pairs with on Windows has no macOS equivalent.
    pub fn start_window(_id: isize) -> Result<ScreenCapture> {
        bail!(
            "capturing a single window is Windows-only. On macOS pick \"Whole screen\", or relay a \
             browser cast with --capture web --video"
        )
    }
}

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        if let Some(mut s) = self.stream.take() {
            if let Err(e) = s.stop_capture() {
                tracing::warn!("macOS screen capture did not stop cleanly: {e:?}");
            }
        }
        self.closed.store(true, Ordering::Relaxed);
    }
}
