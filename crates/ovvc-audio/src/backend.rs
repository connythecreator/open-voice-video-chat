//! Device backends for capture and playback.
//!
//! The engine talks to one [`AudioBackend`], never to a device API directly.
//! This task ships [`NullBackend`], which drives the synthetic source and null
//! sink from `ovvc-media` so the whole engine can run in CI. A PipeWire
//! backend implements the same trait later without touching the engine.

use ovvc_media::MediaError;
use ovvc_media::loopback::{AudioSink, AudioSource, NullSink, SyntheticSource};
use ovvc_media::packet::MAX_MEDIA_PAYLOAD;
use ovvc_media::pipeline::{AudioNodeKind, AudioPipeline, PipelineRole};

use crate::error::{AudioError, Result};

/// The tone [`NullBackend`] generates by default, in Hz.
pub const DEFAULT_TONE_HZ: f32 = 1_000.0;

/// Settings shared by the codec, the jitter buffer and the device backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineConfig {
    /// Samples per second per channel. Only 48 kHz is supported.
    pub sample_rate: u32,
    /// Length of one Opus frame in milliseconds: 10, 20 or 40.
    pub frame_ms: u16,
    /// Interleaved channels. Only mono is supported.
    pub channels: u8,
    /// Encoder target in kilobits per second.
    pub bitrate_kbps: u16,
    /// Packets the jitter buffer collects before it starts playout.
    pub jitter_depth: usize,
    /// The microphone processing graph, ending at the encoder.
    pub capture_pipeline: AudioPipeline,
    /// The per-peer playback graph, from decoders through the mixer to the
    /// speaker.
    pub playback_pipeline: AudioPipeline,
}

impl Default for EngineConfig {
    fn default() -> Self {
        let pipelines = crate::pipeline_config::default_pipelines();
        Self {
            sample_rate: 48_000,
            frame_ms: 20,
            channels: 1,
            bitrate_kbps: 16,
            jitter_depth: 3,
            capture_pipeline: pipelines.capture,
            playback_pipeline: pipelines.playback,
        }
    }
}

impl EngineConfig {
    /// Reject a config the codec or backend cannot honour.
    pub fn validate(&self) -> Result<()> {
        if self.sample_rate != 48_000 {
            return Err(AudioError::Config(format!(
                "the audio engine runs at 48 kHz, got {} Hz",
                self.sample_rate
            )));
        }
        if !matches!(self.frame_ms, 10 | 20 | 40) {
            return Err(AudioError::Config(format!(
                "unsupported frame size {} ms, expected 10, 20 or 40",
                self.frame_ms
            )));
        }
        if self.channels != 1 {
            return Err(AudioError::Config(format!(
                "only mono is supported, got {} channels",
                self.channels
            )));
        }
        if self.bitrate_kbps == 0 {
            return Err(AudioError::Config(
                "bitrate must be greater than zero".to_string(),
            ));
        }
        // A media packet holds at most 1200 bytes, which is 9600 bits. Since
        // bitrate_kbps is bits per millisecond, the frame length must stay
        // below that or Opus will produce packets the transport cannot carry.
        let max_bitrate_kbps = (MAX_MEDIA_PAYLOAD * 8) / usize::from(self.frame_ms);
        if usize::from(self.bitrate_kbps) > max_bitrate_kbps {
            return Err(AudioError::Config(format!(
                "{} kbps at {} ms can exceed the {MAX_MEDIA_PAYLOAD}-byte media payload \
                 (at most {max_bitrate_kbps} kbps)",
                self.bitrate_kbps, self.frame_ms
            )));
        }
        if !(1..=16).contains(&self.jitter_depth) {
            return Err(AudioError::Config(format!(
                "jitter depth {} is outside 1..=16",
                self.jitter_depth
            )));
        }
        validate_pipeline(&self.capture_pipeline, PipelineRole::Capture, "capture")?;
        validate_pipeline(&self.playback_pipeline, PipelineRole::Playback, "playback")?;
        Ok(())
    }

    /// Interleaved samples in one frame at the configured rate and channels.
    pub fn frame_samples(&self) -> usize {
        (self.sample_rate as usize / 1_000)
            * usize::from(self.frame_ms)
            * usize::from(self.channels)
    }
}

/// Validate one processing graph and its role.
fn validate_pipeline(pipeline: &AudioPipeline, role: PipelineRole, label: &str) -> Result<()> {
    pipeline
        .validate()
        .map_err(|err| AudioError::Config(format!("{label} pipeline is invalid: {err}")))?;
    if pipeline.role != role {
        return Err(AudioError::Config(format!(
            "{label} pipeline has role {:?}, expected {role:?}",
            pipeline.role
        )));
    }
    for node in &pipeline.nodes {
        if node.kind != AudioNodeKind::Gain {
            continue;
        }
        if let Some(raw) = node.params.get("gain") {
            let gain = raw.trim().parse::<f32>().map_err(|_| {
                AudioError::Config(format!(
                    "pipeline node {:?} has a non-numeric gain {raw:?}",
                    node.id
                ))
            })?;
            if !gain.is_finite() || gain < 0.0 {
                return Err(AudioError::Config(format!(
                    "pipeline node {:?} has gain {gain}, expected a finite value >= 0",
                    node.id
                )));
            }
        }
    }
    Ok(())
}

/// The linear gain a pipeline's first `Gain` node asks for.
///
/// `params["gain"]` is parsed as a linear multiplier and defaults to 1.0 when
/// the node is missing or has no setting. Configuration validation has already
/// rejected non-finite and negative values, so a failed parse here is only
/// possible for a hand-built engine and falls back to unity.
pub(crate) fn pipeline_gain(pipeline: &AudioPipeline) -> f32 {
    for node in &pipeline.nodes {
        if node.kind == AudioNodeKind::Gain {
            return node
                .params
                .get("gain")
                .and_then(|raw| raw.trim().parse::<f32>().ok())
                .filter(|gain| gain.is_finite() && *gain >= 0.0)
                .unwrap_or(1.0);
        }
    }
    1.0
}

/// Counters a backend reports after it has run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackendStats {
    /// Frames handed to the engine by [`AudioBackend::read_capture`].
    pub capture_frames: u64,
    /// Frames accepted by [`AudioBackend::write_playback`].
    pub playback_frames: u64,
    /// Device underruns or overruns. The null backend never has any.
    pub xruns: u64,
    /// Estimated device latency in milliseconds.
    pub latency_ms: u64,
}

/// A capture and playback device the engine can drive.
pub trait AudioBackend {
    /// Open or reset the device for `config`.
    fn start(&mut self, config: &EngineConfig) -> Result<()>;

    /// Read the next capture frame, or `None` when the source is exhausted.
    fn read_capture(&mut self) -> Result<Option<Vec<i16>>>;

    /// Send one frame to the speaker.
    fn write_playback(&mut self, samples: &[i16]) -> Result<()>;

    /// Pause or resume capture. The engine pauses capture while muted so the
    /// backend does not accumulate microphone audio it will never read.
    ///
    /// The default is a no-op, which is correct for a backend whose source
    /// only advances when [`AudioBackend::read_capture`] is called.
    fn set_capture_active(&mut self, _active: bool) -> Result<()> {
        Ok(())
    }

    /// The counters accumulated since the last start.
    fn stats(&self) -> BackendStats;

    /// Close the device. The null backend has nothing to close.
    fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}

/// A backend with no device attached: a synthetic source and a measuring sink.
pub struct NullBackend {
    config: EngineConfig,
    source: SyntheticSource,
    sink: NullSink,
    stats: BackendStats,
    tone_hz: f32,
}

impl NullBackend {
    /// Build a null backend that emits a 1 kHz tone.
    pub fn new(config: EngineConfig) -> Self {
        Self::with_tone(config, DEFAULT_TONE_HZ)
    }

    /// Build a null backend that emits `tone_hz`. A frequency of zero is a
    /// silent source, which is useful for push-to-talk tests.
    pub fn with_tone(config: EngineConfig, tone_hz: f32) -> Self {
        let source = Self::source_for(&config, tone_hz);
        Self {
            config,
            source,
            sink: NullSink::new(),
            stats: BackendStats::default(),
            tone_hz,
        }
    }

    /// Build a silent null backend.
    pub fn silent(config: EngineConfig) -> Self {
        Self::with_tone(config, 0.0)
    }

    /// The sink, so a test can read the measured level.
    pub fn sink(&self) -> &NullSink {
        &self.sink
    }

    fn source_for(config: &EngineConfig, tone_hz: f32) -> SyntheticSource {
        SyntheticSource::sine(
            config.sample_rate,
            config.channels,
            config.frame_ms,
            tone_hz,
        )
    }
}

impl AudioBackend for NullBackend {
    fn start(&mut self, config: &EngineConfig) -> Result<()> {
        config.validate()?;
        self.config = config.clone();
        self.source = Self::source_for(config, self.tone_hz);
        self.sink.reset();
        self.stats = BackendStats::default();
        Ok(())
    }

    fn read_capture(&mut self) -> Result<Option<Vec<i16>>> {
        match self.source.read().map_err(backend_error)? {
            Some(frame) => {
                self.stats.capture_frames = self.stats.capture_frames.saturating_add(1);
                Ok(Some(frame))
            }
            None => Ok(None),
        }
    }

    fn write_playback(&mut self, samples: &[i16]) -> Result<()> {
        if samples.len() != self.config.frame_samples() {
            return Err(AudioError::Backend(format!(
                "playback frame is {} samples, expected {}",
                samples.len(),
                self.config.frame_samples()
            )));
        }
        self.sink.write(samples).map_err(backend_error)?;
        self.stats.playback_frames = self.stats.playback_frames.saturating_add(1);
        Ok(())
    }

    fn stats(&self) -> BackendStats {
        BackendStats {
            capture_frames: self.stats.capture_frames,
            playback_frames: self.sink.frames_written(),
            xruns: 0,
            latency_ms: u64::from(self.config.frame_ms),
        }
    }
}

/// Which device backend a call should use.
///
/// [`BackendKind::Auto`] may fall back to the null backend with a name of
/// `"null"`; an explicit [`BackendKind::PipeWire`] must open the device or
/// fail, so a caller can tell the difference between "no device requested" and
/// "the device is broken".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendKind {
    /// Use PipeWire when it is compiled in and starts, otherwise null.
    #[default]
    Auto,
    /// A hardware-free backend, silent in production and measured by tests.
    Null,
    /// The native PipeWire capture and playback device.
    PipeWire,
}

impl BackendKind {
    /// Parse a backend name, case-insensitively.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "null" => Some(Self::Null),
            "pipewire" => Some(Self::PipeWire),
            _ => None,
        }
    }

    /// The canonical name, as reported to the daemon and the UI.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Null => "null",
            Self::PipeWire => "pipewire",
        }
    }
}

/// Whether this build compiled the native PipeWire backend.
pub fn pipewire_compiled() -> bool {
    cfg!(all(target_os = "linux", feature = "pipewire"))
}

/// The device backends this build compiled in, in a stable order with `null`
/// first. The daemon reports this so a user can see that a default build has
/// no live device before a call rather than after a silent one.
pub fn compiled_backends() -> Vec<&'static str> {
    let mut backends = vec!["null"];
    if pipewire_compiled() {
        backends.push("pipewire");
    }
    backends
}

/// The backend a call will try to open.
///
/// [`BackendKind::Auto`] cannot be resolved without opening a device: on a
/// PipeWire build it tries the device and silently falls back to `null` when
/// the server or device is not reachable. Reporting `pipewire` here would
/// claim a live device the service may never open, so an auto build reports
/// `auto` and lets the per-call stats name the backend that actually started.
/// On a build without the feature, auto can only ever be `null`.
pub fn resolved_backend(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Null => "null",
        BackendKind::PipeWire => "pipewire",
        BackendKind::Auto if pipewire_compiled() => "auto",
        BackendKind::Auto => "null",
    }
}

/// Refuse an explicitly requested backend this build cannot open.
///
/// [`BackendKind::Auto`] is always allowed because it has a documented
/// fallback. An explicit `pipewire` on a build without the feature is a
/// configuration mistake, and starting a call only to find no audio service is
/// worse than refusing it at the daemon boundary.
pub fn validate_backend(kind: BackendKind) -> Result<()> {
    match kind {
        BackendKind::PipeWire if !pipewire_compiled() => Err(AudioError::Backend(
            "built without the pipewire feature; rebuild with `--features pipewire` or set \
             [audio] backend = \"null\""
                .to_string(),
        )),
        _ => Ok(()),
    }
}

/// The null backend a service opens.
///
/// Without a device the source is silent, because a tone belongs to a test
/// fixture rather than a call. A test starts the process with
/// `OVVC_AUDIO_TEST_TONE=<hz>` to make the synthetic source audible, which is
/// how the two-daemon audio test proves a packet crossed the transport and
/// decoded.
fn null_backend(config: &EngineConfig) -> Box<dyn AudioBackend> {
    let tone = std::env::var("OVVC_AUDIO_TEST_TONE")
        .ok()
        .and_then(|value| value.trim().parse::<f32>().ok())
        .filter(|hz| hz.is_finite() && *hz > 0.0);
    match tone {
        Some(hz) => Box::new(NullBackend::with_tone(config.clone(), hz)),
        None => Box::new(NullBackend::silent(config.clone())),
    }
}

/// Build and start a backend of the requested kind.
///
/// Returns the backend and the name of the device actually chosen, so a
/// caller can report when [`BackendKind::Auto`] fell back to `"null"`. The
/// null backend is silent unless `OVVC_AUDIO_TEST_TONE` asks for a tone.
pub fn build_backend(
    kind: BackendKind,
    config: &EngineConfig,
) -> Result<(Box<dyn AudioBackend>, &'static str)> {
    match kind {
        BackendKind::Null => Ok((null_backend(config), "null")),
        BackendKind::PipeWire => pipewire_backend(config),
        BackendKind::Auto => Ok(auto_backend(config)),
    }
}

#[cfg(all(target_os = "linux", feature = "pipewire"))]
fn pipewire_backend(config: &EngineConfig) -> Result<(Box<dyn AudioBackend>, &'static str)> {
    let mut backend = crate::pipewire::PipeWireBackend::new();
    backend.start(config)?;
    Ok((Box::new(backend), "pipewire"))
}

#[cfg(not(all(target_os = "linux", feature = "pipewire")))]
fn pipewire_backend(_config: &EngineConfig) -> Result<(Box<dyn AudioBackend>, &'static str)> {
    Err(AudioError::Backend(
        "built without the pipewire feature".to_string(),
    ))
}

#[cfg(all(target_os = "linux", feature = "pipewire"))]
fn auto_backend(config: &EngineConfig) -> (Box<dyn AudioBackend>, &'static str) {
    match pipewire_backend(config) {
        Ok(pair) => pair,
        Err(_) => (null_backend(config), "null"),
    }
}

#[cfg(not(all(target_os = "linux", feature = "pipewire")))]
fn auto_backend(config: &EngineConfig) -> (Box<dyn AudioBackend>, &'static str) {
    (null_backend(config), "null")
}

fn backend_error(err: MediaError) -> AudioError {
    AudioError::Backend(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ovvc_media::pipeline::MAX_SOURCES;

    #[test]
    fn the_default_config_values_are_the_documented_ones() {
        let config = EngineConfig::default();
        assert_eq!(config.sample_rate, 48_000);
        assert_eq!(config.frame_ms, 20);
        assert_eq!(config.channels, 1);
        assert_eq!(config.bitrate_kbps, 16);
        assert_eq!(config.jitter_depth, 3);
        assert_eq!(config.frame_samples(), 960);
        config.validate().expect("the default config is valid");
    }

    #[test]
    fn invalid_configs_are_rejected() {
        let config = EngineConfig {
            sample_rate: 44_100,
            ..EngineConfig::default()
        };
        assert!(matches!(config.validate(), Err(AudioError::Config(_))));

        let config = EngineConfig {
            frame_ms: 5,
            ..EngineConfig::default()
        };
        assert!(matches!(config.validate(), Err(AudioError::Config(_))));

        let config = EngineConfig {
            channels: 2,
            ..EngineConfig::default()
        };
        assert!(matches!(config.validate(), Err(AudioError::Config(_))));

        let config = EngineConfig {
            jitter_depth: 0,
            ..EngineConfig::default()
        };
        assert!(matches!(config.validate(), Err(AudioError::Config(_))));
    }

    #[test]
    fn a_bitrate_that_would_overflow_the_media_payload_is_rejected() {
        // 1200 bytes per frame is 9600 bits, so the cap is 9600 / frame_ms kbps.
        let config = EngineConfig {
            bitrate_kbps: 480,
            frame_ms: 20,
            ..EngineConfig::default()
        };
        config.validate().expect("480 kbps fits a 20 ms frame");

        let config = EngineConfig {
            bitrate_kbps: 510,
            frame_ms: 20,
            ..EngineConfig::default()
        };
        assert!(
            matches!(config.validate(), Err(AudioError::Config(_))),
            "510 kbps at 20 ms can emit packets above the 1200-byte limit"
        );

        let config = EngineConfig {
            bitrate_kbps: 241,
            frame_ms: 40,
            ..EngineConfig::default()
        };
        assert!(matches!(config.validate(), Err(AudioError::Config(_))));
    }

    #[test]
    fn the_null_backend_counts_capture_and_playback() {
        let config = EngineConfig::default();
        let mut backend = NullBackend::new(config.clone());
        backend.start(&config).expect("start");

        let frame = backend.read_capture().expect("read").expect("frame");
        assert_eq!(frame.len(), config.frame_samples());
        backend.write_playback(&frame).expect("write");

        let stats = backend.stats();
        assert_eq!(stats.capture_frames, 1);
        assert_eq!(stats.playback_frames, 1);
        assert_eq!(stats.xruns, 0);
        assert_eq!(stats.latency_ms, u64::from(config.frame_ms));
        backend.stop().expect("stop");
    }

    #[test]
    fn start_resets_the_counters() {
        let config = EngineConfig::default();
        let mut backend = NullBackend::new(config.clone());
        backend.start(&config).expect("start");
        backend.read_capture().expect("read");
        backend
            .write_playback(&vec![0i16; config.frame_samples()])
            .expect("write");
        assert_eq!(backend.stats().capture_frames, 1);

        backend.start(&config).expect("restart");
        assert_eq!(backend.stats().capture_frames, 0);
        assert_eq!(backend.stats().playback_frames, 0);
    }

    #[test]
    fn the_default_config_carries_both_pipelines() {
        let config = EngineConfig::default();
        assert_eq!(config.capture_pipeline.role, PipelineRole::Capture);
        assert_eq!(config.playback_pipeline.role, PipelineRole::Playback);
        assert_eq!(config.playback_pipeline.inputs.len(), MAX_SOURCES);
        config.validate().expect("the default pipelines validate");
    }

    #[test]
    fn a_pipeline_with_the_wrong_role_is_rejected() {
        let mut config = EngineConfig::default();
        config.capture_pipeline.role = PipelineRole::Playback;
        assert!(matches!(config.validate(), Err(AudioError::Config(_))));

        let mut config = EngineConfig::default();
        config.playback_pipeline.role = PipelineRole::Capture;
        assert!(matches!(config.validate(), Err(AudioError::Config(_))));
    }

    #[test]
    fn a_bad_gain_value_is_rejected_by_config_validation() {
        let mut config = EngineConfig::default();
        {
            let gain = config
                .playback_pipeline
                .nodes
                .iter_mut()
                .find(|node| node.kind == AudioNodeKind::Gain)
                .expect("the default playback graph has a gain");
            gain.params
                .insert("gain".to_string(), "not-a-number".to_string());
        }
        assert!(matches!(config.validate(), Err(AudioError::Config(_))));

        {
            let gain = config
                .playback_pipeline
                .nodes
                .iter_mut()
                .find(|node| node.kind == AudioNodeKind::Gain)
                .expect("the default playback graph has a gain");
            gain.params.insert("gain".to_string(), "-1".to_string());
        }
        assert!(matches!(config.validate(), Err(AudioError::Config(_))));
    }

    #[test]
    fn backend_kind_parses_and_round_trips_its_names() {
        for (name, kind) in [
            ("auto", BackendKind::Auto),
            ("null", BackendKind::Null),
            ("pipewire", BackendKind::PipeWire),
            ("  AUTO  ", BackendKind::Auto),
            (" Null ", BackendKind::Null),
            ("PipeWire", BackendKind::PipeWire),
        ] {
            assert_eq!(BackendKind::parse(name), Some(kind), "{name:?}");
        }
        assert_eq!(BackendKind::parse("alsa"), None);
        assert_eq!(BackendKind::parse(""), None);

        for kind in [BackendKind::Auto, BackendKind::Null, BackendKind::PipeWire] {
            assert_eq!(BackendKind::parse(kind.as_str()), Some(kind));
        }
    }

    #[test]
    fn null_builds_a_silent_backend_and_auto_prefers_it_without_pipewire() {
        let config = EngineConfig::default();
        let (_backend, name) = build_backend(BackendKind::Null, &config).expect("null backend");
        assert_eq!(name, "null");

        let (_backend, auto_name) =
            build_backend(BackendKind::Auto, &config).expect("auto backend");
        assert!(
            matches!(auto_name, "null" | "pipewire"),
            "unexpected auto backend {auto_name}"
        );
        #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
        assert_eq!(auto_name, "null");
    }

    #[test]
    fn the_compiled_backend_report_matches_the_feature_set() {
        let backends = compiled_backends();
        assert_eq!(backends.first(), Some(&"null"), "null must always compile");
        assert_eq!(resolved_backend(BackendKind::Null), "null");
        assert!(validate_backend(BackendKind::Null).is_ok());
        assert!(validate_backend(BackendKind::Auto).is_ok());

        if pipewire_compiled() {
            assert!(backends.contains(&"pipewire"));
            assert_eq!(resolved_backend(BackendKind::PipeWire), "pipewire");
            assert_eq!(
                resolved_backend(BackendKind::Auto),
                "auto",
                "auto must not claim a device it may fall back from"
            );
            assert!(validate_backend(BackendKind::PipeWire).is_ok());
        } else {
            assert!(!backends.contains(&"pipewire"));
            assert_eq!(resolved_backend(BackendKind::Auto), "null");
            let error = validate_backend(BackendKind::PipeWire)
                .expect_err("an explicit pipewire backend must be refused");
            assert!(
                error.to_string().contains("built without the pipewire feature"),
                "unexpected error: {error}"
            );
        }
    }

    #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
    #[test]
    fn explicit_pipewire_without_the_feature_is_an_error() {
        let err = match build_backend(BackendKind::PipeWire, &EngineConfig::default()) {
            Ok(_) => panic!("pipewire must not silently fall back"),
            Err(err) => err,
        };
        assert!(
            err.to_string()
                .contains("built without the pipewire feature"),
            "unexpected error: {err}"
        );
    }
}
