use super::export::prepare_export;
use super::{AudioOutputs, CachedGraphOutput, Event, ProcessingResult, SourceStamp, graph, neural};
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
    cancel: &AtomicBool,
    tx: &Sender<Event>,
) -> Result<ProcessingResult> {
    let open_source = || -> Result<audio::SourceRenderer> {
        let renderer = audio::SourceRenderer::open(
            &track.path,
            track.cue.as_ref().map(|cue| audio::PlaybackRange {
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
    ensure!(
        status.operation != ProcessingOperation::Separate
            || native.layout.as_ref() == [audio::Channel::FrontLeft, audio::Channel::FrontRight],
        "Vocal separation requires a stereo source; no implicit channel conversion is performed"
    );
    ensure!(
        status.operation != ProcessingOperation::Speech
            || native.layout.as_ref() == [audio::Channel::FrontCenter]
            || native.layout.as_ref() == [audio::Channel::FrontLeft, audio::Channel::FrontRight],
        "Speech enhancement supports mono/stereo speaker layouts only"
    );
    status.sample_rate = Some(native.sample_rate);
    status.channels = Some(native.channels());
    let mut runner = None;
    if let Some(mode) = status
        .settings
        .graph_mode
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
    progress.report(ProcessingPhase::Rendering, 0, None);
    let mut graph_outputs = BTreeMap::new();
    let outputs = if status.operation == ProcessingOperation::Denoise {
        let pcm = classical_native(
            renderer,
            open_source,
            &config.denoise,
            status.settings.profile,
            Arc::clone(&budget),
            cancel,
            |frames| {
                progress.report(
                    ProcessingPhase::Processing,
                    frames,
                    native.estimated_frames(),
                );
            },
        )?;
        status.frames = Some(pcm.frames());
        AudioOutputs {
            denoised: Some(pcm),
            ..Default::default()
        }
    } else if status.operation == ProcessingOperation::Graph && graph::native_only(&status)? {
        graph_outputs = graph::execute_native(
            renderer,
            &open_source,
            Arc::clone(&budget),
            &mut status,
            cancel,
            &mut progress,
        )?;
        AudioOutputs::default()
    } else {
        // Whole-signal model boundaries require seekable input. Native-only
        // paths above never allocate a complete decoded source alongside output.
        let input = renderer.render_pcm(Arc::clone(&budget), cancel)?;
        status.frames = Some(input.frames());
        if status.operation == ProcessingOperation::Graph {
            graph_outputs = graph::execute(
                &input,
                Arc::clone(&budget),
                &mut status,
                cancel,
                &mut progress,
            )?;
            AudioOutputs::default()
        } else {
            neural::run(
                &input,
                &config.neural,
                Arc::clone(&budget),
                cancel,
                &mut progress,
                &mut status,
            )?
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
    let pending_export = if let Some(destination) = &status.settings.graph_export_path {
        let target = status
            .settings
            .graph_target
            .context("Missing export target")?;
        Some(prepare_export(
            &graph_outputs
                .get(&target)
                .context("Missing export output")?
                .pcm,
            destination,
            cancel,
            |done, total| {
                progress.report(ProcessingPhase::Processing, done, Some(total));
            },
        )?)
    } else {
        None
    };
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

fn classical_native(
    mut renderer: audio::SourceRenderer,
    reopen: impl FnOnce() -> Result<audio::SourceRenderer>,
    config: &rivu_dsp::denoise::DenoiseConfig,
    selection: Option<NoiseSelection>,
    budget: Arc<audio::PcmBudget>,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64),
) -> Result<audio::PcmAudio> {
    use rivu_dsp::denoise::{DenoiseMode, Denoiser, ProfileCollector};
    let rate = renderer.format().sample_rate;
    let layout = Arc::clone(&renderer.format().layout);
    let channels = renderer.format().channels();
    config.validate_for_format(rate, usize::from(channels))?;
    let profile = if config.mode == DenoiseMode::Profile {
        let selection =
            selection.context("Profile mode requires an explicit selected noise-only range")?;
        selection.validate()?;
        let start = (selection.start_seconds * f64::from(rate)).floor() as u64;
        let end = (selection.end_seconds * f64::from(rate)).floor() as u64;
        ensure!(
            start < end,
            "Noise profile selection contains no source frames"
        );
        let mut capture = reopen()?;
        ensure!(
            capture.format().sample_rate == rate && capture.format().layout == layout,
            "Source format changed during profile capture"
        );
        let mut collector =
            ProfileCollector::new(rate, usize::from(channels), config.fft_size, end - start)?;
        let mut block = Vec::new();
        let mut position = 0_u64;
        while position < end && capture.read_block(&mut block, cancel)? {
            ensure!(
                block.len().is_multiple_of(usize::from(channels)),
                "Incomplete profile source frame"
            );
            let next = position
                .checked_add((block.len() / usize::from(channels)) as u64)
                .context("Profile source length overflow")?;
            let from = position.max(start);
            let to = next.min(end);
            if from < to {
                collector.push_block(
                    &block[(from - position) as usize * usize::from(channels)
                        ..(to - position) as usize * usize::from(channels)],
                    &mut || cancel.load(Ordering::Acquire),
                )?;
            }
            position = next;
        }
        ensure!(
            end <= position,
            "Noise profile selection lies outside the source"
        );
        capture.finish(cancel)?;
        Some(collector.finish()?)
    } else {
        None
    };
    let mut denoiser = if graph::denoise_bypassed(config) {
        None
    } else {
        Some(Denoiser::new(
            rate,
            usize::from(channels),
            config.clone(),
            profile,
        )?)
    };
    let mut builder = audio::PcmBuilder::new(rate, Arc::clone(&layout), budget)?;
    let mut block = Vec::new();
    let mut processed = Vec::new();
    let mut frames = 0_u64;
    while renderer.read_block(&mut block, cancel)? {
        ensure!(
            block.len().is_multiple_of(usize::from(channels)),
            "Incomplete denoise source frame"
        );
        frames = frames
            .checked_add((block.len() / usize::from(channels)) as u64)
            .context("Denoise source length overflow")?;
        if let Some(denoiser) = &mut denoiser {
            processed.clear();
            denoiser.process_block(&block, &mut processed, &mut || {
                cancel.load(Ordering::Acquire)
            })?;
            builder.push(&processed)?;
        } else {
            builder.push(&block)?;
        }
        progress(frames);
    }
    if let Some(denoiser) = &mut denoiser {
        processed.clear();
        denoiser.finish(&mut processed, &mut || cancel.load(Ordering::Acquire))?;
        builder.push(&processed)?;
    }
    renderer.finish(cancel)?;
    let pcm = builder.finish()?;
    validate_output(&pcm, rate, &layout, frames, cancel)?;
    Ok(pcm)
}
