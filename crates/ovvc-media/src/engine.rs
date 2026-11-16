//! Pluggable media engine capability descriptors.
//!
//! An engine is the thing that actually encodes and decodes audio or video.
//! The hot path stays compile-time Rust (there is no stable ABI to load a
//! plugin into), so this module only describes what an engine can do. The
//! daemon and the GUI use the same descriptors to decide whether a request is
//! possible and to show the user what the shipped engine offers.
//!
//! [`shipped_audio_engine`] and [`shipped_video_engine`] are the defaults the
//! product ships with, matching the profile table in [`crate::codec`].

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::pipeline::PipelineError;

/// Upper bounds a descriptor may claim, so a malformed or hostile plugin
/// descriptor cannot turn validation into a memory or UI amplification vector.
const MAX_CODECS: usize = 16;
const MAX_SAMPLE_RATES: usize = 16;
const MAX_FRAME_SIZES: usize = 16;
const MAX_CODEC_NAME_BYTES: usize = 24;

/// One codec an audio engine can encode or decode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioCodecInfo {
    /// Lowercase codec name, unique within the engine.
    pub name: String,
    /// Nominal bitrate in kilobits per second.
    pub bitrate_kbps: u16,
    /// Native sample rate in hertz.
    pub sample_rate: u32,
    /// The codec is only used for push-to-talk links.
    pub ptt_only: bool,
}

/// The capabilities of one audio engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioEngineInfo {
    /// Engine name, 1 to 32 bytes.
    pub name: String,
    /// Every codec the engine can use, at least one.
    pub codecs: Vec<AudioCodecInfo>,
    /// Every sample rate the engine accepts, in hertz.
    pub sample_rates: Vec<u32>,
    /// Every frame size the engine accepts, in milliseconds.
    pub frame_sizes_ms: Vec<u16>,
    /// Whether the engine supports push-to-talk gating.
    pub supports_ptt: bool,
    /// One-way pipeline latency the engine adds, in milliseconds.
    pub latency_ms: u16,
}

/// One codec a video engine can encode or decode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoCodecInfo {
    /// Lowercase codec name, unique within the engine.
    pub name: String,
    /// Whether the codec can use a hardware encoder or decoder.
    pub hardware_accel: bool,
    /// Whether the codec avoids patent licensing.
    pub royalty_free: bool,
}

/// The capabilities of one video engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoEngineInfo {
    /// Engine name, 1 to 32 bytes.
    pub name: String,
    /// Every codec the engine can use, at least one.
    pub codecs: Vec<VideoCodecInfo>,
    /// Whether the engine can send the same stream at several resolutions.
    pub simulcast: bool,
    /// Largest frame width the engine accepts.
    pub max_width: u16,
    /// Largest frame height the engine accepts.
    pub max_height: u16,
    /// Largest frame rate the engine accepts.
    pub max_fps: u8,
}

/// The audio engine ovvc ships with.
///
/// It carries the three voice profiles from [`crate::codec`]: the standard and
/// low tiers both use Opus (at different bitrates and sample rates), while the
/// minimal tier uses Codec 2. Engine support for push-to-talk and 20 ms frames
/// keeps it compatible with the negotiated default.
pub fn shipped_audio_engine() -> AudioEngineInfo {
    AudioEngineInfo {
        name: "opus".to_string(),
        codecs: vec![
            AudioCodecInfo {
                name: "opus".to_string(),
                bitrate_kbps: 10,
                sample_rate: 48_000,
                ptt_only: false,
            },
            AudioCodecInfo {
                name: "opus-low".to_string(),
                bitrate_kbps: 6,
                sample_rate: 8_000,
                ptt_only: false,
            },
            AudioCodecInfo {
                name: "codec2".to_string(),
                bitrate_kbps: 2,
                sample_rate: 8_000,
                ptt_only: true,
            },
        ],
        sample_rates: vec![48_000, 16_000, 8_000],
        frame_sizes_ms: vec![10, 20, 40],
        supports_ptt: true,
        latency_ms: 20,
    }
}

/// The video engine ovvc ships with.
///
/// The royalty-free codecs come first so a peer that can decode them prefers
/// them. AV1 is listed because hardware that supports it should be offered it.
/// H.264 is included because it is the one codec with dependable hardware
/// acceleration on every platform.
pub fn shipped_video_engine() -> VideoEngineInfo {
    VideoEngineInfo {
        name: "webrtc".to_string(),
        codecs: vec![
            VideoCodecInfo {
                name: "vp9".to_string(),
                hardware_accel: false,
                royalty_free: true,
            },
            VideoCodecInfo {
                name: "vp8".to_string(),
                hardware_accel: false,
                royalty_free: true,
            },
            VideoCodecInfo {
                name: "av1".to_string(),
                hardware_accel: true,
                royalty_free: true,
            },
            VideoCodecInfo {
                name: "h264".to_string(),
                hardware_accel: true,
                royalty_free: false,
            },
        ],
        simulcast: true,
        max_width: 1920,
        max_height: 1080,
        max_fps: 60,
    }
}

impl AudioEngineInfo {
    /// Check the descriptor is internally consistent and within the ranges the
    /// rest of the media stack assumes.
    pub fn validate(&self) -> Result<(), PipelineError> {
        validate_engine_name(&self.name)?;
        if self.codecs.is_empty() {
            return Err(PipelineError::Empty);
        }
        if self.codecs.len() > MAX_CODECS {
            return Err(PipelineError::InvalidEngine(format!(
                "audio engine has {} codecs, above the {MAX_CODECS} limit",
                self.codecs.len()
            )));
        }
        if self.sample_rates.len() > MAX_SAMPLE_RATES {
            return Err(PipelineError::InvalidEngine(format!(
                "audio engine has {} sample rates, above the {MAX_SAMPLE_RATES} limit",
                self.sample_rates.len()
            )));
        }
        if self.frame_sizes_ms.len() > MAX_FRAME_SIZES {
            return Err(PipelineError::InvalidEngine(format!(
                "audio engine has {} frame sizes, above the {MAX_FRAME_SIZES} limit",
                self.frame_sizes_ms.len()
            )));
        }

        let mut seen = BTreeSet::new();
        for codec in &self.codecs {
            validate_codec_name(&codec.name)?;
            if !seen.insert(codec.name.to_ascii_lowercase()) {
                return Err(PipelineError::DuplicateId(codec.name.clone()));
            }
            if !(8_000..=96_000).contains(&codec.sample_rate) {
                return Err(PipelineError::InvalidEngine(format!(
                    "codec '{}' sample rate {} is outside 8000..=96000",
                    codec.name, codec.sample_rate
                )));
            }
        }

        for rate in &self.sample_rates {
            if !(8_000..=96_000).contains(rate) {
                return Err(PipelineError::InvalidEngine(format!(
                    "sample rate {rate} is outside 8000..=96000"
                )));
            }
        }
        for frame in &self.frame_sizes_ms {
            if !(5..=60).contains(frame) {
                return Err(PipelineError::InvalidEngine(format!(
                    "frame size {frame} ms is outside 5..=60 ms"
                )));
            }
        }
        if self.latency_ms > 1_000 {
            return Err(PipelineError::InvalidEngine(format!(
                "latency {} ms is above 1000 ms",
                self.latency_ms
            )));
        }

        Ok(())
    }

    /// Whether the engine can run at this sample rate and frame size.
    pub fn supports(&self, sample_rate: u32, frame_ms: u16) -> bool {
        self.sample_rates.contains(&sample_rate) && self.frame_sizes_ms.contains(&frame_ms)
    }

    /// Look up a codec by its exact lowercase name.
    pub fn codec(&self, name: &str) -> Option<&AudioCodecInfo> {
        self.codecs.iter().find(|codec| codec.name == name)
    }
}

impl VideoEngineInfo {
    /// Check the descriptor is internally consistent and within the ranges the
    /// rest of the media stack assumes.
    pub fn validate(&self) -> Result<(), PipelineError> {
        validate_engine_name(&self.name)?;
        if self.codecs.is_empty() {
            return Err(PipelineError::Empty);
        }
        if self.codecs.len() > MAX_CODECS {
            return Err(PipelineError::InvalidEngine(format!(
                "video engine has {} codecs, above the {MAX_CODECS} limit",
                self.codecs.len()
            )));
        }

        let mut seen = BTreeSet::new();
        for codec in &self.codecs {
            validate_codec_name(&codec.name)?;
            if !seen.insert(codec.name.to_ascii_lowercase()) {
                return Err(PipelineError::DuplicateId(codec.name.clone()));
            }
        }

        if !(16..=8_192).contains(&self.max_width) {
            return Err(PipelineError::InvalidEngine(format!(
                "max width {} is outside 16..=8192",
                self.max_width
            )));
        }
        if !(16..=8_192).contains(&self.max_height) {
            return Err(PipelineError::InvalidEngine(format!(
                "max height {} is outside 16..=8192",
                self.max_height
            )));
        }
        if !(1..=240).contains(&self.max_fps) {
            return Err(PipelineError::InvalidEngine(format!(
                "max fps {} is outside 1..=240",
                self.max_fps
            )));
        }

        Ok(())
    }

    /// Look up a codec by its exact lowercase name.
    pub fn codec(&self, name: &str) -> Option<&VideoCodecInfo> {
        self.codecs.iter().find(|codec| codec.name == name)
    }
}

fn validate_engine_name(name: &str) -> Result<(), PipelineError> {
    if name.is_empty() {
        return Err(PipelineError::InvalidEngine(
            "engine name must not be empty".to_string(),
        ));
    }
    if name.len() > 32 {
        return Err(PipelineError::InvalidEngine(format!(
            "engine name is {} bytes, above the 32 byte limit",
            name.len()
        )));
    }
    Ok(())
}

/// Codec names are lookup keys, so they must be non-empty lowercase ASCII,
/// digits or `-`; otherwise `codec()` could never find them.
fn validate_codec_name(name: &str) -> Result<(), PipelineError> {
    if !(1..=MAX_CODEC_NAME_BYTES).contains(&name.len()) {
        return Err(PipelineError::InvalidEngine(format!(
            "codec name {name:?} must be 1 to {MAX_CODEC_NAME_BYTES} bytes"
        )));
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(PipelineError::InvalidEngine(format!(
            "codec name {name:?} must be lowercase ascii, digits or '-'"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_engines_validate() {
        assert!(shipped_audio_engine().validate().is_ok());
        assert!(shipped_video_engine().validate().is_ok());
    }

    #[test]
    fn shipped_audio_engine_exposes_the_voice_profiles() {
        let engine = shipped_audio_engine();
        assert_eq!(engine.name, "opus");
        assert!(engine.supports_ptt);

        let standard = engine.codec("opus").expect("standard codec is present");
        assert_eq!(standard.bitrate_kbps, 10);
        assert_eq!(standard.sample_rate, 48_000);
        assert!(!standard.ptt_only);

        let minimal = engine.codec("codec2").expect("minimal codec is present");
        assert_eq!(minimal.bitrate_kbps, 2);
        assert!(minimal.ptt_only);

        assert!(engine.supports(48_000, 20));
        assert!(engine.supports(8_000, 40));
        assert!(!engine.supports(44_100, 20));
        assert!(!engine.supports(48_000, 5));
        assert_eq!(engine.codec("nope"), None);
    }

    #[test]
    fn shipped_video_engine_exposes_the_expected_codecs() {
        let engine = shipped_video_engine();
        assert_eq!(engine.name, "webrtc");
        assert!(engine.simulcast);
        assert_eq!((engine.max_width, engine.max_height), (1920, 1080));
        assert_eq!(engine.max_fps, 60);

        assert!(engine.codec("vp9").expect("vp9 is present").royalty_free);
        assert!(engine.codec("vp8").expect("vp8 is present").royalty_free);
        let av1 = engine.codec("av1").expect("av1 is present");
        assert!(av1.royalty_free);
        assert!(av1.hardware_accel);
        let h264 = engine.codec("h264").expect("h264 is present");
        assert!(!h264.royalty_free);
        assert!(h264.hardware_accel);
        assert_eq!(engine.codec("nope"), None);
    }

    #[test]
    fn empty_engine_name_is_rejected() {
        let mut engine = shipped_audio_engine();
        engine.name.clear();
        assert!(matches!(
            engine.validate(),
            Err(PipelineError::InvalidEngine(_))
        ));
    }

    #[test]
    fn overlong_engine_name_is_rejected() {
        let mut engine = shipped_audio_engine();
        engine.name = "x".repeat(33);
        assert!(matches!(
            engine.validate(),
            Err(PipelineError::InvalidEngine(_))
        ));
    }

    #[test]
    fn engine_without_codecs_is_rejected() {
        let mut engine = shipped_audio_engine();
        engine.codecs.clear();
        assert!(matches!(engine.validate(), Err(PipelineError::Empty)));
    }

    #[test]
    fn duplicate_codec_is_rejected() {
        let mut engine = shipped_audio_engine();
        engine.codecs[1].name = "opus".to_string();
        assert!(matches!(
            engine.validate(),
            Err(PipelineError::DuplicateId(_))
        ));
    }

    #[test]
    fn codec_names_must_be_lowercase_and_within_limits() {
        let mut engine = shipped_audio_engine();
        engine.codecs[0].name = "OPUS".to_string();
        assert!(matches!(
            engine.validate(),
            Err(PipelineError::InvalidEngine(_))
        ));

        engine.codecs[0].name.clear();
        assert!(matches!(
            engine.validate(),
            Err(PipelineError::InvalidEngine(_))
        ));

        engine.codecs[0].name = "x".repeat(MAX_CODEC_NAME_BYTES + 1);
        assert!(matches!(
            engine.validate(),
            Err(PipelineError::InvalidEngine(_))
        ));

        let mut video = shipped_video_engine();
        video.codecs[0].name = "VP9".to_string();
        assert!(matches!(
            video.validate(),
            Err(PipelineError::InvalidEngine(_))
        ));
    }

    #[test]
    fn oversized_descriptor_lists_are_rejected() {
        let mut audio = shipped_audio_engine();
        audio.codecs = (0..=MAX_CODECS)
            .map(|index| AudioCodecInfo {
                name: format!("codec-{index}"),
                bitrate_kbps: 10,
                sample_rate: 48_000,
                ptt_only: false,
            })
            .collect();
        assert!(matches!(
            audio.validate(),
            Err(PipelineError::InvalidEngine(_))
        ));

        let mut video = shipped_video_engine();
        video.codecs = (0..=MAX_CODECS)
            .map(|index| VideoCodecInfo {
                name: format!("codec-{index}"),
                hardware_accel: false,
                royalty_free: true,
            })
            .collect();
        assert!(matches!(
            video.validate(),
            Err(PipelineError::InvalidEngine(_))
        ));

        let mut audio = shipped_audio_engine();
        audio.sample_rates = vec![48_000; MAX_SAMPLE_RATES + 1];
        assert!(matches!(
            audio.validate(),
            Err(PipelineError::InvalidEngine(_))
        ));

        let mut audio = shipped_audio_engine();
        audio.frame_sizes_ms = vec![20; MAX_FRAME_SIZES + 1];
        assert!(matches!(
            audio.validate(),
            Err(PipelineError::InvalidEngine(_))
        ));
    }

    #[test]
    fn impossible_sample_rate_and_frame_size_are_rejected() {
        let mut engine = shipped_audio_engine();
        engine.sample_rates.push(4_000);
        assert!(matches!(
            engine.validate(),
            Err(PipelineError::InvalidEngine(_))
        ));

        let mut engine = shipped_audio_engine();
        engine.frame_sizes_ms.push(3);
        assert!(matches!(
            engine.validate(),
            Err(PipelineError::InvalidEngine(_))
        ));

        let mut engine = shipped_audio_engine();
        engine.latency_ms = 1_001;
        assert!(matches!(
            engine.validate(),
            Err(PipelineError::InvalidEngine(_))
        ));
    }

    #[test]
    fn impossible_video_geometry_is_rejected() {
        let mut engine = shipped_video_engine();
        engine.max_width = 0;
        assert!(matches!(
            engine.validate(),
            Err(PipelineError::InvalidEngine(_))
        ));

        let mut engine = shipped_video_engine();
        engine.max_fps = 0;
        assert!(matches!(
            engine.validate(),
            Err(PipelineError::InvalidEngine(_))
        ));
    }
}
