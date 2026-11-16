//! The star-hub audio mixer: decode every leaf once, mix per listener, encode
//! per listener.
//!
//! This module is the media-plane half of a star call. It is additive: the
//! mesh [`AudioEngine`](crate::engine::AudioEngine) and the two-person direct
//! pair keep using their own decode-and-play path and never touch this type.
//!
//! # Model
//!
//! Every call member, including the hub itself, is a *source*. A source offers
//! encoded Opus frames through [`HubMixer::receive`], which puts them in that
//! source's jitter buffer. Each [`HubMixer::pump`] pops at most one frame from
//! every source, decodes it once (a gap becomes one Opus PLC frame), and then
//! builds:
//!
//! * the hub's local playback, which sums every source except the hub, so the
//!   hub hears the leaves and never its own microphone;
//! * one encoded mix per listener, which sums every source except that
//!   listener, so a leaf hears the other leaves and the hub but not itself.
//!
//! Each listener owns a separate [`OpusEncoder`](crate::codec::OpusEncoder),
//! because the two mixes differ and Opus encoder state cannot be shared.
//!
//! # Daemon integration
//!
//! For a hub-role call, after `reconcile_rtc` has established the flat star:
//!
//! ```ignore
//! use ovvc_audio::{HubMixer, ReceiveOutcome};
//!
//! // 1. One mixer for the call, using the engine settings already negotiated.
//! let mut mixer = HubMixer::new(local_author, &engine_config)?;
//!
//! // 2. Register every non-hub member that should receive a mix.
//! for leaf in topology.children_of(local_author) {
//!     mixer.add_listener(leaf)?;
//! }
//!
//! // 3. On each decrypted leaf packet (the daemon already removed the
//! //    pairwise seal):
//! mixer.receive(packet.author, packet.seq, packet.payload.clone())?;
//!
//! // 4. Feed the hub's own captured and encoded voice as the local author.
//! //    It goes to every listener and never into local playback.
//! mixer.receive(local_author, own_seq, own_payload)?;
//!
//! // 5. Once per frame period:
//! if let Some(frame) = mixer.pump()? {
//!     if let Some(samples) = frame.local_playback {
//!         backend.write_playback(&samples)?;
//!     }
//!     for listener in frame.listener_frames {
//!         let packet = MediaPacket::new(
//!             MediaKind::Audio,
//!             call_id,
//!             local_author,
//!             downlink_seq,
//!             timestamp_ms,
//!             listener.payload,
//!         )?;
//!         // Seal `packet` with the listener's leaf-to-hub pairwise key.
//!         send_to(listener.listener, packet);
//!     }
//! }
//!
//! // 6. On a member leaving:
//! mixer.remove_source(author);
//! mixer.remove_listener(author);
//! ```
//!
//! The mixer is transport-free and keeps no device. A leaf-role daemon uses
//! the existing [`AudioEngine`](crate::engine::AudioEngine) unchanged; only
//! the hub needs this path.
//!
//! On a hub-role call the mixer owns local playback: do not also feed leaf
//! packets into `AudioEngine`, or the hub plays every leaf twice. The engine
//! still owns the hub microphone, mute, deafen and push-to-talk, and its
//! encoded microphone packet is the `own_payload` above.
//!
//! [`HubMixer::add_listener`]: crate::hub_mixer::HubMixer::add_listener
//! [`HubMixer::receive`]: crate::hub_mixer::HubMixer::receive
//! [`HubMixer::pump`]: crate::hub_mixer::HubMixer::pump

use std::collections::BTreeMap;

use ovvc_core::AuthorId;
use ovvc_media::MediaError;
use ovvc_media::jitter::{JitterBuffer, PushOutcome};
use ovvc_media::packet::MAX_MEDIA_PAYLOAD;
use ovvc_media::pipeline::MAX_SOURCES;

use crate::backend::EngineConfig;
use crate::codec::{OpusDecoder, OpusEncoder};
use crate::error::{AudioError, Result};

/// What happened to a frame offered to [`HubMixer::receive`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiveOutcome {
    /// The frame entered the source's jitter buffer.
    Accepted,
    /// The sequence number was already buffered.
    Duplicate,
    /// The frame arrived after its playout point and was dropped.
    Late,
    /// The source's jitter buffer is full.
    BufferFull,
    /// The payload is above [`MAX_MEDIA_PAYLOAD`].
    TooLarge,
    /// There are already [`MAX_SOURCES`] sources in the call.
    OverCapacity,
}

impl ReceiveOutcome {
    /// Whether the frame entered the jitter buffer.
    pub fn accepted(self) -> bool {
        matches!(self, Self::Accepted)
    }
}

/// One source's decode path: a jitter buffer and the decoder that owns its
/// Opus concealment state.
struct SourceStream {
    decoder: OpusDecoder,
    jitter: JitterBuffer,
    decode_errors: u64,
}

/// One listener's encode path. Every listener needs its own encoder state
/// because every listener receives a different mix.
struct ListenerSink {
    encoder: OpusEncoder,
    frames_encoded: u64,
    bytes_encoded: u64,
    source_frames: u64,
    peak: i16,
    sum_squares: f64,
    samples: u64,
}

/// Counters for one source (a leaf or the hub's own microphone).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SourceStats {
    /// Frames accepted by the jitter buffer since the source was added.
    pub received: u64,
    /// Real frames the jitter buffer handed to the mixer. Concealed frames
    /// are counted in [`SourceStats::concealed`].
    pub played: u64,
    /// Duplicate sequence numbers.
    pub duplicates: u64,
    /// Frames that arrived too late to play.
    pub late: u64,
    /// Packets the jitter buffer considered lost.
    pub lost: u64,
    /// Missing frames concealed with Opus PLC.
    pub concealed: u64,
    /// Frames that arrived out of order but in time.
    pub reordered: u64,
    /// Payloads the decoder rejected. That source contributes silence for the
    /// affected frame, but the rest of the mix continues.
    pub decode_errors: u64,
    /// Frames currently buffered and waiting to play.
    pub buffered: usize,
}

/// Counters for one listener's encoded mix.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ListenerStats {
    /// Frames encoded for this listener.
    pub frames_encoded: u64,
    /// Total bytes of encoded payload produced for this listener.
    pub bytes_encoded: u64,
    /// Sum of the decoded source frames that contributed to those mixes.
    pub source_frames: u64,
    /// Largest absolute sample in any mix sent to this listener.
    pub peak: i16,
    /// RMS of every sample in every mix sent to this listener.
    pub rms: f32,
}

/// A whole-mixer snapshot.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HubMixerStats {
    /// Sources currently tracked, including the hub when it is sending.
    pub sources: usize,
    /// Listeners currently registered.
    pub listeners: usize,
    /// Frames produced for the hub's local playback.
    pub local_frames: u64,
    /// Largest absolute sample in the hub's local mix.
    pub local_peak: i16,
    /// RMS of every sample in the hub's local mix.
    pub local_rms: f32,
    /// Frames encoded across every listener.
    pub listener_frames: u64,
    /// Bytes encoded across every listener.
    pub listener_bytes: u64,
    /// Source payloads rejected by every decoder.
    pub decode_errors: u64,
    /// Frames dropped before a jitter buffer because they were oversized or
    /// over capacity.
    pub rejected: u64,
}

/// One encoded mix for one listener, ready for the daemon to seal and send.
#[derive(Debug, Clone, PartialEq)]
pub struct ListenerFrame {
    /// The leaf this mix is for.
    pub listener: AuthorId,
    /// The encoded Opus frame. The daemon stamps the call id, sequence and
    /// timestamp when it builds the [`MediaPacket`](ovvc_media::MediaPacket).
    pub payload: Vec<u8>,
    /// Number of decoded source frames that contributed to this mix.
    pub sources: usize,
    /// Largest absolute sample in the mix before encoding.
    pub peak: i16,
    /// RMS of the mix before encoding.
    pub rms: f32,
}

/// The output of one [`HubMixer::pump`].
#[derive(Debug, Clone, PartialEq)]
pub struct HubMixFrame {
    /// The hub's local playback: every leaf, never the hub's own microphone.
    /// `None` when no non-hub source produced a frame this tick, so the
    /// daemon should not write a playback frame.
    pub local_playback: Option<Vec<i16>>,
    /// One encoded mix per listener that had at least one other source.
    pub listener_frames: Vec<ListenerFrame>,
}

impl HubMixFrame {
    /// Whether this tick produced nothing to play or send.
    pub fn is_empty(&self) -> bool {
        self.local_playback.is_none() && self.listener_frames.is_empty()
    }
}

/// The hub side of a star call.
///
/// See the [module documentation](self) for the daemon call sequence.
pub struct HubMixer {
    hub: AuthorId,
    config: EngineConfig,
    frame_samples: usize,
    sources: BTreeMap<AuthorId, SourceStream>,
    listeners: BTreeMap<AuthorId, ListenerSink>,
    local_frames: u64,
    local_peak: i16,
    local_sum_squares: f64,
    local_samples: u64,
    rejected: u64,
}

impl HubMixer {
    /// Build a hub mixer for `hub` from the same [`EngineConfig`] the call's
    /// [`AudioEngine`](crate::engine::AudioEngine) uses.
    ///
    /// Only the codec and jitter fields are read: sample rate, frame length,
    /// bitrate and jitter depth. The capture and playback pipelines are
    /// validated with the rest of the config but are not applied here; the
    /// hub has no device of its own.
    pub fn new(hub: AuthorId, config: &EngineConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            hub,
            config: config.clone(),
            frame_samples: config.frame_samples(),
            sources: BTreeMap::new(),
            listeners: BTreeMap::new(),
            local_frames: 0,
            local_peak: 0,
            local_sum_squares: 0.0,
            local_samples: 0,
            rejected: 0,
        })
    }

    /// The local author this mixer treats as the hub.
    pub fn hub(&self) -> AuthorId {
        self.hub
    }

    /// The configuration the mixer was built from.
    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    /// Samples in one decoded or mixed frame.
    pub fn frame_samples(&self) -> usize {
        self.frame_samples
    }

    /// Register a leaf to receive a per-listener mix.
    ///
    /// Returns `Ok(true)` when the listener was added and `Ok(false)` when it
    /// was already registered or the [`MAX_SOURCES`] listener limit is
    /// reached. An encoder failure is returned, so a call cannot silently run
    /// with a listener that will never receive audio. Registering a listener
    /// does not create a source; the first [`receive`](Self::receive) from
    /// that author does.
    pub fn add_listener(&mut self, listener: AuthorId) -> Result<bool> {
        if self.listeners.contains_key(&listener) {
            return Ok(false);
        }
        if self.listeners.len() >= MAX_SOURCES {
            return Ok(false);
        }
        let encoder = OpusEncoder::new(
            self.config.sample_rate,
            self.config.frame_ms,
            self.config.bitrate_kbps,
        )?;
        self.listeners.insert(
            listener,
            ListenerSink {
                encoder,
                frames_encoded: 0,
                bytes_encoded: 0,
                source_frames: 0,
                peak: 0,
                sum_squares: 0.0,
                samples: 0,
            },
        );
        Ok(true)
    }

    /// Stop sending a mix to `listener`. Returns `true` when it was registered.
    pub fn remove_listener(&mut self, listener: AuthorId) -> bool {
        self.listeners.remove(&listener).is_some()
    }

    /// The listeners currently registered, in author order.
    pub fn listeners(&self) -> impl Iterator<Item = AuthorId> + '_ {
        self.listeners.keys().copied()
    }

    /// Remove a departed source and drop its jitter buffer and decoder state.
    /// Returns `true` when it was tracked.
    pub fn remove_source(&mut self, author: AuthorId) -> bool {
        self.sources.remove(&author).is_some()
    }

    /// The sources currently tracked, in author order.
    pub fn sources(&self) -> impl Iterator<Item = AuthorId> + '_ {
        self.sources.keys().copied()
    }

    /// Offer one encoded Opus frame from `author`.
    ///
    /// `author` may be a leaf or the hub itself. The hub's own frames are
    /// mixed into every listener's output and excluded from local playback,
    /// which is what keeps the hub from echoing to itself. A new author
    /// creates its own decoder and jitter buffer, up to [`MAX_SOURCES`];
    /// frames above that are counted in [`HubMixerStats::rejected`] and
    /// dropped rather than treated as malformed input.
    ///
    /// The daemon calls this after decrypting a leaf's packet with that leaf's
    /// pairwise key, so the mixer never sees ciphertext.
    pub fn receive(
        &mut self,
        author: AuthorId,
        seq: u16,
        payload: Vec<u8>,
    ) -> Result<ReceiveOutcome> {
        if payload.len() > MAX_MEDIA_PAYLOAD {
            self.rejected = self.rejected.saturating_add(1);
            return Ok(ReceiveOutcome::TooLarge);
        }
        if !self.sources.contains_key(&author) {
            if self.sources.len() >= MAX_SOURCES {
                self.rejected = self.rejected.saturating_add(1);
                return Ok(ReceiveOutcome::OverCapacity);
            }
            let decoder = OpusDecoder::new(self.config.sample_rate, self.config.frame_ms)?;
            let jitter = JitterBuffer::new(self.config.jitter_depth);
            self.sources.insert(
                author,
                SourceStream {
                    decoder,
                    jitter,
                    decode_errors: 0,
                },
            );
        }

        let source = self
            .sources
            .get_mut(&author)
            .expect("the source was inserted above");
        let outcome = source.jitter.push(seq, payload).map_err(media_error)?;
        Ok(match outcome {
            PushOutcome::Accepted => ReceiveOutcome::Accepted,
            PushOutcome::Duplicate => ReceiveOutcome::Duplicate,
            PushOutcome::Late => ReceiveOutcome::Late,
            PushOutcome::BufferFull => ReceiveOutcome::BufferFull,
        })
    }

    /// Decode, mix and encode one frame period.
    ///
    /// Every source is popped once and decoded once per call, so the hub pays
    /// one decoder per leaf rather than one decoder per leaf and listener.
    /// A gap in a source's jitter buffer becomes one Opus PLC frame, exactly
    /// as in the mesh engine. A payload the decoder rejects is counted for
    /// that source and treated as silence; it never aborts the other sources.
    ///
    /// Returns `None` when no source produced a frame this tick. A tick where
    /// only the hub's own voice is ready produces listener frames but no local
    /// playback, so the hub does not hear itself.
    pub fn pump(&mut self) -> Result<Option<HubMixFrame>> {
        let frame_samples = self.frame_samples;
        let mut decoded: Vec<(AuthorId, Vec<i16>)> = Vec::with_capacity(self.sources.len());
        let mut produced = false;

        for (author, source) in self.sources.iter_mut() {
            let Some(frame) = source.jitter.pop() else {
                continue;
            };
            produced = true;
            match source.decoder.decode(&frame.payload) {
                Ok(samples) if samples.len() == frame_samples => decoded.push((*author, samples)),
                Ok(_) => {
                    // A future decoder must not be allowed to index past the
                    // mix; this is unreachable with OpusDecoder today.
                    source.decode_errors = source.decode_errors.saturating_add(1);
                }
                Err(AudioError::Codec(_)) => {
                    source.decode_errors = source.decode_errors.saturating_add(1);
                }
                Err(err) => return Err(err),
            }
        }

        if !produced {
            return Ok(None);
        }

        let local_playback = self.build_local_mix(&decoded);
        let mut listener_frames = Vec::with_capacity(self.listeners.len());
        for (listener, sink) in self.listeners.iter_mut() {
            let (mixed, sources) = mix_excluding(&decoded, *listener, frame_samples);
            if sources == 0 {
                // The only source that produced this tick was this listener's
                // own stream, so there is nothing to send. This is the
                // self-exclusion rule at frame level.
                continue;
            }

            let payload = sink.encoder.encode(&mixed)?;
            let (peak, sum_squares) = energy(&mixed);
            sink.frames_encoded = sink.frames_encoded.saturating_add(1);
            sink.bytes_encoded = sink.bytes_encoded.saturating_add(payload.len() as u64);
            sink.source_frames = sink.source_frames.saturating_add(sources as u64);
            sink.peak = sink.peak.max(peak);
            sink.sum_squares += sum_squares;
            sink.samples = sink.samples.saturating_add(mixed.len() as u64);

            listener_frames.push(ListenerFrame {
                listener: *listener,
                payload,
                sources,
                peak,
                rms: rms_of(sum_squares, mixed.len()),
            });
        }

        Ok(Some(HubMixFrame {
            local_playback,
            listener_frames,
        }))
    }

    /// The hub's local playback, which is every source except the hub.
    fn build_local_mix(&mut self, decoded: &[(AuthorId, Vec<i16>)]) -> Option<Vec<i16>> {
        let (mixed, sources) = mix_excluding(decoded, self.hub, self.frame_samples);
        if sources == 0 {
            return None;
        }
        let (peak, sum_squares) = energy(&mixed);
        self.local_frames = self.local_frames.saturating_add(1);
        self.local_peak = self.local_peak.max(peak);
        self.local_sum_squares += sum_squares;
        self.local_samples = self.local_samples.saturating_add(mixed.len() as u64);
        Some(mixed)
    }

    /// Jitter and decode counters for one source.
    pub fn source_stats(&self, author: AuthorId) -> Option<SourceStats> {
        self.sources.get(&author).map(|source| {
            let jitter = source.jitter.stats();
            SourceStats {
                received: jitter.received,
                played: jitter.played,
                duplicates: jitter.duplicates,
                late: jitter.late,
                lost: jitter.lost,
                concealed: jitter.concealed,
                reordered: jitter.reordered,
                decode_errors: source.decode_errors,
                buffered: source.jitter.buffered(),
            }
        })
    }

    /// Encode counters for one listener.
    pub fn listener_stats(&self, listener: AuthorId) -> Option<ListenerStats> {
        self.listeners.get(&listener).map(|sink| ListenerStats {
            frames_encoded: sink.frames_encoded,
            bytes_encoded: sink.bytes_encoded,
            source_frames: sink.source_frames,
            peak: sink.peak,
            rms: rms_of(sink.sum_squares, sink.samples as usize),
        })
    }

    /// A snapshot of every counter in the mixer.
    pub fn stats(&self) -> HubMixerStats {
        let mut decode_errors = 0u64;
        for source in self.sources.values() {
            decode_errors = decode_errors.saturating_add(source.decode_errors);
        }

        let mut listener_frames = 0u64;
        let mut listener_bytes = 0u64;
        for sink in self.listeners.values() {
            listener_frames = listener_frames.saturating_add(sink.frames_encoded);
            listener_bytes = listener_bytes.saturating_add(sink.bytes_encoded);
        }

        HubMixerStats {
            sources: self.sources.len(),
            listeners: self.listeners.len(),
            local_frames: self.local_frames,
            local_peak: self.local_peak,
            local_rms: rms_of(self.local_sum_squares, self.local_samples as usize),
            listener_frames,
            listener_bytes,
            decode_errors,
            rejected: self.rejected,
        }
    }
}

/// Sum every decoded source except `exclude`, returning the clamped mix and
/// how many sources contributed. A source that produced no frame this tick is
/// simply absent from `decoded`.
fn mix_excluding(
    decoded: &[(AuthorId, Vec<i16>)],
    exclude: AuthorId,
    frame_samples: usize,
) -> (Vec<i16>, usize) {
    let mut mix = vec![0i32; frame_samples];
    let mut sources = 0usize;
    for (author, samples) in decoded {
        if *author == exclude {
            continue;
        }
        sources = sources.saturating_add(1);
        for (target, &sample) in mix.iter_mut().zip(samples.iter()) {
            *target = target.saturating_add(i32::from(sample));
        }
    }
    let mixed = mix
        .into_iter()
        .map(|value| value.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16)
        .collect();
    (mixed, sources)
}

/// Largest absolute sample and sum of squares of one frame.
fn energy(samples: &[i16]) -> (i16, f64) {
    let mut peak = 0i16;
    let mut sum_squares = 0f64;
    for &sample in samples {
        peak = peak.max(sample.saturating_abs());
        let value = f64::from(sample);
        sum_squares += value * value;
    }
    (peak, sum_squares)
}

fn rms_of(sum_squares: f64, count: usize) -> f32 {
    if count == 0 {
        0.0
    } else {
        (sum_squares / count as f64).sqrt() as f32
    }
}

fn media_error(err: MediaError) -> AudioError {
    // The length is checked above, so this is unreachable until the jitter
    // buffer grows another fallible input. Keep it a non-fatal error type.
    AudioError::Backend(format!("media: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::OpusCodec;
    use std::f32::consts::TAU;

    const HUB: AuthorId = AuthorId([0x10; 32]);
    const LEAF_A: AuthorId = AuthorId([0x21; 32]);
    const LEAF_B: AuthorId = AuthorId([0x22; 32]);

    /// Frequencies that are exact multiples of the 50 Hz bin width of a 20 ms
    /// frame, so a one-frame DFT isolates each source with little leakage.
    const TONE_A: f32 = 400.0;
    const TONE_B: f32 = 1_000.0;
    const TONE_HUB: f32 = 2_000.0;
    const AMPLITUDE: f32 = 8_000.0;

    fn config() -> EngineConfig {
        EngineConfig {
            // Play every packet as soon as it arrives so pump counts are
            // about the mix rather than about priming.
            jitter_depth: 1,
            ..EngineConfig::default()
        }
    }

    fn mixer() -> HubMixer {
        HubMixer::new(HUB, &config()).expect("mixer")
    }

    fn sine(frequency_hz: f32) -> Vec<i16> {
        (0..960)
            .map(|index| {
                let seconds = index as f32 / 48_000.0;
                (AMPLITUDE * (TAU * frequency_hz * seconds).sin()).round() as i16
            })
            .collect()
    }

    fn encoders() -> [OpusCodec; 3] {
        [
            OpusCodec::new(48_000, 20, 16).expect("encoder a"),
            OpusCodec::new(48_000, 20, 16).expect("encoder b"),
            OpusCodec::new(48_000, 20, 16).expect("encoder hub"),
        ]
    }

    fn encode(codec: &mut OpusCodec, frequency_hz: f32) -> Vec<u8> {
        codec.encode(&sine(frequency_hz)).expect("encode")
    }

    /// One-frame DFT magnitude at `frequency_hz`. Frames are concatenated, and
    /// every test frequency is an integer number of cycles per frame, so the
    /// phase stays continuous across frame boundaries.
    fn tone_energy(samples: &[i16], frequency_hz: f32) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let mut re = 0f64;
        let mut im = 0f64;
        for (index, &sample) in samples.iter().enumerate() {
            let phase = TAU as f64 * f64::from(frequency_hz) * index as f64 / 48_000.0;
            re += f64::from(sample) * phase.cos();
            im += f64::from(sample) * phase.sin();
        }
        ((re * re + im * im).sqrt() / samples.len() as f64) as f32
    }

    fn decode(codec: &mut OpusDecoder, payload: &[u8]) -> Vec<i16> {
        codec.decode(payload).expect("decode listener mix")
    }

    fn find_listener(frame: &HubMixFrame, listener: AuthorId) -> Option<&ListenerFrame> {
        frame
            .listener_frames
            .iter()
            .find(|candidate| candidate.listener == listener)
    }

    #[test]
    fn three_peer_star_excludes_each_listener_and_the_hub_microphone() {
        const TICKS: usize = 30;

        let mut mixer = mixer();
        assert!(mixer.add_listener(LEAF_A).expect("listener a"));
        assert!(mixer.add_listener(LEAF_B).expect("listener b"));
        assert!(
            !mixer.add_listener(LEAF_A).expect("duplicate listener"),
            "a listener must only be registered once"
        );

        let [mut encoder_a, mut encoder_b, mut encoder_hub] = encoders();
        let mut decoder_a = OpusDecoder::new(48_000, 20).expect("decoder a");
        let mut decoder_b = OpusDecoder::new(48_000, 20).expect("decoder b");

        let mut mix_a = Vec::new();
        let mut mix_b = Vec::new();
        let mut local = Vec::new();

        for tick in 0..TICKS {
            let payload_a = encode(&mut encoder_a, TONE_A);
            let payload_b = encode(&mut encoder_b, TONE_B);
            let payload_hub = encode(&mut encoder_hub, TONE_HUB);
            assert!(
                mixer
                    .receive(LEAF_A, tick as u16, payload_a)
                    .expect("receive a")
                    .accepted()
            );
            assert!(
                mixer
                    .receive(LEAF_B, tick as u16, payload_b)
                    .expect("receive b")
                    .accepted()
            );
            assert!(
                mixer
                    .receive(HUB, tick as u16, payload_hub)
                    .expect("receive hub")
                    .accepted()
            );

            let frame = mixer.pump().expect("pump").expect("three sources");
            assert!(!frame.is_empty());
            local.extend(
                frame
                    .local_playback
                    .as_ref()
                    .expect("the leaves play locally"),
            );

            let listener_a = find_listener(&frame, LEAF_A).expect("leaf A gets a mix");
            let listener_b = find_listener(&frame, LEAF_B).expect("leaf B gets a mix");
            assert_eq!(listener_a.sources, 2, "A hears B and the hub");
            assert_eq!(listener_b.sources, 2, "B hears A and the hub");
            assert_ne!(
                listener_a.payload, listener_b.payload,
                "each listener needs its own mix and encoder state"
            );

            mix_a.extend(decode(&mut decoder_a, &listener_a.payload));
            mix_b.extend(decode(&mut decoder_b, &listener_b.payload));
        }

        let a_own = tone_energy(&mix_a, TONE_A);
        let b_own = tone_energy(&mix_b, TONE_B);
        let hub_in_a = tone_energy(&mix_a, TONE_HUB);
        let b_in_a = tone_energy(&mix_a, TONE_B);
        let a_in_b = tone_energy(&mix_b, TONE_A);
        let hub_in_b = tone_energy(&mix_b, TONE_HUB);

        assert!(
            b_in_a > 1_000.0 && hub_in_a > 1_000.0,
            "leaf A must hear B ({b_in_a}) and the hub ({hub_in_a})"
        );
        assert!(
            a_in_b > 1_000.0 && hub_in_b > 1_000.0,
            "leaf B must hear A ({a_in_b}) and the hub ({hub_in_b})"
        );
        assert!(
            a_own < b_in_a * 0.2 && a_own < hub_in_a * 0.2,
            "leaf A heard its own stream: own {a_own}, b {b_in_a}, hub {hub_in_a}"
        );
        assert!(
            b_own < a_in_b * 0.2 && b_own < hub_in_b * 0.2,
            "leaf B heard its own stream: own {b_own}, a {a_in_b}, hub {hub_in_b}"
        );

        let local_a = tone_energy(&local, TONE_A);
        let local_b = tone_energy(&local, TONE_B);
        let local_hub = tone_energy(&local, TONE_HUB);
        assert!(
            local_a > 1_000.0 && local_b > 1_000.0,
            "the hub must hear both leaves: a {local_a}, b {local_b}"
        );
        assert!(
            local_hub < local_a * 0.2 && local_hub < local_b * 0.2,
            "the hub echoed its own voice: hub {local_hub}, a {local_a}, b {local_b}"
        );

        let stats = mixer.stats();
        assert_eq!(stats.sources, 3);
        assert_eq!(stats.listeners, 2);
        assert_eq!(stats.local_frames, TICKS as u64);
        assert_eq!(stats.listener_frames, (TICKS * 2) as u64);
        assert_eq!(stats.decode_errors, 0);
        assert!(stats.listener_bytes > 0);
        assert!(stats.local_rms > 1_000.0);

        let stats_a = mixer.listener_stats(LEAF_A).expect("stats a");
        assert_eq!(stats_a.frames_encoded, TICKS as u64);
        assert_eq!(stats_a.source_frames, (TICKS * 2) as u64);
        assert!(stats_a.bytes_encoded > 0);
        assert!(stats_a.peak > 1_000);
        assert!(stats_a.rms > 1_000.0);
        assert_eq!(
            mixer
                .listener_stats(LEAF_B)
                .expect("stats b")
                .frames_encoded,
            TICKS as u64
        );
    }

    #[test]
    fn a_listener_who_is_the_only_source_receives_no_echo() {
        let mut mixer = mixer();
        assert!(mixer.add_listener(LEAF_A).expect("listener a"));
        assert!(mixer.add_listener(LEAF_B).expect("listener b"));

        let [mut encoder_a, ..] = encoders();
        let payload = encode(&mut encoder_a, TONE_A);
        assert_eq!(
            mixer.receive(LEAF_A, 0, payload).expect("receive a"),
            ReceiveOutcome::Accepted
        );

        let frame = mixer.pump().expect("pump").expect("a source");
        assert!(
            find_listener(&frame, LEAF_A).is_none(),
            "leaf A must not receive a mix of only its own stream"
        );
        let listener_b = find_listener(&frame, LEAF_B).expect("leaf B hears A");
        assert_eq!(listener_b.sources, 1);

        let mut decoder = OpusDecoder::new(48_000, 20).expect("decoder");
        let samples = decode(&mut decoder, &listener_b.payload);
        assert!(
            tone_energy(&samples, TONE_A) > 1_000.0,
            "leaf B should hear A's tone"
        );

        // The hub's local playback is still the leaf mix.
        let local = frame.local_playback.expect("the hub hears the leaf");
        assert!(tone_energy(&local, TONE_A) > 1_000.0);

        assert_eq!(
            mixer.listener_stats(LEAF_A).expect("stats").frames_encoded,
            0
        );
    }

    #[test]
    fn the_hub_voice_reaches_every_listener_but_not_local_playback() {
        let mut mixer = mixer();
        assert!(mixer.add_listener(LEAF_A).expect("listener a"));
        assert!(mixer.add_listener(LEAF_B).expect("listener b"));

        let [.., mut encoder_hub] = encoders();
        // The first frame from a fresh Opus encoder and decoder is a
        // transient, so measure a later frame. In a call both ends have been
        // running for several frames before a listener is unmuted.
        const WARMUP_FRAMES: u16 = 4;
        let listeners = [LEAF_A, LEAF_B];
        let mut decoders = [
            OpusDecoder::new(48_000, 20).expect("decoder a"),
            OpusDecoder::new(48_000, 20).expect("decoder b"),
        ];

        for seq in 0..=WARMUP_FRAMES {
            let payload = encode(&mut encoder_hub, TONE_HUB);
            assert_eq!(
                mixer.receive(HUB, seq, payload).expect("receive hub"),
                ReceiveOutcome::Accepted
            );

            let frame = mixer.pump().expect("pump").expect("hub source");
            assert!(
                frame.local_playback.is_none(),
                "the hub must not echo its own microphone into local playback"
            );
            for (index, listener) in listeners.iter().enumerate() {
                let frame_for =
                    find_listener(&frame, *listener).expect("hub voice reaches every leaf");
                assert_eq!(frame_for.sources, 1);
                let samples = decode(&mut decoders[index], &frame_for.payload);
                if seq == WARMUP_FRAMES {
                    assert!(
                        tone_energy(&samples, TONE_HUB) > 1_000.0,
                        "listener {listener:?} did not hear the hub"
                    );
                }
            }
        }
    }

    #[test]
    fn jitter_reorders_frames_before_the_mix() {
        let mut mixer = mixer();
        assert!(mixer.add_listener(LEAF_B).expect("listener b"));

        let [mut encoder_a, ..] = encoders();
        let second = encode(&mut encoder_a, TONE_B);
        let first = encode(&mut encoder_a, TONE_A);

        // Deliver sequence 1 before sequence 0.
        mixer.receive(LEAF_A, 1, second).expect("receive 1");
        mixer.receive(LEAF_A, 0, first).expect("receive 0");

        let stats = mixer.source_stats(LEAF_A).expect("source stats");
        assert_eq!(stats.reordered, 1);
        assert_eq!(stats.received, 2);

        let first_frame = mixer
            .pump()
            .expect("pump")
            .expect("first")
            .local_playback
            .expect("the hub hears the first frame");
        assert!(
            tone_energy(&first_frame, TONE_A) > tone_energy(&first_frame, TONE_B),
            "the earlier sequence must play first"
        );

        let second_frame = mixer
            .pump()
            .expect("pump")
            .expect("second")
            .local_playback
            .expect("the hub hears the second frame");
        assert!(
            tone_energy(&second_frame, TONE_B) > tone_energy(&second_frame, TONE_A),
            "the later sequence must play second"
        );

        let stats = mixer.source_stats(LEAF_A).expect("source stats");
        assert_eq!(stats.played, 2);
        assert_eq!(stats.lost, 0);
    }

    #[test]
    fn a_lost_leaf_frame_is_concealed_with_plc_and_the_mix_continues() {
        let mut mixer = mixer();
        assert!(mixer.add_listener(LEAF_B).expect("listener b"));

        let [mut encoder_a, ..] = encoders();
        let first = encode(&mut encoder_a, TONE_A);
        let third = encode(&mut encoder_a, TONE_A);
        mixer.receive(LEAF_A, 0, first).expect("receive first");
        // Sequence 1 is never delivered.
        mixer.receive(LEAF_A, 2, third).expect("receive third");

        assert!(mixer.pump().expect("pump first").is_some());
        let concealed_tick = mixer.pump().expect("pump concealed");
        assert!(
            concealed_tick.is_some(),
            "a lost frame must be concealed, not stop the mixer"
        );
        let concealed_tick = concealed_tick.expect("concealed tick");
        assert!(
            concealed_tick.local_playback.is_some(),
            "PLC audio must still reach the hub"
        );
        assert!(
            find_listener(&concealed_tick, LEAF_B).is_some(),
            "PLC audio must still reach the other leaf"
        );

        let stats = mixer.source_stats(LEAF_A).expect("source stats");
        assert_eq!(stats.lost, 1);
        assert_eq!(stats.concealed, 1);
        assert_eq!(stats.decode_errors, 0);
        // `played` counts real packets; the concealed frame is reported by
        // `concealed` above, matching the mesh engine's counters.
        assert_eq!(stats.played, 1);

        // The third real frame still plays on the next tick.
        let third_tick = mixer.pump().expect("pump third").expect("third frame");
        assert!(third_tick.local_playback.is_some());
    }

    #[test]
    fn a_corrupt_leaf_frame_is_counted_and_the_other_sources_still_mix() {
        let mut mixer = mixer();
        assert!(mixer.add_listener(LEAF_B).expect("listener b"));

        let [mut encoder_a, mut encoder_b, ..] = encoders();
        let good = encode(&mut encoder_b, TONE_B);
        mixer
            .receive(LEAF_A, 0, vec![0xFF, 0xFF, 0xFF])
            .expect("corrupt");
        mixer.receive(LEAF_B, 0, good).expect("good");

        let frame = mixer.pump().expect("pump").expect("a tick");
        assert_eq!(
            mixer.source_stats(LEAF_A).expect("stats a").decode_errors,
            1
        );
        assert_eq!(mixer.stats().decode_errors, 1);
        // Leaf B still gets a mix (from leaf A's silence is not a source; but
        // the mix for B contains A's failed source? No: only successfully
        // decoded sources contribute, so B gets nothing this tick).
        assert!(find_listener(&frame, LEAF_B).is_none());
        // The hub's local playback is B's good audio.
        let local = frame.local_playback.expect("B's audio reaches the hub");
        assert!(tone_energy(&local, TONE_B) > 1_000.0);

        // One more good tick after the corrupt stream proves A's decoder did
        // not poison the mixer permanently.
        let good_a = encode(&mut encoder_a, TONE_A);
        let good_b = encode(&mut encoder_b, TONE_B);
        mixer.receive(LEAF_A, 1, good_a).expect("receive a");
        mixer.receive(LEAF_B, 1, good_b).expect("receive b");
        let frame = mixer.pump().expect("pump").expect("recovered");
        assert!(find_listener(&frame, LEAF_B).expect("B's mix").sources >= 1);
    }

    #[test]
    fn oversized_and_capacity_frames_are_rejected_without_creating_a_source() {
        let mut mixer = mixer();
        assert_eq!(
            mixer
                .receive(LEAF_A, 0, vec![0u8; MAX_MEDIA_PAYLOAD + 1])
                .expect("oversized is not fatal"),
            ReceiveOutcome::TooLarge
        );
        assert!(mixer.sources().next().is_none());
        assert_eq!(mixer.stats().rejected, 1);

        // Fill the source table, then one more author is refused.
        let payload = vec![0u8; 8];
        for index in 0..MAX_SOURCES {
            let mut author = [0u8; 32];
            author[0] = index as u8;
            let author = AuthorId(author);
            assert_eq!(
                mixer.receive(author, 0, payload.clone()).expect("receive"),
                ReceiveOutcome::Accepted
            );
        }
        assert_eq!(
            mixer
                .receive(AuthorId([0xFE; 32]), 0, payload)
                .expect("receive"),
            ReceiveOutcome::OverCapacity
        );
        assert_eq!(mixer.stats().rejected, 2);
    }

    #[test]
    fn removing_a_listener_or_source_stops_its_output() {
        let mut mixer = mixer();
        assert!(mixer.add_listener(LEAF_A).expect("listener a"));
        assert!(mixer.add_listener(LEAF_B).expect("listener b"));

        let [mut encoder_a, mut encoder_b, ..] = encoders();
        let payload_a = encode(&mut encoder_a, TONE_A);
        let payload_b = encode(&mut encoder_b, TONE_B);
        mixer.receive(LEAF_A, 0, payload_a.clone()).expect("a");
        mixer.receive(LEAF_B, 0, payload_b.clone()).expect("b");

        assert!(mixer.remove_listener(LEAF_A));
        assert!(!mixer.remove_listener(LEAF_A));
        let frame = mixer.pump().expect("pump").expect("tick");
        assert!(find_listener(&frame, LEAF_A).is_none());
        assert!(find_listener(&frame, LEAF_B).is_some());

        assert!(mixer.remove_source(LEAF_B));
        assert!(!mixer.remove_source(LEAF_B));
        assert!(mixer.source_stats(LEAF_B).is_none());

        mixer.receive(LEAF_A, 1, payload_a).expect("a again");
        let frame = mixer.pump().expect("pump").expect("tick");
        assert!(
            find_listener(&frame, LEAF_B).is_some(),
            "B is still a listener"
        );
        assert_eq!(mixer.stats().sources, 1);
    }

    #[test]
    fn a_tick_with_no_ready_frame_produces_nothing() {
        let mut mixer = mixer();
        mixer.add_listener(LEAF_A).expect("listener a");
        assert!(mixer.pump().expect("empty pump").is_none());

        // One packet with the default depth is not primed yet, so it is not
        // ready to play.
        let mut deep = HubMixer::new(HUB, &EngineConfig::default()).expect("mixer");
        deep.add_listener(LEAF_A).expect("listener a");
        let [mut encoder_a, ..] = encoders();
        deep.receive(LEAF_A, 0, encode(&mut encoder_a, TONE_A))
            .expect("receive");
        assert!(deep.pump().expect("priming pump").is_none());
    }

    #[test]
    fn a_new_mixer_rejects_an_invalid_engine_config() {
        let config = EngineConfig {
            frame_ms: 5,
            ..EngineConfig::default()
        };
        assert!(HubMixer::new(HUB, &config).is_err());
    }
}
