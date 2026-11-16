#![no_main]

//! Fuzzes Opus decoding and the engine's receive/playback path.
//!
//! Arbitrary bytes go straight to [`OpusCodec::decode`], so the pure-Rust Opus
//! decoder is exercised with corrupt and truncated packets. When the payload
//! fits a media packet it is also offered to [`AudioEngine::receive`] and
//! played, which must never panic and must never emit more than one configured
//! frame. This covers the decoder state, the jitter buffer and the concealment
//! replacement together.

use libfuzzer_sys::fuzz_target;
use ovvc_audio::backend::{EngineConfig, NullBackend};
use ovvc_audio::codec::OpusCodec;
use ovvc_audio::engine::AudioEngine;
use ovvc_core::AuthorId;
use ovvc_media::call::CallId;
use ovvc_media::packet::MediaPacket;

const CALL: CallId = CallId([3; 16]);
const AUTHOR: AuthorId = AuthorId([4; 32]);
const FRAME_SAMPLES: usize = 960;

fn config() -> EngineConfig {
    EngineConfig {
        sample_rate: 48_000,
        frame_ms: 20,
        channels: 1,
        bitrate_kbps: 16,
        jitter_depth: 1,
    }
}

fuzz_target!(|data: &[u8]| {
    // Keep one iteration cheap; far above a real media packet.
    if data.len() > 8 * 1024 {
        return;
    }

    if let Ok(mut codec) = OpusCodec::new(48_000, 20, 16)
        && let Ok(samples) = codec.decode(data)
    {
        assert_eq!(
            samples.len(),
            FRAME_SAMPLES,
            "a decoded frame must have the configured length"
        );
    }

    let config = config();
    let Ok(mut engine) = AudioEngine::new(
        CALL,
        AUTHOR,
        config.clone(),
        Box::new(NullBackend::silent(config)),
    ) else {
        return;
    };

    if let Ok(packet) = MediaPacket::new(
        ovvc_media::packet::MediaKind::Audio,
        CALL,
        AUTHOR,
        0,
        0,
        data.to_vec(),
    ) && engine.receive(&packet).is_ok()
        && let Ok(Some(samples)) = engine.pump_playback()
    {
        assert_eq!(
            samples.len(),
            FRAME_SAMPLES,
            "the engine must never play more than one configured frame"
        );
    }
});
