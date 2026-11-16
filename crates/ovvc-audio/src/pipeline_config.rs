//! Loading capture and playback pipelines from a TOML file.
//!
//! The file has two top-level tables, `[capture]` and `[playback]`, each
//! matching [`AudioPipeline`]. Both graphs are validated and must have the
//! role their table names imply, so a playback graph cannot be loaded as the
//! microphone chain. Unknown top-level keys are rejected.
//!
//! ```toml
//! [capture]
//! nodes = [...]
//! inputs = ["capture"]
//! output = "encode"
//! role = "Capture"
//!
//! [playback]
//! nodes = [...]
//! inputs = ["decode-0", "decode-1"]
//! output = "playback"
//! role = "Playback"
//! ```

use std::fs;
use std::path::Path;

use ovvc_media::pipeline::{AudioNodeKind, AudioPipeline, MAX_SOURCES, PipelineRole};
use serde::{Deserialize, Serialize};

use crate::error::{AudioError, Result};

/// Both processing graphs for one call.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineConfig {
    /// Microphone chain ending at the encoder.
    pub capture: AudioPipeline,
    /// Remote-audio chain from decoders to the speaker.
    pub playback: AudioPipeline,
}

/// Read, parse and validate a pipeline configuration file.
pub fn load(path: &Path) -> Result<PipelineConfig> {
    let text = fs::read_to_string(path)?;
    let config: PipelineConfig = toml::from_str(&text)
        .map_err(|err| AudioError::Config(format!("invalid pipeline config: {err}")))?;
    config
        .capture
        .validate()
        .map_err(|err| AudioError::Config(format!("capture pipeline is invalid: {err}")))?;
    config
        .playback
        .validate()
        .map_err(|err| AudioError::Config(format!("playback pipeline is invalid: {err}")))?;
    if config.capture.role != PipelineRole::Capture {
        return Err(AudioError::Config(format!(
            "capture pipeline has role {:?}, expected Capture",
            config.capture.role
        )));
    }
    if config.playback.role != PipelineRole::Playback {
        return Err(AudioError::Config(format!(
            "playback pipeline has role {:?}, expected Playback",
            config.playback.role
        )));
    }
    require_executable(&config.capture, PipelineRole::Capture)?;
    require_executable(&config.playback, PipelineRole::Playback)?;
    Ok(config)
}

/// Reject a user graph containing a node the engine does not execute.
///
/// The engine currently runs the capture and playback endpoints, the mixer,
/// and `Gain` nodes. A file asking for echo cancellation, noise suppression,
/// automatic gain, an equaliser, a compressor, a limiter, a soundboard or a
/// plugin would validate structurally but then be silently ignored, so the
/// user would believe a filter was active when the raw signal was encoded.
/// Rejecting it keeps the configuration honest until those nodes ship.
fn require_executable(pipeline: &AudioPipeline, role: PipelineRole) -> Result<()> {
    for node in &pipeline.nodes {
        let executable = match role {
            PipelineRole::Capture => matches!(
                node.kind,
                AudioNodeKind::Capture | AudioNodeKind::Gain | AudioNodeKind::Encode
            ),
            PipelineRole::Playback => matches!(
                node.kind,
                AudioNodeKind::Decode
                    | AudioNodeKind::Mixer
                    | AudioNodeKind::Gain
                    | AudioNodeKind::Playback
            ),
        };
        if !executable {
            return Err(AudioError::Config(format!(
                "{role:?} pipeline node {:?} is {:?}, which the audio engine cannot run yet",
                node.id, node.kind
            )));
        }
    }
    Ok(())
}

/// The capture graph the engine can actually run: capture, gain, encode.
///
/// [`AudioPipeline::capture_default`] declares the full DSP chain for a later
/// phase. Until those nodes are executed, shipping that graph would claim
/// processing the engine does not perform, so the default is the executable
/// subset.
fn executable_capture() -> AudioPipeline {
    use ovvc_media::pipeline::AudioNode;

    let node = |id: &str, kind: AudioNodeKind, inputs: &[&str]| AudioNode {
        id: id.to_string(),
        kind,
        inputs: inputs.iter().map(|input| (*input).to_string()).collect(),
        plugin: None,
        params: Default::default(),
    };
    AudioPipeline {
        nodes: vec![
            node("capture", AudioNodeKind::Capture, &[]),
            node("gain", AudioNodeKind::Gain, &["capture"]),
            node("encode", AudioNodeKind::Encode, &["gain"]),
        ],
        inputs: vec!["capture".to_string()],
        output: "encode".to_string(),
        role: PipelineRole::Capture,
    }
}

/// The graphs ovvc ships with.
pub fn default_pipelines() -> PipelineConfig {
    PipelineConfig {
        capture: executable_capture(),
        playback: AudioPipeline::playback_for(MAX_SOURCES),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_pipelines_validate_with_the_expected_roles() {
        let config = default_pipelines();
        assert_eq!(config.capture.role, PipelineRole::Capture);
        assert_eq!(config.playback.role, PipelineRole::Playback);
        assert_eq!(config.playback.inputs.len(), MAX_SOURCES);
        config.capture.validate().expect("capture default");
        config.playback.validate().expect("playback default");
    }

    #[test]
    fn a_written_config_round_trips_through_toml_and_load() {
        let config = default_pipelines();
        let text = toml::to_string(&config).expect("serialise pipelines");
        let parsed: PipelineConfig = toml::from_str(&text).expect("parse pipelines");
        assert_eq!(parsed, config);

        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("pipelines.toml");
        fs::write(&path, &text).expect("write");
        let loaded = load(&path).expect("load");
        assert_eq!(loaded, config);
    }

    #[test]
    fn dsp_nodes_the_engine_does_not_run_are_rejected() {
        let mut config = PipelineConfig {
            capture: AudioPipeline::capture_default(),
            playback: default_pipelines().playback,
        };
        let text = toml::to_string(&config).expect("serialise");
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("declarative.toml");
        fs::write(&path, text).expect("write");

        let err = load(&path).expect_err("a declared DSP graph is not executed");
        assert!(
            err.to_string().contains("cannot run yet"),
            "unexpected error: {err}"
        );

        // A graph with only the executable nodes still loads.
        config.capture = executable_capture();
        let text = toml::to_string(&config).expect("serialise executable graph");
        fs::write(&path, text).expect("write");
        load(&path).expect("an executable graph must load");
    }

    #[test]
    fn a_role_mismatch_is_rejected() {
        let mut config = default_pipelines();
        config.capture.role = PipelineRole::Playback;
        let text = toml::to_string(&config).expect("serialise");
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("bad-role.toml");
        fs::write(&path, text).expect("write");

        let err = load(&path).expect_err("role mismatch must fail");
        assert!(
            err.to_string().contains("capture pipeline"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn unknown_top_level_keys_are_rejected() {
        let text = "[capture]\nnodes = []\ninputs = []\noutput = \"x\"\nrole = \"Capture\"\n\
                    [playback]\nnodes = []\ninputs = []\noutput = \"y\"\nrole = \"Playback\"\n\
                    [extra]\nkey = 1\n";
        let err = toml::from_str::<PipelineConfig>(text).expect_err("unknown key must fail");
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    #[test]
    fn a_missing_table_is_rejected() {
        let text = "[capture]\nnodes = []\ninputs = []\noutput = \"x\"\nrole = \"Capture\"\n";
        assert!(toml::from_str::<PipelineConfig>(text).is_err());
    }
}
