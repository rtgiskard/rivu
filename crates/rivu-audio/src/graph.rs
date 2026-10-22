//! Immutable, bounded worker-thread execution of a selected audio DAG closure.
//!
//! Each reachable node runs once per block, even with fan-out. Mix edges delay
//! their shorter paths to the longest path's declared algorithmic latency. There
//! is no limiter or normalization. Preparation and destruction belong on the
//! audio worker (or admission thread), never in the device callback.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use anyhow::{Context, Result, bail, ensure};
use rivu_dsp::{
    denoise::{DenoiseMode, Denoiser},
    equalizer::Equalizer,
    fir_eq::FirEqualizer,
};
use rivu_format::config::processing_graph::{
    EqImplementation, GraphNodeKind, GraphPort, ProcessingGraph,
};

pub const GRAPH_BLOCK_FRAMES: usize = 256;
const MAX_CALL_FRAMES: usize = 65_536;
const MAX_GRAPH_CHANNELS: usize = 32;
const MAX_STATE_BYTES: usize = 64 * 1024 * 1024;
// Conservative arithmetic-work admission units, not a wall-clock deadline.
const MAX_WORK_PER_SECOND: u64 = 300_000_000;

struct DelayLine {
    samples: Vec<f32>,
    position: usize,
}
impl DelayLine {
    fn new(frames: usize, channels: usize) -> Self {
        Self {
            samples: vec![0.0; frames * channels],
            position: 0,
        }
    }
    fn add(&mut self, input: &[f32], output: &mut [f32], gain: f32) {
        if self.samples.is_empty() {
            for (output, input) in output.iter_mut().zip(input) {
                *output += input * gain;
            }
        } else {
            for (output, input) in output.iter_mut().zip(input) {
                *output += self.samples[self.position] * gain;
                self.samples[self.position] = *input;
                self.position += 1;
                if self.position == self.samples.len() {
                    self.position = 0;
                }
            }
        }
    }
    fn reset(&mut self) {
        self.samples.fill(0.0);
        self.position = 0;
    }
}

struct StreamingDenoise {
    denoiser: Denoiser,
    output: Vec<f32>,
    queue: VecDeque<f32>,
    delay_samples: usize,
    finished: bool,
}
impl StreamingDenoise {
    fn new(denoiser: Denoiser, channels: usize) -> Self {
        let delay_samples = denoiser.lookahead_frames() * channels;
        let burst =
            denoiser.config().fft_size.max(denoiser.lookahead_frames()) + GRAPH_BLOCK_FRAMES;
        let mut queue = VecDeque::with_capacity(delay_samples + burst * channels);
        queue.resize(delay_samples, 0.0);
        Self {
            denoiser,
            output: Vec::with_capacity(burst * channels),
            queue,
            delay_samples,
            finished: false,
        }
    }
    fn process(&mut self, samples: &mut [f32], input_samples: usize, eof: bool) -> Result<()> {
        self.output.clear();
        if input_samples > 0 {
            self.denoiser.process_block(
                &samples[..input_samples],
                &mut self.output,
                &mut || false,
            )?;
        }
        if eof && !self.finished {
            self.denoiser.finish(&mut self.output, &mut || false)?;
            self.finished = true;
        }
        ensure!(
            self.queue.len() + self.output.len() <= self.queue.capacity(),
            "streaming denoise exceeded its admitted output capacity"
        );
        self.queue.extend(self.output.iter().copied());
        ensure!(
            self.queue.len() >= samples.len(),
            "streaming denoise exceeded declared lookahead"
        );
        for sample in samples {
            *sample = self.queue.pop_front().expect("checked queue length");
        }
        Ok(())
    }
    fn reset(&mut self) {
        self.denoiser.reset();
        self.output.clear();
        self.queue.clear();
        self.queue.resize(self.delay_samples, 0.0);
        self.finished = false;
    }
}

enum Processor {
    Source,
    Pass,
    Iir(Equalizer),
    Fir(FirEqualizer),
    Denoise(Box<StreamingDenoise>),
}
struct Input {
    node: usize,
    gain: f32,
    delay: DelayLine,
}
struct Node {
    id: u64,
    inputs: Vec<Input>,
    processor: Processor,
    output: Vec<f32>,
    latency: usize,
    tail: usize,
    input_tail: usize,
    blocks: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct GraphSnapshot {
    graph: std::sync::Arc<ProcessingGraph>,
    target: GraphPort,
    boundary: Option<GraphPort>,
    sample_rate: u32,
    channels: usize,
    source_start_frame: i64,
    published_frames: Option<u64>,
}

impl GraphSnapshot {
    pub(crate) fn source_context(&self) -> (usize, Option<u64>) {
        (
            self.source_start_frame.unsigned_abs() as usize,
            self.published_frames,
        )
    }

    pub(crate) fn prepare(&self) -> Result<GraphRunner> {
        let mut runner = GraphRunner::prepare(
            std::sync::Arc::clone(&self.graph),
            self.target,
            self.boundary,
            self.sample_rate,
            self.channels,
        )?;
        if let Some(frames) = self.published_frames {
            runner.set_source_context(self.source_start_frame, frames)?;
        }
        Ok(runner)
    }
}

/// A prepared immutable graph snapshot, including a cached-prefix boundary when
/// requested. `process_interleaved` has zero steady-state allocations. Input and
/// output remain native-rate, native-layout, unclipped PCM. Calls are bounded to
/// 65,536 frames; internal node rounds contain at most 256 frames.
pub struct GraphRunner {
    nodes: Vec<Node>,
    channels: usize,
    sample_rate: u32,
    latency: usize,
    tail: usize,
    estimated_state_bytes: usize,
    estimated_work_per_frame: u64,
    processed_frames: u64,
    source_frames: Option<u64>,
    source_start_frame: i64,
    published_frames: Option<u64>,
    graph: std::sync::Arc<ProcessingGraph>,
    target: GraphPort,
    boundary: Option<GraphPort>,
}

impl std::fmt::Debug for GraphRunner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GraphRunner")
            .field("nodes", &self.nodes.len())
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("latency_frames", &self.latency)
            .field("tail_frames", &self.tail)
            .field("estimated_state_bytes", &self.estimated_state_bytes)
            .field("estimated_work_per_frame", &self.estimated_work_per_frame)
            .finish_non_exhaustive()
    }
}
impl GraphRunner {
    pub fn compile(
        graph: &ProcessingGraph,
        target: GraphPort,
        sample_rate: u32,
        channels: usize,
    ) -> Result<Self> {
        Self::prepare(
            std::sync::Arc::new(graph.clone()),
            target,
            None,
            sample_rate,
            channels,
        )
    }

    /// The supplied PCM is the source-aligned cached output of `boundary`.
    /// Every path to the target must cross that exact output port. Other source
    /// paths and a second output of the same separation node are rejected, not
    /// replaced with the wrong cached stream or silently rendered offline.
    pub fn compile_from_boundary(
        graph: &ProcessingGraph,
        target: GraphPort,
        boundary: GraphPort,
        sample_rate: u32,
        channels: usize,
    ) -> Result<Self> {
        Self::prepare(
            std::sync::Arc::new(graph.clone()),
            target,
            Some(boundary),
            sample_rate,
            channels,
        )
    }

    fn prepare(
        graph: std::sync::Arc<ProcessingGraph>,
        target: GraphPort,
        boundary: Option<GraphPort>,
        sample_rate: u32,
        channels: usize,
    ) -> Result<Self> {
        graph.validate()?;
        ensure!(
            sample_rate > 0 && channels > 0 && channels <= MAX_GRAPH_CHANNELS,
            "realtime graph requires a nonzero sample rate and 1..={MAX_GRAPH_CHANNELS} channels"
        );
        if let Some(boundary) = boundary {
            ensure!(
                (boundary.port as usize) < graph.node(boundary.node)?.kind.outputs(),
                "invalid cached graph boundary port"
            );
        }
        let mut ordered = Vec::new();
        visit(&graph, target, boundary, &mut BTreeSet::new(), &mut ordered)?;
        let mut indices: BTreeMap<GraphPort, usize> = BTreeMap::new();
        let mut nodes: Vec<Node> = Vec::with_capacity(ordered.len());
        let mut state_bytes = 0usize;
        let mut work_per_frame = 0u64;
        for port in ordered {
            let graph_node = graph.node(port.node)?;
            let is_source =
                boundary == Some(port) || matches!(graph_node.kind, GraphNodeKind::Source);
            let mut inputs = Vec::new();
            let mut upstream_latency = 0;
            let mut upstream_tail = 0;
            if !is_source {
                for input_port in 0..graph_node.kind.inputs() {
                    let edge = graph
                        .edges
                        .iter()
                        .find(|edge| {
                            edge.to
                                == GraphPort {
                                    node: port.node,
                                    port: input_port as u16,
                                }
                        })
                        .context("realtime graph has a disconnected input")?;
                    let index = *indices
                        .get(&edge.from)
                        .context("graph input was not compiled")?;
                    upstream_latency = upstream_latency.max(nodes[index].latency);
                    inputs.push(Input {
                        node: index,
                        gain: 1.0,
                        delay: DelayLine::new(0, channels),
                    });
                }
                for (input_port, input) in inputs.iter_mut().enumerate() {
                    let delay = upstream_latency - nodes[input.node].latency;
                    state_bytes = state_bytes.saturating_add(delay * channels * 4);
                    ensure!(
                        state_bytes <= MAX_STATE_BYTES,
                        "realtime graph exceeds 64 MiB state budget"
                    );
                    input.delay = DelayLine::new(delay, channels);
                    upstream_tail = upstream_tail.max(nodes[input.node].tail + delay);
                    if let GraphNodeKind::Mix {
                        gains_db,
                        muted,
                        solo,
                    } = &graph_node.kind
                    {
                        let audible = !muted[input_port]
                            && (!solo.iter().any(|value| *value) || solo[input_port]);
                        input.gain = if audible {
                            10.0_f32.powf(gains_db[input_port] / 20.0)
                        } else {
                            0.0
                        };
                    }
                }
            }
            // Bound cumulative state and work before allocating each processor.
            let (extra_state, extra_work) = if is_source {
                (0, 0)
            } else {
                estimate(&graph_node.kind, channels)
            };
            state_bytes =
                state_bytes.saturating_add(extra_state + GRAPH_BLOCK_FRAMES * channels * 4);
            work_per_frame =
                work_per_frame.saturating_add(extra_work + inputs.len() as u64 * channels as u64);
            ensure!(
                state_bytes <= MAX_STATE_BYTES,
                "realtime graph exceeds 64 MiB state budget"
            );
            ensure!(
                work_per_frame.saturating_mul(u64::from(sample_rate)) <= MAX_WORK_PER_SECOND,
                "realtime graph exceeds arithmetic work budget; use Hybrid or Offline execution"
            );
            let (processor, latency, tail) = if is_source {
                (Processor::Source, 0, 0)
            } else {
                match &graph_node.kind {
                    GraphNodeKind::Source => unreachable!(),
                    GraphNodeKind::Output | GraphNodeKind::Mix { .. } => (Processor::Pass, 0, 0),
                    GraphNodeKind::Equalizer {
                        config,
                        implementation: EqImplementation::Iir,
                    } => (
                        Processor::Iir(Equalizer::new(config, sample_rate, channels)?),
                        0,
                        0,
                    ),
                    GraphNodeKind::Equalizer {
                        config,
                        implementation: EqImplementation::LinearPhase { taps },
                    } => {
                        let fir = FirEqualizer::new(config, sample_rate, channels, *taps)?;
                        let latency = fir.latency_frames();
                        let tail = fir.tail_frames();
                        (Processor::Fir(fir), latency, tail)
                    }
                    GraphNodeKind::Denoise { config, .. } => {
                        ensure!(
                            config.mode != DenoiseMode::Profile,
                            "profile denoise requires offline profile capture; use a cached prefix in Hybrid mode"
                        );
                        let denoiser = Denoiser::new(sample_rate, channels, config.clone(), None)?;
                        let latency = denoiser.lookahead_frames();
                        (
                            Processor::Denoise(Box::new(StreamingDenoise::new(denoiser, channels))),
                            latency,
                            latency,
                        )
                    }
                    GraphNodeKind::Speech { .. } | GraphNodeKind::Separate { .. } => bail!(
                        "node {} requires model inference and is unsupported realtime; select Hybrid or Offline execution",
                        port.node
                    ),
                }
            };
            let latency = upstream_latency + latency;
            ensure!(
                latency <= sample_rate as usize * 2,
                "realtime graph latency exceeds two seconds; use Offline execution"
            );
            indices.insert(port, nodes.len());
            nodes.push(Node {
                id: port.node,
                inputs,
                processor,
                output: vec![0.0; GRAPH_BLOCK_FRAMES * channels],
                latency,
                tail: upstream_tail + tail,
                input_tail: upstream_tail,
                blocks: 0,
            });
        }
        let output = nodes.last().context("empty realtime graph")?;
        Ok(Self {
            latency: output.latency,
            tail: output.tail,
            nodes,
            channels,
            sample_rate,
            estimated_state_bytes: state_bytes,
            estimated_work_per_frame: work_per_frame,
            processed_frames: 0,
            source_frames: None,
            source_start_frame: 0,
            published_frames: None,
            graph,
            target,
            boundary,
        })
    }

    pub(crate) fn snapshot(&self) -> GraphSnapshot {
        GraphSnapshot {
            graph: std::sync::Arc::clone(&self.graph),
            target: self.target,
            boundary: self.boundary,
            sample_rate: self.sample_rate,
            channels: self.channels,
            source_start_frame: self.source_start_frame,
            published_frames: self.published_frames,
        }
    }
    pub(crate) fn source_lead_frames(&self) -> usize {
        self.source_start_frame.unsigned_abs() as usize
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    pub fn channels(&self) -> usize {
        self.channels
    }
    pub fn latency_frames(&self) -> usize {
        self.latency + self.source_start_frame.unsigned_abs() as usize
    }
    pub fn tail_frames(&self) -> usize {
        self.tail
    }
    pub fn estimated_state_bytes(&self) -> usize {
        self.estimated_state_bytes
    }
    pub fn estimated_work_per_frame(&self) -> u64 {
        self.estimated_work_per_frame
    }
    /// No allocation: each entry identifies a shared node and its block count.
    pub fn node_process_counts(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.nodes.iter().map(|node| (node.id, node.blocks))
    }
    /// Describe a full guarded cached prefix. Its first frame may precede the
    /// selected source; only the original source timeline is published.
    pub fn set_source_context(&mut self, start_frame: i64, frames: u64) -> Result<()> {
        ensure!(
            start_frame <= 0,
            "cached graph context starts after the source"
        );
        let preguard = usize::try_from(start_frame.unsigned_abs())
            .context("cached graph preguard exceeds addressable frame count")?;
        self.latency
            .checked_add(preguard)
            .context("cached graph latency exceeds addressable frame count")?;
        self.source_start_frame = start_frame;
        self.published_frames = Some(frames);
        Ok(())
    }
    pub fn published_frames(&self) -> Option<u64> {
        self.published_frames
    }
    pub fn reset(&mut self) {
        self.processed_frames = 0;
        self.source_frames = None;
        for node in &mut self.nodes {
            node.output.fill(0.0);
            node.blocks = 0;
            for input in &mut node.inputs {
                input.delay.reset();
            }
            match &mut node.processor {
                Processor::Source | Processor::Pass => {}
                Processor::Iir(eq) => eq.reset(),
                Processor::Fir(eq) => eq.reset(),
                Processor::Denoise(denoise) => denoise.reset(),
            }
        }
    }
    pub fn process_interleaved(&mut self, samples: &mut [f32]) -> Result<()> {
        ensure!(
            self.source_frames.is_none(),
            "graph source already finished"
        );
        ensure!(
            samples.len().is_multiple_of(self.channels),
            "incomplete graph input frame"
        );
        ensure!(
            samples.len() / self.channels <= MAX_CALL_FRAMES,
            "graph call exceeds 65,536 frames"
        );
        ensure!(
            samples.iter().all(|value| value.is_finite()),
            "graph input is not finite"
        );
        for chunk in samples.chunks_mut(GRAPH_BLOCK_FRAMES * self.channels) {
            self.process_chunk(chunk)?;
        }
        Ok(())
    }

    /// Drain actual per-node tails after source EOF. Padding is never admitted
    /// as source input to a denoiser; each is finished at its upstream EOF.
    pub fn finish_interleaved(&mut self, samples: &mut [f32]) -> Result<usize> {
        ensure!(
            samples.len().is_multiple_of(self.channels),
            "incomplete graph output frame"
        );
        ensure!(
            samples.len() / self.channels <= MAX_CALL_FRAMES,
            "graph drain exceeds work bound"
        );
        let source_frames = *self.source_frames.get_or_insert(self.processed_frames);
        let remaining = (source_frames + self.tail as u64).saturating_sub(self.processed_frames);
        let frames = (samples.len() / self.channels).min(remaining as usize);
        for chunk in
            samples[..frames * self.channels].chunks_mut(GRAPH_BLOCK_FRAMES * self.channels)
        {
            chunk.fill(0.0);
            self.process_chunk(chunk)?;
        }
        Ok(frames)
    }

    fn process_chunk(&mut self, chunk: &mut [f32]) -> Result<()> {
        let frames = chunk.len() / self.channels;
        for index in 0..self.nodes.len() {
            let (previous, remaining) = self.nodes.split_at_mut(index);
            let node = &mut remaining[0];
            let output_frames = self.source_frames.map_or(frames, |source| {
                (source + node.tail as u64)
                    .saturating_sub(self.processed_frames)
                    .min(frames as u64) as usize
            });
            node.output[output_frames * self.channels..chunk.len()].fill(0.0);
            let output = &mut node.output[..output_frames * self.channels];
            if matches!(node.processor, Processor::Source) {
                if self.source_frames.is_none() {
                    output.copy_from_slice(&chunk[..output.len()]);
                }
            } else {
                if node.inputs.len() == 1
                    && node.inputs[0].gain == 1.0
                    && node.inputs[0].delay.samples.is_empty()
                {
                    output.copy_from_slice(&previous[node.inputs[0].node].output[..output.len()]);
                } else {
                    output.fill(0.0);
                    for input in &mut node.inputs {
                        input.delay.add(
                            &previous[input.node].output[..output.len()],
                            output,
                            input.gain,
                        );
                    }
                }
                let input_frames = self.source_frames.map_or(output_frames, |source| {
                    (source + node.input_tail as u64)
                        .saturating_sub(self.processed_frames)
                        .min(output_frames as u64) as usize
                });
                match &mut node.processor {
                    Processor::Source | Processor::Pass => {}
                    Processor::Iir(eq) => eq.process(output)?,
                    Processor::Fir(eq) => eq.process_interleaved(output)?,
                    Processor::Denoise(denoise) => {
                        let eof = self.source_frames.is_some_and(|source| {
                            self.processed_frames + frames as u64 >= source + node.input_tail as u64
                        });
                        denoise.process(output, input_frames * self.channels, eof)?;
                    }
                }
            }
            node.blocks = node.blocks.saturating_add(1);
        }
        chunk.copy_from_slice(&self.nodes.last().expect("compiled target").output[..chunk.len()]);
        self.processed_frames += frames as u64;
        Ok(())
    }
}

fn visit(
    graph: &ProcessingGraph,
    port: GraphPort,
    boundary: Option<GraphPort>,
    seen: &mut BTreeSet<GraphPort>,
    ordered: &mut Vec<GraphPort>,
) -> Result<()> {
    let node = graph.node(port.node)?;
    ensure!(
        (port.port as usize) < node.kind.outputs(),
        "invalid realtime graph output port"
    );
    if !seen.insert(port) {
        return Ok(());
    }
    if boundary != Some(port) {
        ensure!(
            boundary.is_none_or(|cut| cut.node != port.node),
            "cached prefix contains only one output port; the suffix requires a second stream"
        );
        ensure!(
            !(boundary.is_some() && matches!(node.kind, GraphNodeKind::Source)),
            "cached prefix does not dominate target; another live source path remains"
        );
        for input in 0..node.kind.inputs() {
            let edge = graph
                .edges
                .iter()
                .find(|edge| {
                    edge.to
                        == GraphPort {
                            node: port.node,
                            port: input as u16,
                        }
                })
                .context("realtime graph has a disconnected input")?;
            visit(graph, edge.from, boundary, seen, ordered)?;
        }
    }
    ordered.push(port);
    Ok(())
}

fn estimate(kind: &GraphNodeKind, channels: usize) -> (usize, u64) {
    let (state, work) = match kind {
        GraphNodeKind::Equalizer {
            config,
            implementation,
        } if config.enabled => match implementation {
            EqImplementation::Iir => {
                let stages: usize = config.bands.iter().map(|band| band.stages as usize).sum();
                (
                    stages * channels * 16 * 3,
                    stages as u64 * channels as u64 * 12,
                )
            }
            EqImplementation::LinearPhase { taps } => {
                let partitions = taps.div_ceil(GRAPH_BLOCK_FRAMES);
                (
                    channels * partitions * 512 * 16 + channels * 256 * 12 + 512 * 32,
                    channels as u64 * (partitions as u64 * 16 + 288),
                )
            }
        },
        GraphNodeKind::Denoise { config, .. } => (
            config.fft_size * (channels * 64 + 128),
            channels as u64 * (config.fft_size.ilog2() as u64 * 32 + 128)
                + config.smoothing_bins as u64 * 16
                + 256,
        ),
        _ => (0, 0),
    };
    (state, work)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rivu_dsp::{denoise::DenoiseConfig, equalizer::EqualizerConfig};
    use rivu_format::config::processing_graph::{GraphEdge, GraphNode};

    fn port(node: u64, port: u16) -> GraphPort {
        GraphPort { node, port }
    }
    fn branch_graph() -> ProcessingGraph {
        ProcessingGraph {
            revision: 0,
            next_id: 6,
            nodes: vec![
                GraphNode {
                    id: 1,
                    position: [0.0; 2],
                    kind: GraphNodeKind::Source,
                },
                GraphNode {
                    id: 2,
                    position: [0.0; 2],
                    kind: GraphNodeKind::Equalizer {
                        config: EqualizerConfig {
                            enabled: true,
                            ..Default::default()
                        },
                        implementation: EqImplementation::LinearPhase { taps: 513 },
                    },
                },
                GraphNode {
                    id: 3,
                    position: [0.0; 2],
                    kind: GraphNodeKind::Equalizer {
                        config: EqualizerConfig::default(),
                        implementation: EqImplementation::Iir,
                    },
                },
                GraphNode {
                    id: 4,
                    position: [0.0; 2],
                    kind: GraphNodeKind::Mix {
                        gains_db: vec![0.0; 2],
                        muted: vec![false; 2],
                        solo: vec![false; 2],
                    },
                },
                GraphNode {
                    id: 5,
                    position: [0.0; 2],
                    kind: GraphNodeKind::Output,
                },
            ],
            edges: [
                (port(1, 0), port(2, 0)),
                (port(1, 0), port(3, 0)),
                (port(2, 0), port(4, 0)),
                (port(3, 0), port(4, 1)),
                (port(4, 0), port(5, 0)),
            ]
            .into_iter()
            .map(|(from, to)| GraphEdge { from, to })
            .collect(),
        }
    }
    #[test]
    fn branches_align_without_limiter_and_fanout_is_shared() {
        let graph = branch_graph();
        let mut runner = GraphRunner::compile(&graph, port(5, 0), 48_000, 2).unwrap();
        let mut input = vec![0.0; 2048 * 2];
        input[0] = 0.9;
        input[1] = -0.7;
        runner.process_interleaved(&mut input).unwrap();
        let delay = runner.latency_frames();
        assert!((input[delay * 2] - 1.8).abs() < 2e-6);
        assert!((input[delay * 2 + 1] + 1.4).abs() < 2e-6);
        for (index, sample) in input.iter().enumerate() {
            if index / 2 != delay {
                assert!(sample.abs() < 2e-6);
            }
        }
        assert!(runner.node_process_counts().all(|(_, count)| count == 8));
    }
    #[test]
    fn preview_uses_only_ancestors_and_cached_boundary_is_exact() {
        let graph = branch_graph();
        let mut preview = GraphRunner::compile(&graph, port(3, 0), 48_000, 1).unwrap();
        assert_eq!(
            preview
                .node_process_counts()
                .map(|(id, _)| id)
                .collect::<Vec<_>>(),
            [1, 3]
        );
        let mut sample = [0.4];
        preview.process_interleaved(&mut sample).unwrap();
        assert_eq!(sample, [0.4]);
        assert!(
            GraphRunner::compile_from_boundary(&graph, port(5, 0), port(2, 0), 48_000, 1).is_err()
        );
        let mut suffix =
            GraphRunner::compile_from_boundary(&graph, port(5, 0), port(4, 0), 48_000, 1).unwrap();
        suffix.process_interleaved(&mut sample).unwrap();
        assert_eq!(sample, [0.4]);
        assert_eq!(
            suffix
                .node_process_counts()
                .map(|(id, _)| id)
                .collect::<Vec<_>>(),
            [4, 5]
        );
    }
    #[test]
    fn streaming_denoise_is_chunk_independent_and_reset_clears_lookahead() {
        let mut graph = branch_graph();
        graph.nodes[1].kind = GraphNodeKind::Denoise {
            config: DenoiseConfig {
                mode: DenoiseMode::Stationary,
                fft_size: 512,
                ..Default::default()
            },
            profile: None,
        };
        let mut one = GraphRunner::compile(&graph, port(2, 0), 48_000, 1).unwrap();
        let mut split = GraphRunner::compile(&graph, port(2, 0), 48_000, 1).unwrap();
        let mut input: Vec<_> = (0..4096).map(|i| (i as f32 * 0.017).sin() * 0.1).collect();
        let mut other = input.clone();
        one.process_interleaved(&mut input).unwrap();
        for chunk in other.chunks_mut(37) {
            split.process_interleaved(chunk).unwrap();
        }
        assert_eq!(input, other);
        one.reset();
        input.fill(0.0);
        one.process_interleaved(&mut input).unwrap();
        assert!(input.iter().all(|value| value.abs() < 1e-8));
    }
    #[test]
    fn profile_and_unbounded_work_fail_explicitly() {
        let mut graph = branch_graph();
        graph.nodes[1].kind = GraphNodeKind::Denoise {
            config: DenoiseConfig {
                mode: DenoiseMode::Profile,
                ..Default::default()
            },
            profile: None,
        };
        let error = GraphRunner::compile(&graph, port(2, 0), 48_000, 1)
            .err()
            .unwrap();
        assert!(error.to_string().contains("profile"));
        graph.nodes[1].kind = GraphNodeKind::Equalizer {
            config: EqualizerConfig {
                enabled: true,
                ..Default::default()
            },
            implementation: EqImplementation::LinearPhase { taps: 8193 },
        };
        assert!(GraphRunner::compile(&graph, port(2, 0), 768_000, 32).is_err());
    }
}
