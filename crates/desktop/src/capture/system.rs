//! System-wide loopback capture via cpal.
//!
//! Opens an *input* stream on the default *output* device — cpal sets the WASAPI
//! `AUDCLNT_STREAMFLAGS_LOOPBACK` flag for render endpoints, capturing the system
//! mix. Incoming audio (device rate / channels / f32) is folded to stereo,
//! linear-resampled to the canonical 48 kHz, and accumulated into canonical 20 ms
//! `i16` frames delivered to `on_frame`.
//!
//! `on_frame` runs on cpal's capture thread, so it must be quick (encode + a
//! non-blocking UDP fan-out is fine).

use anyhow::{anyhow, bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use newfoundsync_core::codec::FRAME_SAMPLE_COUNT;
use newfoundsync_core::config;

fn f32_to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * 32767.0) as i16
}

/// Streaming linear resampler from `src_rate` to the canonical 48 kHz, stereo.
/// State persists across cpal callbacks (carries the previous input sample and
/// the fractional read position).
struct Resampler {
    inc: f64, // input samples consumed per output sample = src_rate / 48000
    t: f64,   // fractional position within [prev, cur]
    prev_l: f32,
    prev_r: f32,
    have_prev: bool,
}

impl Resampler {
    fn new(src_rate: u32) -> Self {
        Resampler {
            inc: src_rate as f64 / config::SAMPLE_RATE as f64,
            t: 0.0,
            prev_l: 0.0,
            prev_r: 0.0,
            have_prev: false,
        }
    }

    /// Feed one stereo input sample; emit interpolated 48 kHz samples into `acc`
    /// (interleaved L,R i16), flushing a full frame to `on_frame` each 20 ms.
    fn push<F: FnMut(&[i16])>(&mut self, l: f32, r: f32, acc: &mut Vec<i16>, on_frame: &mut F) {
        if !self.have_prev {
            self.prev_l = l;
            self.prev_r = r;
            self.have_prev = true;
            self.t = 0.0;
            return;
        }
        while self.t < 1.0 {
            let u = self.t as f32;
            acc.push(f32_to_i16(self.prev_l + (l - self.prev_l) * u));
            acc.push(f32_to_i16(self.prev_r + (r - self.prev_r) * u));
            if acc.len() >= FRAME_SAMPLE_COUNT {
                on_frame(acc);
                acc.clear();
            }
            self.t += self.inc;
        }
        self.t -= 1.0;
        self.prev_l = l;
        self.prev_r = r;
    }
}

/// A running system-loopback capture. Stops when dropped.
pub struct SystemCapture {
    _stream: cpal::Stream,
    pub device_name: String,
    pub source_rate: u32,
    pub source_channels: u16,
}

impl SystemCapture {
    /// Start capturing the system mix, delivering canonical 48 kHz stereo `i16`
    /// 20 ms frames (`FRAME_SAMPLE_COUNT` interleaved samples) to `on_frame`.
    pub fn start<F>(mut on_frame: F) -> Result<SystemCapture>
    where
        F: FnMut(&[i16]) + Send + 'static,
    {
        let host = cpal::default_host();
        let (device, supported) = pick_capture_device(&host)?;
        let device_name = device.to_string();
        let source_rate = supported.sample_rate();
        let source_channels = supported.channels();
        let sample_format = supported.sample_format();

        if sample_format != cpal::SampleFormat::F32 {
            // WASAPI shared mode and Pulse/PipeWire monitors are virtually always f32.
            bail!(
                "capture format is {sample_format:?}; v1 supports f32 only — \
                 set the source device's shared format to 32-bit float"
            );
        }

        let cfg: cpal::StreamConfig = supported.config();
        let ch = source_channels as usize;

        let mut rs = Resampler::new(source_rate);
        let mut acc: Vec<i16> = Vec::with_capacity(FRAME_SAMPLE_COUNT + 2);

        let err_fn = |e| tracing::warn!("cpal loopback capture error: {e}");
        let stream = device
            .build_input_stream(
                cfg,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    for frame in data.chunks_exact(ch) {
                        let l = frame[0];
                        let r = if ch >= 2 { frame[1] } else { frame[0] };
                        rs.push(l, r, &mut acc, &mut on_frame);
                    }
                },
                err_fn,
                None,
            )
            .context("build loopback input stream")?;
        stream.play().context("start loopback capture")?;

        tracing::info!(
            device = %device_name,
            source_rate,
            source_channels,
            "system loopback capture started"
        );

        Ok(SystemCapture {
            _stream: stream,
            device_name,
            source_rate,
            source_channels,
        })
    }
}

/// Pick the system-audio capture device and its format, per platform.
///
/// - **Windows:** the default *output* device opened as an *input* stream — cpal
///   sets `AUDCLNT_STREAMFLAGS_LOOPBACK` for render endpoints, capturing the mix.
/// - **Linux:** a PulseAudio/PipeWire `.monitor` input device (the system output
///   mirrored as a capture source), falling back to the default input device.
/// - **Other:** the default input device (no system loopback in v1).
#[cfg(target_os = "windows")]
fn pick_capture_device(
    host: &cpal::Host,
) -> Result<(cpal::Device, cpal::SupportedStreamConfig)> {
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no default audio output device to capture"))?;
    let cfg = device
        .default_output_config()
        .context("query default output (mix) format for loopback")?;
    Ok((device, cfg))
}

#[cfg(target_os = "linux")]
fn pick_capture_device(
    host: &cpal::Host,
) -> Result<(cpal::Device, cpal::SupportedStreamConfig)> {
    // Prefer a PulseAudio/PipeWire monitor source (mirrors the system output).
    let monitor = host
        .input_devices()
        .ok()
        .and_then(|mut devs| devs.find(|d| d.to_string().to_lowercase().contains("monitor")));
    let device = monitor.or_else(|| host.default_input_device()).ok_or_else(|| {
        anyhow!(
            "no '.monitor' source or default input device — enable a PulseAudio/\
             PipeWire monitor source to share system audio"
        )
    })?;
    let cfg = device
        .default_input_config()
        .context("query monitor input format")?;
    Ok((device, cfg))
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn pick_capture_device(
    host: &cpal::Host,
) -> Result<(cpal::Device, cpal::SupportedStreamConfig)> {
    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow!("no default input device"))?;
    let cfg = device
        .default_input_config()
        .context("query default input format")?;
    Ok((device, cfg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_passthrough_at_48k() {
        // src == dst → linear resampler is ~identity (one-sample group delay).
        let mut rs = Resampler::new(config::SAMPLE_RATE);
        let mut acc = Vec::new();
        let mut out: Vec<i16> = Vec::new();
        let mut sink = |f: &[i16]| out.extend_from_slice(f);
        // Feed a couple of frames' worth of a constant signal.
        for _ in 0..(FRAME_SAMPLE_COUNT) {
            rs.push(0.5, -0.5, &mut acc, &mut sink);
        }
        // Should have produced about one full frame of (L=0.5, R=-0.5) samples.
        assert!(!out.is_empty());
        // Interleaved: even indices ~ +16383, odd ~ -16383.
        let l = out[2];
        let r = out[3];
        assert!((l as i32 - 16383).abs() < 50, "L≈0.5 full-scale, got {l}");
        assert!((r as i32 + 16383).abs() < 50, "R≈-0.5 full-scale, got {r}");
    }

    #[test]
    fn resampler_upsamples_44100_to_more_samples() {
        // 44.1k → 48k should produce MORE output samples than input.
        let mut rs = Resampler::new(44_100);
        let mut acc = Vec::new();
        let mut count = 0usize;
        let mut sink = |f: &[i16]| count += f.len() / 2; // stereo pairs
        let inputs = 44_100;
        for i in 0..inputs {
            let s = (i as f32 * 0.001).sin();
            rs.push(s, s, &mut acc, &mut sink);
        }
        // ~48000 output stereo samples for ~44100 input; allow generous slack.
        assert!(
            count > 45_000 && count < 49_000,
            "expected ~48000 output samples, got {count}"
        );
    }
}
