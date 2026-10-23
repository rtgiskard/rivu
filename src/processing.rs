//! Offline processing runs outside Core and never publishes directly into player state.
use crate::{config::Config, model::*};
use anyhow::{Context, Result, bail, ensure};
use rivu_audio as audio;
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command as Process, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use tokio::sync::mpsc::{Receiver, Sender, channel};

pub(crate) mod graph;

const MAX_SOURCE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_OUTPUT_BYTES: u64 = 16 * 1024 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SourceStamp(audio::SourceIdentity);
impl SourceStamp {
    pub(crate) fn read(path: &Path) -> Result<Self> {
        let m = fs::metadata(path)?;
        ensure!(
            m.is_file() && m.len() <= MAX_SOURCE_BYTES,
            "source must be a regular file no larger than 8 GiB"
        );
        Ok(Self(audio::SourceIdentity::from_metadata(&m)))
    }
    pub(crate) fn identity(self) -> audio::SourceIdentity {
        self.0
    }
    pub(crate) fn validate_rendered(&self, identity: audio::SourceIdentity) -> Result<()> {
        ensure!(
            self.0 == identity,
            "Rendered source differs from the source admitted for processing"
        );
        Ok(())
    }
}

pub(crate) struct GraphRequest {
    pub target: GraphPort,
    pub mode: GraphExecutionMode,
    pub export_path: Option<PathBuf>,
    pub cached_output: Option<CachedGraphOutput>,
}

pub(crate) struct CachedGraphOutput {
    pub path: PathBuf,
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
}

pub(crate) struct ProcessingResult {
    pub track: Track,
    pub stamp: SourceStamp,
    pub status: ProcessingTaskStatus,
    pub directory: tempfile::TempDir,
    pub mix: Option<PathBuf>,
    pub cache_bytes: u64,
    pub pending_export: Option<(tempfile::NamedTempFile, PathBuf)>,
    pub runner: Option<audio::GraphRunner>,
}
pub(crate) enum Event {
    GraphNode {
        id: u64,
        status: GraphNodeStatus,
    },
    Progress {
        id: u64,
        completed: u64,
        total: Option<u64>,
        phase: ProcessingPhase,
    },
    Finished {
        id: u64,
        result: Result<Box<ProcessingResult>>,
    },
    MixFinished {
        id: u64,
        generation: u64,
        result: Result<PathBuf>,
    },
}
pub(crate) struct Active {
    pub id: u64,
    pub cancelled: Arc<AtomicBool>,
    pub worker: JoinHandle<()>,
}
pub(crate) struct Service {
    pub sender: Sender<Event>,
    pub receiver: Receiver<Event>,
    pub active: Option<Active>,
    pub results: std::collections::VecDeque<ProcessingResult>,
    pub next_id: u64,
    pub audition: Option<u64>,
    pub audition_generation: Option<u64>,
    pub pending_graph_audition: Option<(u64, u64)>,
    root: Option<tempfile::TempDir>,
    data_dir: PathBuf,
}
impl Service {
    pub fn new(data_dir: PathBuf) -> Self {
        let (sender, receiver) = channel(8);
        Self {
            sender,
            receiver,
            active: None,
            results: Default::default(),
            next_id: 1,
            root: None,
            data_dir,
            audition: None,
            audition_generation: None,
            pending_graph_audition: None,
        }
    }
    pub fn start(
        &mut self,
        track: Track,
        config: Arc<Config>,
        operation: ProcessingOperation,
        profile: Option<NoiseSelection>,
    ) -> Result<ProcessingTaskStatus> {
        ensure!(
            operation != ProcessingOperation::Graph,
            "Use ProcessingGraphRun with an explicit target"
        );
        self.start_request(track, config, operation, profile, None)
    }
    pub fn start_graph(
        &mut self,
        track: Track,
        config: Arc<Config>,
        request: GraphRequest,
    ) -> Result<ProcessingTaskStatus> {
        config.processing_graph.ancestors(request.target)?;
        self.start_request(
            track,
            config,
            ProcessingOperation::Graph,
            None,
            Some(request),
        )
    }
    fn start_request(
        &mut self,
        track: Track,
        config: Arc<Config>,
        operation: ProcessingOperation,
        profile: Option<NoiseSelection>,
        request: Option<GraphRequest>,
    ) -> Result<ProcessingTaskStatus> {
        let (target, mode, export_path, cached_output) = if let Some(request) = request {
            (
                Some(request.target),
                request.mode,
                request.export_path,
                request.cached_output,
            )
        } else {
            (None, GraphExecutionMode::Offline, None, None)
        };
        ensure!(
            self.active.is_none(),
            "An audio processing task is already active"
        );
        config.validate()?;
        if let Some(selection) = profile {
            ensure!(
                selection.start_seconds.is_finite()
                    && selection.end_seconds.is_finite()
                    && selection.start_seconds >= 0.0
                    && selection.end_seconds > selection.start_seconds,
                "Noise profile selection must be a finite nonempty source-relative range"
            );
        }
        let stamp = SourceStamp::read(&track.path)?;
        if matches!(
            operation,
            ProcessingOperation::Speech | ProcessingOperation::Separate
        ) {
            let manifest = if operation == ProcessingOperation::Speech {
                &config.neural.speech_manifest
            } else {
                &config.neural.separation_manifest
            };
            ensure!(
                manifest
                    .as_ref()
                    .is_some_and(|p| p.is_absolute() && p.is_file()),
                "Select an absolute local prepared model manifest first"
            );
            ensure!(
                config.neural.worker.is_file(),
                "Neural worker does not exist: {}",
                config.neural.worker.display()
            );
        }
        if self.root.is_none() {
            fs::create_dir_all(&self.data_dir)?;
            let parent = fs::canonicalize(&self.data_dir)?;
            self.root = Some(
                tempfile::Builder::new()
                    .prefix("processing-")
                    .tempdir_in(parent)?,
            );
        }
        let directory = tempfile::Builder::new()
            .prefix("task-")
            .tempdir_in(self.root.as_ref().unwrap().path())?;
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .context("Processing task id exhausted")?;
        let status = ProcessingTaskStatus {
            id,
            track_id: track.id,
            operation,
            phase: ProcessingPhase::Queued,
            completed: 0,
            total: None,
            error: None,
            outputs: None,
            sample_rate: None,
            channels: None,
            frames: None,
            model_id: None,
            model_sha256: None,
            source_path: track.path.clone(),
            source_version: format!("{stamp:?}"),
            source_cue: track.cue.clone(),
            settings: ProcessingSettings {
                denoise: config.denoise.clone(),
                neural: config.neural.clone(),
                profile,
                graph: target.map(|_| config.processing_graph.clone()),
                graph_target: target,
                graph_mode: target.map(|_| mode),
                graph_boundary: None,
                graph_export_path: export_path,
            },
            graph_revision: target.map(|_| config.processing_graph.revision),
            graph_outputs: Vec::new(),
            graph_exported_path: None,
            graph_nodes: if let Some(target) = target {
                config
                    .processing_graph
                    .ancestors(target)?
                    .into_iter()
                    .map(|node| GraphNodeStatus {
                        node,
                        phase: ProcessingPhase::Queued,
                        error: None,
                        model_id: None,
                        model_sha256: None,
                    })
                    .collect()
            } else {
                Vec::new()
            },
            graph_plan: None,
        };
        let initial = status.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        let tx = self.sender.clone();
        let worker = thread::Builder::new()
            .name("rivu-processing".into())
            .spawn(move || {
                let result = if let Some(cached_output) = cached_output {
                    run_cached_export(track, stamp, directory, initial, cached_output, &flag, &tx)
                } else {
                    run(track, stamp, config, directory, initial, &flag, &tx)
                };
                let _ = tx.blocking_send(Event::Finished {
                    id,
                    result: result.map(Box::new),
                });
            })?;
        self.active = Some(Active {
            id,
            cancelled,
            worker,
        });
        Ok(status)
    }
    pub fn cancel(&self, id: u64) -> Result<()> {
        let task = self
            .active
            .as_ref()
            .filter(|t| t.id == id)
            .context("No matching active processing task")?;
        task.cancelled.store(true, Ordering::Release);
        Ok(())
    }
    pub fn shutdown(&mut self) {
        let task = self.active.take();
        if let Some(task) = &task {
            task.cancelled.store(true, Ordering::Release);
        }
        // Closing admission wakes every reliable sender, including a worker
        // blocked behind buffered progress. Joining establishes retirement;
        // draining afterward drops unpublished outputs, not retained auditions.
        self.receiver.close();
        if let Some(task) = task {
            let _ = task.worker.join();
        }
        while self.receiver.try_recv().is_ok() {}
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        self.shutdown();
    }
}
fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::Acquire), "Processing cancelled");
    Ok(())
}
fn report(tx: &Sender<Event>, id: u64, phase: ProcessingPhase, completed: u64, total: Option<u64>) {
    let _ = tx.try_send(Event::Progress {
        id,
        phase,
        completed,
        total,
    });
}
fn run_cached_export(
    track: Track,
    stamp: SourceStamp,
    directory: tempfile::TempDir,
    mut status: ProcessingTaskStatus,
    cached: CachedGraphOutput,
    cancel: &AtomicBool,
    tx: &Sender<Event>,
) -> Result<ProcessingResult> {
    check_cancel(cancel)?;
    validate_output(
        &cached.path,
        cached.sample_rate,
        cached.channels,
        cached.frames,
        cancel,
    )?;
    let destination = status
        .settings
        .graph_export_path
        .as_ref()
        .context("Missing export destination")?;
    let pending_export = Some(prepare_export(
        &cached.path,
        destination,
        cancel,
        |done, total| {
            report(
                tx,
                status.id,
                ProcessingPhase::Processing,
                done,
                Some(total),
            );
        },
    )?);
    ensure!(
        SourceStamp::read(&track.path)? == stamp,
        "Source changed while exporting; output discarded"
    );
    status.phase = ProcessingPhase::Complete;
    status.graph_nodes.clear();
    status.sample_rate = Some(cached.sample_rate);
    status.channels = Some(cached.channels);
    status.frames = Some(cached.frames);
    status.completed = cached.frames;
    status.total = Some(cached.frames);
    Ok(ProcessingResult {
        track,
        stamp,
        status,
        directory,
        mix: None,
        cache_bytes: 0,
        pending_export,
        runner: None,
    })
}

fn run(
    track: Track,
    stamp: SourceStamp,
    config: Arc<Config>,
    directory: tempfile::TempDir,
    mut status: ProcessingTaskStatus,
    cancel: &AtomicBool,
    tx: &Sender<Event>,
) -> Result<ProcessingResult> {
    let mut runner = None;
    if let Some(mode) = status
        .settings
        .graph_mode
        .filter(|mode| *mode != GraphExecutionMode::Offline)
    {
        let native = audio::probe(&track.path, config.ffmpeg_enabled)?;
        check_cancel(cancel)?;
        ensure!(
            SourceStamp::read(&track.path)? == stamp,
            "Source changed during graph admission"
        );
        status.sample_rate = Some(native.sample_rate);
        status.channels = Some(native.channels);
        let snapshot = status
            .settings
            .graph
            .as_ref()
            .context("Missing graph snapshot")?;
        let target = status
            .settings
            .graph_target
            .context("Missing graph target")?;
        let (actual, boundary, prepared) = graph::prepare(
            snapshot,
            target,
            mode,
            native.sample_rate,
            usize::from(native.channels),
            cancel,
        )?;
        status.settings.graph_mode = Some(actual);
        status.settings.graph_boundary = boundary;
        status.graph_plan = prepared.as_ref().map(graph::plan_info);
        runner = prepared;
    }
    if status.settings.graph_mode == Some(GraphExecutionMode::Realtime) {
        check_cancel(cancel)?;
        let snapshot = status
            .settings
            .graph
            .as_ref()
            .context("Missing graph snapshot")?;
        let target = status
            .settings
            .graph_target
            .context("Missing live target")?;
        status.graph_nodes = snapshot
            .ancestors(target)?
            .into_iter()
            .map(|node| GraphNodeStatus {
                node,
                phase: ProcessingPhase::Processing,
                error: None,
                model_id: None,
                model_sha256: None,
            })
            .collect();
        status.phase = ProcessingPhase::Complete;
        return Ok(ProcessingResult {
            track,
            stamp,
            status,
            directory,
            mix: None,
            cache_bytes: 0,
            pending_export: None,
            runner,
        });
    }
    report(tx, status.id, ProcessingPhase::Rendering, 0, None);
    let input = directory.path().join("input.wav");
    let info = audio::render_source(
        &track.path,
        track.cue.as_ref().map(|cue| audio::PlaybackRange {
            start_seconds: cue.start_seconds(),
            end_seconds: cue.end_seconds(),
        }),
        track.decoder_hint,
        config.media_read_buffer_mb,
        config.ffmpeg_enabled,
        &input,
        cancel,
    )?;
    stamp.validate_rendered(info.source_identity)?;
    ensure!(
        fs::metadata(&input)?.len() <= MAX_OUTPUT_BYTES,
        "Rendered source exceeds 16 GiB processing limit"
    );
    ensure!(
        status.operation != ProcessingOperation::Separate || info.channels == 2,
        "Vocal separation requires a stereo source; no implicit channel conversion is performed"
    );
    ensure!(
        status.operation != ProcessingOperation::Speech || (1..=2).contains(&info.channels),
        "Speech enhancement supports mono/stereo only"
    );
    status.sample_rate = Some(info.sample_rate);
    status.channels = Some(info.channels);
    status.frames = Some(info.frames);
    if status.operation == ProcessingOperation::Graph {
        graph::execute(&input, directory.path(), &mut status, cancel, tx)?;
        if status.settings.graph_mode == Some(GraphExecutionMode::Hybrid) {
            let snapshot = status
                .settings
                .graph
                .as_ref()
                .context("Missing graph snapshot")?;
            let target = status
                .settings
                .graph_target
                .context("Missing hybrid target")?;
            for node in snapshot.ancestors(target)? {
                if !status.graph_nodes.iter().any(|entry| entry.node == node) {
                    status.graph_nodes.push(GraphNodeStatus {
                        node,
                        phase: ProcessingPhase::Processing,
                        error: None,
                        model_id: None,
                        model_sha256: None,
                    });
                }
            }
        }
        check_cancel(cancel)?;
        ensure!(
            SourceStamp::read(&track.path)? == stamp,
            "Source changed while processing; output discarded"
        );
        let cache_bytes = graph::directory_bytes(directory.path())?;
        let pending_export = if let Some(destination) = &status.settings.graph_export_path {
            let target = status
                .settings
                .graph_target
                .context("Missing export target")?;
            let source = &status
                .graph_outputs
                .iter()
                .find(|o| o.port == target)
                .context("Missing export output")?
                .path;
            Some(prepare_export(
                source,
                destination,
                cancel,
                |done, total| {
                    report(
                        tx,
                        status.id,
                        ProcessingPhase::Processing,
                        done,
                        Some(total),
                    );
                },
            )?)
        } else {
            None
        };
        check_cancel(cancel)?;
        return Ok(ProcessingResult {
            track,
            stamp,
            status,
            directory,
            mix: None,
            cache_bytes,
            pending_export,
            runner,
        });
    }
    report(
        tx,
        status.id,
        ProcessingPhase::Processing,
        0,
        Some(info.frames),
    );
    let outputs = if status.operation == ProcessingOperation::Denoise {
        let destination = directory.path().join("denoised.wav");
        classical(
            &input,
            &destination,
            &config.denoise,
            status.settings.profile,
            0,
            cancel,
            |n| {
                report(
                    tx,
                    status.id,
                    ProcessingPhase::Processing,
                    n,
                    Some(info.frames),
                )
            },
        )?;
        ProcessingOutputs {
            denoised: Some(destination),
            ..Default::default()
        }
    } else {
        neural(
            &input,
            directory.path(),
            &config.neural,
            cancel,
            tx,
            &mut status,
        )?
    };
    check_cancel(cancel)?;
    ensure!(
        SourceStamp::read(&track.path)? == stamp,
        "Source changed while processing; output discarded"
    );
    for path in [&outputs.denoised, &outputs.vocals, &outputs.accompaniment]
        .into_iter()
        .flatten()
    {
        validate_output(path, info.sample_rate, info.channels, info.frames, cancel)?;
    }
    status.phase = ProcessingPhase::Complete;
    status.completed = info.frames;
    status.total = Some(info.frames);
    status.outputs = Some(outputs);
    let cache_bytes = graph::directory_bytes(directory.path())?;
    Ok(ProcessingResult {
        track,
        stamp,
        status,
        directory,
        mix: None,
        cache_bytes,
        pending_export: None,
        runner: None,
    })
}

fn prepare_export(
    source: &Path,
    destination: &Path,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<(tempfile::NamedTempFile, PathBuf)> {
    ensure!(
        destination.is_absolute(),
        "Export destination must be an absolute path"
    );
    ensure!(
        !destination.exists(),
        "Export destination already exists; choose a new file"
    );
    let parent = destination
        .parent()
        .context("Export destination has no parent")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    let mut input = fs::File::open(source)?;
    let total = input.metadata()?.len();
    let mut copied = 0;
    progress(0, total);
    let mut block = vec![0_u8; 1024 * 1024];
    loop {
        check_cancel(cancel)?;
        let count = input.read(&mut block)?;
        if count == 0 {
            break;
        }
        temporary.write_all(&block[..count])?;
        copied += count as u64;
        progress(copied, total);
    }
    temporary.as_file().sync_all()?;
    check_cancel(cancel)?;
    Ok((temporary, destination.to_path_buf()))
}
fn validate_output(
    path: &Path,
    rate: u32,
    channels: u16,
    frames: u64,
    cancel: &AtomicBool,
) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && metadata.len() <= MAX_OUTPUT_BYTES,
        "Invalid processing output file"
    );
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    ensure!(
        spec.sample_format == hound::SampleFormat::Float
            && spec.bits_per_sample == 32
            && spec.sample_rate == rate
            && spec.channels == channels
            && u64::from(reader.duration()) == frames,
        "Processing output format or alignment differs from source"
    );
    for (index, sample) in reader.samples::<f32>().enumerate() {
        if index % 8192 == 0 {
            check_cancel(cancel)?;
        }
        ensure!(
            sample?.is_finite(),
            "Processing output contains nonfinite PCM"
        );
    }
    Ok(())
}
fn classical(
    input: &Path,
    output: &Path,
    config: &rivu_dsp::denoise::DenoiseConfig,
    selection: Option<NoiseSelection>,
    selection_offset_frames: u64,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64),
) -> Result<u64> {
    use rivu_dsp::denoise::{DenoiseMode, Denoiser, ProfileCollector};
    let reader = hound::WavReader::open(input)?;
    let spec = reader.spec();
    let total = u64::from(reader.duration());
    drop(reader);
    let mut cancelled = || cancel.load(Ordering::Acquire);
    let profile = if config.mode == DenoiseMode::Profile {
        let selection =
            selection.context("Profile mode requires an explicit selected noise-only range")?;
        selection.validate()?;
        let start = ((selection.start_seconds * f64::from(spec.sample_rate)).floor() as u64)
            .checked_add(selection_offset_frames)
            .context("Noise profile frame offset overflow")?;
        let end = ((selection.end_seconds * f64::from(spec.sample_rate)).floor() as u64)
            .checked_add(selection_offset_frames)
            .context("Noise profile frame offset overflow")?;
        ensure!(
            start < end && end <= total,
            "Noise profile selection lies outside the source"
        );
        let mut collector = ProfileCollector::new(
            spec.sample_rate,
            usize::from(spec.channels),
            config.fft_size,
            end - start,
        )?;
        let mut reader = hound::WavReader::open(input)?;
        reader.seek(u32::try_from(start)?)?;
        let mut samples = reader.samples::<f32>();
        let mut block = Vec::with_capacity(8192 * usize::from(spec.channels));
        let mut remaining = (end - start) * u64::from(spec.channels);
        while remaining > 0 {
            check_cancel(cancel)?;
            block.clear();
            for _ in 0..remaining.min(block.capacity() as u64) {
                block.push(samples.next().context("Truncated profile input")??);
            }
            remaining -= block.len() as u64;
            collector.push_block(&block, &mut cancelled)?;
        }
        Some(collector.finish()?)
    } else {
        None
    };
    let mut denoiser = Denoiser::new(
        spec.sample_rate,
        usize::from(spec.channels),
        config.clone(),
        profile,
    )?;
    let latency = denoiser.lookahead_frames() as u64;
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    let mut writer = hound::WavWriter::new(std::io::BufWriter::new(file), spec)?;
    let mut reader = hound::WavReader::open(input)?;
    let mut samples = reader.samples::<f32>();
    let mut block = Vec::with_capacity(8192 * usize::from(spec.channels));
    let mut processed = Vec::new();
    let mut frames = 0;
    loop {
        check_cancel(cancel)?;
        block.clear();
        for _ in 0..block.capacity() {
            match samples.next() {
                Some(sample) => block.push(sample?),
                None => break,
            }
        }
        if block.is_empty() {
            break;
        }
        processed.clear();
        denoiser.process_block(&block, &mut processed, &mut cancelled)?;
        for &sample in &processed {
            writer.write_sample(sample)?;
        }
        frames += block.len() as u64 / u64::from(spec.channels);
        progress(frames);
    }
    processed.clear();
    denoiser.finish(&mut processed, &mut cancelled)?;
    for sample in processed {
        writer.write_sample(sample)?;
    }
    writer.finalize()?;
    Ok(latency)
}
fn neural(
    input: &Path,
    dir: &Path,
    neural: &rivu_format::config::NeuralConfig,
    cancel: &AtomicBool,
    tx: &Sender<Event>,
    status: &mut ProcessingTaskStatus,
) -> Result<ProcessingOutputs> {
    let id = status.id;
    let operation = status.operation;
    let output_dir = dir.join("neural");
    let request_path = dir.join("request.json");
    let task_id = id.to_string();
    let manifest = if operation == ProcessingOperation::Speech {
        neural.speech_manifest.as_ref()
    } else {
        neural.separation_manifest.as_ref()
    }
    .context("Missing model manifest")?;
    let mut parameters = if operation == ProcessingOperation::Speech {
        serde_json::json!({"wet_mix":neural.speech_wet_mix,"attenuation_limit_db":neural.speech_attenuation_limit_db})
    } else {
        serde_json::json!({"overlap":neural.separation_overlap})
    };
    if operation == ProcessingOperation::Speech && neural.speech_post_filter {
        parameters["post_filter"] = serde_json::Value::Bool(true);
    }
    let request = serde_json::json!({"version":1,"task_id":task_id,"operation":if operation==ProcessingOperation::Speech {"speech"}else{"separate"},"input_path":input,"output_dir":output_dir,"model_manifest":manifest,"device":if neural.device==rivu_format::config::NeuralDevice::Cpu {"cpu"}else{"cuda"},"threads":neural.threads,"parameters":parameters});
    fs::write(&request_path, serde_json::to_vec(&request)?)?;
    let stderr = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join("worker.stderr"))?;
    let interpreter = if operation == ProcessingOperation::Speech {
        &neural.speech_interpreter
    } else {
        &neural.separation_interpreter
    };
    let mut child = Process::new(interpreter)
        .arg(&neural.worker)
        .arg("--request")
        .arg(&request_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Starting local neural interpreter")?;
    // Read both pipes on this worker with finite nonblocking rounds. In particular,
    // stdout EOF is not process completion and must not bypass cancellation.
    let result = (|| -> Result<ProcessingOutputs> {
        let mut diagnostics = child.stderr.take().context("Missing worker stderr")?;
        let mut stdout = child.stdout.take().context("Missing worker stdout")?;
        set_pipe_nonblocking(&diagnostics)?;
        set_pipe_nonblocking(&stdout)?;
        let mut diagnostics_output = stderr;
        let mut diagnostics_kept = 0usize;
        let mut stdout_eof = false;
        let mut diagnostics_eof = false;
        let mut pending = Vec::with_capacity(8192);
        let mut block = [0u8; 8192];
        let mut exit = None;
        let mut complete = None;
        let mut consume_event = |line: &[u8]| -> Result<()> {
            let event: serde_json::Value = serde_json::from_slice(line)?;
            ensure!(
                event["version"] == 1 && event["task_id"] == task_id,
                "Neural event identity/version mismatch"
            );
            match event["type"].as_str() {
                Some("progress") => report(
                    tx,
                    id,
                    ProcessingPhase::Processing,
                    event["completed"].as_u64().unwrap_or(0),
                    event["total"].as_u64(),
                ),
                Some("error") => bail!("Neural processing failed: {}", event["message"]),
                Some("complete") => {
                    ensure!(complete.is_none(), "Duplicate neural completion");
                    complete = Some(event);
                }
                _ => bail!("Unknown neural event type"),
            }
            Ok(())
        };
        let exit = loop {
            check_cancel(cancel)?;
            if !stdout_eof {
                for _ in 0..8 {
                    match stdout.read(&mut block) {
                        Ok(0) => {
                            stdout_eof = true;
                            if !pending.is_empty() {
                                consume_event(&pending)?;
                                pending.clear();
                            }
                            break;
                        }
                        Ok(n) => {
                            for &byte in &block[..n] {
                                ensure!(
                                    pending.len() < 65536,
                                    "Neural worker event exceeds 64 KiB"
                                );
                                pending.push(byte);
                                if byte == b'\n' {
                                    consume_event(&pending)?;
                                    pending.clear();
                                }
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) => return Err(error.into()),
                    }
                }
            }
            if !diagnostics_eof {
                for _ in 0..8 {
                    match diagnostics.read(&mut block) {
                        Ok(0) => {
                            diagnostics_eof = true;
                            break;
                        }
                        Ok(n) => {
                            let count = n.min(65536usize.saturating_sub(diagnostics_kept));
                            diagnostics_output.write_all(&block[..count])?;
                            diagnostics_kept += count;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) => return Err(error.into()),
                    }
                }
            }
            if exit.is_none() {
                exit = child.try_wait()?;
            }
            if stdout_eof && let Some(exit) = exit {
                break exit;
            }
            thread::sleep(Duration::from_millis(20));
        };
        check_cancel(cancel)?;
        ensure!(exit.success(), "Neural interpreter failed: {exit}");
        let event = complete.context("Neural worker exited without completion")?;
        ensure!(
            event["sample_rate"].as_u64() == status.sample_rate.map(u64::from)
                && event["channels"].as_u64() == status.channels.map(u64::from)
                && event["frames"].as_u64() == status.frames,
            "Neural completion alignment mismatch"
        );
        status.model_id = Some(
            event["model"]["id"]
                .as_str()
                .context("Missing model id")?
                .to_owned(),
        );
        status.model_sha256 = Some(
            event["model"]["sha256"]
                .as_str()
                .context("Missing model hash")?
                .to_owned(),
        );
        let output = |key: &str| -> Result<PathBuf> {
            let path = PathBuf::from(
                event["outputs"][key]
                    .as_str()
                    .context("Missing neural output")?,
            );
            let path = fs::canonicalize(path)?;
            ensure!(
                path.starts_with(fs::canonicalize(&output_dir)?),
                "Neural output escaped task-owned directory"
            );
            Ok(path)
        };
        Ok(if operation == ProcessingOperation::Speech {
            ProcessingOutputs {
                denoised: Some(output("speech")?),
                ..Default::default()
            }
        } else {
            ProcessingOutputs {
                vocals: Some(output("vocals")?),
                accompaniment: Some(output("accompaniment")?),
                ..Default::default()
            }
        })
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

fn set_pipe_nonblocking(pipe: &impl std::os::fd::AsRawFd) -> Result<()> {
    let fd = pipe.as_raw_fd();
    // SAFETY: fd is borrowed from a live child pipe; fcntl does not retain it.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: F_SETFL accepts the existing flags plus O_NONBLOCK for this fd.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

pub(crate) fn mix(
    service: &mut Service,
    id: u64,
    generation: u64,
    vocal_gain_db: f32,
    accompaniment_gain_db: f32,
) -> Result<()> {
    ensure!(
        service.active.is_none(),
        "An audio processing task is already active"
    );
    ensure!(
        vocal_gain_db.is_finite()
            && accompaniment_gain_db.is_finite()
            && (-96.0..=24.0).contains(&vocal_gain_db)
            && (-96.0..=24.0).contains(&accompaniment_gain_db),
        "Stem gains must be finite and within -96..24 dB"
    );
    let result = service
        .results
        .iter()
        .find(|r| r.status.id == id)
        .context("Processing result is no longer retained")?;
    let outputs = result
        .status
        .outputs
        .as_ref()
        .context("No processing outputs")?;
    let vocals = outputs.vocals.clone().context("No vocal stem")?;
    let accompaniment = outputs
        .accompaniment
        .clone()
        .context("No accompaniment stem")?;
    let file = tempfile::Builder::new()
        .prefix("mix-")
        .suffix(".wav")
        .tempfile_in(result.directory.path())?;
    let (file, path) = file.keep().map_err(|e| e.error)?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancel = cancelled.clone();
    let tx = service.sender.clone();
    let worker = thread::Builder::new()
        .name("rivu-stem-mix".into())
        .spawn(move || {
            let outcome = (|| -> Result<PathBuf> {
                let mut vocals = hound::WavReader::open(vocals)?;
                let mut accompaniment = hound::WavReader::open(accompaniment)?;
                ensure!(
                    vocals.spec() == accompaniment.spec()
                        && vocals.duration() == accompaniment.duration(),
                    "Stem alignment mismatch"
                );
                let mut writer =
                    hound::WavWriter::new(std::io::BufWriter::new(file), vocals.spec())?;
                let vg = 10.0_f32.powf(vocal_gain_db / 20.0);
                let ag = 10.0_f32.powf(accompaniment_gain_db / 20.0);
                for (index, (v, a)) in vocals
                    .samples::<f32>()
                    .zip(accompaniment.samples::<f32>())
                    .enumerate()
                {
                    if index % 8192 == 0 {
                        check_cancel(&cancel)?;
                    }
                    let sample = v? * vg + a? * ag;
                    ensure!(sample.is_finite(), "Nonfinite mixed sample");
                    writer.write_sample(sample)?;
                }
                writer.finalize()?;
                check_cancel(&cancel)?;
                Ok(path.clone())
            })();
            if outcome.is_err() {
                let _ = fs::remove_file(&path);
            }
            let _ = tx.blocking_send(Event::MixFinished {
                id,
                generation,
                result: outcome,
            });
        })?;
    service.active = Some(Active {
        id,
        cancelled,
        worker,
    });
    Ok(())
}
