//! The voice engine: capture, Opus, jitter buffering and playback.
//!
//! [`AudioEngine`] joins the transport-free pieces from `ovvc-media` to one
//! [`AudioBackend`]. The harness owns the encoder and packet sequence, the
//! engine's jitter buffer owns received packets, and the codec decodes them on
//! the way to the speaker. Mute, deafen and push-to-talk are applied on the
//! respective sides rather than by tearing anything down, so a toggle costs no
//! audio state.
//!
//! [`pump_capture`] and [`pump_playback`] are one-frame steps with no timing
//! loop of their own; a backend or a test drives them. [`run_for`] is the
//! deterministic loop the CI probe and the tests use.
//!
//! [`pump_capture`]: AudioEngine::pump_capture
//! [`pump_playback`]: AudioEngine::pump_playback
//! [`run_for`]: AudioEngine::run_for

use std::collections::BTreeMap;

use ovvc_core::AuthorId;
use ovvc_media::MediaError;
use ovvc_media::call::CallId;
use ovvc_media::jitter::{JitterBuffer, JitterStats};
use ovvc_media::level::SpeakingDetector;
use ovvc_media::packet::{MAX_MEDIA_PAYLOAD, MediaKind, MediaPacket};
use ovvc_media::pipeline::MAX_SOURCES;

use crate::backend::{AudioBackend, BackendStats, EngineConfig, pipeline_gain};
use crate::codec::{OpusCodec, OpusDecoder};
use crate::error::{AudioError, Result};

/// The author [`AudioEngine::run_for`] uses for its simulated remote peer.
const SIMULATED_PEER: AuthorId = AuthorId([0x77; 32]);

/// Counters for one [`AudioEngine::run_for`] pass.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EngineStats {
    /// Frames read from the backend.
    pub captured: u64,
    /// Encoded packets produced by [`AudioEngine::pump_capture`].
    pub packets: u64,
    /// Frames written to the backend.
    pub played: u64,
    /// Frames the jitter buffers concealed.
    pub concealed: u64,
    /// Packets the jitter buffers considered lost.
    pub lost: u64,
    /// Peer payloads the decoder rejected. Each was played as a concealed
    /// frame rather than aborting the audio pass.
    pub decode_errors: u64,
    /// Incoming packets dropped before the jitter buffer because they named a
    /// different call, were not audio, or exceeded the media payload limit.
    /// Bad input is normal on a shared transport, not a reason to stop.
    pub bad_packets: u64,
    /// Remote peer streams the engine is mixing, capped at
    /// [`MAX_SOURCES`].
    pub peers: usize,
    /// Frames produced by the mixer and written to the backend.
    pub mixed_frames: u64,
    /// Largest absolute decoded sample seen.
    pub peak: i16,
    /// Root-mean-square level of every decoded sample seen.
    pub rms: f32,
    /// Device underruns or overruns reported by the backend.
    pub backend_xruns: u64,
}

/// One remote peer's decode path.
struct PeerStream {
    decoder: OpusDecoder,
    jitter: JitterBuffer,
}

/// The Opus voice path for one call.
///
/// The capture side owns one encoder. The playback side owns one decoder and
/// jitter buffer per remote author, so two peers heard at once are mixed
/// rather than overwriting each other.
pub struct AudioEngine {
    config: EngineConfig,
    encoder: OpusCodec,
    peers: BTreeMap<AuthorId, PeerStream>,
    backend: Box<dyn AudioBackend>,
    call_id: CallId,
    author: AuthorId,
    seq: u16,
    packets: u64,
    mixed_frames: u64,
    /// Frames written by [`AudioEngine::play_local_mix`], the star-hub local
    /// playback path. Counted as played so the frontend's counters stay
    /// truthful when the hub mixer owns playback.
    local_mix_frames: u64,
    muted: bool,
    deafened: bool,
    push_to_talk: bool,
    push_to_talk_held: bool,
    speaking: bool,
    detector: SpeakingDetector,
    timestamps: u64,
    decode_errors: u64,
    bad_packets: u64,
}

impl AudioEngine {
    /// Build the codec and per-peer playback state for one call, and start
    /// `backend`.
    ///
    /// `call_id` and `author` are the negotiated identity of this engine.
    /// Outgoing packets are stamped with them, [`receive`] rejects packets for
    /// any other call or media kind, and the map of peer streams is keyed by
    /// the author of each incoming packet.
    ///
    /// [`receive`]: AudioEngine::receive
    pub fn new(
        call_id: CallId,
        author: AuthorId,
        config: EngineConfig,
        backend: Box<dyn AudioBackend>,
    ) -> Result<Self> {
        config.validate()?;

        let encoder = OpusCodec::new(config.sample_rate, config.frame_ms, config.bitrate_kbps)?;

        let mut backend = backend;
        backend.start(&config)?;

        Ok(Self {
            encoder,
            peers: BTreeMap::new(),
            backend,
            call_id,
            author,
            seq: 0,
            packets: 0,
            mixed_frames: 0,
            local_mix_frames: 0,
            config,
            muted: false,
            deafened: false,
            push_to_talk: false,
            push_to_talk_held: false,
            speaking: false,
            detector: SpeakingDetector::new(),
            timestamps: 0,
            decode_errors: 0,
            bad_packets: 0,
        })
    }

    /// The active configuration.
    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    /// Stop sending microphone audio.
    ///
    /// Pauses backend capture so a device cannot keep buffering microphone
    /// audio that would be sent on unmute, and resumes with a flush on unmute.
    /// [`pump_capture`](AudioEngine::pump_capture) still reads and discards
    /// while muted, so a backend without a pause hook cannot accumulate
    /// audio either.
    pub fn set_muted(&mut self, muted: bool) -> Result<()> {
        if self.muted != muted {
            self.backend.set_capture_active(!muted)?;
            self.muted = muted;
            if muted {
                // Muted frames are read and discarded before the detector, so
                // nothing would ever clear a speaking flag set just before the
                // mute. Clear it here so the service publishes the change.
                self.detector = SpeakingDetector::new();
                self.speaking = false;
            }
        }
        Ok(())
    }

    /// Stop playing remote audio.
    ///
    /// Buffered and decoder state is dropped, and packets that arrive while
    /// deafened are discarded, so undeafening does not replay the deafened
    /// period from a stale jitter buffer.
    pub fn set_deafened(&mut self, deafened: bool) -> Result<()> {
        if self.deafened != deafened {
            for peer in self.peers.values_mut() {
                // Clear buffered audio and playout state, but keep the
                // cumulative counters: `played`, `lost` and `concealed` are
                // documented as since the call started.
                peer.jitter.clear();
                peer.decoder.reset()?;
            }
            self.deafened = deafened;
        }
        Ok(())
    }

    /// Only send while the push-to-talk key is held.
    ///
    /// Set this once for the call; [`set_push_to_talk_held`] follows the key.
    /// Turning the gate off also releases the key, so re-enabling it cannot
    /// inherit a stale hold from before.
    ///
    /// [`set_push_to_talk_held`]: AudioEngine::set_push_to_talk_held
    pub fn set_push_to_talk(&mut self, push_to_talk: bool) {
        self.push_to_talk = push_to_talk;
        if !push_to_talk {
            self.push_to_talk_held = false;
        }
    }

    /// Tell the engine whether the push-to-talk key is currently held.
    ///
    /// While push-to-talk is enabled, frames are sent for as long as the key
    /// is held, including a quiet first syllable the speaking detector has not
    /// recognised yet. The detector still drives [`speaking`] for the UI.
    ///
    /// [`speaking`]: AudioEngine::speaking
    pub fn set_push_to_talk_held(&mut self, held: bool) {
        self.push_to_talk_held = held;
    }

    /// Whether the detector currently considers the microphone active.
    pub fn speaking(&self) -> bool {
        self.speaking
    }

    /// Read one capture frame and turn it into an encoded packet.
    ///
    /// Returns `None` when the source has no more frames, when muted, or when
    /// push-to-talk is on and the key is not held. The frame is read even when
    /// it will not be sent, so a live capture ring cannot accumulate audio
    /// that would be handed to the encoder after unmuting. The capture
    /// pipeline's `Gain` node is applied before encoding, and the timestamp
    /// advances by one frame only when a packet is actually sent.
    pub fn pump_capture(&mut self) -> Result<Option<MediaPacket>> {
        let Some(frame) = self.backend.read_capture()? else {
            return Ok(None);
        };

        if self.muted {
            // Drain the source but never queue muted audio for encoding.
            return Ok(None);
        }
        self.speaking = self.detector.push(&frame);

        if self.push_to_talk && !self.push_to_talk_held {
            // The detector still ran for the UI; the held key, not the
            // detector, decides whether audio leaves the machine.
            return Ok(None);
        }

        let gain = pipeline_gain(&self.config.capture_pipeline);
        let scaled;
        let pcm: &[i16] = if (gain - 1.0).abs() > f32::EPSILON {
            scaled = apply_gain(&frame, gain);
            &scaled
        } else {
            &frame
        };

        let payload = self.encoder.encode(pcm)?;
        let packet = MediaPacket::new(
            MediaKind::Audio,
            self.call_id,
            self.author,
            self.seq,
            self.timestamps,
            payload,
        )
        .map_err(media_error)?;
        self.seq = self.seq.wrapping_add(1);
        self.timestamps = self
            .timestamps
            .saturating_add(u64::from(self.config.frame_ms));
        self.packets = self.packets.saturating_add(1);
        Ok(Some(packet))
    }

    /// Pop one frame from every peer, decode, mix and play the result.
    ///
    /// Returns `None` while no peer buffer is ready to play or when deafened.
    /// One decoder and jitter buffer per author means two remote peers are
    /// summed rather than replacing each other. A lost frame decodes through
    /// Opus concealment. A peer payload the decoder rejects becomes one frame
    /// of silence only for that peer and is counted in
    /// [`EngineStats::decode_errors`], so one malformed packet cannot tear
    /// down the rest of the call.
    pub fn pump_playback(&mut self) -> Result<Option<Vec<i16>>> {
        if self.deafened {
            return Ok(None);
        }

        let frame_samples = self.config.frame_samples();
        let mut mix = vec![0i32; frame_samples];
        let mut produced = false;

        for peer in self.peers.values_mut() {
            let Some(frame) = peer.jitter.pop() else {
                continue;
            };
            produced = true;
            match peer.decoder.decode(&frame.payload) {
                Ok(samples) => {
                    if samples.len() != frame_samples {
                        // OpusDecoder already rejects a wrong length; this is
                        // a defensive check so a future decoder cannot index
                        // past the mix.
                        self.decode_errors = self.decode_errors.saturating_add(1);
                        continue;
                    }
                    for (target, &sample) in mix.iter_mut().zip(samples.iter()) {
                        *target = target.saturating_add(i32::from(sample));
                    }
                }
                Err(AudioError::Codec(_)) => {
                    // Play silence for this peer only; the others still mix.
                    self.decode_errors = self.decode_errors.saturating_add(1);
                }
                Err(err) => return Err(err),
            }
        }

        if !produced {
            return Ok(None);
        }

        let gain = pipeline_gain(&self.config.playback_pipeline);
        let mixed: Vec<i16> = mix
            .into_iter()
            .map(|value| {
                let scaled = (value as f32 * gain).round();
                scaled.clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
            })
            .collect();

        self.backend.write_playback(&mixed)?;
        self.mixed_frames = self.mixed_frames.saturating_add(1);
        Ok(Some(mixed))
    }

    /// Write one already-mixed frame to the playback device.
    ///
    /// This is the star hub's local playback path: [`HubMixer`] produced the
    /// samples, and this engine owns the device. Deafen is applied here so a
    /// deafened hub does not play the leaves it is still mixing for everyone
    /// else. The playback pipeline gain is applied for parity with
    /// [`pump_playback`](Self::pump_playback).
    ///
    /// [`HubMixer`]: crate::HubMixer
    pub fn play_local_mix(&mut self, samples: &[i16]) -> Result<()> {
        if self.deafened || samples.is_empty() {
            return Ok(());
        }
        let gain = pipeline_gain(&self.config.playback_pipeline);
        if (gain - 1.0).abs() < f32::EPSILON {
            self.backend.write_playback(samples)?;
        } else {
            let scaled: Vec<i16> = samples
                .iter()
                .map(|sample| {
                    let value = (f32::from(*sample) * gain).round();
                    value.clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
                })
                .collect();
            self.backend.write_playback(&scaled)?;
        }
        self.local_mix_frames = self.local_mix_frames.saturating_add(1);
        self.mixed_frames = self.mixed_frames.saturating_add(1);
        Ok(())
    }

    /// Put one received packet into its author's jitter buffer.
    ///
    /// A packet for a different call, a non-audio media kind, or a payload
    /// above [`MAX_MEDIA_PAYLOAD`] is dropped and counted in
    /// [`EngineStats::bad_packets`]. That input comes from the network, so it
    /// must not stop the audio service; only a backend or codec failure does.
    /// The first packet from an author creates that peer's decoder and jitter
    /// buffer, up to [`MAX_SOURCES`]; a peer beyond the limit is dropped
    /// without an error, because it is extra traffic, not malformed traffic.
    /// While deafened the packet is discarded, because it would otherwise be
    /// replayed on undeafening.
    pub fn receive(&mut self, packet: &MediaPacket) -> Result<()> {
        if packet.kind != MediaKind::Audio
            || packet.call_id != self.call_id
            || packet.payload.len() > MAX_MEDIA_PAYLOAD
        {
            self.bad_packets = self.bad_packets.saturating_add(1);
            return Ok(());
        }
        if self.deafened {
            return Ok(());
        }

        if !self.peers.contains_key(&packet.author) {
            if self.peers.len() >= MAX_SOURCES {
                return Ok(());
            }
            let decoder = self.encoder.new_decoder()?;
            let jitter = JitterBuffer::new(self.config.jitter_depth);
            self.peers
                .insert(packet.author, PeerStream { decoder, jitter });
        }

        let peer = self
            .peers
            .get_mut(&packet.author)
            .expect("the peer was inserted above");
        match peer.jitter.push(packet.seq, packet.payload.clone()) {
            Ok(_) => Ok(()),
            Err(MediaError::PacketTooLarge(_)) => {
                // The length check above should make this unreachable, but a
                // future jitter limit must not become a fatal service error.
                self.bad_packets = self.bad_packets.saturating_add(1);
                Ok(())
            }
            Err(err) => Err(media_error(err)),
        }
    }

    /// Run `frames` capture steps and simulate one remote peer.
    ///
    /// Each captured packet is fed back through [`receive`] with a different
    /// author, so the capture, per-peer jitter, decode and mix path runs
    /// without the network. The peer jitter buffers are drained at the end so
    /// priming does not hide the last frames. The loop allocates only the
    /// frame and payload buffers the codec and backend already allocate.
    ///
    /// [`receive`]: AudioEngine::receive
    pub fn run_for(&mut self, frames: usize) -> Result<EngineStats> {
        let captured_before = self.backend.stats().capture_frames;
        let jitter_before = self.jitter_totals();
        let decode_errors_before = self.decode_errors;
        let bad_packets_before = self.bad_packets;
        let mixed_before = self.mixed_frames;

        let mut packets = 0u64;
        let mut played = 0u64;
        let mut peak = 0i16;
        let mut sum_squares = 0f64;
        let mut sample_count = 0u64;

        for _ in 0..frames {
            let Some(packet) = self.pump_capture()? else {
                continue;
            };
            packets = packets.saturating_add(1);
            // Re-stamp the packet as a remote peer's so this exercises the
            // real receive path rather than a private shortcut.
            let remote = MediaPacket {
                author: SIMULATED_PEER,
                ..packet
            };
            self.receive(&remote)?;

            if let Some(samples) = self.pump_playback()? {
                played = played.saturating_add(1);
                accumulate(&samples, &mut peak, &mut sum_squares, &mut sample_count);
            }
        }

        while let Some(samples) = self.pump_playback()? {
            played = played.saturating_add(1);
            accumulate(&samples, &mut peak, &mut sum_squares, &mut sample_count);
        }

        let backend = self.backend.stats();
        let jitter = self.jitter_totals();
        Ok(EngineStats {
            captured: backend.capture_frames.saturating_sub(captured_before),
            packets,
            played,
            concealed: jitter.concealed.saturating_sub(jitter_before.concealed),
            lost: jitter.lost.saturating_sub(jitter_before.lost),
            decode_errors: self.decode_errors.saturating_sub(decode_errors_before),
            bad_packets: self.bad_packets.saturating_sub(bad_packets_before),
            peers: self.peers.len(),
            mixed_frames: self.mixed_frames.saturating_sub(mixed_before),
            peak,
            rms: if sample_count == 0 {
                0.0
            } else {
                (sum_squares / sample_count as f64).sqrt() as f32
            },
            backend_xruns: backend.xruns,
        })
    }

    /// The counters accumulated so far. Energy fields are only filled by
    /// [`run_for`](AudioEngine::run_for), which sees the decoded samples.
    pub fn stats(&self) -> EngineStats {
        let backend = self.backend.stats();
        let jitter = self.jitter_totals();
        EngineStats {
            captured: backend.capture_frames,
            packets: self.packets,
            // `played` on a jitter buffer counts only real packets; a
            // concealed frame still reaches the speaker as silence. The star
            // hub's local mix is written through `play_local_mix`, so it is
            // added here or a hub would report a silent device while it is
            // playing the leaf mix.
            played: jitter
                .played
                .saturating_add(jitter.concealed)
                .saturating_add(self.local_mix_frames),
            concealed: jitter.concealed,
            lost: jitter.lost,
            decode_errors: self.decode_errors,
            bad_packets: self.bad_packets,
            peers: self.peers.len(),
            mixed_frames: self.mixed_frames,
            peak: 0,
            rms: 0.0,
            backend_xruns: backend.xruns,
        }
    }

    /// The device counters behind the engine, including latency.
    pub fn backend_stats(&self) -> BackendStats {
        self.backend.stats()
    }

    /// Close the device. Idempotent; the engine must not be used afterwards.
    pub fn shutdown(&mut self) -> Result<()> {
        self.backend.stop()
    }

    /// Sum the jitter counters over every peer stream.
    fn jitter_totals(&self) -> JitterStats {
        let mut total = JitterStats::default();
        for peer in self.peers.values() {
            let stats = peer.jitter.stats();
            total.received = total.received.saturating_add(stats.received);
            total.played = total.played.saturating_add(stats.played);
            total.duplicates = total.duplicates.saturating_add(stats.duplicates);
            total.late = total.late.saturating_add(stats.late);
            total.lost = total.lost.saturating_add(stats.lost);
            total.concealed = total.concealed.saturating_add(stats.concealed);
            total.reordered = total.reordered.saturating_add(stats.reordered);
        }
        total
    }
}

/// Scale interleaved samples by a linear gain, clamping to the `i16` range.
fn apply_gain(samples: &[i16], gain: f32) -> Vec<i16> {
    samples
        .iter()
        .map(|&sample| {
            let scaled = (f32::from(sample) * gain).round();
            scaled.clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
        })
        .collect()
}

fn accumulate(samples: &[i16], peak: &mut i16, sum_squares: &mut f64, count: &mut u64) {
    for &sample in samples {
        *peak = (*peak).max(sample.saturating_abs());
        let value = f64::from(sample);
        *sum_squares += value * value;
        *count = count.saturating_add(1);
    }
}

fn media_error(err: MediaError) -> AudioError {
    AudioError::Backend(format!("media: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::NullBackend;
    use ovvc_media::packet::MediaKind;

    const CALL: CallId = CallId([0x6f; 16]);
    const AUTHOR: AuthorId = AuthorId([0x76; 32]);

    fn config() -> EngineConfig {
        EngineConfig {
            sample_rate: 48_000,
            frame_ms: 20,
            channels: 1,
            bitrate_kbps: 16,
            // A depth of one lets each packet play as soon as it arrives, so
            // the counter assertions are about the loop, not priming.
            jitter_depth: 1,
            ..EngineConfig::default()
        }
    }

    fn engine(backend: NullBackend) -> AudioEngine {
        AudioEngine::new(CALL, AUTHOR, config(), Box::new(backend)).expect("engine")
    }

    #[test]
    fn fifty_frames_produce_fifty_packets_and_fifty_played_frames() {
        let mut engine = engine(NullBackend::with_tone(config(), 1_000.0));
        let stats = engine.run_for(50).expect("run");

        assert_eq!(stats.captured, 50);
        assert_eq!(stats.packets, 50);
        assert_eq!(stats.played, 50);
        assert_eq!(stats.concealed, 0);
        assert_eq!(stats.lost, 0);
        assert!(stats.peak > 1_000, "decoded peak {}", stats.peak);
        assert!(stats.rms > 500.0, "decoded rms {}", stats.rms);
    }

    #[test]
    fn muted_still_drains_capture_so_muted_audio_never_queues() {
        let mut engine = engine(NullBackend::with_tone(config(), 1_000.0));
        engine.set_muted(true).expect("mute");
        let stats = engine.run_for(10).expect("run");

        assert_eq!(
            stats.captured, 10,
            "a muted engine must keep draining the capture ring"
        );
        assert_eq!(stats.packets, 0);
        assert_eq!(stats.played, 0);
    }

    #[test]
    fn audio_buffered_while_deafened_is_not_played_after_undeafening() {
        let mut engine = engine(NullBackend::with_tone(config(), 1_000.0));
        for _ in 0..3 {
            let packet = engine.pump_capture().expect("capture").expect("packet");
            engine.receive(&packet).expect("receive");
        }

        engine.set_deafened(true).expect("deafen");
        for _ in 0..3 {
            let packet = engine.pump_capture().expect("capture").expect("packet");
            engine.receive(&packet).expect("receive");
        }
        engine.set_deafened(false).expect("undeafen");

        assert!(
            engine.pump_playback().expect("playback").is_none(),
            "audio captured while deafened must not be played after undeafening"
        );
    }

    #[test]
    fn a_packet_for_another_call_or_media_kind_is_dropped_without_an_error() {
        let mut engine = engine(NullBackend::with_tone(config(), 1_000.0));

        let other_call = MediaPacket::new(
            MediaKind::Audio,
            CallId([0x11; 16]),
            AUTHOR,
            0,
            0,
            vec![1, 2, 3],
        )
        .expect("packet");
        engine
            .receive(&other_call)
            .expect("a foreign call must be dropped, not abort the service");

        let video =
            MediaPacket::new(MediaKind::Video, CALL, AUTHOR, 0, 0, vec![1, 2, 3]).expect("packet");
        engine
            .receive(&video)
            .expect("a non-audio kind must be dropped, not abort the service");

        let oversized = MediaPacket {
            version: ovvc_media::MEDIA_VERSION,
            kind: MediaKind::Audio,
            call_id: CALL,
            author: AUTHOR,
            seq: 0,
            timestamp_ms: 0,
            payload: vec![0u8; MAX_MEDIA_PAYLOAD + 1],
        };
        engine
            .receive(&oversized)
            .expect("an oversized payload must be dropped, not abort the service");

        assert!(
            engine.peers.is_empty(),
            "rejected packets must not create a peer stream"
        );
        assert_eq!(engine.stats().bad_packets, 3);
    }

    #[test]
    fn muting_clears_a_speaking_flag_the_detector_had_already_set() {
        let mut engine = engine(NullBackend::with_tone(config(), 1_000.0));
        engine.pump_capture().expect("capture").expect("packet");
        assert!(engine.speaking(), "the loud tone should have set speaking");

        engine.set_muted(true).expect("mute");
        assert!(
            !engine.speaking(),
            "muting mid-speech must clear the speaking flag"
        );
    }

    #[test]
    fn deafening_preserves_the_cumulative_jitter_counters() {
        let mut engine = engine(NullBackend::with_tone(config(), 1_000.0));
        for _ in 0..3 {
            let packet = engine.pump_capture().expect("capture").expect("packet");
            engine.receive(&packet).expect("receive");
            engine.pump_playback().expect("playback");
        }
        let before = engine.stats();
        assert!(before.played > 0);

        engine.set_deafened(true).expect("deafen");
        let after = engine.stats();
        assert_eq!(
            after.played, before.played,
            "deafening reset the cumulative played counter"
        );
        assert_eq!(after.lost, before.lost);
        assert_eq!(after.concealed, before.concealed);
    }

    #[test]
    fn disabling_push_to_talk_releases_a_stale_hold() {
        let mut engine = engine(NullBackend::with_tone(config(), 1_000.0));
        engine.set_push_to_talk(true);
        engine.set_push_to_talk_held(true);
        engine.set_push_to_talk(false);

        engine.set_push_to_talk(true);
        let packet = engine.pump_capture().expect("capture");
        assert!(
            packet.is_none(),
            "re-enabling push-to-talk inherited the old held key"
        );
    }

    #[test]
    fn a_malformed_payload_is_concealed_instead_of_aborting_playback() {
        let mut engine = engine(NullBackend::with_tone(config(), 1_000.0));
        let packet = MediaPacket::new(MediaKind::Audio, CALL, AUTHOR, 0, 0, vec![0xFF, 0xFF, 0xFF])
            .expect("packet");
        engine.receive(&packet).expect("receive");

        let played = engine
            .pump_playback()
            .expect("a malformed peer payload must not abort playback")
            .expect("a concealed frame");
        assert_eq!(played.len(), 960);
        assert_eq!(engine.stats().decode_errors, 1);
    }

    #[test]
    fn the_engine_writes_every_played_frame_to_the_backend_sink() {
        let mut engine = engine(NullBackend::with_tone(config(), 1_000.0));
        let stats = engine.run_for(20).expect("run");
        let backend = engine.backend.stats();

        assert_eq!(backend.playback_frames, stats.played);
        assert!(backend.playback_frames > 0, "no frame reached the sink");
    }

    #[test]
    fn push_to_talk_with_the_key_up_sends_nothing_but_drains_capture() {
        let mut engine = engine(NullBackend::with_tone(config(), 1_000.0));
        engine.set_push_to_talk(true);
        let stats = engine.run_for(10).expect("run");

        assert_eq!(stats.captured, 10);
        assert_eq!(stats.packets, 0);
        assert_eq!(stats.played, 0);
    }

    #[test]
    fn push_to_talk_sends_a_loud_frame_while_held() {
        let mut engine = engine(NullBackend::with_tone(config(), 1_000.0));
        engine.set_push_to_talk(true);
        engine.set_push_to_talk_held(true);

        let packet = engine.pump_capture().expect("capture");
        assert!(packet.is_some());
        assert!(engine.speaking());
    }

    #[test]
    fn push_to_talk_sends_a_quiet_frame_while_held() {
        let mut engine = engine(NullBackend::silent(config()));
        engine.set_push_to_talk(true);
        engine.set_push_to_talk_held(true);

        let packet = engine.pump_capture().expect("capture");
        assert!(
            packet.is_some(),
            "a held key must send the quiet first syllable the detector has not recognised"
        );
        assert!(!engine.speaking());
    }

    #[test]
    fn deafened_produces_no_playback() {
        let mut engine = engine(NullBackend::with_tone(config(), 1_000.0));
        let packet = engine.pump_capture().expect("capture").expect("a packet");
        engine.receive(&packet).expect("receive");

        engine.set_deafened(true).expect("deafen");
        assert!(engine.pump_playback().expect("playback").is_none());

        let stats = engine.run_for(5).expect("run");
        assert_eq!(stats.played, 0);
    }

    #[test]
    fn a_dropped_packet_is_counted_as_concealed() {
        let mut engine = engine(NullBackend::with_tone(config(), 1_000.0));

        let first = engine.pump_capture().expect("first").expect("packet");
        engine.receive(&first).expect("receive first");
        // Encode the second packet but never deliver it.
        let _dropped = engine.pump_capture().expect("dropped").expect("packet");
        let third = engine.pump_capture().expect("third").expect("packet");
        engine.receive(&third).expect("receive third");

        assert!(engine.pump_playback().expect("first play").is_some());
        let concealed = engine
            .pump_playback()
            .expect("concealed play")
            .expect("a frame");
        assert_eq!(concealed.len(), 960);
        // Opus loss concealment extrapolates the previous frame rather than
        // returning digital silence, so only the length is guaranteed here.
        assert!(engine.pump_playback().expect("third play").is_some());

        let stats = engine.stats();
        assert_eq!(stats.concealed, 1);
        assert_eq!(stats.lost, 1);
        assert_eq!(stats.packets, 3);
        assert_eq!(stats.played, 3);
    }

    #[test]
    fn toggles_default_to_off() {
        let engine = engine(NullBackend::new(config()));
        assert!(!engine.speaking());
        assert_eq!(engine.config().frame_samples(), 960);
    }

    fn sine(frame_ms: u16, frequency_hz: f32) -> Vec<i16> {
        let samples = 48 * usize::from(frame_ms);
        (0..samples)
            .map(|index| {
                let seconds = index as f32 / 48_000.0;
                let value = 12_000.0 * (std::f32::consts::TAU * frequency_hz * seconds).sin();
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

    fn audio_packet(author: AuthorId, seq: u16, payload: Vec<u8>) -> MediaPacket {
        MediaPacket::new(MediaKind::Audio, CALL, author, seq, u64::from(seq), payload)
            .expect("packet")
    }

    #[test]
    fn two_peer_tones_are_mixed_before_playback() {
        const FRAMES: usize = 20;
        const PEER_A: AuthorId = AuthorId([0x21; 32]);
        const PEER_B: AuthorId = AuthorId([0x22; 32]);

        let mut encoder = OpusCodec::new(48_000, 20, 16).expect("encoder");
        let mut tone_a = Vec::with_capacity(FRAMES);
        let mut tone_b = Vec::with_capacity(FRAMES);
        for _ in 0..FRAMES {
            tone_a.push(encoder.encode(&sine(20, 440.0)).expect("encode a"));
            tone_b.push(encoder.encode(&sine(20, 880.0)).expect("encode b"));
        }

        let mut engine = self::engine(NullBackend::silent(config()));
        for index in 0..FRAMES {
            engine
                .receive(&audio_packet(PEER_A, index as u16, tone_a[index].clone()))
                .expect("receive a");
            engine
                .receive(&audio_packet(PEER_B, index as u16, tone_b[index].clone()))
                .expect("receive b");
        }

        let mut mixed = Vec::with_capacity(FRAMES);
        for _ in 0..FRAMES {
            let frame = engine
                .pump_playback()
                .expect("playback")
                .expect("both peers produced a frame");
            assert_eq!(frame.len(), 960);
            mixed.extend_from_slice(&frame);
        }

        let peak = mixed
            .iter()
            .map(|sample| sample.saturating_abs())
            .max()
            .expect("mixed audio");
        // One 12_000-amplitude tone peaks around 12_000; two summed tones
        // regularly exceed that, and their RMS is higher than either alone.
        assert!(peak > 15_000, "mixed peak {peak} is not two summed tones");
        let mixed_rms = rms(&mixed);
        assert!(
            mixed_rms > 9_000.0,
            "mixed rms {mixed_rms} suggests the peers replaced rather than summed"
        );

        let stats = engine.stats();
        assert_eq!(stats.peers, 2);
        assert_eq!(stats.mixed_frames, FRAMES as u64);
        assert_eq!(stats.played, (FRAMES * 2) as u64);
    }

    #[test]
    fn a_bad_peer_payload_is_concealed_while_the_other_peer_plays() {
        const PEER_A: AuthorId = AuthorId([0x31; 32]);
        const PEER_B: AuthorId = AuthorId([0x32; 32]);

        let mut encoder = OpusCodec::new(48_000, 20, 16).expect("encoder");
        let good = encoder.encode(&sine(20, 1_000.0)).expect("encode");

        let mut engine = self::engine(NullBackend::silent(config()));
        engine
            .receive(&audio_packet(PEER_A, 0, good))
            .expect("receive good peer");
        engine
            .receive(&audio_packet(PEER_B, 0, vec![0xFF, 0xFF, 0xFF]))
            .expect("receive corrupt peer");

        let frame = engine
            .pump_playback()
            .expect("a corrupt peer must not abort the mix")
            .expect("both peers produced a frame");
        assert!(
            frame.iter().any(|&sample| sample != 0),
            "the good peer's audio must survive the corrupt peer's silence"
        );

        let stats = engine.stats();
        assert_eq!(stats.decode_errors, 1);
        assert_eq!(stats.peers, 2);
        assert_eq!(stats.mixed_frames, 1);
    }

    #[test]
    fn peers_beyond_the_source_limit_are_dropped_without_an_error() {
        let mut encoder = OpusCodec::new(48_000, 20, 16).expect("encoder");
        let payload = encoder.encode(&sine(20, 1_000.0)).expect("encode");

        let mut engine = self::engine(NullBackend::silent(config()));
        for index in 0..=MAX_SOURCES {
            let mut author = [0u8; 32];
            author[0] = index as u8;
            let author = AuthorId(author);
            engine
                .receive(&audio_packet(author, 0, payload.clone()))
                .expect("extra peers are dropped, not errors");
        }

        assert_eq!(engine.stats().peers, MAX_SOURCES);
    }
}
