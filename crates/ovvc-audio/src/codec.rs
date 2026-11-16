//! Opus encoding and decoding as a [`FrameCodec`].
//!
//! The codec is fixed to 48 kHz mono, which is what the call profile and the
//! device path use. Frames of 10, 20 or 40 ms are accepted; Opus supports more
//! durations but the engine only schedules those three, so anything else is a
//! configuration error rather than a silent change of timing.
//!
//! A lost packet reaches the decoder as an empty payload, because that is how
//! [`JitterBuffer`] marks concealment. Decoding an empty payload runs Opus's
//! own loss concealment for exactly one configured frame, so the decoder state
//! advances across the gap and the next real packet is not decoded from stale
//! state. A short or corrupt payload is handed to Opus, which validates it and
//! reports an error; nothing in this module indexes a packet without a bounds
//! check. A packet whose decoded duration does not match the configured frame
//! is rejected rather than widening a playback step.
//!
//! The codec is implemented with the pure-Rust `opus-pure` crate, so the build
//! needs no C toolchain and no system libopus.
//!
//! [`FrameCodec`]: ovvc_media::loopback::FrameCodec
//! [`JitterBuffer`]: ovvc_media::jitter::JitterBuffer

use opus_pure::{Application, OpusDecoder as PureDecoder, OpusEncoder as PureEncoder};
use ovvc_media::loopback::FrameCodec;
use ovvc_media::packet::MAX_MEDIA_PAYLOAD;
use ovvc_media::{MediaError, Result as MediaResult};

use crate::error::{AudioError, Result};

/// Samples per millisecond at the fixed 48 kHz rate.
const SAMPLES_PER_MS: usize = 48;

/// Opus packets are at most a few hundred bytes at voice bitrates. The buffer
/// is deliberately larger than [`MAX_MEDIA_PAYLOAD`] so an unexpectedly large
/// packet is reported as a codec error instead of a truncated encode.
const MAX_ENCODED_BYTES: usize = 4_000;

/// A decoder for one remote peer's stream.
///
/// A call mixes one decoder per peer, each with its own jitter buffer, so the
/// decoder cannot live inside the encoder the way [`OpusCodec`] originally had
/// it. The frame size rules and concealment behaviour are shared with
/// [`OpusCodec`]: 48 kHz mono, 10, 20 or 40 ms frames, and an empty payload
/// runs Opus loss concealment for exactly one frame.
pub struct OpusDecoder {
    decoder: PureDecoder,
    frame_ms: u16,
    frame_samples: usize,
}

impl OpusDecoder {
    /// Build a 48 kHz mono decoder for the configured frame size.
    pub fn new(sample_rate: u32, frame_ms: u16) -> Result<Self> {
        validate_settings(sample_rate, frame_ms)?;
        let decoder = PureDecoder::new(48_000, 1).map_err(codec_error)?;
        Ok(Self {
            decoder,
            frame_ms,
            frame_samples: SAMPLES_PER_MS * usize::from(frame_ms),
        })
    }

    /// Interleaved samples in one frame (mono, so one sample per slot).
    pub fn frame_samples(&self) -> usize {
        self.frame_samples
    }

    /// Forget the decoder's history so the next frame starts clean.
    pub fn reset(&mut self) -> Result<()> {
        self.decoder.reset_state().map_err(codec_error)
    }

    /// Decode one payload, treating an empty payload as a lost frame.
    ///
    /// A real packet must decode to exactly [`frame_samples`](Self::frame_samples);
    /// a longer packet is a codec error rather than a longer playback step.
    pub fn decode(&mut self, payload: &[u8]) -> Result<Vec<i16>> {
        if payload.is_empty() {
            return self.decode_lost();
        }

        let mut samples = vec![0i16; self.frame_samples];
        let written = self
            .decoder
            .decode_s16(payload, self.frame_samples, &mut samples)
            .map_err(codec_error)?;
        if written != self.frame_samples {
            return Err(AudioError::Codec(format!(
                "packet decoded to {written} samples, expected {} for a {} ms frame",
                self.frame_samples, self.frame_ms
            )));
        }
        Ok(samples)
    }

    /// Run Opus loss concealment for exactly one configured frame.
    pub fn decode_lost(&mut self) -> Result<Vec<i16>> {
        let mut samples = vec![0i16; self.frame_samples];
        let written = self
            .decoder
            .decode_s16(&[], self.frame_samples, &mut samples)
            .map_err(codec_error)?;
        if written != self.frame_samples {
            return Err(AudioError::Codec(format!(
                "concealment produced {written} samples, expected {} for a {} ms frame",
                self.frame_samples, self.frame_ms
            )));
        }
        Ok(samples)
    }
}

/// A stateful Opus encoder for one outgoing voice stream.
///
/// The mesh engine owns one of these. A star hub owns one per listener,
/// because each listener receives a different mix and Opus encoder state
/// cannot be shared between streams.
pub struct OpusEncoder {
    encoder: PureEncoder,
    frame_ms: u16,
    frame_samples: usize,
}

impl OpusEncoder {
    /// Build an encoder for 48 kHz mono at `bitrate_kbps`.
    pub fn new(sample_rate: u32, frame_ms: u16, bitrate_kbps: u16) -> Result<Self> {
        validate_settings(sample_rate, frame_ms)?;
        validate_bitrate(frame_ms, bitrate_kbps)?;

        let mut encoder = PureEncoder::new(48_000, 1, Application::Voip).map_err(codec_error)?;
        encoder.bitrate_bps = i32::from(bitrate_kbps) * 1_000;

        Ok(Self {
            encoder,
            frame_ms,
            frame_samples: SAMPLES_PER_MS * usize::from(frame_ms),
        })
    }

    /// Interleaved samples in one frame (mono, so one sample per slot).
    pub fn frame_samples(&self) -> usize {
        self.frame_samples
    }

    /// The configured frame length in milliseconds.
    pub fn frame_ms(&self) -> u16 {
        self.frame_ms
    }

    /// Encode exactly one frame of interleaved samples.
    pub fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>> {
        if pcm.len() != self.frame_samples {
            return Err(AudioError::Codec(format!(
                "expected {} samples for a frame, got {}",
                self.frame_samples,
                pcm.len()
            )));
        }

        let mut output = vec![0u8; MAX_ENCODED_BYTES];
        let written = self
            .encoder
            .encode_s16(pcm, self.frame_samples, &mut output)
            .map_err(codec_error)?;
        if written == 0 {
            return Err(AudioError::Codec(
                "Opus produced an empty packet".to_string(),
            ));
        }
        if written > MAX_MEDIA_PAYLOAD {
            return Err(AudioError::Codec(format!(
                "encoded packet is {written} bytes, above the {MAX_MEDIA_PAYLOAD}-byte media limit"
            )));
        }

        output.truncate(written);
        Ok(output)
    }
}

/// A stateful Opus encoder and decoder pair for one voice stream.
pub struct OpusCodec {
    encoder: OpusEncoder,
    decoder: OpusDecoder,
}

impl OpusCodec {
    /// Build an encoder and decoder for 48 kHz mono.
    pub fn new(sample_rate: u32, frame_ms: u16, bitrate_kbps: u16) -> Result<Self> {
        let encoder = OpusEncoder::new(sample_rate, frame_ms, bitrate_kbps)?;
        let decoder = OpusDecoder::new(sample_rate, frame_ms)?;
        Ok(Self { encoder, decoder })
    }

    /// Interleaved samples in one frame (mono, so one sample per slot).
    pub fn frame_samples(&self) -> usize {
        self.encoder.frame_samples()
    }

    /// Build a decoder with the same frame size as this encoder pair.
    ///
    /// Each remote peer gets its own decoder, so a call can keep the encoder
    /// here and hand a fresh [`OpusDecoder`] to every peer stream.
    pub fn new_decoder(&self) -> Result<OpusDecoder> {
        OpusDecoder::new(48_000, self.encoder.frame_ms())
    }

    /// Forget the decoder's history so the next frame starts from a clean
    /// state, for example after deafening discarded a run of packets.
    pub fn reset(&mut self) -> Result<()> {
        self.decoder.reset()
    }

    /// Encode exactly one frame of interleaved samples.
    pub fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>> {
        self.encoder.encode(pcm)
    }

    /// Decode one payload, treating an empty payload as a lost frame.
    ///
    /// An empty payload runs Opus loss concealment for one configured frame
    /// (`opus_decode` with a null packet), which advances the decoder state.
    /// A real packet must decode to exactly [`frame_samples`](Self::frame_samples);
    /// a longer packet is a codec error rather than a longer playback step.
    pub fn decode(&mut self, payload: &[u8]) -> Result<Vec<i16>> {
        self.decoder.decode(payload)
    }
}

impl FrameCodec for OpusCodec {
    fn name(&self) -> &str {
        "opus"
    }

    fn encode(&mut self, pcm: &[i16]) -> MediaResult<Vec<u8>> {
        OpusCodec::encode(self, pcm).map_err(media_error)
    }

    fn decode(&mut self, payload: &[u8]) -> MediaResult<Vec<i16>> {
        OpusCodec::decode(self, payload).map_err(media_error)
    }
}

/// Reject the settings Opus cannot honour.
fn validate_settings(sample_rate: u32, frame_ms: u16) -> Result<()> {
    if sample_rate != 48_000 {
        return Err(AudioError::Config(format!(
            "Opus supports 48 kHz only, got {sample_rate} Hz"
        )));
    }
    if !matches!(frame_ms, 10 | 20 | 40) {
        return Err(AudioError::Config(format!(
            "unsupported Opus frame size {frame_ms} ms, expected 10, 20 or 40"
        )));
    }
    Ok(())
}

/// Reject a bitrate that cannot fit the media payload or means nothing.
fn validate_bitrate(frame_ms: u16, bitrate_kbps: u16) -> Result<()> {
    if bitrate_kbps == 0 {
        return Err(AudioError::Config(
            "Opus bitrate must be greater than zero".to_string(),
        ));
    }
    // 1200 bytes is 9600 bits, so the packets fit only while
    // bitrate_kbps (bits per millisecond) times frame_ms is below that.
    let max_bitrate_kbps = (MAX_MEDIA_PAYLOAD * 8) / usize::from(frame_ms);
    if usize::from(bitrate_kbps) > max_bitrate_kbps {
        return Err(AudioError::Config(format!(
            "{bitrate_kbps} kbps at {frame_ms} ms can exceed the \
             {MAX_MEDIA_PAYLOAD}-byte media payload (at most {max_bitrate_kbps} kbps)"
        )));
    }
    Ok(())
}

fn codec_error(err: opus_pure::Error) -> AudioError {
    AudioError::Codec(err.to_string())
}

fn media_error(err: AudioError) -> MediaError {
    MediaError::Malformed(format!("opus codec: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn sine(frame_ms: u16, frequency_hz: f32) -> Vec<i16> {
        let samples = SAMPLES_PER_MS * usize::from(frame_ms);
        (0..samples)
            .map(|index| {
                let seconds = index as f32 / 48_000.0;
                let value = 12_000.0 * (TAU * frequency_hz * seconds).sin();
                value.round() as i16
            })
            .collect()
    }

    fn rms(samples: &[i16]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let sum: f64 = samples
            .iter()
            .map(|&sample| f64::from(sample).powi(2))
            .sum();
        (sum / samples.len() as f64).sqrt() as f32
    }

    /// Encode one 120 ms frame with the raw crate API, bypassing this module's
    /// 10/20/40 ms validation.
    fn raw_120ms_packet() -> Vec<u8> {
        let mut encoder =
            PureEncoder::new(48_000, 1, Application::Voip).expect("encoder");
        let long_frame = vec![0i16; SAMPLES_PER_MS * 120];
        let mut output = vec![0u8; MAX_ENCODED_BYTES];
        let written = encoder
            .encode_s16(&long_frame, long_frame.len(), &mut output)
            .expect("encode 120 ms");
        output.truncate(written);
        output
    }

    #[test]
    fn a_twenty_millisecond_sine_round_trips() {
        let mut codec = OpusCodec::new(48_000, 20, 16).expect("codec");
        let input = sine(20, 1_000.0);
        let payload = codec.encode(&input).expect("encode");
        assert!(!payload.is_empty());
        assert!(payload.len() <= MAX_MEDIA_PAYLOAD);

        let output = codec.decode(&payload).expect("decode");
        assert_eq!(output.len(), input.len());

        let input_rms = rms(&input);
        let output_rms = rms(&output);
        assert!(
            output_rms > input_rms * 0.5 && output_rms < input_rms * 2.0,
            "input rms {input_rms}, output rms {output_rms}"
        );
    }

    #[test]
    fn silence_round_trips() {
        let mut codec = OpusCodec::new(48_000, 20, 16).expect("codec");
        let payload = codec.encode(&[0i16; 960]).expect("encode");
        let output = codec.decode(&payload).expect("decode");
        assert_eq!(output.len(), 960);
        assert!(
            rms(&output) < 50.0,
            "silence decoded to rms {}",
            rms(&output)
        );
    }

    #[test]
    fn a_lost_packet_on_a_fresh_decoder_is_one_full_frame() {
        let mut codec = OpusCodec::new(48_000, 20, 16).expect("codec");
        let output = codec.decode(&[]).expect("concealed frame");
        assert_eq!(output.len(), 960);
    }

    #[test]
    fn a_lost_frame_advances_the_decoder_with_opus_concealment() {
        let mut codec = OpusCodec::new(48_000, 20, 16).expect("codec");
        let input = sine(20, 1_000.0);
        let payload = codec.encode(&input).expect("encode");
        let decoded = codec.decode(&payload).expect("decode");
        assert!(decoded.iter().any(|&sample| sample != 0));

        let concealed = codec.decode(&[]).expect("concealed frame");
        assert_eq!(concealed.len(), 960);
        assert!(
            concealed.iter().any(|&sample| sample != 0),
            "Opus PLC must extrapolate the previous frame, not emit digital silence"
        );
    }

    #[test]
    fn a_packet_longer_than_the_configured_frame_is_rejected() {
        let mut codec = OpusCodec::new(48_000, 20, 16).expect("codec");
        let output = raw_120ms_packet();

        assert!(
            matches!(codec.decode(&output), Err(AudioError::Codec(_))),
            "a 120 ms packet must not become one playback frame for a 20 ms stream"
        );
    }

    #[test]
    fn a_corrupt_payload_is_an_error_or_silence_but_never_a_panic() {
        let mut codec = OpusCodec::new(48_000, 20, 16).expect("codec");
        let cases = [
            vec![0xFF, 0xFF, 0xFF],
            vec![0x7F],
            (0..64).map(|byte| byte as u8).collect(),
            vec![0u8; 40],
        ];

        for payload in cases {
            match codec.decode(&payload) {
                Ok(samples) => assert_eq!(samples.len(), 960),
                Err(AudioError::Codec(_)) => {}
                Err(other) => panic!("unexpected error for a corrupt payload: {other}"),
            }
        }
    }

    #[test]
    fn unsupported_settings_are_configuration_errors() {
        assert!(matches!(
            OpusCodec::new(44_100, 20, 16),
            Err(AudioError::Config(_))
        ));
        assert!(matches!(
            OpusCodec::new(48_000, 5, 16),
            Err(AudioError::Config(_))
        ));
        assert!(matches!(
            OpusCodec::new(48_000, 20, 0),
            Err(AudioError::Config(_))
        ));
    }

    #[test]
    fn the_frame_codec_trait_reports_opus() {
        let mut codec = OpusCodec::new(48_000, 20, 16).expect("codec");
        assert_eq!(codec.name(), "opus");
        let payload = codec.encode(&[0i16; 960]).expect("encode");
        assert_eq!(codec.decode(&payload).expect("decode").len(), 960);
    }

    #[test]
    fn a_standalone_decoder_decodes_a_peer_payload() {
        let input = sine(20, 1_000.0);
        let mut encoder = OpusCodec::new(48_000, 20, 16).expect("encoder");
        let payload = encoder.encode(&input).expect("encode");

        let mut decoder = OpusDecoder::new(48_000, 20).expect("decoder");
        assert_eq!(decoder.frame_samples(), 960);
        let output = decoder.decode(&payload).expect("decode");
        assert_eq!(output.len(), input.len());
        assert!(
            rms(&output) > rms(&input) * 0.5,
            "decoded rms {} is far below input rms {}",
            rms(&output),
            rms(&input)
        );
    }

    #[test]
    fn a_standalone_decoder_conceals_a_lost_payload() {
        let mut decoder = OpusDecoder::new(48_000, 20).expect("decoder");
        assert_eq!(decoder.decode(&[]).expect("lost").len(), 960);
        assert_eq!(decoder.decode_lost().expect("lost again").len(), 960);
    }

    #[test]
    fn a_codec_can_mint_a_matching_decoder() {
        let codec = OpusCodec::new(48_000, 40, 16).expect("codec");
        let decoder = codec.new_decoder().expect("decoder");
        assert_eq!(decoder.frame_samples(), 48 * 40);
    }

    #[test]
    fn a_standalone_encoder_produces_a_decodable_packet() {
        let input = sine(20, 1_000.0);
        let mut encoder = OpusEncoder::new(48_000, 20, 16).expect("encoder");
        assert_eq!(encoder.frame_samples(), 960);
        assert_eq!(encoder.frame_ms(), 20);
        let payload = encoder.encode(&input).expect("encode");
        assert!(!payload.is_empty());

        let mut decoder = OpusDecoder::new(48_000, 20).expect("decoder");
        let output = decoder.decode(&payload).expect("decode");
        assert_eq!(output.len(), input.len());
        assert!(
            rms(&output) > rms(&input) * 0.5,
            "decoded rms {} is far below input rms {}",
            rms(&output),
            rms(&input)
        );
    }

    #[test]
    fn a_standalone_encoder_rejects_a_wrong_frame_and_bad_settings() {
        let mut encoder = OpusEncoder::new(48_000, 20, 16).expect("encoder");
        assert!(matches!(
            encoder.encode(&[0i16; 480]),
            Err(AudioError::Codec(_))
        ));
        assert!(matches!(
            OpusEncoder::new(44_100, 20, 16),
            Err(AudioError::Config(_))
        ));
        assert!(matches!(
            OpusEncoder::new(48_000, 20, 0),
            Err(AudioError::Config(_))
        ));
    }

    #[test]
    fn a_standalone_decoder_rejects_unsupported_settings() {
        assert!(matches!(
            OpusDecoder::new(44_100, 20),
            Err(AudioError::Config(_))
        ));
        assert!(matches!(
            OpusDecoder::new(48_000, 5),
            Err(AudioError::Config(_))
        ));
    }

    #[test]
    fn a_standalone_decoder_rejects_a_packet_of_the_wrong_frame_length() {
        let mut decoder = OpusDecoder::new(48_000, 20).expect("decoder");
        let output = raw_120ms_packet();

        assert!(matches!(decoder.decode(&output), Err(AudioError::Codec(_))));
    }
}
