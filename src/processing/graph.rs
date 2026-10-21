//! File-backed DAG evaluation. Each ancestor is evaluated once; fan-out shares immutable PCM.
use super::*;
use std::collections::BTreeMap;

pub(super) const MAX_GRAPH_CACHE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub(crate) const MAX_RETAINED_CACHE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

pub(super) fn directory_bytes(path: &Path) -> Result<u64> {
    let mut bytes = 0_u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        bytes = bytes
            .checked_add(if metadata.is_dir() {
                directory_bytes(&entry.path())?
            } else {
                metadata.len()
            })
            .context("Cache size overflow")?;
    }
    Ok(bytes)
}

/// Returns an owned prepared live suffix. Explicit modes never downgrade algorithms.
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
/// Internal PCM retains causal startup and tail. Only consumer-facing files are
/// cropped to the original source timeline; downstream nodes always see guards.
#[derive(Clone)]
struct GraphPcm {
    path: PathBuf,
    start_frame: i64,
    frames: u64,
}

impl GraphPcm {
    fn processed(&self, path: PathBuf, latency: u64, tail: u64) -> Result<Self> {
        Ok(Self {
            path,
            start_frame: self
                .start_frame
                .checked_sub(i64::try_from(latency)?)
                .context("Graph timeline overflow")?,
            frames: self
                .frames
                .checked_add(tail)
                .context("Graph PCM length overflow")?,
        })
    }

    fn end_frame(&self) -> Result<i64> {
        self.start_frame
            .checked_add(i64::try_from(self.frames)?)
            .context("Graph timeline overflow")
    }
}

pub(super) fn execute(
    input: &Path,
    directory: &Path,
    status: &mut ProcessingTaskStatus,
    cancel: &AtomicBool,
    tx: &Sender<Event>,
) -> Result<()> {
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
    status.graph_nodes = order
        .iter()
        .map(|&node| GraphNodeStatus {
            node,
            phase: ProcessingPhase::Queued,
            error: None,
            model_id: None,
            model_sha256: None,
        })
        .collect();
    let reader = hound::WavReader::open(input)?;
    let spec = reader.spec();
    let frames = u64::from(reader.duration());
    drop(reader);
    // Reserve both public PCM and causal contexts. Exact guard growth is checked
    // against the same cache limit after each node and each publication.
    let pcm_bytes = fs::metadata(input)?.len();
    let output_files: u64 = order
        .iter()
        .map(|id| {
            let kind = &graph.node(*id).expect("validated ancestor").kind;
            match kind {
                GraphNodeKind::Source | GraphNodeKind::Output => 0,
                _ => kind.outputs() as u64,
            }
        })
        .sum();
    ensure!(
        pcm_bytes
            .checked_mul(output_files * 2 + 1)
            .is_some_and(|n| n <= MAX_GRAPH_CACHE_BYTES),
        "Graph PCM cache would exceed the 2 GiB per-run limit; select a nearer preview target or shorter source"
    );
    let mut outputs = BTreeMap::<GraphPort, GraphPcm>::new();
    for (index, id) in order.iter().copied().enumerate() {
        check_cancel(cancel)?;
        let node = graph.node(id)?;
        let inputs: Vec<GraphPcm> = (0..node.kind.inputs())
            .map(|port| {
                let edge = graph
                    .edges
                    .iter()
                    .find(|e| {
                        e.to == GraphPort {
                            node: id,
                            port: port as u16,
                        }
                    })
                    .context("Disconnected input")?;
                outputs
                    .get(&edge.from)
                    .cloned()
                    .context("Missing ancestor output")
            })
            .collect::<Result<_>>()?;
        report(
            tx,
            status.id,
            ProcessingPhase::Processing,
            index as u64,
            Some(order.len() as u64),
        );
        node_event(status, id, ProcessingPhase::Processing, None, tx);
        let node_dir = directory.join(format!("node-{id}"));
        let outcome = (|| -> Result<()> {
            let paths = match &node.kind {
                GraphNodeKind::Source => vec![GraphPcm {
                    path: input.to_path_buf(),
                    start_frame: 0,
                    frames,
                }],
                GraphNodeKind::Output => vec![inputs[0].clone()],
                GraphNodeKind::Equalizer {
                    config,
                    implementation,
                } => {
                    fs::create_dir(&node_dir)?;
                    let output = node_dir.join("equalized.wav");
                    let (latency, tail) =
                        equalize(&inputs[0].path, &output, config, *implementation, cancel)?;
                    vec![inputs[0].processed(output, latency, tail)?]
                }
                GraphNodeKind::Denoise { config, profile } => {
                    fs::create_dir(&node_dir)?;
                    let output = node_dir.join("denoised.wav");
                    if let Some(selection) = profile {
                        ensure!(
                            (selection.end_seconds * f64::from(spec.sample_rate)).floor()
                                <= frames as f64,
                            "Noise profile selection lies outside the source"
                        );
                    }
                    let latency = classical(
                        &inputs[0].path,
                        &output,
                        config,
                        *profile,
                        inputs[0].start_frame.unsigned_abs(),
                        cancel,
                        |_| {},
                    )?;
                    let output = if latency == 0 {
                        output
                    } else {
                        // Streaming denoise has an explicit startup queue. Keep
                        // it too: another adaptive/nonlinear node must see the
                        // same causal history in Offline, Hybrid and Realtime.
                        let causal = node_dir.join("causal.wav");
                        copy_pcm_window(&output, &causal, latency, 0, inputs[0].frames, cancel)?;
                        fs::remove_file(output)?;
                        causal
                    };
                    vec![inputs[0].processed(output, latency, latency)?]
                }
                GraphNodeKind::Speech { config } | GraphNodeKind::Separate { config } => {
                    let operation = if matches!(node.kind, GraphNodeKind::Speech { .. }) {
                        ensure!(
                            (1..=2).contains(&spec.channels),
                            "Speech supports mono/stereo only; no channel fallback"
                        );
                        ProcessingOperation::Speech
                    } else {
                        ensure!(
                            spec.channels == 2,
                            "Separation requires stereo; no channel fallback"
                        );
                        ProcessingOperation::Separate
                    };
                    let manifest = if operation == ProcessingOperation::Speech {
                        &config.speech_manifest
                    } else {
                        &config.separation_manifest
                    };
                    ensure!(
                        manifest
                            .as_ref()
                            .is_some_and(|p| p.is_absolute() && p.is_file()),
                        "Node {id} requires an absolute local prepared model manifest"
                    );
                    ensure!(
                        config.worker.is_file(),
                        "Node {id} neural worker does not exist"
                    );
                    fs::create_dir(&node_dir)?;
                    let operation_before = status.operation;
                    let frames_before = status.frames;
                    status.operation = operation;
                    status.frames = Some(inputs[0].frames);
                    let result = neural(&inputs[0].path, &node_dir, config, cancel, tx, status);
                    status.operation = operation_before;
                    status.frames = frames_before;
                    let result = result?;
                    let model_id = status.model_id.take();
                    let model_sha256 = status.model_sha256.take();
                    let evidence = status
                        .graph_nodes
                        .iter_mut()
                        .find(|n| n.node == id)
                        .expect("admitted node");
                    evidence.model_id = model_id;
                    evidence.model_sha256 = model_sha256;
                    let paths = if operation == ProcessingOperation::Speech {
                        vec![result.denoised.context("Missing speech output")?]
                    } else {
                        vec![
                            result.vocals.context("Missing vocal output")?,
                            result
                                .accompaniment
                                .context("Missing accompaniment output")?,
                        ]
                    };
                    paths
                        .into_iter()
                        .map(|path| inputs[0].processed(path, 0, 0))
                        .collect::<Result<Vec<_>>>()?
                }
                GraphNodeKind::Mix {
                    gains_db,
                    muted,
                    solo,
                } => {
                    fs::create_dir(&node_dir)?;
                    let output = node_dir.join("mixed.wav");
                    vec![mix_pcm(&inputs, &output, gains_db, muted, solo, cancel)?]
                }
            };
            for (port, pcm) in paths.into_iter().enumerate() {
                if !outputs.values().any(|existing| existing.path == pcm.path) {
                    validate_output(
                        &pcm.path,
                        spec.sample_rate,
                        spec.channels,
                        pcm.frames,
                        cancel,
                    )?;
                }
                outputs.insert(
                    GraphPort {
                        node: id,
                        port: port as u16,
                    },
                    pcm,
                );
            }
            ensure!(
                directory_bytes(directory)? <= MAX_GRAPH_CACHE_BYTES,
                "Graph cache exceeded 2 GiB"
            );
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
        node_event(status, id, ProcessingPhase::Complete, None, tx);
    }
    let mut published = BTreeMap::<PathBuf, PathBuf>::new();
    let mut graph_outputs = Vec::with_capacity(outputs.len());
    for (port, pcm) in outputs {
        check_cancel(cancel)?;
        let context = pcm.start_frame != 0 || pcm.frames != frames;
        let path = if !context {
            pcm.path.clone()
        } else if let Some(path) = published.get(&pcm.path) {
            path.clone()
        } else {
            ensure!(
                pcm.start_frame <= 0 && pcm.end_frame()? >= i64::try_from(frames)?,
                "Graph output does not cover the source timeline"
            );
            let path = directory.join(format!("published-{}-{}.wav", port.node, port.port));
            copy_pcm_window(
                &pcm.path,
                &path,
                0,
                pcm.start_frame.unsigned_abs(),
                frames,
                cancel,
            )?;
            published.insert(pcm.path.clone(), path.clone());
            ensure!(
                directory_bytes(directory)? <= MAX_GRAPH_CACHE_BYTES,
                "Graph cache exceeded 2 GiB"
            );
            path
        };
        graph_outputs.push(GraphNodeOutput {
            port,
            path,
            context: context.then_some(GraphOutputContext {
                path: pcm.path,
                start_frame: pcm.start_frame,
            }),
        });
    }
    check_cancel(cancel)?;
    if let (Some(plan), Some(boundary)) = (&mut status.graph_plan, status.settings.graph_boundary)
        && let Some(context) = graph_outputs
            .iter()
            .find(|output| output.port == boundary)
            .and_then(|output| output.context.as_ref())
    {
        plan.latency_frames = plan
            .latency_frames
            .checked_add(context.start_frame.unsigned_abs())
            .context("Hybrid graph latency overflow")?;
    }
    status.graph_outputs = graph_outputs;
    status.phase = ProcessingPhase::Complete;
    status.completed = frames;
    status.total = Some(frames);
    Ok(())
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
        .find(|n| n.node == node)
        .expect("admitted node");
    entry.phase = phase;
    entry.error = error;
    let event = Event::GraphNode {
        id: status.id,
        status: entry.clone(),
    };
    if matches!(phase, ProcessingPhase::Failed | ProcessingPhase::Cancelled) {
        let _ = tx.send(event);
    } else {
        let _ = tx.try_send(event);
    }
}

pub(super) fn plan_info(runner: &audio::GraphRunner) -> GraphPlanInfo {
    GraphPlanInfo {
        estimated_state_bytes: runner.estimated_state_bytes() as u64,
        estimated_work_per_frame: runner.estimated_work_per_frame(),
        latency_frames: runner.latency_frames() as u64,
        tail_frames: runner.tail_frames() as u64,
    }
}

fn equalize(
    input: &Path,
    output: &Path,
    config: &rivu_dsp::equalizer::EqualizerConfig,
    implementation: EqImplementation,
    cancel: &AtomicBool,
) -> Result<(u64, u64)> {
    use rivu_dsp::{equalizer::Equalizer, fir_eq::FirEqualizer};
    let mut reader = hound::WavReader::open(input)?;
    let spec = reader.spec();
    let channels = usize::from(spec.channels);
    let frames = u64::from(reader.duration());
    enum Processor {
        Iir(Equalizer),
        Fir(FirEqualizer),
    }
    let mut processor = match implementation {
        EqImplementation::Iir => {
            Processor::Iir(Equalizer::new(config, spec.sample_rate, channels)?)
        }
        EqImplementation::LinearPhase { taps } => {
            Processor::Fir(FirEqualizer::new(config, spec.sample_rate, channels, taps)?)
        }
    };
    let (latency, tail) = match &processor {
        Processor::Iir(_) => (0, 0),
        Processor::Fir(fir) => (fir.latency_frames() as u64, fir.tail_frames() as u64),
    };
    let mut samples = reader.samples::<f32>();
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    let mut writer = hound::WavWriter::new(std::io::BufWriter::new(file), spec)?;
    let mut block = vec![0.0_f32; 4096 * channels];
    let mut position = 0_u64;
    // Keep the full causal result, including startup buffering and FIR ringing.
    // Its source-relative origin is tracked separately by GraphPcm.
    let total = frames.checked_add(tail).context("EQ PCM length overflow")?;
    while position < total {
        check_cancel(cancel)?;
        let count = (total - position).min(4096) as usize;
        for frame in 0..count {
            for channel in 0..channels {
                block[frame * channels + channel] = if position + (frame as u64) < frames {
                    samples.next().context("Truncated EQ input")??
                } else {
                    0.0
                };
            }
        }
        let block = &mut block[..count * channels];
        match &mut processor {
            Processor::Iir(eq) => eq.process(block)?,
            Processor::Fir(eq) => eq.process_interleaved(block)?,
        }
        for &sample in block.iter() {
            ensure!(sample.is_finite(), "Nonfinite equalizer output");
            writer.write_sample(sample)?;
        }
        position += count as u64;
    }
    writer.finalize()?;
    Ok((latency, tail))
}

fn mix_pcm(
    inputs: &[GraphPcm],
    output: &Path,
    gains_db: &[f32],
    muted: &[bool],
    solo: &[bool],
    cancel: &AtomicBool,
) -> Result<GraphPcm> {
    let mut readers = inputs
        .iter()
        .map(|pcm| hound::WavReader::open(&pcm.path))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let spec = readers[0].spec();
    ensure!(
        readers
            .iter()
            .zip(inputs)
            .all(|(r, pcm)| r.spec() == spec && u64::from(r.duration()) == pcm.frames),
        "Mix inputs are not aligned PCM"
    );
    let start_frame = inputs
        .iter()
        .map(|pcm| pcm.start_frame)
        .min()
        .context("Empty mix")?;
    let end_frame = inputs
        .iter()
        .try_fold(start_frame, |end, pcm| -> Result<i64> {
            Ok(end.max(pcm.end_frame()?))
        })?;
    let frames = u64::try_from(
        end_frame
            .checked_sub(start_frame)
            .context("Mix timeline overflow")?,
    )?;
    let any_solo = solo.iter().any(|&s| s);
    let alignment: Vec<(u64, f64)> = inputs
        .iter()
        .enumerate()
        .map(|(i, pcm)| -> Result<_> {
            let offset = u64::try_from(
                pcm.start_frame
                    .checked_sub(start_frame)
                    .context("Mix timeline overflow")?,
            )?;
            let gain = if muted[i] || (any_solo && !solo[i]) {
                0.0
            } else {
                10.0_f64.powf(f64::from(gains_db[i]) / 20.0)
            };
            Ok((offset, gain))
        })
        .collect::<Result<_>>()?;
    let mut samples: Vec<_> = readers.iter_mut().map(|r| r.samples::<f32>()).collect();
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    let mut writer = hound::WavWriter::new(std::io::BufWriter::new(file), spec)?;
    for frame in 0..frames {
        if frame % 4096 == 0 {
            check_cancel(cancel)?;
        }
        for _ in 0..spec.channels {
            let mut value = 0.0_f64;
            for ((samples, pcm), &(offset, gain)) in samples.iter_mut().zip(inputs).zip(&alignment)
            {
                if frame >= offset && frame - offset < pcm.frames {
                    value += f64::from(samples.next().context("Truncated mix input")??) * gain;
                }
            }
            let value = value as f32;
            ensure!(value.is_finite(), "Nonfinite mix output");
            writer.write_sample(value)?;
        }
    }
    writer.finalize()?;
    Ok(GraphPcm {
        path: output.to_path_buf(),
        start_frame,
        frames,
    })
}

/// Copy an exact PCM window, optionally retaining a processor's causal startup.
fn copy_pcm_window(
    input: &Path,
    output: &Path,
    leading_frames: u64,
    skip_frames: u64,
    frames: u64,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut reader = hound::WavReader::open(input)?;
    let spec = reader.spec();
    ensure!(
        skip_frames
            .checked_add(frames)
            .is_some_and(|end| end <= u64::from(reader.duration())),
        "PCM window lies outside graph context"
    );
    reader.seek(u32::try_from(skip_frames)?)?;
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    let mut writer = hound::WavWriter::new(std::io::BufWriter::new(file), spec)?;
    for frame in 0..leading_frames {
        if frame % 4096 == 0 {
            check_cancel(cancel)?;
        }
        for _ in 0..spec.channels {
            writer.write_sample(0.0_f32)?;
        }
    }
    let mut samples = reader.samples::<f32>();
    for frame in 0..frames {
        if frame % 4096 == 0 {
            check_cancel(cancel)?;
        }
        for _ in 0..spec.channels {
            writer.write_sample(samples.next().context("Truncated graph context")??)?;
        }
    }
    writer.finalize()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source(path: &Path, pcm: &[f32]) {
        let mut writer = hound::WavWriter::create(
            path,
            hound::WavSpec {
                channels: 1,
                sample_rate: 48000,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            },
        )
        .unwrap();
        for &sample in pcm {
            writer.write_sample(sample).unwrap();
        }
        writer.finalize().unwrap();
    }
    fn samples(path: &Path) -> Vec<f32> {
        hound::WavReader::open(path)
            .unwrap()
            .samples()
            .collect::<std::result::Result<_, _>>()
            .unwrap()
    }
    #[test]
    fn mix_preserves_gain_solo_alignment_and_unclipped_pcm() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.wav");
        let b = dir.path().join("b.wav");
        source(&a, &[0.75, -0.75, 0.0]);
        source(&b, &[0.75, -0.75, 0.0]);
        let a = GraphPcm {
            path: a,
            start_frame: 0,
            frames: 3,
        };
        let b = GraphPcm {
            path: b,
            start_frame: 0,
            frames: 3,
        };
        let output = dir.path().join("mix.wav");
        mix_pcm(
            &[a.clone(), b.clone()],
            &output,
            &[0.0, 0.0],
            &[false; 2],
            &[false; 2],
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(samples(&output), vec![1.5, -1.5, 0.0]);
        let output = dir.path().join("solo.wav");
        mix_pcm(
            &[a, b],
            &output,
            &[0.0, -6.0206],
            &[false; 2],
            &[false, true],
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!((samples(&output)[0] - 0.375).abs() < 1e-6);
    }
    #[test]
    fn mix_retains_union_of_guarded_source_relative_support() {
        let dir = tempfile::tempdir().unwrap();
        let guarded = dir.path().join("guarded.wav");
        let plain = dir.path().join("plain.wav");
        source(&guarded, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        source(&plain, &[10.0, 20.0]);
        let output = dir.path().join("mixed.wav");
        let mixed = mix_pcm(
            &[
                GraphPcm {
                    path: guarded,
                    start_frame: -2,
                    frames: 6,
                },
                GraphPcm {
                    path: plain,
                    start_frame: 0,
                    frames: 2,
                },
            ],
            &output,
            &[0.0; 2],
            &[false; 2],
            &[false; 2],
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(mixed.start_frame, -2);
        assert_eq!(mixed.frames, 6);
        assert_eq!(samples(&output), [1.0, 2.0, 13.0, 24.0, 5.0, 6.0]);
    }
    #[test]
    fn guarded_profile_selection_adds_offset_after_source_frame_floor() {
        use rivu_dsp::denoise::{DenoiseConfig, DenoiseMode, Denoiser, ProfileCollector};
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("guarded.wav");
        let output = dir.path().join("profile.wav");
        let mut pcm: Vec<f32> = (0..1400)
            .map(|i| {
                if i < 288 {
                    0.0
                } else {
                    (i as f32 * 0.07).sin() * 0.3
                }
            })
            .collect();
        pcm[314] = 0.9;
        pcm[315] = -0.7;
        source(&input, &pcm);
        let selection = NoiseSelection {
            start_seconds: 0.0005625,
            end_seconds: 0.012,
        };
        let start = (selection.start_seconds * 48000.0).floor() as usize;
        let end = (selection.end_seconds * 48000.0).floor() as usize;
        assert_eq!(start, 26);
        // Shifting seconds instead would select frame315, not frame314.
        assert_ne!(
            ((selection.start_seconds + 288.0 / 48000.0) * 48000.0).floor() as usize,
            start + 288
        );
        let config = DenoiseConfig {
            mode: DenoiseMode::Profile,
            fft_size: 256,
            ..Default::default()
        };
        let mut collector = ProfileCollector::new(48000, 1, 256, (end - start) as u64).unwrap();
        collector
            .push_block(&pcm[start + 288..end + 288], &mut || false)
            .unwrap();
        let mut denoiser =
            Denoiser::new(48000, 1, config.clone(), Some(collector.finish().unwrap())).unwrap();
        let mut expected = Vec::new();
        denoiser
            .process_block(&pcm, &mut expected, &mut || false)
            .unwrap();
        denoiser.finish(&mut expected, &mut || false).unwrap();
        classical(
            &input,
            &output,
            &config,
            Some(selection),
            288,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        assert_eq!(samples(&output), expected);
    }
    #[test]
    fn eq_instances_are_independent_and_cancel_does_not_finish_output() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.wav");
        source(&input, &[0.1; 513]);
        let config = rivu_dsp::equalizer::EqualizerConfig {
            enabled: true,
            preamp_db: 6.020599913,
            ..Default::default()
        };
        let first = dir.path().join("one.wav");
        let second = dir.path().join("two.wav");
        equalize(
            &input,
            &first,
            &config,
            EqImplementation::Iir,
            &AtomicBool::new(false),
        )
        .unwrap();
        equalize(
            &input,
            &second,
            &Default::default(),
            EqImplementation::Iir,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!((samples(&first)[0] - 0.2).abs() < 1e-6);
        assert_eq!(samples(&second), vec![0.1; 513]);
        assert!(
            equalize(
                &input,
                &dir.path().join("cancel.wav"),
                &config,
                EqImplementation::Iir,
                &AtomicBool::new(true)
            )
            .is_err()
        );
    }
    #[test]
    fn offline_fir_retains_original_timeline_including_last_frame() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("impulses.wav");
        let mut expected = vec![0.0; 513];
        expected[0] = 0.5;
        expected[512] = -0.5;
        source(&input, &expected);
        let output = dir.path().join("aligned.wav");
        let config = rivu_dsp::equalizer::EqualizerConfig {
            enabled: true,
            ..Default::default()
        };
        let guarded = dir.path().join("guarded.wav");
        let (latency, tail) = equalize(
            &input,
            &guarded,
            &config,
            EqImplementation::LinearPhase { taps: 65 },
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(samples(&guarded).len(), expected.len() + tail as usize);
        copy_pcm_window(
            &guarded,
            &output,
            0,
            latency,
            expected.len() as u64,
            &AtomicBool::new(false),
        )
        .unwrap();
        let actual = samples(&output);
        assert_eq!(actual.len(), expected.len());
        for (a, e) in actual.iter().zip(expected) {
            assert!((*a - e).abs() < 1e-5);
        }
    }

    #[test]
    fn auto_selects_exact_realtime_hybrid_or_offline_without_downgrading_nodes() {
        let target = GraphPort { node: 2, port: 0 };
        let mut graph = ProcessingGraph::default();
        let flag = AtomicBool::new(false);
        let (mode, boundary, runner) =
            prepare(&graph, target, GraphExecutionMode::Auto, 48000, 2, &flag).unwrap();
        assert_eq!(mode, GraphExecutionMode::Realtime);
        assert!(boundary.is_none());
        assert!(runner.is_some());
        graph.next_id = 5;
        graph.nodes.push(GraphNode {
            id: 3,
            position: [0.0; 2],
            kind: GraphNodeKind::Speech {
                config: Default::default(),
            },
        });
        graph.edges = vec![
            GraphEdge {
                from: GraphPort { node: 1, port: 0 },
                to: GraphPort { node: 3, port: 0 },
            },
            GraphEdge {
                from: GraphPort { node: 3, port: 0 },
                to: target,
            },
        ];
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
        graph.nodes.push(GraphNode {
            id: 4,
            position: [0.0; 2],
            kind: GraphNodeKind::Equalizer {
                config: Default::default(),
                implementation: EqImplementation::Iir,
            },
        });
        graph.edges[1].to = GraphPort { node: 4, port: 0 };
        graph.edges.push(GraphEdge {
            from: GraphPort { node: 4, port: 0 },
            to: target,
        });
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
