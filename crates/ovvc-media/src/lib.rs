//! Transport-free voice media for ovvc.
//!
//! This crate owns the parts of a voice call that are independent of the
//! network and the audio device:
//!
//! * [`codec`] — voice profiles and deterministic codec negotiation;
//! * [`call`] — the call membership state machine, mesh/flat-star topology
//!   and per-call codec, expiry and coordinator state;
//! * [`packet`] — the bounded, big-endian media packet;
//! * [`jitter`] — sequence extension, reordering and concealment;
//! * [`level`] — frame RMS and hysteresis-based speaking detection;
//! * [`loopback`] — a null-backend synthetic source, null sink and harness
//!   that drive encode, packet, jitter and decode without a sound card;
//! * [`engine`] — capability descriptors for the shipped and custom engines;
//! * [`pipeline`] — the user-editable, declarative audio graph.
//!
//! A later Opus/cpal stage drives these types but none of them require either.

pub mod call;
pub mod codec;
pub mod engine;
pub mod error;
pub mod jitter;
pub mod level;
pub mod loopback;
pub mod packet;
pub mod pipeline;

pub use call::{
    CallId, CallTopology, MESH_LIMIT, VOICE_MEMBER_TIMEOUT_MS, VoiceAction, VoiceAudioStats,
    VoiceCall, VoiceChange, VoiceMember, VoiceMemberState, VoicePlane, VoiceSnapshot, VoiceUpdate,
    topology_for, topology_for_with_limit,
};
pub use codec::{NegotiatedCodec, ProfileSet, VoiceProfile, negotiate};
pub use engine::{
    AudioCodecInfo, AudioEngineInfo, VideoCodecInfo, VideoEngineInfo, shipped_audio_engine,
    shipped_video_engine,
};
pub use error::{MediaError, Result};
pub use jitter::{
    DEFAULT_DEPTH, JitterBuffer, JitterStats, MAX_BUFFERED, MediaFrame, PushOutcome,
    extend_sequence,
};
pub use level::{
    SPEAKING_HOLD_FRAMES, SPEAKING_OFF_DB, SPEAKING_ON_DB, SpeakingDetector, frame_rms_db,
};
pub use loopback::{
    AudioSink, AudioSource, FrameCodec, LoopbackHarness, LoopbackStats, NullSink, PcmCodec,
    SyntheticSource,
};
pub use packet::{MAX_MEDIA_PAYLOAD, MEDIA_VERSION, MediaKind, MediaPacket};
pub use pipeline::{
    AudioNode, AudioNodeKind, AudioPipeline, MAX_ID_LEN, MAX_INPUTS, MAX_NODES, MAX_SOURCES,
    PipelineError, PipelineRole,
};
