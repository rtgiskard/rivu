//! Block-streamed DAG evaluation. Only model boundaries and the selected output retain PCM.
use super::render::{ProgressReporter, check_cancel, validate_output};
use super::*;
use anyhow::bail;
use rivu_dsp::{
    denoise::{DenoiseMode, Denoiser, NoiseProfile, ProfileCollector},
    equalizer::{fir::FirEqualizer, iir::Equalizer},
};
use std::collections::{BTreeMap, VecDeque};

const BLOCK: usize = 256;

pub(super) fn denoise_bypassed(config: &rivu_dsp::denoise::DenoiseConfig) -> bool {
    config.wet == 0.0
        || (config.mode == DenoiseMode::Off && !config.hum.enabled && !config.impulse.enabled)
}

pub(super) fn prepare(
    graph: &ProcessingGraph,
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
    if mode == GraphExecutionMode::Offline {
        return Ok((mode, None, None));
    }
    if matches!(
        mode,
        GraphExecutionMode::Auto | GraphExecutionMode::Realtime
    ) {
        match audio::GraphRunner::compile(graph, target, sample_rate, channels) {
            Ok(runner) => {
                check_cancel(cancel)?;
                return Ok((GraphExecutionMode::Realtime, None, Some(runner)));
            }
            Err(error) if mode == GraphExecutionMode::Realtime => return Err(error),
            Err(_) => {}
        }
    }
    let order = graph.ancestors(target)?;
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
            if let Ok(runner) = audio::GraphRunner::compile_from_boundary(
                graph,
                target,
                boundary,
                sample_rate,
                channels,
            ) {
                check_cancel(cancel)?;
                return Ok((GraphExecutionMode::Hybrid, Some(boundary), Some(runner)));
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

struct DenoiseStream {
    processor: Denoiser,
    queue: VecDeque<f32>,
    output: Vec<f32>,
    finished: bool,
}
impl DenoiseStream {
    fn new(processor: Denoiser, channels: usize) -> Self {
        let latency = processor.lookahead_frames() * channels;
        let burst = processor
            .config()
            .fft_size
            .max(processor.lookahead_frames())
            + BLOCK;
        let mut queue = VecDeque::with_capacity(latency + burst * channels);
        queue.resize(latency, 0.0);
        Self {
            processor,
            queue,
            output: Vec::with_capacity(burst * channels),
            finished: false,
        }
    }
    fn process(
        &mut self,
        samples: &mut [f32],
        input_samples: usize,
        eof: bool,
        cancel: &AtomicBool,
    ) -> Result<()> {
        self.output.clear();
        if input_samples > 0 {
            self.processor.process_block(
                &samples[..input_samples],
                &mut self.output,
                &mut || cancel.load(Ordering::Acquire),
            )?;
        }
        if eof && !self.finished {
            self.processor
                .finish(&mut self.output, &mut || cancel.load(Ordering::Acquire))?;
            self.finished = true;
        }
        ensure!(
            self.queue.len() + self.output.len() <= self.queue.capacity(),
            "Denoise output exceeded bounded staging capacity"
        );
        self.queue.extend(self.output.iter().copied());
        ensure!(
            self.queue.len() >= samples.len(),
            "Denoise exceeded declared lookahead"
        );
        for sample in samples {
            *sample = self.queue.pop_front().expect("checked queue length");
        }
        Ok(())
    }
}

enum Processor {
    Source(audio::PcmAudio),
    Decoder,
    Pass,
    Iir(Equalizer),
    Fir(FirEqualizer),
    Denoise(Box<DenoiseStream>),
}
struct Node {
    inputs: Vec<Edge>,
    processor: Processor,
    output: Vec<f32>,
    sum: Vec<f64>,
    latency: u64,
    frames: u64,
    input_frames: u64,
}

/// One block per reachable native node, plus bounded causal delay queues. Model
/// outputs act as immutable sources, never as a second implicit file protocol.
struct Engine {
    nodes: Vec<Node>,
    indices: BTreeMap<GraphPort, usize>,
    channels: usize,
    layout: Arc<[audio::Channel]>,
    rate: u32,
    source: Option<audio::PcmAudio>,
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
        }
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
        let processor;
        if let Some(boundary) = boundaries.get(&port) {
            ensure!(boundary.start_frame <= 0, "Invalid causal graph boundary");
            latency = boundary.start_frame.unsigned_abs();
            frames = boundary.pcm.frames();
            processor = Processor::Source(boundary.pcm.clone());
        } else if matches!(kind, GraphNodeKind::Source) {
            processor = self.source.as_ref().map_or(Processor::Decoder, |source| {
                Processor::Source(source.clone())
            });
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
            processor = match kind {
                GraphNodeKind::Source => unreachable!(),
                GraphNodeKind::Output | GraphNodeKind::Mix { .. } => Processor::Pass,
                GraphNodeKind::Equalizer {
                    config,
                    implementation,
                } => {
                    config.validate_for_stream(self.rate, self.channels)?;
                    implementation.validate()?;
                    if !config.enabled {
                        Processor::Pass
                    } else {
                        match implementation {
                            EqImplementation::Iir => {
                                Processor::Iir(Equalizer::new(config, self.rate, self.channels)?)
                            }
                            EqImplementation::LinearPhase { taps } => Processor::Fir(
                                FirEqualizer::new(config, self.rate, self.channels, *taps)?,
                            ),
                        }
                    }
                }
                GraphNodeKind::Denoise { config, .. } => {
                    let profile = profiles.get(&port.node).cloned();
                    let denoiser =
                        Denoiser::new(self.rate, self.channels, config.clone(), profile)?;
                    if denoise_bypassed(config) {
                        Processor::Pass
                    } else {
                        Processor::Denoise(Box::new(DenoiseStream::new(denoiser, self.channels)))
                    }
                }
                GraphNodeKind::Speech { .. } | GraphNodeKind::Separate { .. } => {
                    bail!("Missing prepared neural boundary")
                }
            };
        }
        let input_frames = frames;
        let (delay, tail) = match &processor {
            Processor::Fir(fir) => (fir.latency_frames() as u64, fir.tail_frames() as u64),
            Processor::Denoise(denoise) => {
                let delay = denoise.processor.lookahead_frames() as u64;
                (delay, delay)
            }
            _ => (0, 0),
        };
        latency = latency
            .checked_add(delay)
            .context("Graph latency overflow")?;
        frames = frames.checked_add(tail).context("Graph length overflow")?;
        let index = self.nodes.len();
        self.nodes.push(Node {
            inputs,
            processor,
            output: vec![0.0; BLOCK * self.channels],
            sum: vec![0.0; BLOCK * self.channels],
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
        let streaming = self.source.is_none();
        for index in 0..self.nodes.len() {
            let (previous, rest) = self.nodes.split_at_mut(index);
            let node = &mut rest[0];
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
            node.output.fill(0.0);
            let output = &mut node.output[..frames * self.channels];
            match &node.processor {
                Processor::Source(pcm) => {
                    pcm.read_frames(position, output);
                }
                Processor::Decoder => {
                    if let Some(input) = decoder {
                        output.copy_from_slice(&input[..output.len()]);
                    }
                }
                _ => {
                    if node.inputs.len() == 1
                        && node.inputs[0].gain == 1.0
                        && node.inputs[0].delay.is_empty()
                    {
                        output
                            .copy_from_slice(&previous[node.inputs[0].node].output[..output.len()]);
                    } else {
                        let sum = &mut node.sum[..output.len()];
                        sum.fill(0.0);
                        for edge in &mut node.inputs {
                            let input = &previous[edge.node].output[..output.len()];
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
                    match &mut node.processor {
                        Processor::Source(_) | Processor::Decoder | Processor::Pass => {}
                        Processor::Iir(eq) => eq.process(output)?,
                        Processor::Fir(eq) => eq.process_interleaved(output)?,
                        Processor::Denoise(denoise) => denoise.process(
                            output,
                            input_frames * self.channels,
                            input_end.is_some_and(|end| position + count as u64 >= end),
                            cancel,
                        )?,
                    }
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
                &self.nodes[target].output[..count * self.channels],
            )?;
            position += count as u64;
        }
        Ok(())
    }
    /// Returns the source frames traversed, or the exact length after real EOF.
    /// A bounded prefix therefore still proves any earlier source selection fits.
    fn decode(
        &mut self,
        target: usize,
        mut renderer: audio::SourceRenderer,
        prefix_end: Option<u64>,
        cancel: &AtomicBool,
        mut consume: impl FnMut(u64, &[f32]) -> Result<()>,
    ) -> Result<u64> {
        ensure!(
            self.source.is_none(),
            "Decoder engine cannot contain a retained source"
        );
        let mut decoded = Vec::new();
        let mut position = 0_u64;
        loop {
            if prefix_end.is_some_and(|end| position >= end) {
                // A requested prefix is not EOF. Keep the real input used by
                // causal FIR/STFT lookahead, but do not synthesize a DSP flush.
                renderer.finish(cancel)?;
                return Ok(position);
            }
            if !renderer.read_block(&mut decoded, cancel)? {
                break;
            }
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
                    &self.nodes[target].output[..count * self.channels],
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
                &self.nodes[target].output[..count * self.channels],
            )?;
            position += count as u64;
        }
        renderer.finish(cancel)?;
        Ok(frames)
    }
    fn alias(&self, target: usize) -> Option<audio::PcmAudio> {
        let node = &self.nodes[target];
        match &node.processor {
            Processor::Source(pcm) => Some(pcm.clone()),
            Processor::Pass
                if node.inputs.len() == 1
                    && node.inputs[0].gain == 1.0
                    && node.inputs[0].delay.is_empty() =>
            {
                self.alias(node.inputs[0].node)
            }
            _ => None,
        }
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
) -> Result<()> {
    if !visited.insert(port) {
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
        reachable_boundaries(graph, from, boundaries, visited, retained)?;
    }
    Ok(())
}

pub(super) fn execute(
    input: &audio::PcmAudio,
    budget: Arc<audio::PcmBudget>,
    status: &mut ProcessingTaskStatus,
    cancel: &AtomicBool,
    progress: &mut ProgressReporter<'_>,
) -> Result<BTreeMap<GraphPort, GraphAudioOutput>> {
    let tx = progress.sender;
    let graph = status
        .settings
        .graph
        .clone()
        .context("Missing immutable graph snapshot")?;
    let target = status
        .settings
        .graph_boundary
        .or(status.settings.graph_target)
        .context("Missing graph target")?;
    let order = graph.ancestors(target)?;
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
                    (selection.end_seconds * f64::from(input.sample_rate())).floor()
                        <= input.frames() as f64,
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
                    let start =
                        (selection.start_seconds * f64::from(input.sample_rate())).floor() as u64;
                    let end =
                        (selection.end_seconds * f64::from(input.sample_rate())).floor() as u64;
                    ensure!(
                        start < end && end <= input.frames(),
                        "Noise profile selection lies outside the source"
                    );
                    let mut engine = Engine::new(input);
                    let upstream = engine.compile(
                        &graph,
                        input_port(&graph, id)?,
                        &boundaries,
                        &profiles,
                        cancel,
                    )?;
                    let offset = engine.nodes[upstream].latency;
                    let start = start
                        .checked_add(offset)
                        .context("Profile offset overflow")?;
                    let end = end.checked_add(offset).context("Profile offset overflow")?;
                    let channels = usize::from(input.channels());
                    let mut collector = ProfileCollector::new(
                        input.sample_rate(),
                        channels,
                        config.fft_size,
                        end - start,
                    )?;
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
                    ensure!(
                        if operation == ProcessingOperation::Speech {
                            (1..=2).contains(&input.channels())
                        } else {
                            input.channels() == 2
                        },
                        "Neural graph node has unsupported channel layout"
                    );
                    let mut engine = Engine::new(input);
                    let upstream = engine.compile(
                        &graph,
                        input_port(&graph, id)?,
                        &boundaries,
                        &profiles,
                        cancel,
                    )?;
                    let upstream = engine.render(upstream, Arc::clone(&budget), cancel)?;
                    let original_operation = status.operation;
                    let original_frames = status.frames;
                    status.operation = operation;
                    status.frames = Some(upstream.pcm.frames());
                    let result = neural::run(
                        &upstream.pcm,
                        config,
                        Arc::clone(&budget),
                        cancel,
                        progress,
                        status,
                    );
                    status.operation = original_operation;
                    status.frames = original_frames;
                    let result = result?;
                    let entry = status
                        .graph_nodes
                        .iter_mut()
                        .find(|entry| entry.node == id)
                        .context("Missing graph node status")?;
                    entry.model_id = status.model_id.take();
                    entry.model_sha256 = status.model_sha256.take();
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
                        validate_output(
                            &pcm,
                            input.sample_rate(),
                            input.layout(),
                            upstream.pcm.frames(),
                            cancel,
                        )?;
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
                }
                _ => {}
            }
            Ok(())
        })();
        if let Err(error) = outcome {
            node_event(
                status,
                id,
                if cancel.load(Ordering::Acquire) {
                    ProcessingPhase::Cancelled
                } else {
                    ProcessingPhase::Failed
                },
                Some(format!("{error:#}")),
                tx,
            );
            return Err(error);
        }
        if matches!(
            graph.node(id)?.kind,
            GraphNodeKind::Speech { .. } | GraphNodeKind::Separate { .. }
        ) {
            let mut retained = std::collections::BTreeSet::new();
            reachable_boundaries(
                &graph,
                target,
                &boundaries,
                &mut std::collections::BTreeSet::new(),
                &mut retained,
            )?;
            boundaries.retain(|port, _| retained.contains(port));
        }
    }
    let mut engine = Engine::new(input);
    let index = engine.compile(&graph, target, &boundaries, &profiles, cancel)?;
    // The engine owns only boundaries reachable from this target; obsolete model
    // ancestors are released before allocating the final retained output.
    drop(boundaries);
    drop(profiles);
    let output = engine.render(index, budget, cancel)?;
    publish(output, input.frames(), target, order, status, tx)
}

fn publish(
    output: GraphPcm,
    frames: u64,
    target: GraphPort,
    order: Vec<u64>,
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
    for id in order {
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

pub(super) fn native_only(status: &ProcessingTaskStatus) -> Result<bool> {
    let graph = status
        .settings
        .graph
        .as_ref()
        .context("Missing graph snapshot")?;
    let target = status
        .settings
        .graph_boundary
        .or(status.settings.graph_target)
        .context("Missing graph target")?;
    Ok(graph.ancestors(target)?.into_iter().all(|id| {
        graph.node(id).is_ok_and(|node| {
            !matches!(
                node.kind,
                GraphNodeKind::Speech { .. } | GraphNodeKind::Separate { .. }
            )
        })
    }))
}

pub(super) fn execute_native(
    renderer: audio::SourceRenderer,
    mut reopen: impl FnMut() -> Result<audio::SourceRenderer>,
    budget: Arc<audio::PcmBudget>,
    status: &mut ProcessingTaskStatus,
    cancel: &AtomicBool,
    progress: &mut ProgressReporter<'_>,
) -> Result<BTreeMap<GraphPort, GraphAudioOutput>> {
    let tx = progress.sender;
    let graph = status
        .settings
        .graph
        .clone()
        .context("Missing immutable graph snapshot")?;
    let target = status
        .settings
        .graph_boundary
        .or(status.settings.graph_target)
        .context("Missing graph target")?;
    let order = graph.ancestors(target)?;
    let mut active_node = None;
    let outcome = (|| {
        let rate = renderer.format().sample_rate;
        let layout = Arc::clone(&renderer.format().layout);
        let channels = renderer.format().channels();
        let mut profiles = BTreeMap::new();
        for (index, &id) in order.iter().enumerate() {
            active_node = None;
            check_cancel(cancel)?;
            progress.report(
                ProcessingPhase::Processing,
                index as u64,
                Some(order.len() as u64),
            );
            node_event(status, id, ProcessingPhase::Processing, None, tx);
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
                    &graph,
                    input_port(&graph, id)?,
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
                    |position, samples| {
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
        let index = engine.compile(&graph, target, &BTreeMap::new(), &profiles, cancel)?;
        let start_frame = -i64::try_from(engine.nodes[index].latency)?;
        drop(profiles);
        let mut builder = audio::PcmBuilder::new(rate, layout, budget)?;
        let frames = engine.decode(index, renderer, None, cancel, |_, block| {
            builder.push(block)
        })?;
        for &id in &order {
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
        publish(
            GraphPcm {
                pcm: builder.finish()?,
                start_frame,
            },
            frames,
            target,
            order,
            status,
            tx,
        )
    })();
    if let Err(error) = &outcome
        && let Some(id) = active_node
    {
        node_event(
            status,
            id,
            if cancel.load(Ordering::Acquire) {
                ProcessingPhase::Cancelled
            } else {
                ProcessingPhase::Failed
            },
            Some(format!("{error:#}")),
            tx,
        );
    }
    outcome
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
    fn equalizer_instances_are_independent_across_every_block() {
        let budget = audio::PcmBudget::new(MEMORY_LIMIT);
        let data: Vec<f32> = (0..513).map(|i| (i as f32 * 0.07).sin() * 0.3).collect();
        let input = pcm(&data, &budget);
        let config = rivu_dsp::equalizer::config::EqualizerConfig {
            enabled: true,
            preamp_db: 6.020599913,
            ..Default::default()
        };
        let graph = chain(vec![GraphNodeKind::Equalizer {
            config,
            implementation: EqImplementation::Iir,
        }]);
        let actual = samples(&render(&graph, &input, &budget).pcm);
        assert_eq!(actual.len(), data.len());
        for (&actual, &source) in actual.iter().zip(&data) {
            assert!((actual - source * 2.0).abs() < 1e-6);
        }
        assert_eq!(
            samples(&render(&ProcessingGraph::default(), &input, &budget).pcm),
            data
        );
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
            audio::SourceIdentity::from_metadata(&metadata),
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
            engine.decode(target, renderer, None, &cancel, |_, block| {
                builder.push(block)?;
                blocks += 1;
                cancel.store(true, Ordering::Release);
                Ok(())
            })?;
            builder.finish()
        })();
        assert!(format!("{:#}", result.err().unwrap()).contains("cancelled"));
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
            audio::SourceRenderer::from_pcm(input, audio::SourceIdentity::from_metadata(&metadata))
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
            .decode(target, renderer, None, &cancel, |_, block| {
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
        let identity = audio::SourceIdentity::from_metadata(&metadata);
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
                    .decode(target, renderer, Some(end), &cancel, |_, block| {
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
                        audio::SourceIdentity::from_metadata(&metadata),
                    )
                    .unwrap();
                    engine
                        .decode(target, renderer, prefix, &cancel, &mut consume)
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
        let result = engine.decode(target, renderer, Some(97), &cancel, |_, block| {
            assert_eq!(block, &[0.125; 97]);
            fs::OpenOptions::new().write(true).open(&path)?.set_len(0)?;
            Ok(())
        });
        assert!(format!("{:#}", result.unwrap_err()).contains("source changed"));
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
        let identity = audio::SourceIdentity::from_metadata(&metadata);
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
                |_, block| {
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
            &ProcessingGraph::default(),
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
                &graph,
                target,
                GraphExecutionMode::Realtime,
                48000,
                2,
                &flag
            )
            .is_err()
        );
        let (mode, boundary, runner) =
            prepare(&graph, target, GraphExecutionMode::Auto, 48000, 2, &flag).unwrap();
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
        let (mode, boundary, runner) =
            prepare(&graph, target, GraphExecutionMode::Auto, 48000, 2, &flag).unwrap();
        assert_eq!(mode, GraphExecutionMode::Hybrid);
        assert_eq!(boundary, Some(GraphPort { node: 3, port: 0 }));
        assert!(runner.is_some());
        assert!(
            prepare(
                &graph,
                target,
                GraphExecutionMode::Auto,
                48000,
                2,
                &AtomicBool::new(true)
            )
            .is_err()
        );
    }
}
