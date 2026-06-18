//! Audio output: a cpal stream draining a lock-free `rtrb` ring, plus the real
//! [`PlayoutSink`] that decodes scheduled frames and feeds that ring.
//!
//! The scheduler thread pushes decoded f32 samples into the ring at playout
//! cadence; the cpal callback pulls them at the device rate and writes silence on
//! underrun. The callback is lock-free and allocation-free (only `pop`), as the
//! real-time audio path requires.
//!
//! v1 requests the canonical 48 kHz / stereo / f32 format from the default
//! device. (Device-rate resampling via `rubato` and `IAudioClient::GetStreamLatency`
//! pre-seeding of the delay slider are noted follow-ups.)

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rtrb::{Producer, RingBuffer};

use crate::client::playout::PlayoutSink;
use crate::codec::{Decoder, FRAME_SAMPLE_COUNT};
use crate::config::{self, CHANNELS};

/// EMA smoothing for the ring-fill estimate driving the drift nudge.
const EMA_ALPHA: f32 = 0.05;

/// Conservative single-(stereo-)sample drift nudge from the smoothed ring fill.
///
/// The scheduler feeds the ring at master-clock cadence; the cpal callback drains
/// it at the device rate. A mismatch slowly fills or drains the ring. Rather than
/// let it reach an audible skip/underrun, nudge by one stereo sample per frame
/// when the smoothed fill leaves a deadband around the half-full target — an
/// inaudible ~0.1%/frame correction that bounds long-run drift without a PI servo
/// (the approach the `ensemble`/`soundsync` research converged on).
///
/// Returns -1 (drop one stereo sample: ring too full → we're ahead),
/// +1 (duplicate one: ring too empty → we're behind), or 0.
fn decide_nudge(ema_fill: f32) -> i32 {
    const HIGH: f32 = 0.60;
    const LOW: f32 = 0.40;
    if ema_fill > HIGH {
        -1
    } else if ema_fill < LOW {
        1
    } else {
        0
    }
}

/// Lock-free linear volume in `[0.0, 1.0]`, stored as f32 bits.
#[derive(Clone)]
pub struct Volume(Arc<AtomicU32>);

impl Volume {
    pub fn new(v: f32) -> Self {
        Volume(Arc::new(AtomicU32::new(v.clamp(0.0, 1.0).to_bits())))
    }
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
    pub fn set(&self, v: f32) {
        self.0.store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
}

impl Default for Volume {
    fn default() -> Self {
        Volume::new(1.0)
    }
}

/// Shared output-path counters for the UI.
#[derive(Default)]
pub struct OutputStats {
    /// Samples the cpal callback wanted but the ring couldn't supply (underruns).
    pub underrun_samples: AtomicU64,
}

/// A live cpal output stream. Dropping it stops playback.
pub struct AudioOutput {
    _stream: cpal::Stream,
    pub device_name: String,
    pub sample_rate: u32,
    pub channels: u16,
}

/// Names of the available output devices (for a device picker). Best-effort.
pub fn output_device_names() -> Vec<String> {
    cpal::default_host()
        .output_devices()
        .map(|it| it.map(|d| d.to_string()).collect())
        .unwrap_or_default()
}

impl AudioOutput {
    /// Open the default output device at 48 kHz/stereo/f32 and start a stream that
    /// drains a fresh ring. Returns the stream handle (keep it alive) and the
    /// producer end to hand to the [`RtrbSink`].
    pub fn open_default(
        ring_frames: usize,
        stats: Arc<OutputStats>,
    ) -> Result<(AudioOutput, Producer<f32>)> {
        Self::open_named(None, ring_frames, stats)
    }

    /// Open a specific output device by name (`None` = system default).
    pub fn open_named(
        name: Option<&str>,
        ring_frames: usize,
        stats: Arc<OutputStats>,
    ) -> Result<(AudioOutput, Producer<f32>)> {
        let host = cpal::default_host();
        let device = match name {
            Some(n) => host
                .output_devices()
                .ok()
                .and_then(|mut it| it.find(|d| d.to_string() == n))
                .ok_or_else(|| anyhow!("output device '{n}' not found"))?,
            None => host
                .default_output_device()
                .ok_or_else(|| anyhow!("no default audio output device"))?,
        };
        let device_name = device.to_string(); // cpal 0.18: Device: Display

        // cpal 0.18: SampleRate = u32, ChannelCount = u16 (plain aliases).
        let sample_rate: u32 = config::SAMPLE_RATE;
        let channels: u16 = config::CHANNELS as u16;
        let cfg = cpal::StreamConfig {
            channels,
            sample_rate,
            buffer_size: cpal::BufferSize::Default,
        };

        let cap = ring_frames.max(2) * FRAME_SAMPLE_COUNT;
        let (producer, mut consumer) = RingBuffer::<f32>::new(cap);

        let cb_stats = stats.clone();
        let err_fn = |e| tracing::warn!("cpal output error: {e}");
        let stream = device
            .build_output_stream(
                cfg,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let mut i = 0;
                    while i < data.len() {
                        match consumer.pop() {
                            Ok(s) => {
                                data[i] = s;
                                i += 1;
                            }
                            Err(_) => break, // ring empty → underrun
                        }
                    }
                    if i < data.len() {
                        cb_stats
                            .underrun_samples
                            .fetch_add((data.len() - i) as u64, Ordering::Relaxed);
                        for s in &mut data[i..] {
                            *s = 0.0;
                        }
                    }
                },
                err_fn,
                None,
            )
            .context("build_output_stream (48kHz stereo f32)")?;
        stream.play().context("stream.play")?;

        Ok((
            AudioOutput {
                _stream: stream,
                device_name,
                sample_rate,
                channels,
            },
            producer,
        ))
    }
}

/// The production [`PlayoutSink`]: decode → apply volume → push f32 into the ring.
pub struct RtrbSink {
    producer: Producer<f32>,
    decoder: Decoder,
    volume: Volume,
    overruns: u64,
    capacity: usize,
    ema_fill: f32,
}

impl RtrbSink {
    pub fn new(producer: Producer<f32>, decoder: Decoder, volume: Volume) -> Self {
        // A fresh ring is empty, so free slots == total capacity.
        let capacity = producer.slots().max(1);
        RtrbSink {
            producer,
            decoder,
            volume,
            overruns: 0,
            capacity,
            ema_fill: 0.5,
        }
    }

    fn push_sample(&mut self, s: i16, gain: f32) {
        let f = (s as f32 / 32768.0) * gain;
        if self.producer.push(f).is_err() {
            self.overruns += 1; // ring full (consumer behind) → drop sample
        }
    }

    fn push_i16(&mut self, frame: &[i16]) {
        let gain = self.volume.get();

        // Smoothed ring fill → conservative drift nudge.
        let free = self.producer.slots().min(self.capacity);
        let fill = (self.capacity - free) as f32 / self.capacity as f32;
        self.ema_fill = self.ema_fill * (1.0 - EMA_ALPHA) + fill * EMA_ALPHA;
        let nudge = decide_nudge(self.ema_fill);

        // Drop/keep: emit one fewer stereo sample when the ring is running full.
        let emit = if nudge < 0 {
            frame.len().saturating_sub(CHANNELS)
        } else {
            frame.len()
        };
        for &s in &frame[..emit] {
            self.push_sample(s, gain);
        }
        // Add: duplicate the last stereo sample when the ring is running empty.
        if nudge > 0 && frame.len() >= CHANNELS {
            for &s in &frame[frame.len() - CHANNELS..] {
                self.push_sample(s, gain);
            }
        }
    }
}

impl PlayoutSink for RtrbSink {
    fn play(&mut self, _seq: u64, payload: &[u8]) {
        match self.decoder.decode(payload) {
            Ok(frame) => self.push_i16(&frame),
            Err(e) => tracing::debug!("frame decode failed: {e}"),
        }
    }

    fn silence(&mut self, _seq: u64) {
        // Keep the ring fed at cadence so the device clock stays aligned.
        for _ in 0..FRAME_SAMPLE_COUNT {
            if self.producer.push(0.0).is_err() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_clamps_and_roundtrips() {
        let v = Volume::new(0.5);
        assert!((v.get() - 0.5).abs() < 1e-6);
        v.set(2.0);
        assert_eq!(v.get(), 1.0);
        v.set(-1.0);
        assert_eq!(v.get(), 0.0);
    }

    #[test]
    fn drift_nudge_direction() {
        assert_eq!(decide_nudge(0.5), 0, "centered → no correction");
        assert_eq!(decide_nudge(0.50), 0);
        assert_eq!(decide_nudge(0.75), -1, "ring too full → drop a sample");
        assert_eq!(decide_nudge(0.25), 1, "ring too empty → add a sample");
        // Deadband edges are inclusive-ish; just outside flips.
        assert_eq!(decide_nudge(0.61), -1);
        assert_eq!(decide_nudge(0.39), 1);
    }
}
