//! A declarative audio processing graph.
//!
//! The shipped audio engine is not a fixed chain: the GUI edits a
//! [`AudioPipeline`], a list of [`AudioNode`]s wired by name, and the crate
//! validates it before anything is run. A node names its inputs rather than
//! holding pointers, so the same graph survives serialisation, editing and a
//! restart. The graph is deliberately tiny and transport-free; the daemon is
//! responsible for turning a validated order into real audio.
//!
//! A pipeline has one or more input roots and a single output. Capture graphs
//! have exactly one microphone root; playback graphs use one decoder root per
//! remote peer, all feeding a shared mixer.
//!
//! [`AudioPipeline::capture_default`], [`AudioPipeline::playback_default`] and
//! [`AudioPipeline::playback_for`] are the chains ovvc ships with: microphone
//! capture through echo cancellation, noise suppression and the usual dynamics
//! chain into the encoder, and decoded remote audio through the mixer to the
//! speaker. Echo cancellation runs before noise suppression because the
//! suppressor would otherwise alter the linear echo path that the canceller
//! models.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The most nodes one pipeline may contain.
pub const MAX_NODES: usize = 64;

/// The most inputs a non-mixer node may take. A mixer may take [`MAX_SOURCES`].
pub const MAX_INPUTS: usize = 8;

/// The longest node id, in bytes.
pub const MAX_ID_LEN: usize = 64;

/// The most independent sources a playback pipeline may mix.
pub const MAX_SOURCES: usize = 16;

/// Which side of a call a pipeline belongs to.
///
/// The role fixes the kind of every input root and of the output node, so a
/// capture graph cannot end in `Playback` and a playback graph cannot start
/// from a microphone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PipelineRole {
    /// Microphone capture, ending at an encoder.
    Capture,
    /// Remote audio playback, starting at one decoder per peer.
    Playback,
}

/// What an [`AudioNode`] does to the signal passing through it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AudioNodeKind {
    /// Reads from a microphone or other capture device.
    Capture,
    /// Writes to a speaker or other playback device.
    Playback,
    /// Scales the signal.
    Gain,
    /// Suppresses background noise.
    NoiseSuppression,
    /// Cancels acoustic echo from the speakers.
    EchoCancellation,
    /// Keeps the level steady automatically.
    AutomaticGain,
    /// Adjusts frequency bands.
    Equaliser,
    /// Reduces dynamic range.
    Compressor,
    /// Prevents the signal from clipping.
    Limiter,
    /// Sums several signals into one.
    Mixer,
    /// Mixes a soundboard clip into the signal.
    Soundboard,
    /// Encodes audio for the wire.
    Encode,
    /// Decodes audio from the wire.
    Decode,
    /// Delegates to an out-of-process or WASM plugin.
    Plugin,
}

/// One node in an [`AudioPipeline`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioNode {
    /// Unique node id.
    pub id: String,
    /// What the node does.
    pub kind: AudioNodeKind,
    /// Ids of the nodes feeding this one, in order.
    pub inputs: Vec<String>,
    /// Plugin name, required when `kind` is [`AudioNodeKind::Plugin`].
    pub plugin: Option<String>,
    /// Free-form settings interpreted by the node implementation.
    pub params: BTreeMap<String, String>,
}

impl AudioNode {
    fn simple(id: &str, kind: AudioNodeKind, inputs: &[&str]) -> Self {
        Self {
            id: id.to_string(),
            kind,
            inputs: inputs.iter().map(|input| (*input).to_string()).collect(),
            plugin: None,
            params: BTreeMap::new(),
        }
    }
}

/// A declarative audio graph with one or more input roots and one output node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioPipeline {
    /// Every node in declaration order.
    pub nodes: Vec<AudioNode>,
    /// The source nodes, which must have no inputs of their own.
    pub inputs: Vec<String>,
    /// The sink node the caller wants the result at.
    pub output: String,
    /// Whether this graph captures the microphone or plays remote audio.
    pub role: PipelineRole,
}

impl AudioPipeline {
    /// The default microphone chain:
    /// capture -> echo cancellation -> noise suppression -> automatic gain ->
    /// equaliser -> compressor -> soundboard -> limiter -> encode.
    pub fn capture_default() -> Self {
        Self {
            nodes: vec![
                AudioNode::simple("capture", AudioNodeKind::Capture, &[]),
                AudioNode::simple(
                    "echo-cancellation",
                    AudioNodeKind::EchoCancellation,
                    &["capture"],
                ),
                AudioNode::simple(
                    "noise-suppression",
                    AudioNodeKind::NoiseSuppression,
                    &["echo-cancellation"],
                ),
                AudioNode::simple(
                    "automatic-gain",
                    AudioNodeKind::AutomaticGain,
                    &["noise-suppression"],
                ),
                AudioNode::simple("equaliser", AudioNodeKind::Equaliser, &["automatic-gain"]),
                AudioNode::simple("compressor", AudioNodeKind::Compressor, &["equaliser"]),
                AudioNode::simple("soundboard", AudioNodeKind::Soundboard, &["compressor"]),
                AudioNode::simple("limiter", AudioNodeKind::Limiter, &["soundboard"]),
                AudioNode::simple("encode", AudioNodeKind::Encode, &["limiter"]),
            ],
            inputs: vec!["capture".to_string()],
            output: "encode".to_string(),
            role: PipelineRole::Capture,
        }
    }

    /// The default speaker chain for one remote peer:
    /// decode -> mixer -> gain -> playback.
    pub fn playback_default() -> Self {
        Self {
            nodes: vec![
                AudioNode::simple("decode", AudioNodeKind::Decode, &[]),
                AudioNode::simple("mixer", AudioNodeKind::Mixer, &["decode"]),
                AudioNode::simple("gain", AudioNodeKind::Gain, &["mixer"]),
                AudioNode::simple("playback", AudioNodeKind::Playback, &["gain"]),
            ],
            inputs: vec!["decode".to_string()],
            output: "playback".to_string(),
            role: PipelineRole::Playback,
        }
    }

    /// A playback chain with one decoder per remote source.
    ///
    /// `sources` decoders named `decode-0` to `decode-(sources - 1)` all feed a
    /// single mixer, then gain and playback. The decoder ids are the pipeline
    /// inputs in declaration order. A count outside `1..=MAX_SOURCES` builds a
    /// pipeline that [`AudioPipeline::validate`] rejects.
    pub fn playback_for(sources: usize) -> Self {
        let source_ids: Vec<String> = (0..sources)
            .map(|index| format!("decode-{index}"))
            .collect();

        let mut nodes: Vec<AudioNode> = source_ids
            .iter()
            .map(|id| AudioNode::simple(id, AudioNodeKind::Decode, &[]))
            .collect();
        let mixer_inputs: Vec<&str> = source_ids.iter().map(String::as_str).collect();
        nodes.push(AudioNode::simple(
            "mixer",
            AudioNodeKind::Mixer,
            &mixer_inputs,
        ));
        nodes.push(AudioNode::simple("gain", AudioNodeKind::Gain, &["mixer"]));
        nodes.push(AudioNode::simple(
            "playback",
            AudioNodeKind::Playback,
            &["gain"],
        ));

        Self {
            nodes,
            inputs: source_ids,
            output: "playback".to_string(),
            role: PipelineRole::Playback,
        }
    }

    /// Check every structural rule before the graph is run or saved.
    pub fn validate(&self) -> Result<(), PipelineError> {
        if self.nodes.is_empty() {
            return Err(PipelineError::Empty);
        }
        if self.nodes.len() > MAX_NODES {
            return Err(PipelineError::TooManyNodes(self.nodes.len()));
        }

        let mut ids = BTreeSet::new();
        for node in &self.nodes {
            if !valid_id(&node.id) {
                return Err(PipelineError::InvalidId(node.id.clone()));
            }
            if !ids.insert(node.id.as_str()) {
                return Err(PipelineError::DuplicateId(node.id.clone()));
            }
        }

        // The role fixes how many roots there are and what kinds they must be.
        if self.inputs.is_empty() {
            return Err(PipelineError::NoInputs);
        }
        match self.role {
            PipelineRole::Capture if self.inputs.len() != 1 => {
                return Err(PipelineError::WrongInputCount {
                    expected: 1,
                    got: self.inputs.len(),
                });
            }
            PipelineRole::Playback if self.inputs.len() > MAX_SOURCES => {
                return Err(PipelineError::TooManySources(self.inputs.len()));
            }
            _ => {}
        }

        for node in &self.nodes {
            let input_limit = match node.kind {
                AudioNodeKind::Mixer => MAX_SOURCES,
                _ => MAX_INPUTS,
            };
            if node.inputs.len() > input_limit {
                return if node.kind == AudioNodeKind::Mixer {
                    Err(PipelineError::TooManySources(node.inputs.len()))
                } else {
                    Err(PipelineError::TooManyInputs {
                        node: node.id.clone(),
                        count: node.inputs.len(),
                    })
                };
            }
            if node.kind == AudioNodeKind::Mixer && node.inputs.is_empty() {
                return Err(PipelineError::MixerHasNoInputs(node.id.clone()));
            }
            for input in &node.inputs {
                if !ids.contains(input.as_str()) {
                    return Err(PipelineError::UnknownInput {
                        node: node.id.clone(),
                        input: input.clone(),
                    });
                }
            }
            if node.kind == AudioNodeKind::Plugin
                && node
                    .plugin
                    .as_deref()
                    .is_none_or(|plugin| plugin.trim().is_empty())
            {
                return Err(PipelineError::MissingPlugin(node.id.clone()));
            }
        }

        for input in &self.inputs {
            if !ids.contains(input.as_str()) {
                return Err(PipelineError::UnknownEndpoint(input.clone()));
            }
        }
        if !ids.contains(self.output.as_str()) {
            return Err(PipelineError::UnknownEndpoint(self.output.clone()));
        }

        // Ordering first, so a cycle is reported as a cycle even though the
        // input node in a two-node cycle necessarily has inputs.
        self.topological_order()?;

        for input in &self.inputs {
            let node = self.node(input).expect("input existence was checked above");
            if !node.inputs.is_empty() {
                return Err(PipelineError::InputHasInputs(input.clone()));
            }
            let expected = match self.role {
                PipelineRole::Capture => AudioNodeKind::Capture,
                PipelineRole::Playback => AudioNodeKind::Decode,
            };
            if node.kind != expected {
                return Err(PipelineError::WrongEndpointKind {
                    node: input.clone(),
                    expected,
                    got: node.kind,
                });
            }
        }

        let reachable = self.reachable_from(&self.inputs);
        for node in &self.nodes {
            if !reachable.contains(&node.id) {
                return Err(PipelineError::Unreachable(node.id.clone()));
            }
        }

        let expected = match self.role {
            PipelineRole::Capture => AudioNodeKind::Encode,
            PipelineRole::Playback => AudioNodeKind::Playback,
        };
        let output = self
            .node(&self.output)
            .expect("output existence was checked above");
        if output.kind != expected {
            return Err(PipelineError::WrongEndpointKind {
                node: self.output.clone(),
                expected,
                got: output.kind,
            });
        }

        Ok(())
    }

    /// A deterministic topological order.
    ///
    /// Every root (a node with no inputs, including every declared input) seeds
    /// Kahn's algorithm in declaration order. When several nodes are ready at
    /// once the one declared earliest wins, so the same pipeline always sorts
    /// the same way. A graph with a cycle is rejected with the ids that could
    /// not be placed.
    pub fn topological_order(&self) -> Result<Vec<String>, PipelineError> {
        if self.nodes.is_empty() {
            return Err(PipelineError::Empty);
        }

        let mut index: BTreeMap<&str, usize> = BTreeMap::new();
        for (position, node) in self.nodes.iter().enumerate() {
            if index.insert(node.id.as_str(), position).is_some() {
                return Err(PipelineError::DuplicateId(node.id.clone()));
            }
        }

        let mut indegree = vec![0usize; self.nodes.len()];
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); self.nodes.len()];
        for (position, node) in self.nodes.iter().enumerate() {
            for input in &node.inputs {
                let Some(&source) = index.get(input.as_str()) else {
                    return Err(PipelineError::UnknownInput {
                        node: node.id.clone(),
                        input: input.clone(),
                    });
                };
                indegree[position] += 1;
                dependents[source].push(position);
            }
        }

        let mut ready: BTreeSet<usize> = (0..self.nodes.len())
            .filter(|position| indegree[*position] == 0)
            .collect();
        let mut placed = vec![false; self.nodes.len()];
        let mut order = Vec::with_capacity(self.nodes.len());

        while let Some(&next) = ready.iter().next() {
            ready.remove(&next);
            placed[next] = true;
            order.push(self.nodes[next].id.clone());
            for &dependent in &dependents[next] {
                indegree[dependent] -= 1;
                if indegree[dependent] == 0 {
                    ready.insert(dependent);
                }
            }
        }

        if order.len() != self.nodes.len() {
            let remaining = self
                .nodes
                .iter()
                .zip(placed)
                .filter(|(_, done)| !done)
                .map(|(node, _)| node.id.clone())
                .collect();
            return Err(PipelineError::Cycle(remaining));
        }

        Ok(order)
    }

    /// Look up a node by id.
    pub fn node(&self, id: &str) -> Option<&AudioNode> {
        self.nodes.iter().find(|node| node.id == id)
    }

    fn reachable_from(&self, starts: &[String]) -> BTreeSet<String> {
        let mut reachable = BTreeSet::new();
        let mut queue = VecDeque::new();
        for start in starts {
            if reachable.insert(start.clone()) {
                queue.push_back(start.clone());
            }
        }

        while let Some(current) = queue.pop_front() {
            for node in &self.nodes {
                if !reachable.contains(&node.id)
                    && node.inputs.iter().any(|input| input == &current)
                {
                    reachable.insert(node.id.clone());
                    queue.push_back(node.id.clone());
                }
            }
        }

        reachable
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// Why a pipeline or engine descriptor was rejected.
#[derive(Debug, Error)]
pub enum PipelineError {
    /// The pipeline has no nodes, or an engine has no codecs.
    #[error("empty: no nodes or codecs are defined")]
    Empty,

    /// The pipeline exceeds [`MAX_NODES`].
    #[error("pipeline has {0} nodes, above the limit of {MAX_NODES}")]
    TooManyNodes(usize),

    /// A node id is empty, too long, or has characters outside the allowed set.
    #[error("invalid id: {0:?}")]
    InvalidId(String),

    /// Two nodes share an id.
    #[error("duplicate id: {0}")]
    DuplicateId(String),

    /// A node names an input that does not exist.
    #[error("node {node:?} names unknown input {input:?}")]
    UnknownInput { node: String, input: String },

    /// A non-mixer node takes more than [`MAX_INPUTS`] inputs.
    #[error("node {node:?} has {count} inputs, above the limit of {MAX_INPUTS}")]
    TooManyInputs { node: String, count: usize },

    /// A plugin node has no plugin name.
    #[error("plugin node {0:?} has no plugin name")]
    MissingPlugin(String),

    /// The graph has a cycle; the payload lists nodes that could not be placed.
    #[error("pipeline contains a cycle involving {0:?}")]
    Cycle(Vec<String>),

    /// The pipeline input or output names a missing node.
    #[error("pipeline endpoint names unknown node {0:?}")]
    UnknownEndpoint(String),

    /// A node cannot be reached from any pipeline input.
    #[error("node {0:?} is not reachable from any pipeline input")]
    Unreachable(String),

    /// The pipeline input node has inputs of its own.
    #[error("input node {0:?} must not have inputs")]
    InputHasInputs(String),

    /// The pipeline declares no input roots.
    #[error("pipeline has no input nodes")]
    NoInputs,

    /// A capture pipeline does not have exactly one input root.
    #[error("pipeline has {got} input nodes, expected {expected}")]
    WrongInputCount { expected: usize, got: usize },

    /// The pipeline mixes more than [`MAX_SOURCES`] independent sources.
    #[error("pipeline has {0} sources, above the limit of {MAX_SOURCES}")]
    TooManySources(usize),

    /// An input root or the output node has the wrong kind for the role.
    #[error("node {node:?} has kind {got:?}, expected {expected:?}")]
    WrongEndpointKind {
        node: String,
        expected: AudioNodeKind,
        got: AudioNodeKind,
    },

    /// A mixer node has no inputs to sum.
    #[error("mixer node {0:?} has no inputs")]
    MixerHasNoInputs(String),

    /// An engine descriptor is malformed or out of range.
    #[error("invalid engine: {0}")]
    InvalidEngine(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, kind: AudioNodeKind, inputs: &[&str]) -> AudioNode {
        AudioNode::simple(id, kind, inputs)
    }

    #[test]
    fn default_capture_chain_validates_and_orders() {
        let pipeline = AudioPipeline::capture_default();
        assert_eq!(pipeline.role, PipelineRole::Capture);
        assert_eq!(pipeline.inputs, ["capture"]);
        assert_eq!(pipeline.output, "encode");
        pipeline.validate().expect("capture default is valid");
        assert_eq!(
            pipeline.topological_order().expect("acyclic"),
            vec![
                "capture",
                "echo-cancellation",
                "noise-suppression",
                "automatic-gain",
                "equaliser",
                "compressor",
                "soundboard",
                "limiter",
                "encode"
            ]
        );
    }

    #[test]
    fn default_playback_chain_validates_and_orders() {
        let pipeline = AudioPipeline::playback_default();
        assert_eq!(pipeline.role, PipelineRole::Playback);
        assert_eq!(pipeline.inputs, ["decode"]);
        assert_eq!(pipeline.output, "playback");
        pipeline.validate().expect("playback default is valid");
        assert_eq!(
            pipeline.topological_order().expect("acyclic"),
            vec!["decode", "mixer", "gain", "playback"]
        );
    }

    #[test]
    fn playback_for_builds_one_decoder_per_source() {
        for sources in [1usize, 2, MAX_SOURCES] {
            let pipeline = AudioPipeline::playback_for(sources);
            assert_eq!(pipeline.role, PipelineRole::Playback);
            assert_eq!(pipeline.output, "playback");
            pipeline.validate().expect("playback graph is valid");

            let expected: Vec<String> = (0..sources)
                .map(|index| format!("decode-{index}"))
                .collect();
            assert_eq!(pipeline.inputs, expected);

            let mixer = pipeline.node("mixer").expect("mixer exists");
            assert_eq!(mixer.kind, AudioNodeKind::Mixer);
            assert_eq!(mixer.inputs, expected);

            let order = pipeline.topological_order().expect("acyclic");
            assert_eq!(order.len(), pipeline.nodes.len());
            for input in &pipeline.inputs {
                assert_eq!(order.iter().filter(|id| *id == input).count(), 1);
            }
        }
    }

    #[test]
    fn playback_for_rejects_counts_outside_the_source_range() {
        let none = AudioPipeline::playback_for(0);
        assert!(matches!(none.validate(), Err(PipelineError::NoInputs)));

        let too_many = AudioPipeline::playback_for(MAX_SOURCES + 1);
        assert!(matches!(
            too_many.validate(),
            Err(PipelineError::TooManySources(count)) if count == MAX_SOURCES + 1
        ));
    }

    #[test]
    fn capture_and_playback_put_the_requested_endpoints_first_and_last() {
        let capture = AudioPipeline::capture_default();
        let order = capture.topological_order().expect("acyclic");
        assert_eq!(
            order.first().map(String::as_str),
            Some(capture.inputs[0].as_str())
        );
        assert_eq!(
            order.last().map(String::as_str),
            Some(capture.output.as_str())
        );

        let playback = AudioPipeline::playback_default();
        let order = playback.topological_order().expect("acyclic");
        assert_eq!(
            order.first().map(String::as_str),
            Some(playback.inputs[0].as_str())
        );
        assert_eq!(
            order.last().map(String::as_str),
            Some(playback.output.as_str())
        );
    }

    #[test]
    fn multi_root_order_is_deterministic_and_covers_every_node_once() {
        let pipeline = AudioPipeline::playback_for(4);
        let first = pipeline.topological_order().expect("acyclic");
        let second = pipeline.topological_order().expect("acyclic");
        assert_eq!(first, second);
        assert_eq!(first.len(), pipeline.nodes.len());

        let mut unique: Vec<&str> = first.iter().map(String::as_str).collect();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), pipeline.nodes.len());

        // Every root comes out in declaration order, then the shared tail.
        assert_eq!(
            &first[..4],
            &["decode-0", "decode-1", "decode-2", "decode-3"]
        );
        assert_eq!(&first[4..], &["mixer", "gain", "playback"]);
    }

    #[test]
    fn capture_accepts_exactly_one_input() {
        let mut pipeline = AudioPipeline::capture_default();
        pipeline.inputs.push("encode".to_string());
        assert!(matches!(
            pipeline.validate(),
            Err(PipelineError::WrongInputCount {
                expected: 1,
                got: 2
            })
        ));
    }

    #[test]
    fn playback_inputs_must_be_decoders() {
        let pipeline = AudioPipeline {
            nodes: vec![
                node("gain", AudioNodeKind::Gain, &[]),
                node("playback", AudioNodeKind::Playback, &["gain"]),
            ],
            inputs: vec!["gain".to_string()],
            output: "playback".to_string(),
            role: PipelineRole::Playback,
        };
        assert!(matches!(
            pipeline.validate(),
            Err(PipelineError::WrongEndpointKind { node, expected, got })
                if node == "gain"
                    && expected == AudioNodeKind::Decode
                    && got == AudioNodeKind::Gain
        ));
    }

    #[test]
    fn capture_output_must_be_an_encoder() {
        let pipeline = AudioPipeline {
            nodes: vec![
                node("capture", AudioNodeKind::Capture, &[]),
                node("gain", AudioNodeKind::Gain, &["capture"]),
            ],
            inputs: vec!["capture".to_string()],
            output: "gain".to_string(),
            role: PipelineRole::Capture,
        };
        assert!(matches!(
            pipeline.validate(),
            Err(PipelineError::WrongEndpointKind { node, expected, got })
                if node == "gain"
                    && expected == AudioNodeKind::Encode
                    && got == AudioNodeKind::Gain
        ));
    }

    #[test]
    fn a_mixer_without_inputs_is_rejected() {
        let pipeline = AudioPipeline {
            nodes: vec![
                node("capture", AudioNodeKind::Capture, &[]),
                node("mixer", AudioNodeKind::Mixer, &[]),
                node("encode", AudioNodeKind::Encode, &["capture"]),
            ],
            inputs: vec!["capture".to_string()],
            output: "encode".to_string(),
            role: PipelineRole::Capture,
        };
        assert!(matches!(
            pipeline.validate(),
            Err(PipelineError::MixerHasNoInputs(id)) if id == "mixer"
        ));
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let pipeline = AudioPipeline {
            nodes: vec![
                node("same", AudioNodeKind::Capture, &[]),
                node("same", AudioNodeKind::Gain, &["same"]),
            ],
            inputs: vec!["same".to_string()],
            output: "same".to_string(),
            role: PipelineRole::Capture,
        };
        assert!(matches!(
            pipeline.validate(),
            Err(PipelineError::DuplicateId(id)) if id == "same"
        ));
        assert!(matches!(
            pipeline.topological_order(),
            Err(PipelineError::DuplicateId(_))
        ));
    }

    #[test]
    fn unknown_input_is_rejected() {
        let pipeline = AudioPipeline {
            nodes: vec![
                node("capture", AudioNodeKind::Capture, &[]),
                node("encode", AudioNodeKind::Encode, &["nope"]),
            ],
            inputs: vec!["capture".to_string()],
            output: "encode".to_string(),
            role: PipelineRole::Capture,
        };
        assert!(matches!(
            pipeline.validate(),
            Err(PipelineError::UnknownInput { node, input })
                if node == "encode" && input == "nope"
        ));
    }

    #[test]
    fn plugin_node_without_a_name_is_rejected() {
        let pipeline = AudioPipeline {
            nodes: vec![
                node("capture", AudioNodeKind::Capture, &[]),
                node("fx", AudioNodeKind::Plugin, &["capture"]),
            ],
            inputs: vec!["capture".to_string()],
            output: "fx".to_string(),
            role: PipelineRole::Capture,
        };
        assert!(matches!(
            pipeline.validate(),
            Err(PipelineError::MissingPlugin(id)) if id == "fx"
        ));
    }

    #[test]
    fn two_node_cycle_is_rejected() {
        let pipeline = AudioPipeline {
            nodes: vec![
                node("a", AudioNodeKind::Gain, &["b"]),
                node("b", AudioNodeKind::Gain, &["a"]),
            ],
            inputs: vec!["a".to_string()],
            output: "b".to_string(),
            role: PipelineRole::Capture,
        };
        assert!(matches!(pipeline.validate(), Err(PipelineError::Cycle(_))));
        assert!(matches!(
            pipeline.topological_order(),
            Err(PipelineError::Cycle(ids)) if ids.len() == 2
        ));
    }

    #[test]
    fn unreachable_node_is_rejected() {
        let pipeline = AudioPipeline {
            nodes: vec![
                node("capture", AudioNodeKind::Capture, &[]),
                node("orphan", AudioNodeKind::Gain, &[]),
            ],
            inputs: vec!["capture".to_string()],
            output: "capture".to_string(),
            role: PipelineRole::Capture,
        };
        assert!(matches!(
            pipeline.validate(),
            Err(PipelineError::Unreachable(id)) if id == "orphan"
        ));
    }

    #[test]
    fn a_missing_output_is_rejected() {
        let pipeline = AudioPipeline {
            nodes: vec![
                node("capture", AudioNodeKind::Capture, &[]),
                node("encode", AudioNodeKind::Encode, &["capture"]),
            ],
            inputs: vec!["capture".to_string()],
            output: "missing".to_string(),
            role: PipelineRole::Capture,
        };
        assert!(matches!(
            pipeline.validate(),
            Err(PipelineError::UnknownEndpoint(id)) if id == "missing"
        ));
    }

    #[test]
    fn input_node_with_inputs_is_rejected() {
        let mut pipeline = AudioPipeline::capture_default();
        pipeline.inputs = vec!["equaliser".to_string()];
        assert!(matches!(
            pipeline.validate(),
            Err(PipelineError::InputHasInputs(id)) if id == "equaliser"
        ));
    }

    #[test]
    fn too_many_inputs_are_rejected_outside_a_mixer() {
        let sources = [
            "source-0", "source-1", "source-2", "source-3", "source-4", "source-5", "source-6",
            "source-7", "source-8",
        ];
        let mut nodes: Vec<AudioNode> = sources
            .iter()
            .map(|id| node(id, AudioNodeKind::Capture, &[]))
            .collect();
        nodes.push(node("merge", AudioNodeKind::Gain, &sources));

        let pipeline = AudioPipeline {
            nodes,
            inputs: vec!["source-0".to_string()],
            output: "merge".to_string(),
            role: PipelineRole::Capture,
        };
        assert!(matches!(
            pipeline.validate(),
            Err(PipelineError::TooManyInputs { node, count }) if node == "merge" && count == 9
        ));
    }

    #[test]
    fn empty_graph_is_rejected() {
        let pipeline = AudioPipeline {
            nodes: Vec::new(),
            inputs: Vec::new(),
            output: "out".to_string(),
            role: PipelineRole::Capture,
        };
        assert!(matches!(pipeline.validate(), Err(PipelineError::Empty)));
        assert!(matches!(
            pipeline.topological_order(),
            Err(PipelineError::Empty)
        ));
    }

    #[test]
    fn diamond_orders_the_shared_ancestor_before_both_children() {
        // Declared out of dependency order on purpose.
        let pipeline = AudioPipeline {
            nodes: vec![
                node("out", AudioNodeKind::Mixer, &["left", "right"]),
                node("left", AudioNodeKind::Gain, &["root"]),
                node("root", AudioNodeKind::Capture, &[]),
                node("right", AudioNodeKind::Gain, &["root"]),
            ],
            inputs: vec!["root".to_string()],
            output: "out".to_string(),
            role: PipelineRole::Capture,
        };

        let order = pipeline.topological_order().expect("acyclic");
        assert_eq!(order, vec!["root", "left", "right", "out"]);

        let position = |id: &str| order.iter().position(|node| node == id).expect("present");
        assert!(position("root") < position("left"));
        assert!(position("root") < position("right"));
        assert!(position("left") < position("out"));
        assert!(position("right") < position("out"));
    }

    #[test]
    fn arbitrary_ids_and_names_never_panic() {
        let long = "a".repeat(MAX_ID_LEN + 1);
        let cases = [
            "",
            " ",
            "a b",
            "-",
            "..",
            "x/y",
            "π",
            "a\nb",
            long.as_str(),
            "ok-id_1.2",
        ];

        for id in cases {
            let pipeline = AudioPipeline {
                nodes: vec![node(id, AudioNodeKind::Capture, &[])],
                inputs: vec![id.to_string()],
                output: id.to_string(),
                role: PipelineRole::Capture,
            };
            let _ = pipeline.validate();
            let _ = pipeline.topological_order();

            let mut engine = crate::engine::shipped_audio_engine();
            engine.name = id.to_string();
            let _ = engine.validate();
        }
    }
}
