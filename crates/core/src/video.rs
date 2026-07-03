// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Video stream configuration — the user-facing resolution + frame-rate toggles.
//!
//! This is the fixed data model for the toggles (720p/1080p/2K/4K × 30/60). The
//! capture → hardware-encode → transport → decode → display pipeline is designed
//! separately; it builds around these types. A resolution/fps change bumps the
//! session generation and forces a keyframe so clients re-arm cleanly.

/// Output resolution presets (16:9). Labels match the UI toggles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    P720,
    P1080,
    P1440,
    P2160,
}

impl Resolution {
    /// All presets, low→high (for building the toggle).
    pub const ALL: [Resolution; 4] = [
        Resolution::P720,
        Resolution::P1080,
        Resolution::P1440,
        Resolution::P2160,
    ];

    /// (width, height) in pixels.
    pub fn dims(self) -> (u32, u32) {
        match self {
            Resolution::P720 => (1280, 720),
            Resolution::P1080 => (1920, 1080),
            Resolution::P1440 => (2560, 1440),
            Resolution::P2160 => (3840, 2160),
        }
    }

    pub fn width(self) -> u32 {
        self.dims().0
    }
    pub fn height(self) -> u32 {
        self.dims().1
    }

    /// UI / wire label.
    pub fn label(self) -> &'static str {
        match self {
            Resolution::P720 => "720p",
            Resolution::P1080 => "1080p",
            Resolution::P1440 => "2K (1440p)",
            Resolution::P2160 => "4K (2160p)",
        }
    }

    /// Short wire token.
    pub fn as_str(self) -> &'static str {
        match self {
            Resolution::P720 => "720p",
            Resolution::P1080 => "1080p",
            Resolution::P1440 => "1440p",
            Resolution::P2160 => "2160p",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "720p" => Some(Resolution::P720),
            "1080p" => Some(Resolution::P1080),
            "1440p" | "2k" | "2K" => Some(Resolution::P1440),
            "2160p" | "4k" | "4K" => Some(Resolution::P2160),
            _ => None,
        }
    }
}

/// Frame-rate presets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fps {
    F30,
    F60,
}

impl Fps {
    pub const ALL: [Fps; 2] = [Fps::F30, Fps::F60];

    pub fn value(self) -> u32 {
        match self {
            Fps::F30 => 30,
            Fps::F60 => 60,
        }
    }

    /// Nanoseconds between frames (the video PTS step on the master clock).
    pub fn frame_nanos(self) -> i64 {
        1_000_000_000 / self.value() as i64
    }

    pub fn label(self) -> &'static str {
        match self {
            Fps::F30 => "30 fps",
            Fps::F60 => "60 fps",
        }
    }
}

/// Which encoder the server uses. Server-side only (the client just decodes
/// H.264), so this lives apart from the negotiated [`VideoConfig`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncoderBackend {
    /// Prefer the GPU; fall back to software if unavailable.
    Auto,
    /// GPU hardware encode (Windows Media Foundation → AMD AMF / NVENC / QuickSync).
    Hardware,
    /// CPU/software encode (openh264). Works everywhere.
    Cpu,
}

impl EncoderBackend {
    pub const ALL: [EncoderBackend; 3] =
        [EncoderBackend::Auto, EncoderBackend::Hardware, EncoderBackend::Cpu];

    pub fn label(self) -> &'static str {
        match self {
            EncoderBackend::Auto => "Auto",
            EncoderBackend::Hardware => "Hardware (GPU)",
            EncoderBackend::Cpu => "CPU (software)",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            EncoderBackend::Auto => "auto",
            EncoderBackend::Hardware => "hardware",
            EncoderBackend::Cpu => "cpu",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Some(EncoderBackend::Auto),
            "hardware" | "hw" | "gpu" => Some(EncoderBackend::Hardware),
            "cpu" | "software" | "sw" => Some(EncoderBackend::Cpu),
            _ => None,
        }
    }
}

impl Default for EncoderBackend {
    fn default() -> Self {
        EncoderBackend::Auto
    }
}

/// A complete video stream configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoConfig {
    pub resolution: Resolution,
    pub fps: Fps,
    /// Encode quality as a percentage of the baseline bitrate (100 = default). The GUI's
    /// quality slider sets this so the user can trade picture quality for bandwidth and
    /// encoder load depending on their server/network.
    pub quality_pct: u16,
}

impl Default for VideoConfig {
    fn default() -> Self {
        // A safe, widely-deployable default; users dial up to 4K60.
        VideoConfig {
            resolution: Resolution::P1080,
            fps: Fps::F30,
            quality_pct: 100,
        }
    }
}

impl VideoConfig {
    /// A reasonable starting encoder bitrate (kbps) for *screen content* at this
    /// resolution/fps. Screen content compresses better than camera video; the
    /// encoder/rate-control will refine this. Clamped to a sane LAN range.
    pub fn suggested_bitrate_kbps(self) -> u32 {
        let (w, h) = self.resolution.dims();
        let px = w as u64 * h as u64;
        let fps = self.fps.value() as u64;
        // ~0.07 bits per pixel per frame for screen content (baseline), then scaled by the
        // user's quality setting (100 = baseline). HEVC squeezes more out of every bit, so
        // even the low end stays sharp.
        let base = px * fps * 7 / 100;
        let scaled = base * self.quality_pct.max(1) as u64 / 100;
        ((scaled / 1000) as u32).clamp(1_000, 80_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dims_and_labels() {
        assert_eq!(Resolution::P720.dims(), (1280, 720));
        assert_eq!(Resolution::P2160.dims(), (3840, 2160));
        assert_eq!(Resolution::P1440.label(), "2K (1440p)");
        assert_eq!(Resolution::ALL.len(), 4);
    }

    #[test]
    fn resolution_parse_roundtrip_and_aliases() {
        for r in Resolution::ALL {
            assert_eq!(Resolution::parse(r.as_str()), Some(r));
        }
        assert_eq!(Resolution::parse("4K"), Some(Resolution::P2160));
        assert_eq!(Resolution::parse("2k"), Some(Resolution::P1440));
        assert_eq!(Resolution::parse("nope"), None);
    }

    #[test]
    fn fps_frame_nanos() {
        assert_eq!(Fps::F30.frame_nanos(), 33_333_333);
        assert_eq!(Fps::F60.frame_nanos(), 16_666_666);
    }

    #[test]
    fn encoder_backend_parse_roundtrip_and_aliases() {
        for b in EncoderBackend::ALL {
            assert_eq!(EncoderBackend::parse(b.as_str()), Some(b));
        }
        assert_eq!(EncoderBackend::parse("GPU"), Some(EncoderBackend::Hardware));
        assert_eq!(EncoderBackend::parse("software"), Some(EncoderBackend::Cpu));
        assert_eq!(EncoderBackend::parse("nope"), None);
        assert_eq!(EncoderBackend::default(), EncoderBackend::Auto);
    }

    #[test]
    fn bitrate_scales_and_clamps() {
        let lo = VideoConfig { resolution: Resolution::P720, fps: Fps::F30, quality_pct: 100 }.suggested_bitrate_kbps();
        let hi = VideoConfig { resolution: Resolution::P2160, fps: Fps::F60, quality_pct: 100 }.suggested_bitrate_kbps();
        assert!(lo < hi);
        assert!((1_000..=80_000).contains(&lo));
        assert!((1_000..=80_000).contains(&hi));
        // Quality scaling: higher pct → higher bitrate.
        let hq = VideoConfig { resolution: Resolution::P1080, fps: Fps::F60, quality_pct: 200 }.suggested_bitrate_kbps();
        let lq = VideoConfig { resolution: Resolution::P1080, fps: Fps::F60, quality_pct: 50 }.suggested_bitrate_kbps();
        assert!(hq > lq);
    }
}
