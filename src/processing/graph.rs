//! Block-streamed DAG evaluation. Only model boundaries and the selected output retain PCM.
use super::render::{ProgressReporter, check_cancel, validate_output};
use super::*;
use anyhow::bail;
use rivu_dsp::denoise::{DenoiseMode, NoiseProfile, ProfileCollector};
use rivu_format::config::processing_graph::ValidatedGraph;
use std::collections::BTreeMap;

const BLOCK: usize = 256;

pub(super) struct ExecutionGraph<'a> {
    graph: &'a ProcessingGraph,
    target: GraphPort,
    order: Vec<u64>,
}

impl<'a> ExecutionGraph<'a> {
    pub(super) fn new(validated: &ValidatedGraph<'a>, target: GraphPort) -> Result<Self> {
        Ok(Self {
            graph: validated.graph(),
            target,
            order: validated.ancestors(target)?,
        })
    }

    pub(super) fn native_only(&self) -> bool {
        self.order.iter().all(|&id| {
            self.graph.node(id).is_ok_and(|node| {
                !matches!(
                    node.kind,
                    GraphNodeKind::Speech { .. } | GraphNodeKind::Separate { .. }
                )
            })
        })
    }
}

pub(super) fn prepare(
    validated: &ValidatedGraph<'_>,
    target: GraphPort,
    mode: GraphExecutionMode,
    sample_rate: u32,
    channels: usize,
    cancel: &AtomicBool,
) -> Result<(
    GraphExecutionMode,
    Option<GraphPort>,
    Option<audio::GraphRunner>,
)> {
    check_cancel(cancel)?;
    let graph = validated.graph();
    if mode == GraphExecutionMode::Offline {
        return Ok((mode, None, None));
    }
    if matches!(
        mode,
        GraphExecutionMode::Auto | GraphExecutionMode::Realtime
    ) {
        match audio::GraphRunner::plan_validated(validated, target, None, sample_rate, channels)
            .and_then(audio::GraphPlan::prepare)
        {
            Ok(runner) => {
                check_cancel(cancel)?;
                return Ok((GraphExecutionMode::Realtime, None, Some(runner)));
            }
            Err(error) if mode == GraphExecutionMode::Realtime => return Err(error),
            Err(_) => {}
        }
    }
    let order = validated.ancestors(target)?;
    for (index, &id) in order.iter().enumerate() {
        check_cancel(cancel)?;
        let kind = &graph.node(id)?.kind;
        if mode == GraphExecutionMode::Auto {
            if matches!(kind, GraphNodeKind::Source) {
                continue;
            }
            let useful_suffix = order[index + 1..].iter().any(|&id| {
                graph
                    .node(id)
                    .is_ok_and(|n| !matches!(n.kind, GraphNodeKind::Source | GraphNodeKind::Output))
            });
            if !useful_suffix {
                continue;
            }
        }
        for port in 0..kind.outputs() {
            let boundary = GraphPort {
                node: id,
                port: port as u16,
            };
            if let Ok(plan) = audio::GraphRunner::plan_validated(
                validated,
                target,
                Some(boundary),
                sample_rate,
                channels,
            ) {
                check_cancel(cancel)?;
                if let Ok(runner) = plan.prepare() {
                    check_cancel(cancel)?;
                    return Ok((GraphExecutionMode::Hybrid, Some(boundary), Some(runner)));
                }
            }
        }
    }
    ensure!(
        mode == GraphExecutionMode::Auto,
        "No supported exact-quality hybrid suffix exists"
    );
    Ok((GraphExecutionMode::Offline, None, None))
}

#[derive(Clone)]
struct GraphPcm {
    pcm: audio::PcmAudio,
    start_frame: i64,
}

struct Edge {
    node: usize,
    gain: f64,
    delay: Vec<f32>,
    position: usize,
}

struct Node {
    inputs: Vec<Edge>,
    source: Option<audio::PcmAudio>,
    // None is an immutable source boundary or the streaming decoder input.
    processor: Option<audio::NativeProcessor>,
    output: Vec<f32>,
    output_node: usize,
    latency: u64,
    frames: u64,
    input_frames: u64,
}

/// One block per physical native output, plus bounded causal delay queues.
/// Logical pass nodes retain timeline/identity while sharing upstream blocks.
/// Model outputs act as immutable sources, never a second implicit file protocol.
struct Engine {
    nodes: Vec<Node>,
    indices: BTreeMap<GraphPort, usize>,
    channels: usize,
    layout: Arc<[audio::Channel]>,
    rate: u32,
    source: Option<audio::PcmAudio>,
    streaming: bool,
    sum: Vec<f64>,
}
impl Engine {
    fn new(source: &audio::PcmAudio) -> Self {
        Self {
            nodes: Vec::new(),
            indices: BTreeMap::new(),
            channels: usize::from(source.channels()),
            layout: Arc::clone(source.layout()),
            rate: source.sample_rate(),
            source: Some(source.clone()),
            streaming: false,
            sum: Vec::new(),
        }
    }
    fn for_decoder(rate: u32, layout: Arc<[audio::Channel]>) -> Self {
        Self {
            nodes: Vec::new(),
            indices: BTreeMap::new(),
            channels: layout.len(),
            layout,
            rate,
            source: None,
            streaming: true,
            sum: Vec::new(),
        }
    }
    fn for_retained(
        source: Option<&audio::PcmAudio>,
        rate: u32,
        layout: Arc<[audio::Channel]>,
    ) -> Self {
        if let Some(source) = source {
            return Self::new(source);
        }
        let mut engine = Self::for_decoder(rate, layout);
        engine.streaming = false;
        engine
    }
    fn compile(
        &mut self,
        graph: &ProcessingGraph,
        port: GraphPort,
        boundaries: &BTreeMap<GraphPort, GraphPcm>,
        profiles: &BTreeMap<u64, NoiseProfile>,
        cancel: &AtomicBool,
    ) -> Result<usize> {
        if let Some(&index) = self.indices.get(&port) {
            return Ok(index);
        }
        check_cancel(cancel)?;
        let kind = &graph.node(port.node)?.kind;
        let mut inputs = Vec::new();
        let mut latency = 0;
        let mut frames = self.source.as_ref().map_or(0, audio::PcmAudio::frames);
        let source;
        let processor;
        let mut timing = (0, 0);
        if let Some(boundary) = boundaries.get(&port) {
            ensure!(boundary.start_frame <= 0, "Invalid causal graph boundary");
            latency = boundary.start_frame.unsigned_abs();
            frames = boundary.pcm.frames();
            source = Some(boundary.pcm.clone());
            processor = None;
        } else if matches!(kind, GraphNodeKind::Source) {
            source = self.source.clone();
            if source.is_none() {
                ensure!(
                    self.streaming,
                    "Original graph source was released while reachable"
                );
            }
            processor = None;
        } else {
            for input_port in 0..kind.inputs() {
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
                    .context("Disconnected graph input")?;
                let node = self.compile(graph, edge.from, boundaries, profiles, cancel)?;
                latency = latency.max(self.nodes[node].latency);
                let gain = if let GraphNodeKind::Mix {
                    gains_db,
                    muted,
                    solo,
                } = kind
                {
                    if muted[input_port] || (solo.iter().any(|value| *value) && !solo[input_port]) {
                        0.0
                    } else {
                        10.0_f64.powf(f64::from(gains_db[input_port]) / 20.0)
                    }
                } else {
                    1.0
                };
                inputs.push(Edge {
                    node,
                    gain,
                    delay: Vec::new(),
                    position: 0,
                });
            }
            frames = 0;
            for edge in &mut inputs {
                let delay = latency - self.nodes[edge.node].latency;
                frames = frames.max(
                    self.nodes[edge.node]
                        .frames
                        .checked_add(delay)
                        .context("Graph timeline overflow")?,
                );
                let samples = usize::try_from(delay)?
                    .checked_mul(self.channels)
                    .context("Graph delay overflow")?;
                // DSP state is bounded separately from chunked retained PCM.
                ensure!(
                    samples <= 16 * 1024 * 1024,
                    "Graph delay exceeds 64 MiB state bound"
                );
                edge.delay = vec![0.0; samples];
            }
            if matches!(
                kind,
                GraphNodeKind::Speech { .. } | GraphNodeKind::Separate { .. }
            ) {
                bail!("Missing prepared neural boundary");
            }
            let plan = audio::NativeNodePlan::new(kind, self.rate, self.channels)?;
            timing = plan.timing_frames();
            source = None;
            processor = Some(plan.prepare(profiles.get(&port.node), BLOCK)?);
        }
        let input_frames = frames;
        let (delay, tail) = (timing.0 as u64, timing.1 as u64);
        latency = latency
            .checked_add(delay)
            .context("Graph latency overflow")?;
        frames = frames.checked_add(tail).context("Graph length overflow")?;
        let index = self.nodes.len();
        let needs_sum = processor.is_some()
            && !(inputs.len() == 1 && inputs[0].gain == 1.0 && inputs[0].delay.is_empty());
        if needs_sum && self.sum.is_empty() {
            self.sum.resize(BLOCK * self.channels, 0.0);
        }
        let output_node = if processor
            .as_ref()
            .is_some_and(audio::NativeProcessor::is_pass)
            && !needs_sum
        {
            self.nodes[inputs[0].node].output_node
        } else {
            index
        };
        self.nodes.push(Node {
            inputs,
            source,
            processor,
            output: if output_node == index {
                vec![0.0; BLOCK * self.channels]
            } else {
                Vec::new()
            },
            output_node,
            latency,
            frames,
            input_frames,
        });
        self.indices.insert(port, index);
        Ok(index)
    }
    fn round(
        &mut self,
        position: u64,
        count: usize,
        decoder: Option<&[f32]>,
        source_eof: Option<u64>,
        cancel: &AtomicBool,
    ) -> Result<()> {
        check_cancel(cancel)?;
        let streaming = self.streaming;
        for index in 0..self.nodes.len() {
            let (previous, rest) = self.nodes.split_at_mut(index);
            let node = &mut rest[0];
            if node.output_node != index {
                continue;
            }
            let end = if streaming {
                source_eof.and_then(|source| source.checked_add(node.frames))
            } else {
                Some(node.frames)
            };
            let input_end = if streaming {
                source_eof.and_then(|source| source.checked_add(node.input_frames))
            } else {
                Some(node.input_frames)
            };
            let frames = end.map_or(count, |end| {
                end.saturating_sub(position).min(count as u64) as usize
            });
            // Downstream nodes may outlive this node, but only read this round's
            // count. Every processor below writes its complete active prefix.
            node.output[frames * self.channels..count * self.channels].fill(0.0);
            let output = &mut node.output[..frames * self.channels];
            match (&node.source, &mut node.processor) {
                (Some(pcm), _) => {
                    let read = pcm.read_frames(position, output);
                    output[read * self.channels..].fill(0.0);
                }
                (None, None) => {
                    if let Some(input) = decoder {
                        output.copy_from_slice(&input[..output.len()]);
                    } else {
                        output.fill(0.0);
                    }
                }
                (None, Some(processor)) => {
                    if node.inputs.len() == 1
                        && node.inputs[0].gain == 1.0
                        && node.inputs[0].delay.is_empty()
                    {
                        let input = &previous[previous[node.inputs[0].node].output_node].output;
                        output.copy_from_slice(&input[..output.len()]);
                    } else {
                        let sum = &mut self.sum[..output.len()];
                        sum.fill(0.0);
                        for edge in &mut node.inputs {
                            let input =
                                &previous[previous[edge.node].output_node].output[..output.len()];
                            for (sum, &sample) in sum.iter_mut().zip(input) {
                                let sample = if edge.delay.is_empty() {
                                    sample
                                } else {
                                    let delayed = edge.delay[edge.position];
                                    edge.delay[edge.position] = sample;
                                    edge.position = (edge.position + 1) % edge.delay.len();
                                    delayed
                                };
                                *sum += f64::from(sample) * edge.gain;
                            }
                        }
                        for (sample, sum) in output.iter_mut().zip(sum) {
                            // Finite sources and DSP outputs can overflow when
                            // mixed, including at the f64-to-f32 boundary.
                            *sample = *sum as f32;
                            ensure!(sample.is_finite(), "Nonfinite graph output");
                        }
                    }
                    let input_frames = input_end.map_or(frames, |end| {
                        end.saturating_sub(position).min(frames as u64) as usize
                    });
                    processor.process(
                        output,
                        input_frames * self.channels,
                        input_end.is_some_and(|end| position + count as u64 >= end),
                        &mut || cancel.load(Ordering::Acquire),
                    )?;
                }
            }
        }
        Ok(())
    }
    /// Exclusive end on the causal output timeline, including upstream latency.
    /// Input lengths remain real: only actual EOF can trigger a processor flush.
    fn stream(
        &mut self,
        target: usize,
        prefix_end: Option<u64>,
        cancel: &AtomicBool,
        mut consume: impl FnMut(u64, &[f32]) -> Result<()>,
    ) -> Result<()> {
        let total = prefix_end.map_or(self.nodes[target].frames, |end| {
            end.min(self.nodes[target].frames)
        });
        let mut position = 0;
        while position < total {
            let count = (total - position).min(BLOCK as u64) as usize;
            self.round(position, count, None, None, cancel)?;
            consume(
                position,
                &self.nodes[self.nodes[target].output_node].output[..count * self.channels],
            )?;
            position += count as u64;
        }
        Ok(())
    }
    /// Returns the source frames traversed, or the exact length after real EOF.
    /// A bounded prefix therefore still proves any earlier source selection fits.
    /// Consumers receive exact source length on every tail block, never an estimate.
    fn decode(
        &mut self,
        target: usize,
        mut renderer: audio::SourceRenderer,
        prefix_end: Option<u64>,
        cancel: &AtomicBool,
        mut consume: impl FnMut(u64, &[f32], Option<u64>) -> Result<()>,
    ) -> Result<u64> {
        ensure!(
            self.streaming,
            "Decoder engine cannot contain a retained source"
        );
        ensure!(
            renderer.format().sample_rate == self.rate && renderer.format().layout == self.layout,
            "Source format changed during graph decoding"
        );
        let mut position = 0_u64;
        loop {
            if prefix_end.is_some_and(|end| position >= end) {
                // A requested prefix is not EOF. Keep the real input used by
                // causal FIR/STFT lookahead, but do not synthesize a DSP flush.
                renderer.finish(cancel)?;
                return Ok(position);
            }
            let Some(decoded) = renderer.next_block(cancel)? else {
                break;
            };
            ensure!(
                decoded.len().is_multiple_of(self.channels),
                "Incomplete graph source frame"
            );
            for block in decoded.chunks(BLOCK * self.channels) {
                if prefix_end.is_some_and(|end| position >= end) {
                    renderer.finish(cancel)?;
                    return Ok(position);
                }
                let count = block.len() / self.channels;
                let count =
                    prefix_end.map_or(count, |end| (end - position).min(count as u64) as usize);
                self.round(position, count, Some(block), None, cancel)?;
                consume(
                    position,
                    &self.nodes[self.nodes[target].output_node].output[..count * self.channels],
                    None,
                )?;
                position = position
                    .checked_add(count as u64)
                    .context("Decoded source length overflow")?;
            }
        }
        let frames = position;
        self.round(position, 0, None, Some(frames), cancel)?;
        let total = frames
            .checked_add(self.nodes[target].frames)
            .context("Graph tail length overflow")?;
        let total = prefix_end.map_or(total, |end| end.min(total));
        while position < total {
            let count = (total - position).min(BLOCK as u64) as usize;
            self.round(position, count, None, Some(frames), cancel)?;
            consume(
                position,
                &self.nodes[self.nodes[target].output_node].output[..count * self.channels],
                Some(frames),
            )?;
            position += count as u64;
        }
        renderer.finish(cancel)?;
        Ok(frames)
    }
    fn alias(&self, target: usize) -> Option<audio::PcmAudio> {
        self.nodes[self.nodes[target].output_node].source.clone()
    }
    fn render(
        mut self,
        target: usize,
        budget: Arc<audio::PcmBudget>,
        cancel: &AtomicBool,
    ) -> Result<GraphPcm> {
        let start_frame = -i64::try_from(self.nodes[target].latency)?;
        if let Some(pcm) = self.alias(target) {
            return Ok(GraphPcm { pcm, start_frame });
        }
        let mut builder = audio::PcmBuilder::new(self.rate, Arc::clone(&self.layout), budget)?;
        self.stream(target, None, cancel, |_, block| builder.push(block))?;
        Ok(GraphPcm {
            pcm: builder.finish()?,
            start_frame,
        })
    }
}

fn input_port(graph: &ProcessingGraph, node: u64) -> Result<GraphPort> {
    graph
        .edges
        .iter()
        .find(|edge| edge.to == GraphPort { node, port: 0 })
        .map(|edge| edge.from)
        .context("Disconnected graph input")
}

fn reachable_boundaries(
    graph: &ProcessingGraph,
    port: GraphPort,
    boundaries: &BTreeMap<GraphPort, GraphPcm>,
    visited: &mut std::collections::BTreeSet<GraphPort>,
    retained: &mut std::collections::BTreeSet<GraphPort>,
    stop_at: Option<u64>,
) -> Result<()> {
    if !visited.insert(port) {
        return Ok(());
    }
    if stop_at == Some(port.node) {
        return Ok(());
    }
    if boundaries.contains_key(&port) {
        retained.insert(port);
        return Ok(());
    }
    for input in 0..graph.node(port.node)?.kind.inputs() {
        let from = graph
            .edges
            .iter()
            .find(|edge| {
                edge.to
                    == GraphPort {
                        node: port.node,
                        port: input as u16,
                    }
            })
            .context("Disconnected graph input")?
            .from;
        reachable_boundaries(graph, from, boundaries, visited, retained, stop_at)?;
    }
    Ok(())
}
fn retains_source(
    graph: &ProcessingGraph,
    target: GraphPort,
    boundaries: &BTreeMap<GraphPort, GraphPcm>,
    stop_at: Option<u64>,
) -> Result<bool> {
    let mut visited = std::collections::BTreeSet::new();
    reachable_boundaries(
        graph,
        target,
        boundaries,
        &mut visited,
        &mut std::collections::BTreeSet::new(),
        stop_at,
    )?;
    for port in visited {
        if !boundaries.contains_key(&port)
            && matches!(graph.node(port.node)?.kind, GraphNodeKind::Source)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn release_unreachable_source(
    graph: &ProcessingGraph,
    target: GraphPort,
    boundaries: &BTreeMap<GraphPort, GraphPcm>,
    stop_at: Option<u64>,
    input: &mut Option<audio::PcmAudio>,
) -> Result<()> {
    if !retains_source(graph, target, boundaries, stop_at)? {
        *input = None;
    }
    Ok(())
}

fn release_unreachable_pcm(
    graph: &ProcessingGraph,
    target: GraphPort,
    boundaries: &mut BTreeMap<GraphPort, GraphPcm>,
    stop_at: Option<u64>,
    input: &mut Option<audio::PcmAudio>,
) -> Result<()> {
    let mut retained = std::collections::BTreeSet::new();
    reachable_boundaries(
        graph,
        target,
        boundaries,
        &mut std::collections::BTreeSet::new(),
        &mut retained,
        stop_at,
    )?;
    boundaries.retain(|port, _| retained.contains(port));
    release_unreachable_source(graph, target, boundaries, stop_at, input)
}

pub(super) fn prepare_models(
    execution: &ExecutionGraph<'_>,
    status: &mut ProcessingTaskStatus,
    sample_rate: u32,
    layout: &[audio::Channel],
    cancel: &AtomicBool,
    tx: &Sender<Event>,
) -> Result<BTreeMap<u64, neural::PreparedModel>> {
    let graph = execution.graph;
    let mut models = BTreeMap::new();
    for &id in &execution.order {
        check_cancel(cancel)?;
        let mut provenance = ModelProvenance::default();
        let model = {
            let (operation, config) = match &graph.node(id)?.kind {
                GraphNodeKind::Speech { config } => (ProcessingOperation::Speech, config),
                GraphNodeKind::Separate { config } => (ProcessingOperation::Separate, config),
                _ => continue,
            };
            neural::prepare(
                operation,
                config,
                sample_rate,
                layout,
                &mut provenance,
                cancel,
            )
        };
        let entry = status
            .graph_nodes
            .iter_mut()
            .find(|entry| entry.node == id)
            .context("Missing graph node status")?;
        entry.model_id = provenance.id;
        entry.model_sha256 = provenance.sha256;
        match model {
            Ok(model) => {
                models.insert(id, model);
            }
            Err(error) => {
                node_event(status, id, ProcessingPhase::Processing, None, tx);
                node_failure(status, id, &error, cancel, tx);
                return Err(error);
            }
        }
    }
    Ok(models)
}

pub(super) fn execute(
    execution: &ExecutionGraph<'_>,
    input: audio::PcmAudio,
    budget: Arc<audio::PcmBudget>,
    status: &mut ProcessingTaskStatus,
    cancellation: &Cancellation,
    progress: &mut ProgressReporter<'_>,
    mut models: BTreeMap<u64, neural::PreparedModel>,
) -> Result<BTreeMap<GraphPort, GraphAudioOutput>> {
    let cancel = cancellation.flag();
    let tx = progress.sender;
    let graph = execution.graph;
    let target = execution.target;
    let order = &execution.order;
    let rate = input.sample_rate();
    let layout = Arc::clone(input.layout());
    let frames = input.frames();
    let channels = input.channels();
    let mut input = Some(input);
    let mut boundaries = BTreeMap::new();
    let mut profiles = BTreeMap::new();
    for (index, &id) in order.iter().enumerate() {
        check_cancel(cancel)?;
        progress.report(
            ProcessingPhase::Processing,
            index as u64,
            Some(order.len() as u64),
        );
        node_event(status, id, ProcessingPhase::Processing, None, tx);
        let outcome = (|| -> Result<()> {
            if let GraphNodeKind::Denoise {
                profile: Some(selection),
                ..
            } = &graph.node(id)?.kind
            {
                selection.validate()?;
                ensure!(
                    (selection.end_seconds * f64::from(rate)).floor() <= frames as f64,
                    "Noise profile selection lies outside the source"
                );
            }
            match &graph.node(id)?.kind {
                GraphNodeKind::Denoise { config, profile }
                    if config.mode == DenoiseMode::Profile =>
                {
                    let selection = profile
                        .context("Profile mode requires an explicit selected noise-only range")?;
                    selection.validate()?;
                    let start = (selection.start_seconds * f64::from(rate)).floor() as u64;
                    let end = (selection.end_seconds * f64::from(rate)).floor() as u64;
                    ensure!(
                        start < end && end <= frames,
                        "Noise profile selection lies outside the source"
                    );
                    let mut engine =
                        Engine::for_retained(input.as_ref(), rate, Arc::clone(&layout));
                    let upstream = engine.compile(
                        graph,
                        input_port(graph, id)?,
                        &boundaries,
                        &profiles,
                        cancel,
                    )?;
                    let offset = engine.nodes[upstream].latency;
                    let start = start
                        .checked_add(offset)
                        .context("Profile offset overflow")?;
                    let end = end.checked_add(offset).context("Profile offset overflow")?;
                    let channels = usize::from(channels);
                    let mut collector =
                        ProfileCollector::new(rate, channels, config.fft_size, end - start)?;
                    engine.stream(upstream, Some(end), cancel, |position, samples| {
                        let begin = position.max(start);
                        let finish = (position + (samples.len() / channels) as u64).min(end);
                        if begin < finish {
                            collector.push_block(
                                &samples[(begin - position) as usize * channels
                                    ..(finish - position) as usize * channels],
                                &mut || cancel.load(Ordering::Acquire),
                            )?;
                        }
                        Ok(())
                    })?;
                    profiles.insert(id, collector.finish()?);
                }
                GraphNodeKind::Speech { config } | GraphNodeKind::Separate { config } => {
                    let operation = if matches!(graph.node(id)?.kind, GraphNodeKind::Speech { .. })
                    {
                        ProcessingOperation::Speech
                    } else {
                        ProcessingOperation::Separate
                    };
                    let mut engine =
                        Engine::for_retained(input.as_ref(), rate, Arc::clone(&layout));
                    let upstream = engine.compile(
                        graph,
                        input_port(graph, id)?,
                        &boundaries,
                        &profiles,
                        cancel,
                    )?;
                    let upstream = engine.render(upstream, Arc::clone(&budget), cancel)?;
                    // This model now owns its materialized input. Future graph
                    // traversals stop at its outputs before inference allocates
                    // them; a bypass alias still retains its own PCM storage.
                    release_unreachable_pcm(graph, target, &mut boundaries, Some(id), &mut input)?;
                    let outcome = neural::run(
                        ModelRequest {
                            task_id: status.id,
                            operation,
                            input: &upstream.pcm,
                            config,
                            budget: Arc::clone(&budget),
                            cancel: cancellation,
                        },
                        models.remove(&id).context("Missing admitted model")?,
                        progress,
                    );
                    let entry = status
                        .graph_nodes
                        .iter_mut()
                        .find(|entry| entry.node == id)
                        .context("Missing graph node status")?;
                    entry.model_id = outcome.provenance.id;
                    entry.model_sha256 = outcome.provenance.sha256;
                    let result = outcome.result?;
                    let outputs = if operation == ProcessingOperation::Speech {
                        vec![result.denoised.context("Missing speech output")?]
                    } else {
                        vec![
                            result.vocals.context("Missing vocal output")?,
                            result
                                .accompaniment
                                .context("Missing accompaniment output")?,
                        ]
                    };
                    for (port, pcm) in outputs.into_iter().enumerate() {
                        validate_output(&pcm, rate, &layout, upstream.pcm.frames(), cancel)?;
                        boundaries.insert(
                            GraphPort {
                                node: id,
                                port: port as u16,
                            },
                            GraphPcm {
                                pcm,
                                start_frame: upstream.start_frame,
                            },
                        );
                    }
                    node_event(status, id, ProcessingPhase::Complete, None, tx);
                }
                _ => {}
            }
            Ok(())
        })();
        if let Err(error) = outcome {
            node_failure(status, id, &error, cancel, tx);
            return Err(error);
        }
        if matches!(
            graph.node(id)?.kind,
            GraphNodeKind::Speech { .. } | GraphNodeKind::Separate { .. }
        ) {
            // Boundaries terminate every future PCM traversal, but profile
            // validation still uses the immutable original format and length.
            release_unreachable_pcm(graph, target, &mut boundaries, None, &mut input)?;
        }
    }
    let outcome = (|| {
        let mut engine = Engine::for_retained(input.as_ref(), rate, Arc::clone(&layout));
        let index = engine.compile(graph, target, &boundaries, &profiles, cancel)?;
        // The engine owns only boundaries reachable from this target; obsolete model
        // ancestors are released before allocating the final retained output.
        drop(boundaries);
        drop(profiles);
        drop(input);
        let output = engine.render(index, budget, cancel)?;
        publish(output, frames, target, order, status, tx)
    })();
    if let Err(error) = &outcome {
        node_failure(status, target.node, error, cancel, tx);
    }
    outcome
}

fn publish(
    output: GraphPcm,
    frames: u64,
    target: GraphPort,
    order: &[u64],
    status: &mut ProcessingTaskStatus,
    tx: &Sender<Event>,
) -> Result<BTreeMap<GraphPort, GraphAudioOutput>> {
    ensure!(
        output.start_frame <= 0
            && output.pcm.frames()
                >= output
                    .start_frame
                    .unsigned_abs()
                    .checked_add(frames)
                    .context("Graph output timeline overflow")?,
        "Graph output does not cover source timeline"
    );
    let pcm = output
        .pcm
        .slice_frames(output.start_frame.unsigned_abs(), frames)?;
    let context =
        (output.start_frame != 0 || output.pcm.frames() != frames).then_some(GraphAudioContext {
            pcm: output.pcm,
            start_frame: output.start_frame,
        });
    status.graph_outputs = vec![GraphNodeOutput {
        port: target,
        context: context.as_ref().map(|context| GraphOutputContext {
            start_frame: context.start_frame,
        }),
    }];
    if let (Some(plan), Some(context)) = (&mut status.graph_plan, &context) {
        plan.latency_frames = plan
            .latency_frames
            .checked_add(context.start_frame.unsigned_abs())
            .context("Hybrid graph latency overflow")?;
    }
    for &id in order {
        node_event(status, id, ProcessingPhase::Complete, None, tx);
    }
    if status.settings.graph_mode == Some(GraphExecutionMode::Hybrid) {
        for node in &mut status.graph_nodes {
            if node.phase == ProcessingPhase::Queued {
                node.phase = ProcessingPhase::Processing;
            }
        }
    }
    Ok(BTreeMap::from([(
        target,
        GraphAudioOutput { pcm, context },
    )]))
}

pub(super) struct NativeGraphOutput {
    pub(super) outputs: BTreeMap<GraphPort, GraphAudioOutput>,
    pub(super) denoised: Option<audio::PcmAudio>,
    pub(super) pending_export: Option<(tempfile::NamedTempFile, PathBuf)>,
}

fn causal_export_block(
    position: u64,
    samples: &[f32],
    channels: usize,
    latency: u64,
    source_frames: Option<u64>,
) -> Result<&[f32]> {
    let block_end = position
        .checked_add((samples.len() / channels) as u64)
        .context("Graph export block timeline overflow")?;
    let begin = position.max(latency);
    // Before EOF every block ends at or before N, hence also before latency+N.
    // Tail blocks require the exact EOF, especially when N is below latency.
    let end = source_frames.map_or(Ok(block_end), |frames| {
        latency
            .checked_add(frames)
            .map(|end| end.min(block_end))
            .context("Graph export timeline overflow")
    })?;
    if begin >= end {
        return Ok(&samples[..0]);
    }
    Ok(&samples[(begin - position) as usize * channels..(end - position) as usize * channels])
}

pub(super) fn execute_native(
    execution: Option<&ExecutionGraph<'_>>,
    renderer: audio::SourceRenderer,
    mut reopen: impl FnMut() -> Result<audio::SourceRenderer>,
    budget: Arc<audio::PcmBudget>,
    status: &mut ProcessingTaskStatus,
    cancel: &AtomicBool,
    progress: &mut ProgressReporter<'_>,
) -> Result<NativeGraphOutput> {
    let tx = progress.sender;
    let standalone = status.operation == ProcessingOperation::Denoise;
    let standalone_graph;
    let standalone_execution;
    let execution = if standalone {
        let source = GraphPort { node: 1, port: 0 };
        let target = GraphPort { node: 3, port: 0 };
        let graph = ProcessingGraph {
            revision: 0,
            next_id: 4,
            nodes: vec![
                GraphNode {
                    id: source.node,
                    kind: GraphNodeKind::Source,
                    position: [0.0; 2],
                },
                GraphNode {
                    id: target.node,
                    kind: GraphNodeKind::Denoise {
                        config: status.settings.denoise.clone(),
                        profile: status.settings.profile,
                    },
                    position: [0.0; 2],
                },
            ],
            edges: vec![GraphEdge {
                from: source,
                to: target,
            }],
        };
        standalone_graph = graph;
        standalone_execution = ExecutionGraph::new(&standalone_graph.validated()?, target)?;
        &standalone_execution
    } else {
        execution.context("Missing admitted graph execution")?
    };
    let graph = execution.graph;
    let target = execution.target;
    let order = &execution.order;
    let mut active_node = None;
    let outcome = (|| {
        let rate = renderer.format().sample_rate;
        let layout = Arc::clone(&renderer.format().layout);
        let channels = renderer.format().channels();
        let estimated_frames = renderer.format().estimated_frames();
        let mut profiles = BTreeMap::new();
        for (index, &id) in order.iter().enumerate() {
            active_node = None;
            check_cancel(cancel)?;
            if !standalone {
                progress.report(
                    ProcessingPhase::Processing,
                    index as u64,
                    Some(order.len() as u64),
                );
                node_event(status, id, ProcessingPhase::Processing, None, tx);
            }
            active_node = Some(id);
            if let GraphNodeKind::Denoise { config, profile } = &graph.node(id)?.kind
                && config.mode == DenoiseMode::Profile
            {
                let selection = profile
                    .context("Profile mode requires an explicit selected noise-only range")?;
                selection.validate()?;
                let start = (selection.start_seconds * f64::from(rate)).floor() as u64;
                let end = (selection.end_seconds * f64::from(rate)).floor() as u64;
                ensure!(
                    start < end,
                    "Noise profile selection contains no source frames"
                );
                let mut engine = Engine::for_decoder(rate, Arc::clone(&layout));
                let upstream = engine.compile(
                    graph,
                    input_port(graph, id)?,
                    &BTreeMap::new(),
                    &profiles,
                    cancel,
                )?;
                let offset = engine.nodes[upstream].latency;
                let begin = start
                    .checked_add(offset)
                    .context("Profile offset overflow")?;
                let finish = end.checked_add(offset).context("Profile offset overflow")?;
                let mut collector = ProfileCollector::new(
                    rate,
                    usize::from(channels),
                    config.fft_size,
                    end - start,
                )?;
                let source_frames = engine.decode(
                    upstream,
                    reopen()?,
                    Some(finish),
                    cancel,
                    |position, samples, _| {
                        let from = position.max(begin);
                        let to =
                            (position + (samples.len() / usize::from(channels)) as u64).min(finish);
                        if from < to {
                            collector.push_block(
                                &samples[(from - position) as usize * usize::from(channels)
                                    ..(to - position) as usize * usize::from(channels)],
                                &mut || cancel.load(Ordering::Acquire),
                            )?;
                        }
                        Ok(())
                    },
                )?;
                ensure!(
                    end <= source_frames,
                    "Noise profile selection lies outside the source"
                );
                profiles.insert(id, collector.finish()?);
            }
        }
        // Compilation, decoding, and publication execute the requested output;
        // profile collection and validation instead belong to their own node.
        active_node = Some(target.node);
        let mut engine = Engine::for_decoder(rate, Arc::clone(&layout));
        let index = engine.compile(graph, target, &BTreeMap::new(), &profiles, cancel)?;
        let latency = engine.nodes[index].latency;
        drop(profiles);
        let destination = status
            .settings
            .graph_export_path
            .as_ref()
            .filter(|_| status.settings.graph_mode == Some(GraphExecutionMode::Offline));
        let (frames, output, pending_export) = if let Some(destination) = destination {
            let mut frames = None;
            let mut written = 0_u64;
            let pending = super::export::prepare_streaming_export(
                rate,
                &layout,
                destination,
                cancel,
                |writer| {
                    frames = Some(engine.decode(
                        index,
                        renderer,
                        None,
                        cancel,
                        |position, samples, source_frames| {
                            let block = causal_export_block(
                                position,
                                samples,
                                usize::from(channels),
                                latency,
                                source_frames,
                            )?;
                            writer.write_frames(block)?;
                            written = written
                                .checked_add((block.len() / usize::from(channels)) as u64)
                                .context("Graph export length overflow")?;
                            progress.report(ProcessingPhase::Processing, written, source_frames);
                            Ok(())
                        },
                    )?);
                    ensure!(
                        frames == Some(written),
                        "Graph export does not cover source timeline"
                    );
                    Ok(())
                },
            )?;
            (
                frames.context("Missing decoded source frame count")?,
                None,
                Some(pending),
            )
        } else {
            let start_frame = if standalone {
                0
            } else {
                -i64::try_from(latency)?
            };
            let mut builder = audio::PcmBuilder::new(rate, layout, budget)?;
            let frames =
                engine.decode(index, renderer, None, cancel, |position, block, frames| {
                    if standalone {
                        let source_position = frames.unwrap_or_else(|| {
                            position + (block.len() / usize::from(channels)) as u64
                        });
                        progress.report(
                            ProcessingPhase::Processing,
                            source_position,
                            frames.or(estimated_frames),
                        );
                    }
                    let block = if standalone {
                        causal_export_block(
                            position,
                            block,
                            usize::from(channels),
                            latency,
                            frames,
                        )?
                    } else {
                        block
                    };
                    builder.push(block)
                })?;
            let output = GraphPcm {
                pcm: builder.finish()?,
                start_frame,
            };
            (frames, Some(output), None)
        };
        for &id in order {
            active_node = Some(id);
            if let GraphNodeKind::Denoise {
                profile: Some(selection),
                ..
            } = &graph.node(id)?.kind
            {
                selection.validate()?;
                ensure!(
                    (selection.end_seconds * f64::from(rate)).floor() <= frames as f64,
                    "Noise profile selection lies outside the source"
                );
            }
        }
        active_node = Some(target.node);
        status.frames = Some(frames);
        let mut denoised = None;
        let outputs = if standalone {
            let output = output.context("Missing standalone denoise output")?;
            ensure!(
                output.pcm.frames() == frames,
                "Denoise output length mismatch"
            );
            denoised = Some(output.pcm);
            BTreeMap::new()
        } else if let Some(output) = output {
            publish(output, frames, target, order, status, tx)?
        } else {
            status.graph_outputs.clear();
            for &id in order {
                node_event(status, id, ProcessingPhase::Complete, None, tx);
            }
            BTreeMap::new()
        };
        Ok(NativeGraphOutput {
            outputs,
            denoised,
            pending_export,
        })
    })();
    if let Err(error) = &outcome
        && !standalone
        && let Some(id) = active_node
    {
        node_failure(status, id, error, cancel, tx);
    }
    outcome
}

fn node_failure(
    status: &mut ProcessingTaskStatus,
    node: u64,
    error: &anyhow::Error,
    cancel: &AtomicBool,
    tx: &Sender<Event>,
) {
    node_event(
        status,
        node,
        if cancel.load(Ordering::Acquire) {
            ProcessingPhase::Cancelled
        } else {
            ProcessingPhase::Failed
        },
        Some(format!("{error:#}")),
        tx,
    );
}

fn node_event(
    status: &mut ProcessingTaskStatus,
    node: u64,
    phase: ProcessingPhase,
    error: Option<String>,
    tx: &Sender<Event>,
) {
    let entry = status
        .graph_nodes
        .iter_mut()
        .find(|entry| entry.node == node)
        .expect("admitted node");
    entry.phase = phase;
    entry.error = error;
    let event = Event::GraphNode {
        id: status.id,
        status: entry.clone(),
    };
    // Node events are stage transitions, never per-block progress snapshots.
    let _ = tx.send_blocking(event);
}

pub(super) fn plan_info(runner: &audio::GraphRunner) -> GraphPlanInfo {
    GraphPlanInfo {
        estimated_state_bytes: runner.estimated_state_bytes() as u64,
        estimated_work_per_frame: runner.estimated_work_per_frame(),
        latency_frames: runner.latency_frames() as u64,
        tail_frames: runner.tail_frames() as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admitted_native_mix_matches_offline_single_rounding_of_tiny_branches() {
        let mut graph = chain(vec![
            GraphNodeKind::Mix {
                gains_db: vec![-75.0; 2],
                muted: vec![false, true],
                solo: vec![false; 2],
            },
            GraphNodeKind::Mix {
                gains_db: vec![-75.0; 2],
                muted: vec![false, true],
                solo: vec![false; 2],
            },
            GraphNodeKind::Mix {
                gains_db: vec![0.0; 3],
                muted: vec![false; 3],
                solo: vec![false; 3],
            },
        ]);
        graph.edges[2].from = GraphPort { node: 1, port: 0 };
        for node in [3, 4] {
            graph.edges.push(GraphEdge {
                from: GraphPort { node: 1, port: 0 },
                to: GraphPort { node, port: 1 },
            });
        }
        for port in [1, 2] {
            graph.edges.push(GraphEdge {
                from: GraphPort { node: 4, port: 0 },
                to: GraphPort { node: 5, port },
            });
        }
        let budget = audio::PcmBudget::new(MEMORY_LIMIT);
        let input = pcm(&vec![1.0; BLOCK * 2 + 3], &budget);
        let offline = samples(&render(&graph, &input, &budget).pcm);
        let mut native =
            audio::GraphRunner::plan(&graph, GraphPort { node: 2, port: 0 }, None, 48000, 1)
                .unwrap()
                .prepare()
                .unwrap();
        let mut actual = vec![1.0; offline.len()];
        for block in actual.chunks_mut(17) {
            native.process_interleaved(block).unwrap();
        }
        assert_eq!(actual, offline);
        assert!(
            offline
                .iter()
                .all(|sample| sample.to_bits() == 1.0f32.to_bits() + 1)
        );
    }

    fn chain(kinds: Vec<GraphNodeKind>) -> ProcessingGraph {
        let mut graph = ProcessingGraph::default();
        graph.edges.clear();
        let mut from = GraphPort { node: 1, port: 0 };
        for (index, kind) in kinds.into_iter().enumerate() {
            let id = index as u64 + 3;
            graph.nodes.push(GraphNode {
                id,
                kind,
                position: [0.0; 2],
            });
            let to = GraphPort { node: id, port: 0 };
            graph.edges.push(GraphEdge { from, to });
            from = to;
            graph.next_id = id + 1;
        }
        graph.edges.push(GraphEdge {
            from,
            to: GraphPort { node: 2, port: 0 },
        });
        graph
    }
    fn pcm(samples: &[f32], budget: &Arc<audio::PcmBudget>) -> audio::PcmAudio {
        let mut builder = audio::PcmBuilder::new(
            48000,
            Arc::from([audio::Channel::FrontCenter]),
            Arc::clone(budget),
        )
        .unwrap();
        builder.push(samples).unwrap();
        builder.finish().unwrap()
    }
    fn samples(pcm: &audio::PcmAudio) -> Vec<f32> {
        let mut samples = vec![0.0; pcm.frames() as usize];
        assert_eq!(pcm.read_frames(0, &mut samples), samples.len());
        samples
    }
    fn render(
        graph: &ProcessingGraph,
        input: &audio::PcmAudio,
        budget: &Arc<audio::PcmBudget>,
    ) -> GraphPcm {
        let mut engine = Engine::new(input);
        let index = engine
            .compile(
                graph,
                GraphPort { node: 2, port: 0 },
                &BTreeMap::new(),
                &BTreeMap::new(),
                &AtomicBool::new(false),
            )
            .unwrap();
        engine
            .render(index, Arc::clone(budget), &AtomicBool::new(false))
            .unwrap()
    }

    #[test]
    fn model_input_cut_preserves_other_separation_output() {
        let data = vec![0.125; 40000];
        let layout: Arc<[audio::Channel]> =
            Arc::from([audio::Channel::FrontLeft, audio::Channel::FrontRight]);
        let make_pcm = |budget: &Arc<audio::PcmBudget>| {
            let mut builder =
                audio::PcmBuilder::new(48000, Arc::clone(&layout), Arc::clone(budget)).unwrap();
            builder.push(&data).unwrap();
            builder.finish().unwrap()
        };
        let probe_budget = audio::PcmBudget::new(MEMORY_LIMIT);
        let bytes = make_pcm(&probe_budget).retained_bytes();
        let budget = audio::PcmBudget::new(3 * bytes);
        let mut graph = chain(vec![
            GraphNodeKind::Separate {
                config: Default::default(),
            },
            GraphNodeKind::Equalizer {
                config: rivu_dsp::equalizer::config::EqualizerConfig {
                    enabled: true,
                    ..Default::default()
                },
                implementation: EqImplementation::Iir,
            },
            GraphNodeKind::Speech {
                config: Default::default(),
            },
            GraphNodeKind::Mix {
                gains_db: vec![0.0; 2],
                muted: vec![false; 2],
                solo: vec![false; 2],
            },
        ]);
        graph.edges.push(GraphEdge {
            from: GraphPort { node: 3, port: 1 },
            to: GraphPort { node: 6, port: 1 },
        });
        let mut boundaries = BTreeMap::from([
            (
                GraphPort { node: 3, port: 0 },
                GraphPcm {
                    pcm: make_pcm(&budget),
                    start_frame: 0,
                },
            ),
            (
                GraphPort { node: 3, port: 1 },
                GraphPcm {
                    pcm: make_pcm(&budget),
                    start_frame: 0,
                },
            ),
        ]);
        let cancel = AtomicBool::new(false);
        let mut engine = Engine::for_retained(None, 48000, Arc::clone(&layout));
        let index = engine
            .compile(
                &graph,
                GraphPort { node: 4, port: 0 },
                &boundaries,
                &BTreeMap::new(),
                &cancel,
            )
            .unwrap();
        let upstream = engine.render(index, Arc::clone(&budget), &cancel).unwrap();
        assert_eq!(budget.used_bytes(), 3 * bytes);
        release_unreachable_pcm(
            &graph,
            GraphPort { node: 2, port: 0 },
            &mut boundaries,
            Some(5),
            &mut None,
        )
        .unwrap();
        assert_eq!(budget.used_bytes(), 2 * bytes);
        let output = make_pcm(&budget);
        let mut actual = vec![0.0; data.len()];
        assert_eq!(
            boundaries[&GraphPort { node: 3, port: 1 }]
                .pcm
                .read_frames(0, &mut actual),
            20000
        );
        assert_eq!(actual, data);
        assert!(!boundaries.contains_key(&GraphPort { node: 3, port: 0 }));
        drop(output);
        drop(upstream);
        drop(boundaries);
        assert_eq!(budget.used_bytes(), 0);
    }

    #[test]
    fn model_input_cut_releases_only_unreachable_boundary_storage() {
        for alias in [false, true] {
            for bypass in [false, true] {
                let data = vec![0.125; 20000];
                let probe_budget = audio::PcmBudget::new(MEMORY_LIMIT);
                let bytes = pcm(&data, &probe_budget).retained_bytes();
                let budget = audio::PcmBudget::new(if bypass { 3 } else { 2 } * bytes);
                let old = pcm(&data, &budget);
                let rate = old.sample_rate();
                let layout = Arc::clone(old.layout());
                let mut graph = chain(vec![
                    GraphNodeKind::Speech {
                        config: Default::default(),
                    },
                    GraphNodeKind::Equalizer {
                        config: rivu_dsp::equalizer::config::EqualizerConfig {
                            enabled: !alias,
                            ..Default::default()
                        },
                        implementation: EqImplementation::Iir,
                    },
                    GraphNodeKind::Speech {
                        config: Default::default(),
                    },
                ]);
                if bypass {
                    graph.nodes.push(GraphNode {
                        id: 6,
                        kind: GraphNodeKind::Mix {
                            gains_db: vec![0.0; 2],
                            muted: vec![false; 2],
                            solo: vec![false; 2],
                        },
                        position: [0.0; 2],
                    });
                    graph.edges.last_mut().unwrap().from = GraphPort { node: 6, port: 0 };
                    for (node, port) in [(5, 0), (3, 1)] {
                        graph.edges.push(GraphEdge {
                            from: GraphPort { node, port: 0 },
                            to: GraphPort { node: 6, port },
                        });
                    }
                }
                let boundary = GraphPort { node: 3, port: 0 };
                let mut boundaries = BTreeMap::from([(
                    boundary,
                    GraphPcm {
                        pcm: old,
                        start_frame: 0,
                    },
                )]);
                let cancel = AtomicBool::new(false);
                let mut engine = Engine::for_retained(None, rate, layout);
                let index = engine
                    .compile(
                        &graph,
                        GraphPort { node: 4, port: 0 },
                        &boundaries,
                        &BTreeMap::new(),
                        &cancel,
                    )
                    .unwrap();
                let upstream = engine.render(index, Arc::clone(&budget), &cancel).unwrap();
                release_unreachable_pcm(
                    &graph,
                    GraphPort { node: 2, port: 0 },
                    &mut boundaries,
                    Some(5),
                    &mut None,
                )
                .unwrap();
                assert_eq!(
                    budget.used_bytes(),
                    if bypass && !alias { 2 } else { 1 } * bytes
                );
                let output = pcm(&data, &budget);
                assert_eq!(samples(&upstream.pcm), data);
                if bypass {
                    assert_eq!(samples(&boundaries[&boundary].pcm), data);
                }
                drop(output);
                drop(upstream);
                drop(boundaries);
                assert_eq!(budget.used_bytes(), 0);
            }
        }
    }

    #[test]
    fn replacement_boundaries_release_original_before_final_pcm_allocation() {
        let data = vec![0.125; 20000];
        let probe_budget = audio::PcmBudget::new(MEMORY_LIMIT);
        let probe = pcm(&data, &probe_budget);
        let bytes = probe.retained_bytes();
        let budget = audio::PcmBudget::new(2 * bytes);
        let input = pcm(&data, &budget);
        let rate = input.sample_rate();
        let layout = Arc::clone(input.layout());
        let frames = input.frames();
        let iir = || GraphNodeKind::Equalizer {
            config: rivu_dsp::equalizer::config::EqualizerConfig {
                enabled: true,
                ..Default::default()
            },
            implementation: EqImplementation::Iir,
        };
        let graph = chain(vec![
            GraphNodeKind::Speech {
                config: Default::default(),
            },
            iir(),
        ]);
        let target = GraphPort { node: 2, port: 0 };
        let boundary = GraphPort { node: 3, port: 0 };
        let rendered = render(&chain(vec![iir()]), &input, &budget);
        assert_eq!(budget.used_bytes(), 2 * bytes);
        let mut input = Some(input);
        release_unreachable_source(&graph, target, &BTreeMap::new(), Some(3), &mut input).unwrap();
        assert!(input.is_none());
        assert_eq!(budget.used_bytes(), bytes);
        let boundaries = BTreeMap::from([(boundary, rendered)]);
        let mut engine = Engine::for_retained(input.as_ref(), rate, Arc::clone(&layout));
        let index = engine
            .compile(
                &graph,
                target,
                &boundaries,
                &BTreeMap::new(),
                &AtomicBool::new(false),
            )
            .unwrap();
        drop(boundaries);
        let output = engine
            .render(index, Arc::clone(&budget), &AtomicBool::new(false))
            .unwrap();
        assert_eq!(budget.used_bytes(), bytes);
        assert_eq!(output.pcm.frames(), frames);
        assert_eq!(output.pcm.sample_rate(), rate);
        assert_eq!(output.pcm.layout(), &layout);
        assert_eq!(output.start_frame, 0);
        assert_eq!(samples(&output.pcm), data);
    }

    #[test]
    fn bypass_boundary_retains_aliased_source_and_downstream_profile_input() {
        let budget = audio::PcmBudget::new(MEMORY_LIMIT);
        let input = pcm(&vec![0.125; 20000], &budget);
        let bytes = budget.used_bytes();
        let rate = input.sample_rate();
        let layout = Arc::clone(input.layout());
        let graph = chain(vec![
            GraphNodeKind::Speech {
                config: Default::default(),
            },
            GraphNodeKind::Denoise {
                config: rivu_dsp::denoise::DenoiseConfig {
                    mode: DenoiseMode::Profile,
                    wet: 0.0,
                    ..Default::default()
                },
                profile: Some(NoiseSelection {
                    start_seconds: 0.0,
                    end_seconds: 0.02,
                }),
            },
        ]);
        let target = GraphPort { node: 2, port: 0 };
        let mut boundaries = BTreeMap::from([(
            GraphPort { node: 3, port: 0 },
            GraphPcm {
                pcm: input.clone(),
                start_frame: 0,
            },
        )]);
        let mut input = Some(input);
        release_unreachable_source(&graph, target, &boundaries, None, &mut input).unwrap();
        assert!(input.is_none());
        assert_eq!(budget.used_bytes(), bytes);
        let cancel = AtomicBool::new(false);
        let mut engine = Engine::for_retained(None, rate, Arc::clone(&layout));
        let upstream = engine
            .compile(
                &graph,
                input_port(&graph, 4).unwrap(),
                &boundaries,
                &BTreeMap::new(),
                &cancel,
            )
            .unwrap();
        let fft_size = match &graph.node(4).unwrap().kind {
            GraphNodeKind::Denoise { config, .. } => config.fft_size,
            _ => unreachable!(),
        };
        let mut collector = ProfileCollector::new(rate, 1, fft_size, 960).unwrap();
        engine
            .stream(upstream, Some(960), &cancel, |_, block| {
                collector.push_block(block, &mut || false)
            })
            .unwrap();
        let profiles = BTreeMap::from([(4, collector.finish().unwrap())]);
        let mut engine = Engine::for_retained(None, rate, Arc::clone(&layout));
        let index = engine
            .compile(&graph, target, &boundaries, &profiles, &cancel)
            .unwrap();
        boundaries.clear();
        let output = engine.render(index, Arc::clone(&budget), &cancel).unwrap();
        assert_eq!(budget.used_bytes(), bytes);
        assert_eq!(samples(&output.pcm), vec![0.125; 20000]);
    }

    #[test]
    fn unprocessed_source_branch_stays_live_until_its_last_replacement() {
        let budget = audio::PcmBudget::new(MEMORY_LIMIT);
        let mut input = Some(pcm(&[0.25; 1000], &budget));
        let mut graph = chain(vec![
            GraphNodeKind::Speech {
                config: Default::default(),
            },
            GraphNodeKind::Mix {
                gains_db: vec![0.0; 2],
                muted: vec![false; 2],
                solo: vec![false; 2],
            },
        ]);
        graph.edges.push(GraphEdge {
            from: GraphPort { node: 1, port: 0 },
            to: GraphPort { node: 4, port: 1 },
        });
        let boundaries = BTreeMap::from([(
            GraphPort { node: 3, port: 0 },
            GraphPcm {
                pcm: pcm(&[0.5; 1000], &budget),
                start_frame: 0,
            },
        )]);
        release_unreachable_source(
            &graph,
            GraphPort { node: 2, port: 0 },
            &boundaries,
            None,
            &mut input,
        )
        .unwrap();
        assert!(input.is_some());
        let mut engine = Engine::for_retained(
            input.as_ref(),
            48000,
            Arc::from([audio::Channel::FrontCenter]),
        );
        let index = engine
            .compile(
                &graph,
                GraphPort { node: 2, port: 0 },
                &boundaries,
                &BTreeMap::new(),
                &AtomicBool::new(false),
            )
            .unwrap();
        let output = engine
            .render(index, Arc::clone(&budget), &AtomicBool::new(false))
            .unwrap();
        assert_eq!(samples(&output.pcm), vec![0.75; 1000]);
    }

    #[test]
    fn bypass_aliases_charge_once_and_native_chains_retain_only_final_pcm() {
        let budget = audio::PcmBudget::new(MEMORY_LIMIT);
        let input = pcm(&vec![0.125; 20000], &budget);
        let source_bytes = budget.used_bytes();
        let bypass = chain(vec![
            GraphNodeKind::Equalizer {
                config: Default::default(),
                implementation: EqImplementation::LinearPhase { taps: 65 },
            },
            GraphNodeKind::Denoise {
                config: Default::default(),
                profile: None,
            },
        ]);
        let mut engine = Engine::new(&input);
        let target = engine
            .compile(
                &bypass,
                GraphPort { node: 2, port: 0 },
                &BTreeMap::new(),
                &BTreeMap::new(),
                &AtomicBool::new(false),
            )
            .unwrap();
        let mut streamed = Vec::new();
        engine
            .stream(target, None, &AtomicBool::new(false), |_, block| {
                streamed.extend_from_slice(block);
                Ok(())
            })
            .unwrap();
        assert_eq!(streamed, samples(&input));
        let output = render(&bypass, &input, &budget);
        assert_eq!(budget.used_bytes(), source_bytes);
        assert_eq!(samples(&output.pcm), samples(&input));
        drop(output);
        let iir = || GraphNodeKind::Equalizer {
            config: rivu_dsp::equalizer::config::EqualizerConfig {
                enabled: true,
                ..Default::default()
            },
            implementation: EqImplementation::Iir,
        };
        let graph = chain(vec![iir(), iir(), iir()]);
        let output = render(&graph, &input, &budget);
        assert_eq!(
            budget.used_bytes(),
            source_bytes + output.pcm.retained_bytes()
        );
        assert_eq!(samples(&output.pcm), samples(&input));
        drop(output);
        assert_eq!(budget.used_bytes(), source_bytes);
    }

    fn mix_graph(gains_db: Vec<f32>, muted: Vec<bool>, solo: Vec<bool>) -> ProcessingGraph {
        let mut graph = chain(vec![GraphNodeKind::Mix {
            gains_db,
            muted,
            solo,
        }]);
        graph.edges.push(GraphEdge {
            from: GraphPort { node: 1, port: 0 },
            to: GraphPort { node: 3, port: 1 },
        });
        graph
    }

    #[test]
    fn sequential_mix_nodes_reuse_one_accumulator_without_leaking_previous_sums() {
        let mut graph = chain(vec![
            GraphNodeKind::Mix {
                gains_db: vec![0.0; 2],
                muted: vec![false; 2],
                solo: vec![false; 2],
            },
            GraphNodeKind::Mix {
                gains_db: vec![0.0; 2],
                muted: vec![false; 2],
                solo: vec![false; 2],
            },
        ]);
        for node in [3, 4] {
            graph.edges.push(GraphEdge {
                from: GraphPort { node: 1, port: 0 },
                to: GraphPort { node, port: 1 },
            });
        }
        let budget = audio::PcmBudget::new(MEMORY_LIMIT);
        let data: Vec<_> = (0..BLOCK * 2 + 7).map(|i| (i % 17) as f32 / 16.0).collect();
        let input = pcm(&data, &budget);
        let mut engine = Engine::new(&input);
        let target = engine
            .compile(
                &graph,
                GraphPort { node: 2, port: 0 },
                &BTreeMap::new(),
                &BTreeMap::new(),
                &AtomicBool::new(false),
            )
            .unwrap();
        engine.sum.fill(f64::NAN);
        let output = engine
            .render(target, budget, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(
            samples(&output.pcm),
            data.iter().map(|x| x * 3.0).collect::<Vec<_>>()
        );
    }

    #[test]
    fn unequal_mix_branches_zero_extend_after_reusing_output_buffers() {
        let budget = audio::PcmBudget::new(MEMORY_LIMIT);
        let input = pcm(&vec![2.0; BLOCK + 3], &budget);
        let longer = pcm(&vec![1.0; BLOCK * 2 + 7], &budget);
        let mut graph = mix_graph(vec![0.0; 2], vec![false; 2], vec![false; 2]);
        graph.nodes.push(GraphNode {
            id: 4,
            kind: GraphNodeKind::Source,
            position: [0.0; 2],
        });
        graph.edges[0].from = GraphPort { node: 4, port: 0 };
        graph.next_id = 5;
        let boundaries = BTreeMap::from([(
            GraphPort { node: 4, port: 0 },
            GraphPcm {
                pcm: longer,
                start_frame: 0,
            },
        )]);
        let mut engine = Engine::new(&input);
        let target = engine
            .compile(
                &graph,
                GraphPort { node: 2, port: 0 },
                &boundaries,
                &BTreeMap::new(),
                &AtomicBool::new(false),
            )
            .unwrap();
        for node in &mut engine.nodes {
            node.output.fill(f32::NAN);
        }
        engine.sum.fill(f64::NAN);
        let output = engine
            .render(target, budget, &AtomicBool::new(false))
            .unwrap();
        let output = samples(&output.pcm);
        assert_eq!(output.len(), BLOCK * 2 + 7);
        assert!(output[..BLOCK + 3].iter().all(|&sample| sample == 3.0));
        assert!(output[BLOCK + 3..].iter().all(|&sample| sample == 1.0));
    }

    #[test]
    fn source_short_reads_and_decoder_without_input_clear_active_prefix() {
        let budget = audio::PcmBudget::new(MEMORY_LIMIT);
        let input = pcm(&[1.0, 2.0], &budget);
        let graph = chain(Vec::new());
        for streaming in [false, true] {
            let mut engine = if streaming {
                Engine::for_decoder(48000, Arc::clone(input.layout()))
            } else {
                Engine::new(&input)
            };
            let target = engine
                .compile(
                    &graph,
                    GraphPort { node: 2, port: 0 },
                    &BTreeMap::new(),
                    &BTreeMap::new(),
                    &AtomicBool::new(false),
                )
                .unwrap();
            for node in &mut engine.nodes {
                // Exercise the reader's short-read contract independently of
                // the compiled source length and poison previously used data.
                node.frames = 4;
                node.input_frames = 4;
                node.output.fill(f32::NAN);
            }
            engine
                .round(0, 4, None, None, &AtomicBool::new(false))
                .unwrap();
            let expected = if streaming {
                [0.0; 4]
            } else {
                [1.0, 2.0, 0.0, 0.0]
            };
            let physical = engine.nodes[target].output_node;
            assert_eq!(&engine.nodes[physical].output[..4], &expected);
        }
    }

    #[test]
    fn mix_preserves_gain_solo_and_unclipped_pcm() {
        let budget = audio::PcmBudget::new(MEMORY_LIMIT);
        let input = pcm(&[0.75, -0.75, 0.0], &budget);
        let output = render(
            &mix_graph(vec![0.0; 2], vec![false; 2], vec![false; 2]),
            &input,
            &budget,
        );
        assert_eq!(samples(&output.pcm), [1.5, -1.5, 0.0]);
        let output = render(
            &mix_graph(vec![0.0, -6.0206], vec![false; 2], vec![false, true]),
            &input,
            &budget,
        );
        let actual = samples(&output.pcm);
        assert_eq!(actual.len(), 3);
        for (actual, expected) in actual.into_iter().zip([0.375, -0.375, 0.0]) {
            assert!((actual - expected).abs() < 1e-6);
        }
        let output = render(
            &mix_graph(vec![0.0; 2], vec![false, true], vec![false, true]),
            &input,
            &budget,
        );
        assert_eq!(samples(&output.pcm), [0.0; 3]);
    }

    #[test]
    fn mix_retains_union_of_guarded_source_relative_support() {
        let budget = audio::PcmBudget::new(MEMORY_LIMIT);
        let input = pcm(&[10.0, 20.0], &budget);
        let guarded = pcm(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], &budget);
        let mut graph = mix_graph(vec![0.0; 2], vec![false; 2], vec![false; 2]);
        graph.nodes.push(GraphNode {
            id: 4,
            kind: GraphNodeKind::Source,
            position: [0.0; 2],
        });
        graph.next_id = 5;
        graph.edges[0].from = GraphPort { node: 4, port: 0 };
        let boundaries = BTreeMap::from([(
            GraphPort { node: 4, port: 0 },
            GraphPcm {
                pcm: guarded,
                start_frame: -2,
            },
        )]);
        let mut engine = Engine::new(&input);
        let index = engine
            .compile(
                &graph,
                GraphPort { node: 2, port: 0 },
                &boundaries,
                &BTreeMap::new(),
                &AtomicBool::new(false),
            )
            .unwrap();
        let output = engine
            .render(index, budget, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(output.start_frame, -2);
        assert_eq!(samples(&output.pcm), [1.0, 2.0, 13.0, 24.0, 5.0, 6.0]);
    }

    #[test]
    fn cancellation_after_first_decoded_block_releases_partial_pcm_and_allows_retry() {
        let budget = audio::PcmBudget::new(MEMORY_LIMIT);
        let data: Vec<f32> = (0..1401).map(|i| (i as f32 * 0.017).sin() * 0.3).collect();
        let input = pcm(&data, &budget);
        let before = budget.used_bytes();
        let graph = chain(vec![GraphNodeKind::Equalizer {
            config: rivu_dsp::equalizer::config::EqualizerConfig {
                enabled: true,
                preamp_db: 6.020599913,
                ..Default::default()
            },
            implementation: EqImplementation::Iir,
        }]);
        let cancel = AtomicBool::new(true);
        let mut engine = Engine::for_decoder(48000, Arc::clone(input.layout()));
        assert!(
            engine
                .compile(
                    &graph,
                    GraphPort { node: 2, port: 0 },
                    &BTreeMap::new(),
                    &BTreeMap::new(),
                    &cancel,
                )
                .is_err()
        );
        cancel.store(false, Ordering::Release);
        let metadata = fs::metadata(std::env::current_exe().unwrap()).unwrap();
        let renderer = audio::SourceRenderer::from_pcm(
            input.clone(),
            audio::source_identity_from_metadata(&metadata),
        )
        .unwrap();
        let target = engine
            .compile(
                &graph,
                GraphPort { node: 2, port: 0 },
                &BTreeMap::new(),
                &BTreeMap::new(),
                &cancel,
            )
            .unwrap();
        let mut blocks = 0;
        let result: Result<audio::PcmAudio> = (|| {
            let mut builder =
                audio::PcmBuilder::new(48000, Arc::clone(input.layout()), Arc::clone(&budget))?;
            engine.decode(target, renderer, None, &cancel, |_, block, _| {
                builder.push(block)?;
                blocks += 1;
                cancel.store(true, Ordering::Release);
                Ok(())
            })?;
            builder.finish()
        })();
        assert!(result.is_err());
        assert_eq!(blocks, 1);
        assert_eq!(budget.used_bytes(), before);
        let output = render(&graph, &input, &budget);
        let actual = samples(&output.pcm);
        assert_eq!(actual.len(), data.len());
        for (actual, source) in actual.into_iter().zip(data) {
            assert!((actual - source * 2.0).abs() < 1e-6);
        }
    }

    #[test]
    fn offline_fir_retains_original_timeline_including_last_frame() {
        let budget = audio::PcmBudget::new(MEMORY_LIMIT);
        let mut expected = vec![0.0; 513];
        expected[0] = 0.5;
        expected[512] = -0.5;
        let input = pcm(&expected, &budget);
        let config = rivu_dsp::equalizer::config::EqualizerConfig {
            enabled: true,
            ..Default::default()
        };
        let graph = chain(vec![GraphNodeKind::Equalizer {
            config,
            implementation: EqImplementation::LinearPhase { taps: 65 },
        }]);
        let output = render(&graph, &input, &budget);
        assert!(output.start_frame < 0);
        assert!(output.pcm.frames() > input.frames());
        let cropped = output
            .pcm
            .slice_frames(output.start_frame.unsigned_abs(), input.frames())
            .unwrap();
        for (a, e) in samples(&cropped).into_iter().zip(expected) {
            assert!((a - e).abs() < 1e-5);
        }
    }

    #[test]
    fn native_graph_preserves_speaker_layout_and_sample_order() {
        let budget = audio::PcmBudget::new(MEMORY_LIMIT);
        let layout: Arc<[audio::Channel]> =
            Arc::from([audio::Channel::SideRight, audio::Channel::SideLeft]);
        let data = [0.125, -0.25, 0.5, 0.75, -0.1, 0.2];
        let mut builder =
            audio::PcmBuilder::new(48000, Arc::clone(&layout), Arc::clone(&budget)).unwrap();
        builder.push(&data).unwrap();
        let input = builder.finish().unwrap();
        let graph = chain(vec![GraphNodeKind::Equalizer {
            config: rivu_dsp::equalizer::config::EqualizerConfig {
                enabled: true,
                preamp_db: 6.020599913,
                ..Default::default()
            },
            implementation: EqImplementation::Iir,
        }]);
        let output = render(&graph, &input, &budget);
        assert_eq!(output.pcm.layout(), &layout);
        let mut actual = [0.0; 6];
        assert_eq!(output.pcm.read_frames(0, &mut actual), 3);
        for (&actual, &expected) in actual.iter().zip(&data) {
            assert!((actual - expected * 2.0).abs() < 1e-6);
        }
        let metadata = fs::metadata(std::env::current_exe().unwrap()).unwrap();
        let renderer =
            audio::SourceRenderer::from_pcm(input, audio::source_identity_from_metadata(&metadata))
                .unwrap();
        let cancel = AtomicBool::new(false);
        let mut engine = Engine::for_decoder(48000, Arc::clone(&renderer.format().layout));
        let target = engine
            .compile(
                &graph,
                GraphPort { node: 2, port: 0 },
                &BTreeMap::new(),
                &BTreeMap::new(),
                &cancel,
            )
            .unwrap();
        let mut builder =
            audio::PcmBuilder::new(48000, Arc::clone(&renderer.format().layout), budget).unwrap();
        engine
            .decode(target, renderer, None, &cancel, |_, block, _| {
                builder.push(block)
            })
            .unwrap();
        let decoded = builder.finish().unwrap();
        assert_eq!(decoded.layout(), &layout);
        let mut streamed = [0.0; 6];
        assert_eq!(decoded.read_frames(0, &mut streamed), 3);
        assert_eq!(streamed, actual);
    }

    #[test]
    fn output_prefix_matches_full_iir_fir_stft_and_profile_chains() {
        let budget = audio::PcmBudget::new(MEMORY_LIMIT);
        let data: Vec<f32> = (0..4093).map(|i| (i as f32 * 0.037).sin() * 0.3).collect();
        let input = pcm(&data, &budget);
        let cancel = AtomicBool::new(false);
        let eq = rivu_dsp::equalizer::config::EqualizerConfig {
            enabled: true,
            preamp_db: 3.0,
            ..Default::default()
        };
        let denoise = rivu_dsp::denoise::DenoiseConfig {
            mode: DenoiseMode::Adaptive,
            fft_size: 256,
            ..Default::default()
        };
        let kinds = [
            GraphNodeKind::Equalizer {
                config: eq.clone(),
                implementation: EqImplementation::Iir,
            },
            GraphNodeKind::Equalizer {
                config: eq,
                implementation: EqImplementation::LinearPhase { taps: 65 },
            },
            GraphNodeKind::Denoise {
                config: denoise.clone(),
                profile: None,
            },
            GraphNodeKind::Denoise {
                config: rivu_dsp::denoise::DenoiseConfig {
                    mode: DenoiseMode::Profile,
                    ..denoise
                },
                profile: Some(NoiseSelection {
                    start_seconds: 0.0,
                    end_seconds: 0.02,
                }),
            },
        ];
        let mut collector = ProfileCollector::new(48000, 1, 256, 960).unwrap();
        collector.push_block(&data[..960], &mut || false).unwrap();
        let profiles = BTreeMap::from([(6, collector.finish().unwrap())]);
        let metadata = fs::metadata(std::env::current_exe().unwrap()).unwrap();
        let identity = audio::source_identity_from_metadata(&metadata);
        for length in 1..=kinds.len() {
            let graph = chain(kinds[..length].to_vec());
            let compile = |mut engine: Engine| {
                let target = engine
                    .compile(
                        &graph,
                        GraphPort { node: 2, port: 0 },
                        &BTreeMap::new(),
                        &profiles,
                        &cancel,
                    )
                    .unwrap();
                (engine, target)
            };
            let (engine, target) = compile(Engine::new(&input));
            let full = engine.render(target, Arc::clone(&budget), &cancel).unwrap();
            let reference = samples(&full.pcm);
            let latency = full.start_frame.unsigned_abs();
            for end in [
                latency + 517,
                latency + input.frames(),
                reference.len() as u64 + 17,
            ] {
                let expected = &reference[..(end as usize).min(reference.len())];
                let (mut engine, target) = compile(Engine::new(&input));
                let mut retained = Vec::new();
                engine
                    .stream(target, Some(end), &cancel, |_, block| {
                        retained.extend_from_slice(block);
                        Ok(())
                    })
                    .unwrap();
                assert_eq!(retained, expected);
                let (mut engine, target) =
                    compile(Engine::for_decoder(48000, Arc::clone(input.layout())));
                let renderer = audio::SourceRenderer::from_pcm(input.clone(), identity).unwrap();
                let mut decoded = Vec::new();
                let source_frames = engine
                    .decode(target, renderer, Some(end), &cancel, |_, block, _| {
                        decoded.extend_from_slice(block);
                        Ok(())
                    })
                    .unwrap();
                assert_eq!(source_frames, end.min(input.frames()));
                assert_eq!(decoded.len(), expected.len());
                for (actual, &expected) in decoded.into_iter().zip(expected) {
                    assert!((actual - expected).abs() < 1e-5);
                }
            }
        }
    }

    #[test]
    fn output_prefix_does_not_process_later_dsp_failure() {
        let budget = audio::PcmBudget::new(MEMORY_LIMIT);
        let mut data = vec![0.125; 2048];
        data[1024..].fill(f32::MAX);
        let input = pcm(&data, &budget);
        let graph = chain(vec![GraphNodeKind::Equalizer {
            config: rivu_dsp::equalizer::config::EqualizerConfig {
                enabled: true,
                preamp_db: 24.0,
                ..Default::default()
            },
            implementation: EqImplementation::Iir,
        }]);
        let cancel = AtomicBool::new(false);
        for decoder in [false, true] {
            for prefix in [Some(517), None] {
                let mut engine = if decoder {
                    Engine::for_decoder(48000, Arc::clone(input.layout()))
                } else {
                    Engine::new(&input)
                };
                let target = engine
                    .compile(
                        &graph,
                        GraphPort { node: 2, port: 0 },
                        &BTreeMap::new(),
                        &BTreeMap::new(),
                        &cancel,
                    )
                    .unwrap();
                let mut output = Vec::new();
                let mut consume = |_: u64, block: &[f32]| {
                    output.extend_from_slice(block);
                    Ok(())
                };
                let result = if decoder {
                    let metadata = fs::metadata(std::env::current_exe().unwrap()).unwrap();
                    let renderer = audio::SourceRenderer::from_pcm(
                        input.clone(),
                        audio::source_identity_from_metadata(&metadata),
                    )
                    .unwrap();
                    engine
                        .decode(target, renderer, prefix, &cancel, |position, block, _| {
                            consume(position, block)
                        })
                        .map(|_| ())
                } else {
                    engine.stream(target, prefix, &cancel, &mut consume)
                };
                assert_eq!(result.is_ok(), prefix.is_some());
                if prefix.is_some() {
                    assert_eq!(output.len(), 517);
                    let expected = 0.125 * 10.0_f32.powf(24.0 / 20.0);
                    assert!(
                        output
                            .iter()
                            .all(|&sample| (sample - expected).abs() < 1e-5)
                    );
                }
            }
        }
    }

    #[test]
    fn output_prefix_validates_the_opened_source_before_returning() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.wav");
        let mut writer = hound::WavWriter::create(
            &path,
            hound::WavSpec {
                channels: 1,
                sample_rate: 48000,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            },
        )
        .unwrap();
        for _ in 0..2048 {
            writer.write_sample(0.125_f32).unwrap();
        }
        writer.finalize().unwrap();
        let cancel = AtomicBool::new(false);
        let renderer = audio::SourceRenderer::open(
            &path,
            None,
            rivu_format::decoder_hint::DecoderHint::Native,
            1,
            false,
            &cancel,
        )
        .unwrap();
        let mut engine = Engine::for_decoder(48000, Arc::clone(&renderer.format().layout));
        let target = engine
            .compile(
                &ProcessingGraph::default(),
                GraphPort { node: 2, port: 0 },
                &BTreeMap::new(),
                &BTreeMap::new(),
                &cancel,
            )
            .unwrap();
        let result = engine.decode(target, renderer, Some(97), &cancel, |_, block, _| {
            assert_eq!(block, &[0.125; 97]);
            fs::OpenOptions::new().write(true).open(&path)?.set_len(0)?;
            Ok(())
        });
        assert!(result.is_err());
    }

    #[test]
    fn streamed_export_crop_aligns_short_sources_and_unequal_mix_branches() {
        let mut graph = chain(vec![
            GraphNodeKind::Equalizer {
                config: rivu_dsp::equalizer::config::EqualizerConfig {
                    enabled: true,
                    preamp_db: 3.0,
                    ..Default::default()
                },
                implementation: EqImplementation::LinearPhase { taps: 65 },
            },
            GraphNodeKind::Denoise {
                config: rivu_dsp::denoise::DenoiseConfig {
                    mode: DenoiseMode::Adaptive,
                    fft_size: 256,
                    ..Default::default()
                },
                profile: None,
            },
            GraphNodeKind::Mix {
                gains_db: vec![-3.0, -6.0, -9.0],
                muted: vec![false; 3],
                solo: vec![false; 3],
            },
        ]);
        for (node, port) in [(3, 1), (1, 2)] {
            graph.edges.push(GraphEdge {
                from: GraphPort { node, port: 0 },
                to: GraphPort { node: 5, port },
            });
        }
        let metadata = fs::metadata(std::env::current_exe().unwrap()).unwrap();
        let identity = audio::source_identity_from_metadata(&metadata);
        let cancel = AtomicBool::new(false);
        for length in [1, 63, 257, 4093] {
            let budget = audio::PcmBudget::new(MEMORY_LIMIT);
            let data: Vec<f32> = (0..length)
                .map(|i| (i as f32 * 0.037).cos() * 0.3)
                .collect();
            let input = pcm(&data, &budget);
            let retained = render(&graph, &input, &budget);
            let latency = retained.start_frame.unsigned_abs();
            assert!(latency > 63);
            let expected = samples(&retained.pcm.slice_frames(latency, length as u64).unwrap());
            let mut engine = Engine::for_decoder(48000, Arc::clone(input.layout()));
            let target = engine
                .compile(
                    &graph,
                    GraphPort { node: 2, port: 0 },
                    &BTreeMap::new(),
                    &BTreeMap::new(),
                    &cancel,
                )
                .unwrap();
            assert_eq!(engine.nodes[target].latency, latency);
            let renderer = audio::SourceRenderer::from_pcm(input, identity).unwrap();
            let mut actual = Vec::new();
            let mut tail_blocks = 0;
            let frames = engine
                .decode(target, renderer, None, &cancel, |position, block, eof| {
                    if let Some(frames) = eof {
                        assert_eq!(frames, length as u64);
                        assert!(position >= frames);
                        tail_blocks += 1;
                    } else {
                        assert!(position < length as u64);
                    }
                    actual
                        .extend_from_slice(causal_export_block(position, block, 1, latency, eof)?);
                    Ok(())
                })
                .unwrap();
            assert_eq!(frames, length as u64);
            assert!(tail_blocks > 0);
            assert_eq!(actual.len(), length);
            for (actual, expected) in actual.into_iter().zip(expected) {
                assert!((actual - expected).abs() < 1e-5);
            }
        }
    }

    #[test]
    fn streaming_native_fanout_and_eof_match_realtime_causal_output() {
        let budget = audio::PcmBudget::new(MEMORY_LIMIT);
        let data: Vec<f32> = (0..1401).map(|i| (i as f32 * 0.017).sin() * 0.3).collect();
        let input = pcm(&data, &budget);
        let mut graph = chain(vec![
            GraphNodeKind::Equalizer {
                config: rivu_dsp::equalizer::config::EqualizerConfig {
                    enabled: true,
                    ..Default::default()
                },
                implementation: EqImplementation::LinearPhase { taps: 65 },
            },
            GraphNodeKind::Denoise {
                config: rivu_dsp::denoise::DenoiseConfig {
                    mode: DenoiseMode::Adaptive,
                    fft_size: 256,
                    ..Default::default()
                },
                profile: None,
            },
            GraphNodeKind::Mix {
                gains_db: vec![0.0; 2],
                muted: vec![false; 2],
                solo: vec![false; 2],
            },
        ]);
        graph.edges.push(GraphEdge {
            from: GraphPort { node: 3, port: 0 },
            to: GraphPort { node: 5, port: 1 },
        });
        let output = render(&graph, &input, &budget);
        let metadata = std::fs::metadata(std::env::current_exe().unwrap()).unwrap();
        let identity = audio::source_identity_from_metadata(&metadata);
        let renderer = audio::SourceRenderer::from_pcm(input.clone(), identity).unwrap();
        let mut decoder = Engine::for_decoder(48000, Arc::clone(input.layout()));
        let target = decoder
            .compile(
                &graph,
                GraphPort { node: 2, port: 0 },
                &BTreeMap::new(),
                &BTreeMap::new(),
                &AtomicBool::new(false),
            )
            .unwrap();
        let mut decoded = Vec::new();
        let frames = decoder
            .decode(
                target,
                renderer,
                None,
                &AtomicBool::new(false),
                |_, block, _| {
                    decoded.extend_from_slice(block);
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(frames, input.frames());
        let causal = samples(&output.pcm);
        assert_eq!(decoded.len(), causal.len());
        for (actual, reference) in decoded.into_iter().zip(causal) {
            assert!((actual - reference).abs() < 1e-5);
        }
        let mut live =
            audio::GraphRunner::compile(&graph, GraphPort { node: 2, port: 0 }, 48000, 1).unwrap();
        let mut expected = data;
        live.process_interleaved(&mut expected).unwrap();
        let mut tail = [0.0; 256];
        loop {
            let frames = live.finish_interleaved(&mut tail).unwrap();
            if frames == 0 {
                break;
            }
            expected.extend_from_slice(&tail[..frames]);
        }
        let actual = samples(&output.pcm);
        assert_eq!(actual.len(), expected.len());
        for (a, e) in actual.into_iter().zip(expected) {
            assert!((a - e).abs() < 1e-5);
        }
    }

    #[test]
    fn auto_selects_exact_realtime_hybrid_or_offline_without_downgrading_nodes() {
        let target = GraphPort { node: 2, port: 0 };
        let flag = AtomicBool::new(false);
        let (mode, boundary, runner) = prepare(
            &ProcessingGraph::default().validated().unwrap(),
            target,
            GraphExecutionMode::Auto,
            48000,
            2,
            &flag,
        )
        .unwrap();
        assert_eq!(mode, GraphExecutionMode::Realtime);
        assert!(boundary.is_none());
        assert!(runner.is_some());
        let graph = chain(vec![GraphNodeKind::Speech {
            config: Default::default(),
        }]);
        assert!(
            prepare(
                &graph.validated().unwrap(),
                target,
                GraphExecutionMode::Realtime,
                48000,
                2,
                &flag
            )
            .is_err()
        );
        let (mode, boundary, runner) = prepare(
            &graph.validated().unwrap(),
            target,
            GraphExecutionMode::Auto,
            48000,
            2,
            &flag,
        )
        .unwrap();
        assert_eq!(mode, GraphExecutionMode::Offline);
        assert!(boundary.is_none());
        assert!(runner.is_none());
        let graph = chain(vec![
            GraphNodeKind::Speech {
                config: Default::default(),
            },
            GraphNodeKind::Equalizer {
                config: Default::default(),
                implementation: EqImplementation::Iir,
            },
        ]);
        let (mode, boundary, runner) = prepare(
            &graph.validated().unwrap(),
            target,
            GraphExecutionMode::Auto,
            48000,
            2,
            &flag,
        )
        .unwrap();
        assert_eq!(mode, GraphExecutionMode::Hybrid);
        assert_eq!(boundary, Some(GraphPort { node: 3, port: 0 }));
        assert!(runner.is_some());
        assert!(
            prepare(
                &graph.validated().unwrap(),
                target,
                GraphExecutionMode::Auto,
                48000,
                2,
                &AtomicBool::new(true)
            )
            .is_err()
        );
    }

    #[test]
    fn work_budget_candidates_keep_the_first_exact_admissible_suffix() {
        let graph = chain(vec![
            GraphNodeKind::Equalizer {
                config: rivu_dsp::equalizer::config::EqualizerConfig {
                    enabled: true,
                    ..Default::default()
                },
                implementation: EqImplementation::LinearPhase { taps: 8193 },
            };
            4
        ]);
        let target = GraphPort { node: 2, port: 0 };
        let cancel = AtomicBool::new(false);
        let error = prepare(
            &graph.validated().unwrap(),
            target,
            GraphExecutionMode::Realtime,
            48000,
            2,
            &cancel,
        )
        .unwrap_err();
        assert!(error.to_string().contains("arithmetic work budget"));
        for requested in [GraphExecutionMode::Auto, GraphExecutionMode::Hybrid] {
            let (mode, boundary, runner) = prepare(
                &graph.validated().unwrap(),
                target,
                requested,
                48000,
                2,
                &cancel,
            )
            .unwrap();
            assert_eq!(mode, GraphExecutionMode::Hybrid);
            assert_eq!(boundary, Some(GraphPort { node: 3, port: 0 }));
            let mut runner = runner.unwrap();
            assert_eq!(runner.latency_frames(), 3 * (4096 + BLOCK));
            let mut input = [0.25, -0.5];
            runner.process_interleaved(&mut input).unwrap();
            assert_eq!(input, [0.0; 2]);
        }
    }

    #[test]
    fn zero_wet_profile_still_requires_an_offline_prefix() {
        let graph = chain(vec![
            GraphNodeKind::Denoise {
                config: rivu_dsp::denoise::DenoiseConfig {
                    mode: DenoiseMode::Profile,
                    wet: 0.0,
                    ..Default::default()
                },
                profile: None,
            },
            GraphNodeKind::Equalizer {
                config: Default::default(),
                implementation: EqImplementation::Iir,
            },
        ]);
        let target = GraphPort { node: 2, port: 0 };
        let cancel = AtomicBool::new(false);
        assert!(
            prepare(
                &graph.validated().unwrap(),
                target,
                GraphExecutionMode::Realtime,
                48000,
                2,
                &cancel
            )
            .unwrap_err()
            .to_string()
            .contains("offline profile capture")
        );
        for requested in [GraphExecutionMode::Auto, GraphExecutionMode::Hybrid] {
            let (mode, boundary, runner) = prepare(
                &graph.validated().unwrap(),
                target,
                requested,
                48000,
                2,
                &cancel,
            )
            .unwrap();
            assert_eq!(mode, GraphExecutionMode::Hybrid);
            assert_eq!(boundary, Some(GraphPort { node: 3, port: 0 }));
            let mut runner = runner.unwrap();
            let mut input = [0.25, -0.0];
            let expected = input.map(f32::to_bits);
            runner.process_interleaved(&mut input).unwrap();
            assert_eq!(input.map(f32::to_bits), expected);
        }
    }

    #[test]
    fn offline_zero_wet_profile_validates_before_exact_mono_stereo_bypass() {
        use rivu_dsp::denoise::DenoiseConfig;
        for layout in [
            Arc::<[audio::Channel]>::from([audio::Channel::FrontCenter]),
            Arc::<[audio::Channel]>::from([audio::Channel::FrontLeft, audio::Channel::FrontRight]),
        ] {
            let budget = audio::PcmBudget::new(MEMORY_LIMIT);
            let mut builder =
                audio::PcmBuilder::new(48000, Arc::clone(&layout), Arc::clone(&budget)).unwrap();
            let expected = [-0.0, 0.25, -0.5, 0.0];
            builder.push(&expected).unwrap();
            let input = builder.finish().unwrap();
            let config = DenoiseConfig {
                mode: DenoiseMode::Profile,
                fft_size: 256,
                wet: 0.0,
                ..Default::default()
            };
            let graph = chain(vec![GraphNodeKind::Denoise {
                config,
                profile: Some(NoiseSelection {
                    start_seconds: 0.0,
                    end_seconds: 1.0 / 48000.0,
                }),
            }]);
            let target = GraphPort { node: 2, port: 0 };
            let cancel = AtomicBool::new(false);
            let compile = |profiles: &BTreeMap<u64, NoiseProfile>| {
                let mut engine = Engine::new(&input);
                let index = engine.compile(&graph, target, &BTreeMap::new(), profiles, &cancel)?;
                Ok::<_, anyhow::Error>((engine, index))
            };
            assert!(compile(&BTreeMap::new()).is_err());
            let mut profile = NoiseProfile {
                sample_rate: 48000,
                channels: layout.len(),
                fft_size: 256,
                captured_frames: 1,
                power: vec![0.0; 129],
            };
            profile.power[0] = f32::NAN;
            assert!(compile(&BTreeMap::from([(3, profile.clone())])).is_err());
            profile.power[0] = 0.0;
            profile.channels += 1;
            assert!(compile(&BTreeMap::from([(3, profile.clone())])).is_err());
            profile.channels = layout.len();
            let (engine, index) = compile(&BTreeMap::from([(3, profile.clone())])).unwrap();
            let output = engine.render(index, Arc::clone(&budget), &cancel).unwrap();
            assert_eq!(output.start_frame, 0);
            assert_eq!(output.pcm.layout(), &layout);
            let mut actual = [0.0; 4];
            assert_eq!(output.pcm.read_frames(0, &mut actual), 4 / layout.len());
            assert_eq!(actual.map(f32::to_bits), expected.map(f32::to_bits));

            let mut invalid = graph.clone();
            let GraphNodeKind::Denoise { config, .. } = &mut invalid.nodes[2].kind else {
                unreachable!("fixture has a denoise node");
            };
            config.hum.enabled = true;
            config.hum.fundamental_hz = 1000.0;
            config.hum.harmonics = 32;
            assert!(
                Engine::new(&input)
                    .compile(
                        &invalid,
                        target,
                        &BTreeMap::new(),
                        &BTreeMap::from([(3, profile)]),
                        &cancel,
                    )
                    .is_err()
            );
        }
    }

    #[test]
    fn hybrid_never_substitutes_one_separation_output_for_both() {
        let mut graph = chain(vec![
            GraphNodeKind::Separate {
                config: Default::default(),
            },
            GraphNodeKind::Mix {
                gains_db: vec![0.0; 2],
                muted: vec![false; 2],
                solo: vec![false; 2],
            },
            GraphNodeKind::Equalizer {
                config: Default::default(),
                implementation: EqImplementation::Iir,
            },
        ]);
        graph.edges.push(GraphEdge {
            from: GraphPort { node: 3, port: 1 },
            to: GraphPort { node: 4, port: 1 },
        });
        for requested in [GraphExecutionMode::Auto, GraphExecutionMode::Hybrid] {
            let (mode, boundary, runner) = prepare(
                &graph.validated().unwrap(),
                GraphPort { node: 2, port: 0 },
                requested,
                48000,
                2,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(mode, GraphExecutionMode::Hybrid);
            assert_eq!(boundary, Some(GraphPort { node: 4, port: 0 }));
            let mut runner = runner.unwrap();
            let mut input = [0.25, -0.5];
            runner.process_interleaved(&mut input).unwrap();
            assert_eq!(input, [0.25, -0.5]);
        }
    }

    #[test]
    fn offline_mix_keeps_wide_accumulation_and_rejects_finite_overflow() {
        let budget = audio::PcmBudget::new(MEMORY_LIMIT);
        let input = pcm(&[33_554_432.0], &budget);
        let mut graph = mix_graph(vec![0.0; 3], vec![false; 3], vec![false; 3]);
        graph.edges[1].from = GraphPort { node: 4, port: 0 };
        for id in [4, 5] {
            graph.nodes.push(GraphNode {
                id,
                kind: GraphNodeKind::Source,
                position: [0.0; 2],
            });
        }
        graph.next_id = 6;
        graph.edges.push(GraphEdge {
            from: GraphPort { node: 5, port: 0 },
            to: GraphPort { node: 3, port: 2 },
        });
        let boundaries = BTreeMap::from([
            (
                GraphPort { node: 4, port: 0 },
                GraphPcm {
                    pcm: pcm(&[1.0], &budget),
                    start_frame: 0,
                },
            ),
            (
                GraphPort { node: 5, port: 0 },
                GraphPcm {
                    pcm: pcm(&[-33_554_432.0], &budget),
                    start_frame: 0,
                },
            ),
        ]);
        let cancel = AtomicBool::new(false);
        let mut engine = Engine::new(&input);
        let target = engine
            .compile(
                &graph,
                GraphPort { node: 2, port: 0 },
                &boundaries,
                &BTreeMap::new(),
                &cancel,
            )
            .unwrap();
        let output = engine.render(target, Arc::clone(&budget), &cancel).unwrap();
        assert_eq!(samples(&output.pcm), [1.0]);

        let input = pcm(&[f32::MAX * 0.75], &budget);
        let graph = mix_graph(vec![0.0; 2], vec![false; 2], vec![false; 2]);
        let retained = budget.used_bytes();
        let mut engine = Engine::new(&input);
        let target = engine
            .compile(
                &graph,
                GraphPort { node: 2, port: 0 },
                &BTreeMap::new(),
                &BTreeMap::new(),
                &cancel,
            )
            .unwrap();
        assert!(engine.render(target, Arc::clone(&budget), &cancel).is_err());
        assert_eq!(budget.used_bytes(), retained);
    }
}
