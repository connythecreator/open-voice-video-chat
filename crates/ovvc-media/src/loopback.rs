//! A hardware-free loopback harness for the media path.
//!
//! The transport-free types in this crate are easy to test on their own, but
//! the path a voice frame actually takes is encode, packet, jitter and decode
//! together. This module supplies a deterministic synthetic source, a null
//! sink and a harness that wires the stages together, so that path can run in
//! CI without a sound card or an audio backend.
//!
//! [`PcmCodec`] is deliberately trivial: it stores interleaved `i16` samples
//! little-endian, which makes the harness useful for checking framing,
//! reordering and concealment before a real codec such as Opus is plugged in.
//! It is not a compression format and is never meant for the wire.

use ovvc_core::AuthorId;

use crate::call::CallId;
use crate::error::{MediaError, Result};
use crate::jitter::{JitterBuffer, PushOutcome};
use crate::packet::{MediaKind, MediaPacket};

/// Amplitude of the generated sine, in signed 16-bit units.
const SINE_AMPLITUDE: f32 = 12_000.0;

/// A source of interleaved audio frames.
pub trait AudioSource {
    /// Samples per second, per channel.
    fn sample_rate(&self) -> u32;

    /// The number of interleaved channels.
    fn channels(&self) -> u8;

    /// Interleaved samples in one frame.
    fn frame_samples(&self) -> usize;

    /// Read the next frame, or `None` once the source is exhausted.
    fn read(&mut self) -> Result<Option<Vec<i16>>>;
}

/// A destination for interleaved audio frames.
pub trait AudioSink {
    /// Accept one frame of interleaved samples.
    fn write(&mut self, samples: &[i16]) -> Result<()>;

    /// How many times [`AudioSink::write`] has been called.
    fn frames_written(&self) -> u64;
}

/// A deterministic sine source that needs no capture device.
///
/// The phase advances across frames, so concatenating the output of several
/// reads produces a continuous tone rather than a per-frame restart. A source
/// with no frame limit yields audio forever.
#[derive(Debug, Clone)]
pub struct SyntheticSource {
    sample_rate: u32,
    channels: u8,
    frame_samples: usize,
    frequency_hz: f32,
    emitted: u64,
    remaining: Option<usize>,
}

impl SyntheticSource {
    /// Build a sine source.
    ///
    /// `sample_rate` and `channels` are clamped to at least one so callers
    /// cannot create a source that divides by zero or loops on empty frames.
    pub fn sine(sample_rate: u32, channels: u8, frame_ms: u16, frequency_hz: f32) -> Self {
        let sample_rate = sample_rate.max(1);
        let channels = channels.max(1);
        let samples_per_channel = (sample_rate as usize * usize::from(frame_ms) / 1000).max(1);
        let frame_samples = samples_per_channel * usize::from(channels);
        Self {
            sample_rate,
            channels,
            frame_samples,
            frequency_hz,
            emitted: 0,
            remaining: None,
        }
    }

    /// Stop after `frames` frames, after which [`AudioSource::read`] returns
    /// `None`.
    pub fn with_frames(mut self, frames: usize) -> Self {
        self.remaining = Some(frames);
        self
    }

    /// Total interleaved samples handed out so far.
    pub fn samples_emitted(&self) -> u64 {
        self.emitted
    }
}

impl AudioSource for SyntheticSource {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u8 {
        self.channels
    }

    fn frame_samples(&self) -> usize {
        self.frame_samples
    }

    fn read(&mut self) -> Result<Option<Vec<i16>>> {
        if self.remaining == Some(0) {
            return Ok(None);
        }

        let channels = usize::from(self.channels);
        let first = self.emitted / channels as u64;
        let mut frame = Vec::with_capacity(self.frame_samples);
        for slot in 0..self.frame_samples {
            let sample_index = first + (slot / channels) as u64;
            let seconds = sample_index as f32 / self.sample_rate as f32;
            let phase = std::f32::consts::TAU * self.frequency_hz * seconds;
            let value = (SINE_AMPLITUDE * phase.sin()).round();
            frame.push(value.clamp(-32_768.0, 32_767.0) as i16);
        }

        self.emitted = self.emitted.saturating_add(self.frame_samples as u64);
        if let Some(remaining) = &mut self.remaining {
            *remaining -= 1;
        }
        Ok(Some(frame))
    }
}

/// A sink that measures what reaches the speaker and writes nothing.
#[derive(Debug, Default, Clone)]
pub struct NullSink {
    samples_written: u64,
    frames_written: u64,
    peak: i16,
    sum_squares: f64,
}

impl NullSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Interleaved samples written so far.
    pub fn samples_written(&self) -> u64 {
        self.samples_written
    }

    /// The largest absolute sample seen, saturated at [`i16::MAX`] so
    /// `i16::MIN` cannot overflow the return type.
    pub fn peak(&self) -> i16 {
        self.peak
    }

    /// Root-mean-square level of every sample written, in sample units.
    pub fn rms(&self) -> f32 {
        if self.samples_written == 0 {
            0.0
        } else {
            (self.sum_squares / self.samples_written as f64).sqrt() as f32
        }
    }

    /// Clear the measurements. The sink keeps accepting writes.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

impl AudioSink for NullSink {
    fn write(&mut self, samples: &[i16]) -> Result<()> {
        self.frames_written = self.frames_written.saturating_add(1);
        self.samples_written = self.samples_written.saturating_add(samples.len() as u64);
        for &sample in samples {
            self.peak = self.peak.max(sample.saturating_abs());
            let value = f64::from(sample);
            self.sum_squares += value * value;
        }
        Ok(())
    }

    fn frames_written(&self) -> u64 {
        self.frames_written
    }
}

/// The codec stage the harness drives.
///
/// Implementations are stateful, which is what a real Opus encoder and decoder
/// pair needs, but [`PcmCodec`] ignores that.
pub trait FrameCodec {
    /// The codec's name, for diagnostics.
    fn name(&self) -> &str;

    /// Encode one frame of interleaved samples.
    fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>>;

    /// Decode one payload into interleaved samples.
    fn decode(&mut self, payload: &[u8]) -> Result<Vec<i16>>;
}

/// An uncompressed codec: interleaved `i16` samples in little-endian order.
///
/// Its only failure mode is an odd-length payload, which cannot be read as
/// whole samples. An empty payload decodes to an empty frame, which is how the
/// harness represents concealment.
#[derive(Debug, Default, Clone, Copy)]
pub struct PcmCodec;

impl FrameCodec for PcmCodec {
    fn name(&self) -> &str {
        "pcm"
    }

    fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(pcm.len() * 2);
        for &sample in pcm {
            out.extend_from_slice(&sample.to_le_bytes());
        }
        Ok(out)
    }

    fn decode(&mut self, payload: &[u8]) -> Result<Vec<i16>> {
        if !payload.len().is_multiple_of(2) {
            return Err(MediaError::Malformed(format!(
                "PCM payload has an odd length: {} bytes",
                payload.len()
            )));
        }
        Ok(payload
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| i16::from_le_bytes(*pair))
            .collect())
    }
}

/// Counters for one harness, accumulated across calls.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LoopbackStats {
    pub encoded_frames: u64,
    pub encoded_bytes: u64,
    pub received: u64,
    pub played: u64,
    pub concealed: u64,
    pub duplicates: u64,
    pub late: u64,
    pub lost: u64,
}

/// An encode, packet, jitter and decode path with no audio device attached.
///
/// The harness owns a [`JitterBuffer`] and a [`FrameCodec`]. [`encode_frame`]
/// produces a bounded audio [`MediaPacket`] and advances the sequence;
/// [`receive`] validates and buffers a packet; [`play`] pops the buffer and
/// decodes. A concealed frame carries an empty payload, so [`PcmCodec`]
/// decodes it to an empty frame.
///
/// [`encode_frame`]: LoopbackHarness::encode_frame
/// [`receive`]: LoopbackHarness::receive
/// [`play`]: LoopbackHarness::play
pub struct LoopbackHarness {
    call_id: CallId,
    author: AuthorId,
    codec: Box<dyn FrameCodec>,
    jitter: JitterBuffer,
    seq: u16,
    encoded_frames: u64,
    encoded_bytes: u64,
}

impl LoopbackHarness {
    /// Build a harness for one call and author.
    pub fn new(
        call_id: CallId,
        author: AuthorId,
        codec: Box<dyn FrameCodec>,
        jitter_depth: usize,
    ) -> Self {
        Self {
            call_id,
            author,
            codec,
            jitter: JitterBuffer::new(jitter_depth),
            seq: 0,
            encoded_frames: 0,
            encoded_bytes: 0,
        }
    }

    /// Encode `pcm` and wrap it in an audio packet for this harness's call.
    ///
    /// The sequence starts at zero and wraps through `65535 -> 0`. A payload
    /// larger than the packet limit is refused before the sequence advances.
    pub fn encode_frame(&mut self, pcm: &[i16], timestamp_ms: u64) -> Result<MediaPacket> {
        let payload = self.codec.encode(pcm)?;
        let packet = MediaPacket::new(
            MediaKind::Audio,
            self.call_id,
            self.author,
            self.seq,
            timestamp_ms,
            payload,
        )?;
        self.seq = self.seq.wrapping_add(1);
        self.encoded_frames = self.encoded_frames.saturating_add(1);
        self.encoded_bytes = self
            .encoded_bytes
            .saturating_add(packet.payload.len() as u64);
        Ok(packet)
    }

    /// Accept a packet into the jitter buffer.
    ///
    /// A packet for another call or another media kind is malformed for this
    /// harness and is not buffered.
    pub fn receive(&mut self, packet: &MediaPacket) -> Result<PushOutcome> {
        if packet.call_id != self.call_id {
            return Err(MediaError::Malformed(format!(
                "packet for call {} arrived at call {}",
                packet.call_id, self.call_id
            )));
        }
        if packet.kind != MediaKind::Audio {
            return Err(MediaError::Malformed(format!(
                "expected an audio packet, received {:?}",
                packet.kind
            )));
        }
        self.jitter.push(packet.seq, packet.payload.clone())
    }

    /// Pop and decode the next frame.
    ///
    /// Returns `None` while the jitter buffer is still priming, and after it
    /// has drained. A concealed frame is decoded from its empty payload.
    pub fn play(&mut self) -> Result<Option<Vec<i16>>> {
        match self.jitter.pop() {
            None => Ok(None),
            Some(frame) => self.codec.decode(&frame.payload).map(Some),
        }
    }

    /// The current counters, combining the encode side with the jitter buffer.
    pub fn stats(&self) -> LoopbackStats {
        let jitter = self.jitter.stats();
        LoopbackStats {
            encoded_frames: self.encoded_frames,
            encoded_bytes: self.encoded_bytes,
            received: jitter.received,
            played: jitter.played,
            concealed: jitter.concealed,
            duplicates: jitter.duplicates,
            late: jitter.late,
            lost: jitter.lost,
        }
    }

    /// Clear the counters. Audio already buffered by the jitter buffer stays
    /// buffered.
    pub fn reset_stats(&mut self) {
        self.encoded_frames = 0;
        self.encoded_bytes = 0;
        self.jitter.reset_stats();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jitter::MAX_BUFFERED;

    const CALL: CallId = CallId([7; 16]);
    const AUTHOR: AuthorId = AuthorId([9; 32]);

    fn harness(depth: usize) -> LoopbackHarness {
        LoopbackHarness::new(CALL, AUTHOR, Box::new(PcmCodec), depth)
    }

    fn sine_frames(frames: usize) -> SyntheticSource {
        // 10 ms at 48 kHz is 480 mono samples, or 960 PCM bytes, which fits
        // the 1200-byte media payload bound. A 20 ms uncompressed frame would
        // not.
        SyntheticSource::sine(48_000, 1, 10, 440.0).with_frames(frames)
    }

    #[test]
    fn one_second_sine_round_trips_through_pcm() {
        let frames = 50;
        let mut source = SyntheticSource::sine(48_000, 1, 20, 1_000.0).with_frames(frames);
        let mut codec = PcmCodec;
        let mut decoded = Vec::new();
        let mut encoded = 0usize;

        // The 20 ms frame is 1920 bytes of uncompressed PCM, larger than one
        // media packet, so this checks the codec stage on its own.
        while let Some(pcm) = source.read().expect("source") {
            assert_eq!(pcm.len(), 960);
            let payload = codec.encode(&pcm).expect("encode");
            assert_eq!(payload.len(), 1_920);
            let samples = codec.decode(&payload).expect("decode");
            assert_eq!(samples, pcm);
            decoded.extend(samples);
            encoded += 1;
        }

        assert_eq!(encoded, frames);
        assert_eq!(source.samples_emitted(), 48_000);
        assert_eq!(decoded.len(), 48_000);

        let peak = decoded
            .iter()
            .map(|sample| sample.saturating_abs())
            .max()
            .expect("non-empty");
        assert!(
            (i32::from(peak) - 12_000).abs() <= 100,
            "peak {peak} is not near the sine amplitude"
        );

        let sum_squares: f64 = decoded
            .iter()
            .map(|&sample| f64::from(sample).powi(2))
            .sum();
        let rms = (sum_squares / decoded.len() as f64).sqrt();
        let expected = 12_000.0 / 2f64.sqrt();
        assert!(
            (rms - expected).abs() < 200.0,
            "rms {rms} is not near {expected}"
        );
    }

    #[test]
    fn reordered_packets_play_in_sequence_order() {
        let mut source = sine_frames(2);
        let pcm0 = source.read().expect("read").expect("frame");
        let pcm1 = source.read().expect("read").expect("frame");

        let mut harness = harness(2);
        let packet0 = harness.encode_frame(&pcm0, 0).expect("encode");
        let packet1 = harness.encode_frame(&pcm1, 20).expect("encode");

        assert_eq!(
            harness.receive(&packet1).expect("receive late packet"),
            PushOutcome::Accepted
        );
        assert_eq!(
            harness.receive(&packet0).expect("receive early packet"),
            PushOutcome::Accepted
        );

        assert_eq!(harness.play().expect("play"), Some(pcm0));
        assert_eq!(harness.play().expect("play"), Some(pcm1));
        assert_eq!(harness.play().expect("play"), None);
    }

    #[test]
    fn a_dropped_packet_yields_one_concealed_frame() {
        let mut source = sine_frames(3);
        let pcm0 = source.read().expect("read").expect("frame");
        let pcm1 = source.read().expect("read").expect("frame");
        let pcm2 = source.read().expect("read").expect("frame");

        let mut harness = harness(1);
        let packet0 = harness.encode_frame(&pcm0, 0).expect("encode");
        let _packet1 = harness.encode_frame(&pcm1, 20).expect("encode");
        let packet2 = harness.encode_frame(&pcm2, 40).expect("encode");
        // Packet 1 is lost on the way; dropping it still advances the seq it
        // was encoded with.

        assert_eq!(
            harness.receive(&packet0).expect("receive"),
            PushOutcome::Accepted
        );
        assert_eq!(
            harness.receive(&packet2).expect("receive"),
            PushOutcome::Accepted
        );

        assert_eq!(harness.play().expect("play"), Some(pcm0));
        assert_eq!(harness.play().expect("concealed"), Some(Vec::new()));
        assert_eq!(harness.play().expect("play"), Some(pcm2));
        assert_eq!(harness.play().expect("play"), None);

        let stats = harness.stats();
        assert_eq!(stats.lost, 1);
        assert_eq!(stats.concealed, 1);
        assert_eq!(stats.played, 2);
        assert_eq!(stats.received, 2);
    }

    #[test]
    fn a_duplicate_is_rejected_and_counted() {
        let mut harness = harness(1);
        let packet = harness.encode_frame(&[1, 2, 3, 4], 0).expect("encode");

        assert_eq!(
            harness.receive(&packet).expect("first"),
            PushOutcome::Accepted
        );
        assert_eq!(
            harness.receive(&packet).expect("second"),
            PushOutcome::Duplicate
        );
        assert_eq!(harness.stats().duplicates, 1);
        assert_eq!(harness.stats().received, 1);
    }

    #[test]
    fn sequence_wrapping_across_65535_works() {
        let mut harness = harness(1);
        let mut last = 0u16;
        for _ in 0..=u16::MAX {
            last = harness.encode_frame(&[], 0).expect("encode").seq;
        }
        assert_eq!(last, u16::MAX);
        assert_eq!(harness.encode_frame(&[], 0).expect("encode").seq, 0);
        assert_eq!(harness.stats().encoded_frames, 65_537);
    }

    #[test]
    fn packets_for_another_call_or_kind_are_rejected() {
        let mut harness = harness(1);
        let packet = harness.encode_frame(&[1, 2, 3, 4], 0).expect("encode");

        let mut wrong_call = packet.clone();
        wrong_call.call_id = CallId([0; 16]);
        assert!(matches!(
            harness.receive(&wrong_call),
            Err(MediaError::Malformed(_))
        ));

        let mut wrong_kind = packet.clone();
        wrong_kind.kind = MediaKind::Video;
        assert!(matches!(
            harness.receive(&wrong_kind),
            Err(MediaError::Malformed(_))
        ));

        assert_eq!(harness.stats().received, 0);
    }

    #[test]
    fn an_odd_length_pcm_payload_is_rejected() {
        let mut codec = PcmCodec;
        assert!(matches!(
            codec.decode(&[1, 2, 3]),
            Err(MediaError::Malformed(_))
        ));
        assert_eq!(
            codec.decode(&[1, 2, 3, 4]).expect("whole samples"),
            vec![0x0201, 0x0403]
        );

        let mut harness = harness(1);
        let odd =
            MediaPacket::new(MediaKind::Audio, CALL, AUTHOR, 3, 0, vec![1, 2, 3]).expect("packet");
        assert_eq!(
            harness.receive(&odd).expect("receive"),
            PushOutcome::Accepted
        );
        assert!(matches!(harness.play(), Err(MediaError::Malformed(_))));
    }

    #[test]
    fn lossy_delivery_still_plays_every_expected_frame() {
        const N: usize = 141;
        const DROP_EVERY: usize = 7;
        let mut source = sine_frames(N);
        let mut harness = harness(3);

        for index in 0..N {
            let pcm = source.read().expect("read").expect("frame");
            let packet = harness
                .encode_frame(&pcm, index as u64 * 20)
                .expect("encode");
            // Drop indices 6, 13, ... 139: 20 packets, none of them the last.
            if index % DROP_EVERY != DROP_EVERY - 1 {
                assert_eq!(
                    harness.receive(&packet).expect("receive"),
                    PushOutcome::Accepted
                );
            }
        }

        let mut played = 0usize;
        let mut concealed = 0usize;
        while let Some(samples) = harness.play().expect("play") {
            if samples.is_empty() {
                concealed += 1;
            } else {
                played += 1;
            }
        }

        assert_eq!(played, N - 20);
        assert_eq!(concealed, 20);
        assert_eq!(played + concealed, N);

        let stats = harness.stats();
        assert_eq!(stats.encoded_frames, N as u64);
        assert_eq!(stats.played, (N - 20) as u64);
        assert_eq!(stats.concealed, 20);
        assert_eq!(stats.lost, 20);
    }

    #[test]
    fn reset_stats_clears_counters_but_keeps_buffered_audio() {
        let mut harness = harness(1);
        let packet = harness.encode_frame(&[5, 6], 0).expect("encode");
        harness.receive(&packet).expect("receive");

        harness.reset_stats();
        assert_eq!(harness.stats(), LoopbackStats::default());

        assert_eq!(harness.play().expect("play"), Some(vec![5, 6]));
        assert_eq!(harness.stats().played, 1);
    }

    #[test]
    fn null_sink_measures_levels_and_resets() {
        let mut sink = NullSink::new();
        sink.write(&[0, 100, -200]).expect("write");
        sink.write(&[-32_768]).expect("write");

        assert_eq!(sink.frames_written(), 2);
        assert_eq!(sink.samples_written(), 4);
        assert_eq!(sink.peak(), i16::MAX);

        sink.reset();
        assert_eq!(sink.frames_written(), 0);
        assert_eq!(sink.samples_written(), 0);
        assert_eq!(sink.peak(), 0);
        assert_eq!(sink.rms(), 0.0);
    }

    #[test]
    fn synthetic_source_stops_after_the_requested_frames() {
        let mut source = sine_frames(2);
        assert_eq!(source.sample_rate(), 48_000);
        assert_eq!(source.channels(), 1);
        assert_eq!(source.frame_samples(), 480);
        assert!(source.read().expect("frame").is_some());
        assert!(source.read().expect("frame").is_some());
        assert!(source.read().expect("end").is_none());
        assert_eq!(source.samples_emitted(), 960);
    }

    #[test]
    fn random_packets_and_sequences_never_panic() {
        // A small LCG keeps the test deterministic without a rand dependency.
        fn next(state: &mut u64) -> u64 {
            *state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            *state
        }

        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut buffer = JitterBuffer::new(4);

        for _ in 0..10_000 {
            let length = (next(&mut state) % 2_048) as usize;
            let mut bytes = Vec::with_capacity(length);
            for _ in 0..length {
                bytes.push((next(&mut state) >> 56) as u8);
            }
            let _ = MediaPacket::decode(&bytes);

            let seq = (next(&mut state) & 0xFFFF) as u16;
            let payload_length = (next(&mut state) % 64) as usize;
            let mut payload = Vec::with_capacity(payload_length);
            for _ in 0..payload_length {
                payload.push((next(&mut state) >> 56) as u8);
            }
            let _ = buffer.push(seq, payload);
            assert!(buffer.buffered() <= MAX_BUFFERED);

            if next(&mut state).is_multiple_of(3) {
                let _ = buffer.pop();
            }
            assert!(buffer.buffered() <= MAX_BUFFERED);
        }
    }
}
