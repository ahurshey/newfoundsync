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

/// Which video codec the server encodes. Server-side only (the client decodes whatever
/// codec it's told to), so this lives apart from the negotiated [`VideoConfig`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncoderBackend {
    /// AV1 (royalty-free, default): GPU via Media Foundation where the hardware supports it
    /// (Intel Arc/Xe, NVIDIA RTX 40+, AMD RX 7000+), else CPU (SVT-AV1).
    Av1,
    /// VP9 (royalty-free fallback): CPU via libvpx. Broader decode reach than AV1 on older
    /// Apple / Android / smart TVs; no hardware VP9 encode except recent Intel QuickSync.
    Vp9,
}

impl EncoderBackend {
    pub const ALL: [EncoderBackend; 2] = [EncoderBackend::Av1, EncoderBackend::Vp9];

    pub fn label(self) -> &'static str {
        match self {
            EncoderBackend::Av1 => "AV1 (royalty-free)",
            EncoderBackend::Vp9 => "VP9 (royalty-free)",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            EncoderBackend::Av1 => "av1",
            EncoderBackend::Vp9 => "vp9",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            // Legacy aliases (auto/hardware/cpu/…) fold into the AV1 default.
            "av1" | "svt-av1" | "svtav1" | "auto" | "hardware" | "hw" | "gpu" | "cpu"
            | "software" | "sw" => Some(EncoderBackend::Av1),
            "vp9" | "vpx" | "libvpx" => Some(EncoderBackend::Vp9),
            _ => None,
        }
    }
}

impl Default for EncoderBackend {
    fn default() -> Self {
        EncoderBackend::Av1
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
        // user's quality setting (100 = baseline). AV1's screen-content tools keep even the
        // low end sharp.
        let base = px * fps * 7 / 100;
        let scaled = base * self.quality_pct.max(1) as u64 / 100;
        ((scaled / 1000) as u32).clamp(1_000, 80_000)
    }
}

/// AV1 codec string (`av01.P.LLT.DD`) advertised to WebCodecs clients, with a `seq_level_idx`
/// that *covers* this resolution+fps. A spec-conformant decoder requires the declared level to
/// be ≥ the encoded level, so this must never understate the stream. Profile 0 (Main), Main tier
/// (`M`), 8-bit — SVT-AV1 / the MF AV1 encoder write the matching (minimal) level.
pub fn av1_codec_string(res: Resolution, fps: u32) -> String {
    let (w, h) = res.dims();
    let samples = w as u64 * h as u64;
    let rate = samples * fps.max(1) as u64;
    // AV1 spec Annex A: (seq_level_idx, MaxPicSize, MaxDisplayRate), low→high.
    const LEVELS: &[(u8, u64, u64)] = &[
        (4, 665_856, 19_975_680),       // 3.0
        (5, 1_065_024, 31_950_720),     // 3.1
        (8, 2_359_296, 70_778_880),     // 4.0
        (9, 2_359_296, 141_557_760),    // 4.1
        (12, 8_912_896, 267_386_880),   // 5.0
        (13, 8_912_896, 534_773_760),   // 5.1
        (14, 8_912_896, 1_069_547_520), // 5.2
    ];
    let idx = LEVELS
        .iter()
        .find(|&&(_, max_pic, max_rate)| samples <= max_pic && rate <= max_rate)
        .map(|&(i, _, _)| i)
        .unwrap_or(15); // 5.3 — a safe ceiling beyond every preset
    format!("av01.0.{idx:02}M.08")
}

/// VP9 codec string (`vp09.PP.LL.DD`) advertised to WebCodecs clients, with a level that covers
/// this resolution+fps (same "declared ≥ encoded" rule as AV1). Profile 0, 8-bit.
pub fn vp9_codec_string(res: Resolution, fps: u32) -> String {
    let (w, h) = res.dims();
    let samples = w as u64 * h as u64;
    let rate = samples * fps.max(1) as u64;
    // VP9 spec Annex A: (level×10, MaxLumaSampleRate, MaxPicSize), low→high.
    const LEVELS: &[(u8, u64, u64)] = &[
        (30, 20_736_000, 552_960),      // 3.0
        (31, 36_864_000, 983_040),      // 3.1
        (40, 83_558_400, 2_228_224),    // 4.0
        (41, 160_432_128, 2_228_224),   // 4.1
        (50, 311_951_360, 8_912_896),   // 5.0
        (51, 588_251_136, 8_912_896),   // 5.1
        (52, 1_176_502_272, 8_912_896), // 5.2
    ];
    let lvl = LEVELS
        .iter()
        .find(|&&(_, max_rate, max_pic)| samples <= max_pic && rate <= max_rate)
        .map(|&(l, _, _)| l)
        .unwrap_or(60); // 6.0 — a safe ceiling beyond every preset
    format!("vp09.00.{lvl:02}.08")
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
        assert_eq!(EncoderBackend::parse("gpu"), Some(EncoderBackend::Av1));
        assert_eq!(EncoderBackend::parse("vp9"), Some(EncoderBackend::Vp9));
        assert_eq!(EncoderBackend::parse("nope"), None);
        assert_eq!(EncoderBackend::default(), EncoderBackend::Av1);
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

    #[test]
    fn codec_strings_cover_every_preset() {
        // The advertised level must be ≥ the level the encoder actually writes for each
        // resolution+fps preset, so no receiver that honors the level drops to audio-only.
        assert_eq!(av1_codec_string(Resolution::P720, 30), "av01.0.05M.08");
        assert_eq!(av1_codec_string(Resolution::P720, 60), "av01.0.08M.08");
        assert_eq!(av1_codec_string(Resolution::P1080, 30), "av01.0.08M.08");
        assert_eq!(av1_codec_string(Resolution::P1080, 60), "av01.0.09M.08");
        assert_eq!(av1_codec_string(Resolution::P1440, 30), "av01.0.12M.08");
        assert_eq!(av1_codec_string(Resolution::P1440, 60), "av01.0.12M.08");
        assert_eq!(av1_codec_string(Resolution::P2160, 30), "av01.0.12M.08");
        assert_eq!(av1_codec_string(Resolution::P2160, 60), "av01.0.13M.08");

        assert_eq!(vp9_codec_string(Resolution::P720, 30), "vp09.00.31.08");
        assert_eq!(vp9_codec_string(Resolution::P720, 60), "vp09.00.40.08");
        assert_eq!(vp9_codec_string(Resolution::P1080, 30), "vp09.00.40.08");
        assert_eq!(vp9_codec_string(Resolution::P1080, 60), "vp09.00.41.08");
        assert_eq!(vp9_codec_string(Resolution::P1440, 30), "vp09.00.50.08");
        assert_eq!(vp9_codec_string(Resolution::P1440, 60), "vp09.00.50.08");
        assert_eq!(vp9_codec_string(Resolution::P2160, 30), "vp09.00.50.08");
        assert_eq!(vp9_codec_string(Resolution::P2160, 60), "vp09.00.51.08");
    }
}
