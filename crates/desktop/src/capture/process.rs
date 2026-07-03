// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! WASAPI process-loopback capture (Windows 10 2004+).
//!
//! Unlike endpoint loopback (cpal), this taps applications' render streams via
//! `ActivateAudioInterfaceAsync` + `PROCESS_LOOPBACK`, so it keeps capturing even
//! when the system output endpoint is muted. We use EXCLUDE mode against our own
//! PID to capture "all other audio" (survives mute, and never picks up our own
//! local monitor — no feedback), or INCLUDE mode to capture one app's tree.
//!
//! The activated client is asked for 48 kHz stereo f32, so output is already
//! canonical — we just fold to `i16` and chunk into 20 ms frames for `on_frame`,
//! matching `SystemCapture`'s contract.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use windows::core::{implement, Interface, Ref, IUnknown};
use windows::Win32::Foundation::E_POINTER;
use windows::Win32::Media::Audio::{
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDIOCLIENT_ACTIVATION_PARAMS,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    PROCESS_LOOPBACK_MODE, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    WAVEFORMATEX,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::System::Variant::VT_BLOB;

use newfoundsync_core::codec::FRAME_SAMPLE_COUNT;

type ActivateSlot = Arc<(Mutex<Option<windows::core::Result<IAudioClient>>>, Condvar)>;

/// COM completion handler for `ActivateAudioInterfaceAsync` — stashes the
/// resulting `IAudioClient` (or error) and wakes the waiting thread.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivateHandler {
    slot: ActivateSlot,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivateHandler_Impl {
    fn ActivateCompleted(
        &self,
        op: Ref<'_, IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let result = (|| unsafe {
            let op = op.ok()?;
            let mut hr = windows::core::HRESULT(0);
            let mut iface: Option<IUnknown> = None;
            op.GetActivateResult(&mut hr, &mut iface)?;
            hr.ok()?;
            iface
                .ok_or_else(|| windows::core::Error::from(E_POINTER))?
                .cast::<IAudioClient>()
        })();
        let (lock, cv) = &*self.slot;
        // This runs inside a COM callback invoked across the C ABI — never let a
        // panic (e.g. a poisoned mutex) unwind across that boundary.
        if let Ok(mut slot) = lock.lock() {
            *slot = Some(result);
        }
        cv.notify_all();
        Ok(())
    }
}

/// A running process-loopback capture. Stops + joins on drop.
pub struct ProcessCapture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ProcessCapture {
    /// Capture every process's audio EXCEPT the current one (survives endpoint
    /// mute, and excludes our own monitor output so it can't feed back).
    pub fn start_exclude_current<F>(on_frame: F) -> Result<ProcessCapture>
    where
        F: FnMut(&[i16]) + Send + 'static,
    {
        let pid = unsafe { GetCurrentProcessId() };
        Self::start(pid, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE, on_frame)
    }

    /// Capture only the given process (and its child tree).
    pub fn start_include<F>(pid: u32, on_frame: F) -> Result<ProcessCapture>
    where
        F: FnMut(&[i16]) + Send + 'static,
    {
        Self::start(pid, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, on_frame)
    }

    fn start<F>(pid: u32, mode: PROCESS_LOOPBACK_MODE, on_frame: F) -> Result<ProcessCapture>
    where
        F: FnMut(&[i16]) + Send + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        // Init runs on the capture thread (COM apartment + the client must live
        // there); the result comes back so the caller can fall back on failure.
        let (tx, rx) = mpsc::channel::<std::result::Result<(), String>>();
        let stop_t = stop.clone();
        let thread = thread::Builder::new()
            .name("proc-capture".into())
            .spawn(move || run(pid, mode, on_frame, stop_t, tx))
            .context("spawn process-capture thread")?;

        match rx.recv_timeout(Duration::from_secs(6)) {
            Ok(Ok(())) => Ok(ProcessCapture {
                stop,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                stop.store(true, Ordering::Relaxed);
                let _ = thread.join();
                bail!("process-loopback capture failed: {e}");
            }
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                bail!("process-loopback capture init timed out");
            }
        }
    }
}

impl Drop for ProcessCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

unsafe fn activate(pid: u32, mode: PROCESS_LOOPBACK_MODE) -> Result<IAudioClient> {
    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        ..Default::default()
    };
    params.Anonymous.ProcessLoopbackParams = AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
        TargetProcessId: pid,
        ProcessLoopbackMode: mode,
    };

    // PROPVARIANT carrying the activation params as a VT_BLOB pointing at our
    // stack `params`. `PROPVARIANT` has a Drop that runs `PropVariantClear`,
    // which would `CoTaskMemFree` our stack pointer (heap corruption) — so wrap
    // it in `ManuallyDrop` and never let it drop. `params` outlives the call.
    let mut pv = std::mem::ManuallyDrop::new(std::mem::zeroed::<PROPVARIANT>());
    {
        let v = &mut *pv.Anonymous.Anonymous;
        v.vt = VT_BLOB;
        v.Anonymous.blob.cbSize = std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32;
        v.Anonymous.blob.pBlobData = &mut params as *mut _ as *mut u8;
    }

    let slot: ActivateSlot = Arc::new((Mutex::new(None), Condvar::new()));
    let handler: IActivateAudioInterfaceCompletionHandler =
        ActivateHandler { slot: slot.clone() }.into();

    let _op = ActivateAudioInterfaceAsync(
        VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
        &IAudioClient::IID,
        Some(&*pv),
        &handler,
    )
    .context("ActivateAudioInterfaceAsync")?;

    let (lock, cv) = &*slot;
    let mut g = lock.lock().unwrap();
    while g.is_none() {
        let (ng, to) = cv.wait_timeout(g, Duration::from_secs(5)).unwrap();
        g = ng;
        if to.timed_out() && g.is_none() {
            bail!("process-loopback activation timed out");
        }
    }
    let client = g.take().unwrap().context("activation result")?;
    drop(g);
    let _ = &params; // keep alive past the Activate call
    Ok(client)
}

fn run<F>(
    pid: u32,
    mode: PROCESS_LOOPBACK_MODE,
    mut on_frame: F,
    stop: Arc<AtomicBool>,
    tx: Sender<std::result::Result<(), String>>,
) where
    F: FnMut(&[i16]),
{
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        let init = (|| -> Result<(IAudioClient, IAudioCaptureClient)> {
            let client = activate(pid, mode)?;
            let fmt = WAVEFORMATEX {
                wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
                nChannels: 2,
                nSamplesPerSec: 48_000,
                nAvgBytesPerSec: 48_000 * 8,
                nBlockAlign: 8,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK,
                    2_000_000, // 200 ms ring
                    0,
                    &fmt,
                    None,
                )
                .context("IAudioClient::Initialize (process loopback)")?;
            let capture: IAudioCaptureClient =
                client.GetService().context("GetService(IAudioCaptureClient)")?;
            client.Start().context("IAudioClient::Start")?;
            Ok((client, capture))
        })();

        let (client, capture) = match init {
            Ok(v) => {
                let _ = tx.send(Ok(()));
                v
            }
            Err(e) => {
                let _ = tx.send(Err(format!("{e:#}")));
                return;
            }
        };

        // Discard the engine's startup backlog so we emit near-current audio
        // (process loopback pre-buffers ~hundreds of ms; without this, every
        // emitted frame would lag real time and overrun the client's buffer).
        loop {
            let p = capture.GetNextPacketSize().unwrap_or(0);
            if p == 0 {
                break;
            }
            let mut d: *mut u8 = std::ptr::null_mut();
            let mut nf = 0u32;
            let mut fl = 0u32;
            if capture.GetBuffer(&mut d, &mut nf, &mut fl, None, None).is_err() {
                break;
            }
            let _ = capture.ReleaseBuffer(nf);
        }

        // Process loopback delivers jittery / sparse packets (little or nothing
        // during silence). The client schedules playout by sequence number at a
        // steady 20 ms cadence, so we MUST emit exactly real-time frames: drain
        // whatever the engine has into a buffer, then emit one frame every 20 ms
        // of wall time, padding silence on underrun and dropping excess.
        let frame_dur = Duration::from_millis(20);
        let mut buf: VecDeque<i16> = VecDeque::with_capacity(FRAME_SAMPLE_COUNT * 6);
        let mut frame = vec![0i16; FRAME_SAMPLE_COUNT];
        let mut next = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            // Drain available engine packets into the buffer.
            loop {
                let packet = match capture.GetNextPacketSize() {
                    Ok(p) => p,
                    Err(_) => break,
                };
                if packet == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut nframes = 0u32;
                let mut flags = 0u32;
                if capture
                    .GetBuffer(&mut data, &mut nframes, &mut flags, None, None)
                    .is_err()
                {
                    break;
                }
                let silent = (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
                let n = nframes as usize;
                if !data.is_null() && n > 0 {
                    if silent {
                        buf.extend(std::iter::repeat(0).take(n * 2));
                    } else {
                        let samples = std::slice::from_raw_parts(data as *const f32, n * 2);
                        buf.extend(samples.iter().map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16));
                    }
                }
                let _ = capture.ReleaseBuffer(nframes);
            }

            // Emit at real-time cadence (silence-pad on underrun).
            let now = Instant::now();
            if now > next + Duration::from_millis(200) {
                next = now; // recover from a long stall instead of bursting
            }
            while now >= next {
                for slot in frame.iter_mut() {
                    *slot = buf.pop_front().unwrap_or(0);
                }
                on_frame(&frame);
                next += frame_dur;
            }

            // Bound latency if the engine over-delivers.
            while buf.len() > FRAME_SAMPLE_COUNT * 4 {
                buf.pop_front();
            }
            thread::sleep(Duration::from_millis(2));
        }
        let _ = client.Stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    /// Activates process-loopback (exclude-self) and runs briefly. Proves the
    /// COM activation + Initialize + capture loop work on this Windows build.
    /// Ignored by default (needs a desktop session). Frame count is informational
    /// (0 is fine if nothing is playing); the test passes if init didn't error.
    #[test]
    #[ignore = "needs a Windows desktop session; run alone"]
    fn process_loopback_exclude_self_activates() {
        let frames = Arc::new(AtomicU64::new(0));
        let f = frames.clone();
        let cap = match ProcessCapture::start_exclude_current(move |_frame: &[i16]| {
            f.fetch_add(1, Ordering::Relaxed);
        }) {
            Ok(c) => c,
            Err(e) => panic!("process-loopback activation failed: {e}"),
        };
        thread::sleep(Duration::from_millis(800));
        drop(cap);
        eprintln!("process-loopback frames captured: {}", frames.load(Ordering::Relaxed));
    }
}
