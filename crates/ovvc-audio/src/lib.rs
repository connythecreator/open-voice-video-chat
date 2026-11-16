//! Opus coding, device backends and the voice media engine for ovvc.
//!
//! The transport-free media types live in `ovvc-media`. This crate joins them
//! to a real codec and a capture/playback device:
//!
//! * [`codec`] — 48 kHz mono Opus encoding, with one [`OpusDecoder`] per peer;
//! * [`backend`] — [`EngineConfig`], [`AudioBackend`], [`BackendKind`] and the
//!   [`NullBackend`] used by tests and the CI probe;
//! * [`engine`] — [`AudioEngine`], which captures, encodes, jitter-buffers one
//!   stream per remote author, decodes, mixes and plays, and applies mute,
//!   deafen and push-to-talk;
//! * [`hub_mixer`] — [`HubMixer`], the star-hub path that decodes every leaf
//!   once, mixes per listener without that listener's own stream, and encodes
//!   a separate downlink for each leaf;
//! * [`service`] — a threaded call service with a command channel and
//!   broadcast events;
//! * [`pipeline_config`] — loading the capture and playback graphs from TOML.
//!
//! The PipeWire backend sits behind the same [`AudioBackend`] trait and is
//! compiled only on Linux with the `pipewire` cargo feature, so the default
//! workspace build needs no device headers. Because it is `!Send`, the service
//! builds and drives it inside a dedicated OS thread.
//!
//! [`FrameCodec`]: ovvc_media::loopback::FrameCodec

pub mod backend;
pub mod codec;
pub mod engine;
pub mod error;
pub mod hub_mixer;
pub mod pipeline_config;
pub mod service;

#[cfg(all(target_os = "linux", feature = "pipewire"))]
pub mod pipewire;

pub use backend::{
    AudioBackend, BackendKind, BackendStats, EngineConfig, NullBackend, build_backend,
    compiled_backends, pipewire_compiled, resolved_backend, validate_backend,
};
pub use codec::{OpusCodec, OpusDecoder, OpusEncoder};
pub use engine::{AudioEngine, EngineStats};
pub use error::{AudioError, Result};
pub use hub_mixer::{
    HubMixFrame, HubMixer, HubMixerStats, ListenerFrame, ListenerStats, ReceiveOutcome, SourceStats,
};
pub use pipeline_config::{PipelineConfig, default_pipelines};
#[cfg(all(target_os = "linux", feature = "pipewire"))]
pub use pipewire::PipeWireBackend;
pub use service::{
    AudioCallConfig, AudioCommand, AudioEvent, AudioEventEnvelope, AudioHandle, AudioStats, spawn,
};
