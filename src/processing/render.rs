use super::export::prepare_export;
use super::{
    AudioOutputs, CachedGraphOutput, Event, ModelRequest, ProcessingResult, SourceStamp, graph,
    neural,
};
use crate::config::Config;
use anyhow::{Context, Result, ensure};
use async_channel::Sender;
use rivu_audio as audio;
use rivu_protocol::model::*;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub(super) fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::Acquire), "Processing cancelled");
    Ok(())
}
pub(super) struct ProgressReporter<'a> {
    pub(super) sender: &'a Sender<Event>,
    id: u64,
    phase: Option<ProcessingPhase>,
    last_emit: Instant,
}

impl<'a> ProgressReporter<'a> {
    pub(super) fn new(sender: &'a Sender<Event>, id: u64) -> Self {
        Self {
            sender,
            id,
            phase: None,
            last_emit: Instant::now(),
        }
    }

    pub(super) fn report(&mut self, phase: ProcessingPhase, completed: u64, total: Option<u64>) {
        let transition = self.phase != Some(phase);
        if !transition && self.last_emit.elapsed() < Duration::from_millis(100) {
            return;
        }
        self.last_emit = Instant::now();
        let event = Event::Progress {
            id: self.id,
            phase,
            completed,
            total,
        };
        if transition {
            // Stage changes share the reliable terminal channel; blocks within
            // a stage are snapshots, not individual work that must be published.
            self.phase = Some(phase);
            let _ = self.sender.send_blocking(event);
        } else if self.sender.is_empty() {
            let _ = self.sender.try_send(event);
        }
    }
}
pub(super) fn run_cached_export(
    track: Track,
    stamp: SourceStamp,
    _budget: Arc<audio::PcmBudget>,
    mut status: ProcessingTaskStatus,
    cached: CachedGraphOutput,
    cancel: &AtomicBool,
    tx: &Sender<Event>,
) -> Result<ProcessingResult> {
    let pcm = cached.pcm;
    status.settings.graph_mode = Some(GraphExecutionMode::Offline);
    let _ = tx.send_blocking(Event::GraphPrepared {
        id: status.id,
        mode: GraphExecutionMode::Offline,
        boundary: None,
        plan: None,
    });
    let mut progress = ProgressReporter::new(tx, status.id);
    let destination = status
        .settings
        .graph_export_path
        .as_ref()
        .context("Missing export destination")?;
    let pending_export = Some(prepare_export(&pcm, destination, cancel, |done, total| {
        progress.report(ProcessingPhase::Processing, done, Some(total));
    })?);
    ensure!(
        SourceStamp::read(&track.path)? == stamp,
        "Source changed while exporting; output discarded"
    );
    status.phase = ProcessingPhase::Complete;
    status.graph_nodes.clear();
    status.sample_rate = Some(pcm.sample_rate());
    status.channels = Some(pcm.channels());
    status.frames = Some(pcm.frames());
    status.completed = pcm.frames();
    status.total = Some(pcm.frames());
    Ok(ProcessingResult {
        track,
        stamp,
        status,
        outputs: AudioOutputs::default(),
        graph_outputs: BTreeMap::new(),
        mix: None,
        pending_export,
        runner: None,
    })
}

pub(super) fn run(
    track: Track,
    stamp: SourceStamp,
    config: Arc<Config>,
    budget: Arc<audio::PcmBudget>,
    mut status: ProcessingTaskStatus,
    cancellation: &super::Cancellation,
    tx: &Sender<Event>,
) -> Result<ProcessingResult> {
    let cancel = cancellation.flag();
    let open_source = || -> Result<audio::SourceRenderer> {
        let renderer = audio::SourceRenderer::open(
            &track.path,
            track
                .cue
                .as_ref()
                .map(|cue| rivu_protocol::audio::PlaybackRange {
                    start_seconds: cue.start_seconds(),
                    end_seconds: cue.end_seconds(),
                }),
            track.decoder_hint,
            config.media_read_buffer_mib,
            config.ffmpeg_enabled,
            cancel,
        )?;
        stamp.validate_rendered(renderer.format().source_identity)?;
        Ok(renderer)
    };
    let renderer = open_source()?;
    let mut progress = ProgressReporter::new(tx, status.id);
    let native = renderer.format().clone();
    status.sample_rate = Some(native.sample_rate);
    status.channels = Some(native.channels());
    let mut runner = None;
    if let Some(mode) = status
        .settings
        .graph_requested_mode
        .filter(|mode| *mode != GraphExecutionMode::Offline)
    {
        let (actual, boundary, prepared) = graph::prepare(
            status
                .settings
                .graph
                .as_ref()
                .context("Missing graph snapshot")?,
            status
                .settings
                .graph_target
                .context("Missing graph target")?,
            mode,
            native.sample_rate,
            usize::from(native.channels()),
            cancel,
        )?;
        status.settings.graph_mode = Some(actual);
        status.settings.graph_boundary = boundary;
        status.graph_plan = prepared.as_ref().map(graph::plan_info);
        runner = prepared;
    }
    if status.settings.graph_requested_mode == Some(GraphExecutionMode::Offline) {
        status.settings.graph_mode = Some(GraphExecutionMode::Offline);
    }
    if let Some(mode) = status.settings.graph_mode {
        let _ = tx.send_blocking(Event::GraphPrepared {
            id: status.id,
            mode,
            boundary: status.settings.graph_boundary,
            plan: status.graph_plan.clone(),
        });
    }
    if status.settings.graph_mode == Some(GraphExecutionMode::Realtime) {
        check_cancel(cancel)?;
        ensure!(
            SourceStamp::read(&track.path)? == stamp,
            "Source changed while preparing graph"
        );
        for node in &mut status.graph_nodes {
            node.phase = ProcessingPhase::Processing;
        }
        status.phase = ProcessingPhase::Complete;
        return Ok(ProcessingResult {
            track,
            stamp,
            status,
            outputs: AudioOutputs::default(),
            graph_outputs: BTreeMap::new(),
            mix: None,
            pending_export: None,
            runner,
        });
    }
    let model = if matches!(
        status.operation,
        ProcessingOperation::Speech | ProcessingOperation::Separate
    ) {
        let mut provenance = super::ModelProvenance::default();
        let model = neural::prepare(
            status.operation,
            &config.neural,
            native.sample_rate,
            &native.layout,
            &mut provenance,
            cancel,
        );
        if model.is_err() {
            let _ = tx.send_blocking(Event::Model {
                id: status.id,
                provenance,
            });
        }
        Some(model?)
    } else {
        None
    };
    let native_graph =
        status.operation == ProcessingOperation::Graph && graph::native_only(&status)?;
    let models = if status.operation == ProcessingOperation::Graph && !native_graph {
        graph::prepare_models(&mut status, native.sample_rate, &native.layout, cancel, tx)?
    } else {
        BTreeMap::new()
    };
    progress.report(ProcessingPhase::Rendering, 0, None);
    let mut graph_outputs = BTreeMap::new();
    let mut pending_export = None;
    let outputs = if status.operation == ProcessingOperation::Denoise {
        let native_output = graph::execute_native(
            renderer,
            &open_source,
            Arc::clone(&budget),
            &mut status,
            cancel,
            &mut progress,
        )?;
        let pcm = native_output.denoised.context("Missing denoise output")?;
        AudioOutputs {
            denoised: Some(pcm),
            ..Default::default()
        }
    } else if native_graph {
        let native_output = graph::execute_native(
            renderer,
            &open_source,
            Arc::clone(&budget),
            &mut status,
            cancel,
            &mut progress,
        )?;
        graph_outputs = native_output.outputs;
        pending_export = native_output.pending_export;
        AudioOutputs::default()
    } else {
        // Whole-signal model boundaries require seekable input. Native-only
        // paths above never allocate a complete decoded source alongside output.
        let input = renderer.render_pcm(Arc::clone(&budget), cancel)?;
        status.frames = Some(input.frames());
        if status.operation == ProcessingOperation::Graph {
            graph_outputs = graph::execute(
                input,
                Arc::clone(&budget),
                &mut status,
                cancellation,
                &mut progress,
                models,
            )?;
            AudioOutputs::default()
        } else {
            let outcome = neural::run(
                ModelRequest {
                    task_id: status.id,
                    operation: status.operation,
                    input: &input,
                    config: &config.neural,
                    budget: Arc::clone(&budget),
                    cancel: cancellation,
                },
                model.context("Missing admitted model")?,
                &mut progress,
            );
            status.model_id.clone_from(&outcome.provenance.id);
            status.model_sha256.clone_from(&outcome.provenance.sha256);
            // Reliable provenance precedes terminal failure, unlike best-effort progress.
            let _ = tx.send_blocking(Event::Model {
                id: status.id,
                provenance: outcome.provenance,
            });
            outcome.result?
        }
    };
    let frames = status
        .frames
        .context("Missing decoded source frame count")?;
    check_cancel(cancel)?;
    ensure!(
        SourceStamp::read(&track.path)? == stamp,
        "Source changed while processing; output discarded"
    );
    for pcm in [&outputs.denoised, &outputs.vocals, &outputs.accompaniment]
        .into_iter()
        .flatten()
    {
        validate_output(pcm, native.sample_rate, &native.layout, frames, cancel)?;
    }
    if pending_export.is_none()
        && let Some(destination) = &status.settings.graph_export_path
    {
        let target = status
            .settings
            .graph_target
            .context("Missing export target")?;
        pending_export = Some(prepare_export(
            &graph_outputs
                .get(&target)
                .context("Missing export output")?
                .pcm,
            destination,
            cancel,
            |done, total| {
                progress.report(ProcessingPhase::Processing, done, Some(total));
            },
        )?);
    }
    status.phase = ProcessingPhase::Complete;
    status.completed = frames;
    status.total = Some(frames);
    if status.operation != ProcessingOperation::Graph {
        status.outputs = Some(ProcessingOutputs {
            denoised: outputs.denoised.is_some(),
            vocals: outputs.vocals.is_some(),
            accompaniment: outputs.accompaniment.is_some(),
        });
    }
    check_cancel(cancel)?;
    Ok(ProcessingResult {
        track,
        stamp,
        status,
        outputs,
        graph_outputs,
        mix: None,
        pending_export,
        runner,
    })
}

pub(super) fn validate_output(
    pcm: &audio::PcmAudio,
    rate: u32,
    layout: &[audio::Channel],
    frames: u64,
    cancel: &AtomicBool,
) -> Result<()> {
    check_cancel(cancel)?;
    ensure!(
        pcm.sample_rate() == rate && pcm.layout().as_ref() == layout && pcm.frames() == frames,
        "Processing output format or alignment differs from source"
    );
    Ok(())
}
