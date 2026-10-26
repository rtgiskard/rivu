//! Command handling, playback lifecycle, refill scheduling, and analyzer policy.

use super::{
    AudioCommand, AudioEvent, AudioShutdownReceiver, AudioSnapshot, PlaybackMetadata,
    WaveformFrame,
    graph::GraphRunner,
    output::validate_device,
    playback::{Playback, PlaybackOptions},
    source::{Source, SourceSelection},
};
use crate::analysis::AnalysisWorker;
use anyhow::anyhow;
use async_channel::TryRecvError;
use crossbeam_channel::{Receiver, select};
use futures::FutureExt;
use parking_lot::RwLock;
use rivu_dsp::equalizer::config::EqualizerConfig;
use rivu_format::media::DecoderHint;
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

// Include coalesced seeks and read-ahead barriers in each scheduling round.
const MAX_COMMANDS_PER_ROUND: usize = 64;

struct CommandInbox {
    receiver: async_channel::Receiver<AudioCommand>,
    // A non-seek read ahead is an ordering barrier, not a command to discard.
    pending: Option<AudioCommand>,
}

impl CommandInbox {
    fn has_ready(&self) -> bool {
        self.pending.is_some() || !self.receiver.is_empty()
    }

    fn try_recv_round(&mut self, remaining: &mut usize) -> Result<AudioCommand, TryRecvError> {
        if *remaining == 0 {
            return Err(TryRecvError::Empty);
        }
        let command = if let Some(command) = self.pending.take() {
            command
        } else {
            self.receiver.try_recv()?
        };
        *remaining -= 1;
        let AudioCommand::Seek(mut seconds) = command else {
            return Ok(command);
        };
        while *remaining > 0 {
            match self.receiver.try_recv() {
                Ok(AudioCommand::Seek(target)) => {
                    *remaining -= 1;
                    seconds = target;
                }
                Ok(barrier) => {
                    *remaining -= 1;
                    self.pending = Some(barrier);
                    break;
                }
                Err(_) => break,
            }
        }
        Ok(AudioCommand::Seek(seconds))
    }
}

// Only the worker owns an unfinished source. No output or lifecycle event is
// published until its selected opening has completed in finite decode rounds.
struct Opening {
    path: PathBuf,
    range: Option<super::PlaybackRange>,
    decoder_hint: DecoderHint,
    generation: u64,
    metadata: Arc<PlaybackMetadata>,
    start: f64,
    paused: bool,
    source: Option<Source>,
    source_info: Option<super::MediaInfo>,
    listened_seconds: f64,
    graph: Option<GraphRunner>,
    expected_identity: Option<super::SourceIdentity>,
}

impl Opening {
    fn poll(&mut self, buffer_len: usize, ffmpeg_enabled: bool) -> anyhow::Result<bool> {
        if self.source.is_none() {
            self.source = Some(Source::begin_open(
                &self.path,
                buffer_len,
                self.decoder_hint,
                ffmpeg_enabled,
                SourceSelection {
                    range: self.range,
                    start: self.start,
                },
            )?);
            if let Some(expected) = self.expected_identity
                && self.source.as_ref().unwrap().identity() != expected
            {
                anyhow::bail!("Opened source differs from the source admitted for graph audition");
            }
        }
        self.source.as_mut().unwrap().poll_open()
    }

    fn uses_ffmpeg(&self) -> bool {
        self.source.as_ref().map_or(
            self.decoder_hint == DecoderHint::Ffmpeg,
            Source::uses_ffmpeg,
        )
    }
}

pub(super) struct Worker {
    commands: CommandInbox,
    commands_ready: Receiver<()>,
    events: async_channel::Sender<AudioEvent>,
    shutdown: AudioShutdownReceiver,
    analyzer: AnalysisWorker,
    waveform: Arc<RwLock<WaveformFrame>>,
    playback: Option<Playback>,
    opening: Option<Opening>,
    device: Option<String>,
    volume: f32,
    media_read_buffer_len: usize,
    still_mode: bool,
    ffmpeg_enabled: bool,
    pipewire_auto_mix: bool,
    equalizer: Arc<EqualizerConfig>,
    last_snapshot: AudioSnapshot,
    progress: Instant,
    last_progress: Option<(u64, f64, f64)>,
}
impl Worker {
    pub(super) fn new(
        commands: async_channel::Receiver<AudioCommand>,
        commands_ready: Receiver<()>,
        events: async_channel::Sender<AudioEvent>,
        shutdown: AudioShutdownReceiver,
        analyzer: AnalysisWorker,
        waveform: Arc<RwLock<WaveformFrame>>,
        media_read_buffer_len: usize,
    ) -> Self {
        Self {
            commands: CommandInbox {
                receiver: commands,
                pending: None,
            },
            commands_ready,
            events,
            shutdown,
            analyzer,
            waveform,
            playback: None,
            opening: None,
            device: None,
            volume: 0.7,
            media_read_buffer_len,
            still_mode: false,
            ffmpeg_enabled: false,
            pipewire_auto_mix: true,
            equalizer: Arc::new(EqualizerConfig::default()),
            last_snapshot: AudioSnapshot::default(),
            progress: Instant::now(),
            last_progress: None,
        }
    }
    fn event(&self, event: AudioEvent) {
        send_event(&self.events, &self.shutdown.events, event);
    }
    fn stop(&mut self) -> AudioSnapshot {
        self.analyzer.set_playback_active(false);
        if let Some(opening) = self.opening.take() {
            self.last_snapshot = AudioSnapshot {
                generation: opening.generation,
                position_seconds: opening.start,
                listened_seconds: opening.listened_seconds,
            };
        }
        if let Some(mut playback) = self.playback.take() {
            self.last_snapshot = playback.close();
        }
        self.last_snapshot
    }
    fn refresh_analysis(&self) {
        self.analyzer.set_playback_active(
            self.playback
                .as_ref()
                .is_some_and(|playback| !playback.paused()),
        );
    }
    fn command(&mut self, command: AudioCommand) -> bool {
        match command {
            AudioCommand::Shutdown => {
                self.stop();
                return false;
            }
            AudioCommand::Stop => {
                self.stop();
            }
            AudioCommand::StopAndSnapshot(reply) => {
                let snapshot = self.stop();
                let _ = reply.send(snapshot);
            }
            AudioCommand::StillMode(enabled) => {
                self.analyzer.set_still_mode(enabled);
                self.still_mode = enabled;
                if let Some(playback) = self.playback.as_mut() {
                    playback.set_still_mode(enabled);
                } else {
                    self.waveform.write().set_still_mode(enabled);
                }
                self.refresh_analysis();
            }
            AudioCommand::AnalysisSettings(settings) => {
                self.analyzer.configure(settings);
                if let Some(playback) = self.playback.as_mut() {
                    playback.set_signal(self.analyzer.settings().signal);
                }
            }
            AudioCommand::WaveformSnapshot(reply) => {
                let snapshot = if let Some(playback) = &self.playback {
                    Ok(Some(playback.waveform_snapshot()))
                } else if self.opening.is_some() {
                    Err(
                        "Audio source is still opening; try the explicit scan again after it opens"
                            .to_owned(),
                    )
                } else {
                    Ok(None)
                };
                let _ = reply.send(snapshot);
            }
            AudioCommand::FfmpegEnabled(enabled) => {
                self.ffmpeg_enabled = enabled;
                if !enabled
                    && let Some(source) = self
                        .opening
                        .as_mut()
                        .and_then(|opening| opening.source.as_mut())
                {
                    source.disable_opening_fallback();
                }
                if !enabled && self.opening.as_ref().is_some_and(Opening::uses_ffmpeg) {
                    let AudioSnapshot {
                        generation,
                        position_seconds,
                        listened_seconds,
                    } = self.stop();
                    self.event(AudioEvent::Progress {
                        generation,
                        position_seconds,
                        listened_seconds,
                    });
                    self.event(AudioEvent::DecoderStopped { generation });
                }
                if !enabled && self.playback.as_ref().is_some_and(Playback::uses_ffmpeg) {
                    let AudioSnapshot {
                        generation,
                        position_seconds,
                        listened_seconds,
                    } = self.stop();
                    self.event(AudioEvent::Progress {
                        generation,
                        position_seconds,
                        listened_seconds,
                    });
                    self.event(AudioEvent::DecoderStopped { generation });
                }
            }
            AudioCommand::MediaReadBuffer(mebibytes) => {
                self.media_read_buffer_len = mebibytes as usize * super::BYTES_PER_MEBIBYTE;
            }
            AudioCommand::Volume(volume) => {
                self.volume = volume;
                if let Some(playback) = &self.playback {
                    playback.set_volume(volume);
                }
            }
            AudioCommand::Equalizer(config) => {
                self.equalizer = Arc::clone(&config);
                let result = config
                    .validate()
                    .map_err(anyhow::Error::from)
                    .and_then(|()| {
                        if let Some(playback) = self.playback.as_mut() {
                            playback.set_equalizer(&config)
                        } else {
                            Ok(())
                        }
                    });
                if let Err(error) = result {
                    self.event(AudioEvent::EqualizerSettingsFailed {
                        generation: self
                            .playback
                            .as_ref()
                            .map(Playback::generation)
                            .or_else(|| self.opening.as_ref().map(|opening| opening.generation)),
                        config,
                        message: format!("{error:#}"),
                    });
                }
            }
            AudioCommand::Audition {
                path,
                range,
                decoder_hint,
                source_info,
                metadata,
                generation,
                start_seconds,
                paused,
                listened_seconds,
            } => {
                self.stop();
                self.opening = Some(Opening {
                    path,
                    range,
                    decoder_hint,
                    generation,
                    metadata,
                    start: start_seconds,
                    paused,
                    source: None,
                    source_info,
                    listened_seconds,
                    graph: None,
                    expected_identity: None,
                });
            }
            AudioCommand::GraphAudition {
                path,
                expected_identity,
                range,
                decoder_hint,
                source_info,
                metadata,
                generation,
                start_seconds,
                paused,
                listened_seconds,
                runner,
            } => {
                self.stop();
                self.opening = Some(Opening {
                    path,
                    range,
                    decoder_hint,
                    generation,
                    metadata,
                    start: start_seconds,
                    paused,
                    source: None,
                    source_info,
                    listened_seconds,
                    graph: Some(*runner),
                    expected_identity: Some(expected_identity),
                });
            }
            AudioCommand::Load {
                path,
                range,
                decoder_hint,
                generation,
                metadata,
                start_seconds,
                paused,
            } => {
                self.stop();
                self.opening = Some(Opening {
                    path,
                    range,
                    decoder_hint,
                    generation,
                    metadata,
                    start: start_seconds,
                    paused,
                    source: None,
                    source_info: None,
                    listened_seconds: 0.0,
                    graph: None,
                    expected_identity: None,
                });
            }
            AudioCommand::Metadata {
                generation,
                metadata,
            } => {
                if let Some(opening) = self.opening.as_mut()
                    && opening.generation == generation
                {
                    opening.metadata = metadata;
                } else if let Some(playback) = self.playback.as_mut()
                    && playback.generation() == generation
                    && let Err(error) = playback.set_metadata(metadata)
                {
                    self.fail(error);
                }
            }
            AudioCommand::Pause(paused) => {
                if let Some(opening) = self.opening.as_mut() {
                    opening.paused = paused;
                }
                if let Some(playback) = self.playback.as_mut() {
                    playback.pause(paused);
                }
                self.refresh_analysis();
            }
            AudioCommand::Seek(seconds) => {
                let result = if let Some(opening) = self.opening.as_mut() {
                    opening.start = seconds.max(0.0);
                    opening
                        .source
                        .as_mut()
                        .map_or(Ok(()), |source| source.seek(opening.start))
                } else {
                    self.reopen(Some(seconds))
                };
                if let Err(error) = result {
                    self.fail(error);
                }
            }
            AudioCommand::OutputSettings {
                request,
                device,
                auto_mix,
            } => {
                let device_changed = self.device != device;
                let mix_changed = self.pipewire_auto_mix != auto_mix;
                // Explicit idle applications must produce a fresh validation result,
                // even for unchanged settings; request zero is startup initialization.
                let idle_apply = self.playback.is_none() && self.opening.is_none() && request != 0;
                if device_changed || mix_changed || idle_apply {
                    // Desired settings survive failure; later loads must not use the old sink.
                    self.device = device;
                    self.pipewire_auto_mix = auto_mix;
                    let result = if device_changed || idle_apply {
                        validate_device(self.device.as_deref())
                    } else {
                        Ok(())
                    }
                    .and_then(|()| self.reopen(None));
                    if let Err(error) = result {
                        if self.playback.is_some() || self.opening.is_some() {
                            // This generation was stopped, even if a newer
                            // device request supersedes the failed setting.
                            self.fail(error);
                        } else {
                            self.event(AudioEvent::OutputSettingsFailed {
                                request,
                                device: self.device.clone(),
                                auto_mix,
                                message: format!("{error:#}"),
                            });
                        }
                    }
                }
            }
        }
        true
    }
    fn reopen(&mut self, target: Option<f64>) -> anyhow::Result<()> {
        if let Some(playback) = self.playback.as_mut() {
            playback.restart_output(
                target,
                self.device.as_deref(),
                self.volume,
                self.pipewire_auto_mix,
                &self.analyzer,
            )?;
            self.refresh_analysis();
        }
        Ok(())
    }
    fn fail(&mut self, error: anyhow::Error) {
        let generation = self.stop().generation;
        self.event(AudioEvent::Failed {
            generation,
            message: format!("{error:#}"),
        });
    }
    fn poll_opening(&mut self) {
        let Some(opening) = self.opening.as_mut() else {
            return;
        };
        let generation = opening.generation;
        let result = opening.poll(self.media_read_buffer_len, self.ffmpeg_enabled);
        // Shutdown may disconnect during a single, non-interruptible backend
        // call. Do not create output or publish completion after cancellation.
        if matches!(
            self.shutdown.signal.try_recv(),
            Err(crossbeam_channel::TryRecvError::Disconnected)
        ) {
            self.stop();
            return;
        }
        match result {
            Ok(false) => {}
            Ok(true) => {
                let opening = self.opening.take().unwrap();
                let result = Playback::from_source(
                    opening.source.unwrap(),
                    PlaybackOptions {
                        path: opening.path,
                        range: opening.range,
                        decoder_hint: opening.decoder_hint,
                        generation,
                        metadata: opening.metadata,
                        start: opening.start,
                        paused: opening.paused,
                        device: self.device.clone(),
                        volume: self.volume,
                        pipewire_auto_mix: self.pipewire_auto_mix,
                        still_mode: self.still_mode,
                        equalizer: Arc::clone(&self.equalizer),
                        source_info: opening.source_info,
                        listened_seconds: opening.listened_seconds,
                        graph: opening.graph,
                    },
                    &self.analyzer,
                    self.waveform.clone(),
                );
                if matches!(
                    self.shutdown.signal.try_recv(),
                    Err(crossbeam_channel::TryRecvError::Disconnected)
                ) {
                    if let Ok(mut playback) = result {
                        playback.close();
                    }
                    return;
                }
                match result {
                    Ok(playback) => {
                        self.event(AudioEvent::Started {
                            generation,
                            info: playback.info().clone(),
                        });
                        self.playback = Some(playback);
                        self.refresh_analysis();
                    }
                    Err(error) => self.event(AudioEvent::Failed {
                        generation,
                        message: format!("{error:#}"),
                    }),
                }
            }
            Err(error) => {
                self.opening = None;
                self.event(AudioEvent::Failed {
                    generation,
                    message: format!("{error:#}"),
                });
            }
        }
    }

    fn control_round(&mut self) -> bool {
        if matches!(
            self.shutdown.signal.try_recv(),
            Err(crossbeam_channel::TryRecvError::Disconnected)
        ) {
            self.stop();
            return false;
        }
        let mut remaining = MAX_COMMANDS_PER_ROUND;
        while let Ok(command) = self.commands.try_recv_round(&mut remaining) {
            if matches!(
                self.shutdown.signal.try_recv(),
                Err(crossbeam_channel::TryRecvError::Disconnected)
            ) {
                self.stop();
                return false;
            }
            if !self.command(command) {
                return false;
            }
        }
        if self.commands.receiver.is_closed() && !self.commands.has_ready() {
            self.stop();
            return false;
        }
        true
    }

    pub(super) fn run(mut self) {
        loop {
            if !self.control_round() {
                return;
            }
            if self.opening.is_some() {
                self.poll_opening();
                continue;
            }
            if self.playback.is_none() {
                if self.commands.has_ready() {
                    continue;
                }
                select! {
                    recv(self.shutdown.signal) -> _ => break,
                    recv(self.commands_ready) -> wake => if wake.is_err() { break; },
                }
                continue;
            }
            // Fatal backend errors must still be delivered while paused,
            // and must not be hidden by an otherwise completed drain.
            if let Ok(error) = self.playback.as_ref().unwrap().errors().try_recv() {
                self.fail(anyhow!(error));
                continue;
            }
            let refill_ready = match self.playback.as_mut().unwrap().fill() {
                Ok(ready) => ready,
                Err(error) => {
                    self.fail(error);
                    continue;
                }
            };
            let playback = self.playback.as_ref().unwrap();
            if playback.paused() || self.progress.elapsed() >= Duration::from_millis(100) {
                let snapshot = (playback.generation(), playback.position(), playback.heard());
                if self.last_progress != Some(snapshot) {
                    self.event(AudioEvent::Progress {
                        generation: snapshot.0,
                        position_seconds: snapshot.1,
                        listened_seconds: snapshot.2,
                    });
                    self.last_progress = Some(snapshot);
                }
                self.progress = Instant::now();
            }
            if self.playback.as_ref().unwrap().finished() {
                let playback = self.playback.as_ref().unwrap();
                let bitrate_bps = playback.completed_bitrate();
                let AudioSnapshot {
                    generation,
                    position_seconds,
                    listened_seconds,
                } = self.stop();
                self.event(AudioEvent::Progress {
                    generation,
                    position_seconds,
                    listened_seconds,
                });
                self.event(AudioEvent::Ended {
                    generation,
                    bitrate_bps,
                });
                continue;
            }
            // Neither a command-budget yield nor a partial refill needs a timer.
            if self.commands.has_ready() || refill_ready {
                continue;
            }
            let playback = self.playback.as_ref().unwrap();
            let errors = playback.errors().clone();
            let notifications = playback.notifications().clone();
            // Pause waits for commands, errors or the final device-clock tail.
            // Stable paused playback never schedules a status/GUI polling tick.
            let deadline = playback
                .refill_delay()
                .min(Duration::from_millis(100).saturating_sub(self.progress.elapsed()));
            if playback.paused() {
                select! {
                    recv(self.shutdown.signal) -> _ => break,
                    recv(self.commands_ready) -> wake => if wake.is_err() { break; },
                    recv(errors) -> error => {
                        if let Ok(error) = error { self.fail(anyhow!(error)); }
                    },
                    recv(notifications) -> _ => {},
                }
            } else {
                select! {
                    recv(self.shutdown.signal) -> _ => break,
                    recv(self.commands_ready) -> wake => if wake.is_err() { break; },
                    recv(errors) -> error => {
                        if let Ok(error) = error { self.fail(anyhow!(error)); }
                    },
                    recv(notifications) -> _ => {},
                    default(deadline) => {},
                }
            }
        }
        self.stop();
    }
}

// Backpressure stays on the scheduling thread, never the real-time callback.
// The bounded channel cannot silently lose lifecycle events while a receiver
// remains connected. Shutdown explicitly cancels a blocked send.
fn send_event(
    events: &async_channel::Sender<AudioEvent>,
    shutdown: &async_channel::Receiver<()>,
    event: AudioEvent,
) {
    if matches!(event, AudioEvent::Progress { .. }) {
        let _ = events.try_send(event);
    } else {
        // Block only the dedicated non-RT worker. Closing the shutdown signal
        // interrupts capacity backpressure even while Core holds the receiver.
        futures::executor::block_on(async {
            let cancelled = shutdown.recv().fuse();
            let send = events.send(event).fuse();
            futures::pin_mut!(cancelled, send);
            futures::select_biased! {
                _ = cancelled => {},
                _ = send => {},
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AudioShutdown, AudioStop, shutdown_channel};
    use crossbeam_channel::bounded;
    use futures::channel::oneshot;

    fn opening_worker() -> (
        Worker,
        async_channel::Sender<AudioCommand>,
        async_channel::Receiver<AudioEvent>,
        AudioShutdown,
    ) {
        let (send, commands) = async_channel::bounded(MAX_COMMANDS_PER_ROUND);
        let (_wake, commands_ready) = bounded(1);
        let (events, receive) = async_channel::bounded(16);
        let (stop, shutdown) = shutdown_channel();
        let analyzer = AnalysisWorker::new(Arc::new(RwLock::new(
            crate::analysis::AnalysisFrame::default(),
        )))
        .unwrap();
        let worker = Worker::new(
            commands,
            commands_ready,
            events,
            shutdown,
            analyzer,
            Arc::new(RwLock::new(WaveformFrame::default())),
            65536,
        );
        (worker, send, receive, stop)
    }

    fn opening_hints() -> Vec<DecoderHint> {
        #[cfg(feature = "ffmpeg")]
        {
            vec![DecoderHint::Native, DecoderHint::Ffmpeg]
        }
        #[cfg(not(feature = "ffmpeg"))]
        {
            vec![DecoderHint::Native]
        }
    }

    #[test]
    fn graph_audition_rejects_same_format_source_replacement_before_open() {
        use rivu_format::config::processing_graph::{GraphPort, ProcessingGraph};
        for hint in opening_hints() {
            let original = crate::source::opening_prefix_file(0, true);
            let replacement = crate::source::opening_prefix_file(0, true);
            let expected_identity = crate::SourceIdentity::from_file(original.as_file()).unwrap();
            let runner = GraphRunner::compile(
                &ProcessingGraph::default(),
                GraphPort { node: 2, port: 0 },
                8_000,
                1,
            )
            .unwrap();
            let (mut worker, _commands, events, _shutdown) = opening_worker();
            worker.ffmpeg_enabled = hint == DecoderHint::Ffmpeg;
            worker.command(AudioCommand::GraphAudition {
                path: original.path().to_owned(),
                expected_identity,
                range: None,
                decoder_hint: hint,
                source_info: None,
                metadata: Arc::default(),
                generation: 91,
                start_seconds: 0.0,
                paused: true,
                listened_seconds: 0.0,
                runner: Box::new(runner),
            });
            std::fs::rename(replacement.path(), original.path()).unwrap();
            let opened = Source::open(original.path(), 65536, hint, worker.ffmpeg_enabled).unwrap();
            assert_eq!(opened.info().sample_rate, 8_000);
            assert_eq!(opened.info().channels, 1);
            assert_ne!(opened.identity(), expected_identity);
            worker.poll_opening();
            assert!(worker.opening.is_none());
            assert!(worker.playback.is_none());
            let event = events.try_recv().unwrap();
            assert!(
                matches!(event, AudioEvent::Failed { generation: 91, message } if message.contains("admitted for graph audition"))
            );
            assert!(events.is_empty());
        }
    }

    fn load_prefix(
        worker: &mut Worker,
        path: &std::path::Path,
        hint: DecoderHint,
        generation: u64,
    ) {
        worker.ffmpeg_enabled = hint == DecoderHint::Ffmpeg;
        worker.command(AudioCommand::Load {
            path: path.to_owned(),
            range: None,
            decoder_hint: hint,
            metadata: Arc::default(),
            generation,
            start_seconds: 0.0,
            paused: false,
        });
    }

    #[test]
    fn seeking_during_opening_changes_the_first_published_pcm() {
        for hint in opening_hints() {
            let file = crate::source::opening_prefix_file(1024, true);
            let (mut worker, _commands, events, _shutdown) = opening_worker();
            load_prefix(&mut worker, file.path(), hint, 41);
            worker.poll_opening();
            assert!(events.is_empty());
            let target = 1.0 / 8000.0;
            worker.command(AudioCommand::Seek(target));
            assert!(worker.opening.is_some(), "{:?}", events.try_recv());
            let opening = worker.opening.as_mut().unwrap();
            let mut opened = false;
            for _ in 0..64 {
                if opening.poll(65_536, hint == DecoderHint::Ffmpeg).unwrap() {
                    opened = true;
                    break;
                }
            }
            assert!(opened, "finite fixture must finish opening");
            let source = opening.source.as_mut().unwrap();
            let mut samples = Vec::new();
            loop {
                let mut budget = crate::source::DECODE_WORK_BUDGET;
                match source.next_frames_with_budget(&mut budget).unwrap() {
                    crate::source::DecodeStep::Frames(span) => {
                        assert!((span.media_start - target).abs() < 1e-9);
                        samples.extend_from_slice(span.samples);
                    }
                    crate::source::DecodeStep::Yield => (),
                    crate::source::DecodeStep::Eof => break,
                }
            }
            assert_eq!(samples, [-0.5]);
            assert!(events.is_empty());
        }
    }

    #[test]
    fn paused_opening_retains_metadata_and_rejects_retired_generation_updates() {
        let file = crate::source::opening_prefix_file(1024, true);
        let (mut worker, commands, events, _shutdown) = opening_worker();
        load_prefix(&mut worker, file.path(), DecoderHint::Native, 41);
        worker.poll_opening();
        assert!(worker.opening.as_ref().unwrap().source.is_some());

        let edited = Arc::new(PlaybackMetadata {
            title: "Edited CUE title".into(),
            artist: "Library artist".into(),
            album: "Library album".into(),
        });
        let target = 1.0 / 8000.0;
        for command in [
            AudioCommand::Pause(true),
            AudioCommand::Metadata {
                generation: 41,
                metadata: Arc::clone(&edited),
            },
            AudioCommand::Seek(target),
            AudioCommand::Metadata {
                generation: 40,
                metadata: Arc::default(),
            },
        ] {
            commands.try_send(command).unwrap();
        }
        assert!(worker.control_round());
        assert!(worker.opening.is_some(), "{:?}", events.try_recv());
        let opening = worker.opening.as_ref().unwrap();
        assert!(opening.paused);
        assert_eq!(opening.start, target);
        assert!(
            opening.source.is_some(),
            "metadata must not reopen the source"
        );
        assert!(Arc::ptr_eq(&opening.metadata, &edited));
        assert!(worker.playback.is_none());
        assert!(events.is_empty());

        let replacement = Arc::new(PlaybackMetadata {
            title: "Replacement".into(),
            ..PlaybackMetadata::default()
        });
        commands
            .try_send(AudioCommand::Load {
                path: file.path().to_owned(),
                range: None,
                decoder_hint: DecoderHint::Native,
                metadata: Arc::clone(&replacement),
                generation: 42,
                start_seconds: 0.0,
                paused: true,
            })
            .unwrap();
        commands
            .try_send(AudioCommand::Metadata {
                generation: 41,
                metadata: edited,
            })
            .unwrap();
        assert!(worker.control_round());
        let opening = worker.opening.as_ref().unwrap();
        assert_eq!(opening.generation, 42);
        assert!(opening.paused);
        assert!(opening.source.is_none());
        assert!(Arc::ptr_eq(&opening.metadata, &replacement));
        worker.poll_opening();
        assert!(worker.opening.as_ref().unwrap().source.is_some());
        assert!(events.is_empty());

        worker.command(AudioCommand::Stop);
        worker.command(AudioCommand::Metadata {
            generation: 42,
            metadata: replacement,
        });
        assert!(worker.opening.is_none());
        assert!(worker.playback.is_none());
        assert!(events.is_empty());
    }

    #[test]
    fn stop_and_snapshot_cancel_real_first_frame_prefix_without_lifecycle_events() {
        let file = crate::source::opening_prefix_file(1024, true);
        for hint in opening_hints() {
            let (mut worker, commands, events, _shutdown) = opening_worker();
            load_prefix(&mut worker, file.path(), hint, 41);
            for _ in 0..2 {
                worker.poll_opening();
                assert!(worker.opening.is_some());
                assert!(worker.playback.is_none());
                assert!(events.is_empty());
            }
            let (reply, mut snapshot) = oneshot::channel();
            commands
                .try_send(AudioCommand::StopAndSnapshot(reply))
                .unwrap();
            assert!(worker.control_round());
            assert_eq!(
                snapshot.try_recv().unwrap().unwrap(),
                AudioSnapshot {
                    generation: 41,
                    ..Default::default()
                }
            );
            assert!(worker.opening.is_none());
            worker.poll_opening();
            assert!(events.is_empty());

            load_prefix(&mut worker, file.path(), hint, 42);
            worker.poll_opening();
            assert!(worker.opening.is_some());
            let (reply, snapshot) = oneshot::channel();
            drop(snapshot);
            commands
                .try_send(AudioCommand::StopAndSnapshot(reply))
                .unwrap();
            assert!(worker.control_round());
            assert_eq!(
                worker.last_snapshot,
                AudioSnapshot {
                    generation: 42,
                    ..Default::default()
                }
            );
            assert!(worker.opening.is_none());
            assert!(worker.playback.is_none());
            assert!(events.is_empty());
        }
    }

    #[test]
    fn replacing_a_real_first_frame_prefix_reports_only_the_new_generation() {
        let old = crate::source::opening_prefix_file(1024, true);
        let empty = crate::source::opening_prefix_file(0, false);
        for hint in opening_hints() {
            let (mut worker, commands, events, _shutdown) = opening_worker();
            load_prefix(&mut worker, old.path(), hint, 41);
            worker.poll_opening();
            assert!(worker.opening.is_some());
            commands
                .try_send(AudioCommand::Load {
                    path: empty.path().to_owned(),
                    range: None,
                    decoder_hint: DecoderHint::Native,
                    metadata: Arc::default(),
                    generation: 42,
                    start_seconds: 0.0,
                    paused: true,
                })
                .unwrap();
            assert!(worker.control_round());
            worker.poll_opening();
            assert!(matches!(
                events.try_recv(),
                Ok(AudioEvent::Failed { generation: 42, .. })
            ));
            assert!(events.is_empty());
            assert!(worker.opening.is_none());
            assert!(worker.playback.is_none());
        }
    }

    #[test]
    fn shutdown_cancels_real_first_frame_prefix_at_the_next_control_round() {
        let file = crate::source::opening_prefix_file(1024, true);
        for hint in opening_hints() {
            for disconnect in [false, true] {
                let (mut worker, commands, events, shutdown) = opening_worker();
                load_prefix(&mut worker, file.path(), hint, 41);
                worker.poll_opening();
                assert!(worker.opening.is_some());
                if disconnect {
                    drop(shutdown);
                } else {
                    commands.try_send(AudioCommand::Shutdown).unwrap();
                }
                assert!(!worker.control_round());
                assert!(worker.opening.is_none());
                assert!(worker.playback.is_none());
                assert!(events.is_empty());
            }
        }
    }

    #[test]
    fn rejected_output_settings_cancel_the_opening_generation() {
        let file = crate::source::opening_prefix_file(1024, true);
        let (mut worker, commands, events, _shutdown) = opening_worker();
        load_prefix(&mut worker, file.path(), DecoderHint::Native, 41);
        worker.poll_opening();
        assert!(worker.opening.is_some());
        commands
            .try_send(AudioCommand::OutputSettings {
                request: 7,
                device: Some("invalid-opening-target".into()),
                auto_mix: false,
            })
            .unwrap();
        assert!(worker.control_round());
        assert!(worker.opening.is_none());
        assert_eq!(worker.device.as_deref(), Some("invalid-opening-target"));
        assert!(!worker.pipewire_auto_mix);
        assert!(matches!(
            events.try_recv(),
            Ok(AudioEvent::Failed { generation: 41, .. })
        ));
        assert!(events.is_empty());
    }

    #[test]
    fn disabling_ffmpeg_before_the_first_open_round_cancels_its_generation() {
        let (mut worker, commands, events, _shutdown) = opening_worker();
        load_prefix(
            &mut worker,
            std::path::Path::new("unopened.mka"),
            DecoderHint::Ffmpeg,
            41,
        );
        commands
            .try_send(AudioCommand::FfmpegEnabled(false))
            .unwrap();
        assert!(worker.control_round());
        assert!(worker.opening.is_none());
        assert!(matches!(
            events.try_recv(),
            Ok(AudioEvent::Progress { generation: 41, .. })
        ));
        assert!(matches!(
            events.try_recv(),
            Ok(AudioEvent::DecoderStopped { generation: 41 })
        ));
        assert!(events.is_empty());
    }

    #[cfg(feature = "ffmpeg")]
    #[test]
    fn disabling_ffmpeg_cancels_its_real_opening_without_started_or_failed() {
        let file = crate::source::opening_prefix_file(1024, true);
        let (mut worker, commands, events, _shutdown) = opening_worker();
        load_prefix(&mut worker, file.path(), DecoderHint::Ffmpeg, 41);
        worker.poll_opening();
        assert!(worker.opening.is_some());
        commands
            .try_send(AudioCommand::FfmpegEnabled(false))
            .unwrap();
        assert!(worker.control_round());
        assert!(worker.opening.is_none());
        assert!(matches!(
            events.try_recv(),
            Ok(AudioEvent::Progress {
                generation: 41,
                listened_seconds: 0.0,
                ..
            })
        ));
        assert!(matches!(
            events.try_recv(),
            Ok(AudioEvent::DecoderStopped { generation: 41 })
        ));
        assert!(events.is_empty());
    }

    #[test]
    fn rejected_output_change_keeps_new_target_instead_of_reusing_previous_device() {
        let (_send, commands) = async_channel::bounded(1);
        let (_wake, commands_ready) = bounded(1);
        let (events, receive) = async_channel::bounded(2);
        let (_stop, shutdown) = shutdown_channel();
        let analyzer = AnalysisWorker::new(Arc::new(RwLock::new(
            crate::analysis::AnalysisFrame::default(),
        )))
        .unwrap();
        let mut worker = Worker::new(
            commands,
            commands_ready,
            events,
            shutdown,
            analyzer,
            Arc::new(RwLock::new(WaveformFrame::default())),
            1024,
        );
        for (index, target) in [
            "invalid-first-target",
            "invalid-first-target",
            "invalid-replacement-target",
        ]
        .into_iter()
        .enumerate()
        {
            let request = index as u64 + 1;
            worker.command(AudioCommand::OutputSettings {
                request,
                device: Some(target.to_owned()),
                auto_mix: false,
            });
            assert!(matches!(
                receive.try_recv(),
                Ok(AudioEvent::OutputSettingsFailed { request: failed_request, .. }) if failed_request == request
            ));
            assert_eq!(worker.device.as_deref(), Some(target));
            assert!(!worker.pipewire_auto_mix);
            assert!(worker.playback.is_none());
        }
    }

    #[test]
    fn stop_snapshot_drains_backpressured_lifecycle_events_before_acknowledging() {
        futures::executor::block_on(async {
            let (commands, receive) = async_channel::bounded(1);
            let (command_wake, commands_ready) = bounded(1);
            commands.try_send(AudioCommand::Volume(0.4)).unwrap();
            let (events, event_receive) = async_channel::bounded(1);
            events
                .try_send(AudioEvent::Ended {
                    generation: 1,
                    bitrate_bps: None,
                })
                .unwrap();
            let (shutdown, cancelled) = shutdown_channel();
            let analyzer = AnalysisWorker::new(Arc::new(RwLock::new(
                crate::analysis::AnalysisFrame::default(),
            )))
            .unwrap();
            let worker = Worker::new(
                receive,
                commands_ready,
                events,
                cancelled,
                analyzer,
                Arc::new(RwLock::new(WaveformFrame::default())),
                1024,
            );
            let worker = std::thread::spawn(move || {
                worker.event(AudioEvent::Ended {
                    generation: 2,
                    bitrate_bps: None,
                });
                worker.run();
            });
            let mut retired = Vec::new();
            let mut stop = AudioStop::new(commands, command_wake, event_receive);
            let snapshot = stop
                .wait(|event| {
                    if let AudioEvent::Ended { generation, .. } = event {
                        retired.push(generation);
                    }
                })
                .await;
            drop(shutdown);
            worker.join().unwrap();
            assert_eq!(snapshot.unwrap(), AudioSnapshot::default());
            assert_eq!(retired, [1, 2]);
        });
    }

    #[test]
    fn cancelled_stop_wait_retains_its_receiver_and_never_replays_admission() {
        futures::executor::block_on(async {
            let (commands, receive) = async_channel::bounded(1);
            let (command_wake, commands_ready) = bounded(1);
            commands.try_send(AudioCommand::Volume(0.4)).unwrap();
            let (events, event_receive) = async_channel::bounded(1);
            events
                .try_send(AudioEvent::DecoderStopped { generation: 7 })
                .unwrap();
            let mut stop = AudioStop::new(commands, command_wake, event_receive);
            let mut retired = Vec::new();
            assert!(
                stop.wait(|event| retired.push(event))
                    .now_or_never()
                    .is_none()
            );
            assert!(matches!(
                retired.as_slice(),
                [AudioEvent::DecoderStopped { generation: 7 }]
            ));
            assert!(matches!(
                receive.try_recv().unwrap(),
                AudioCommand::Volume(0.4)
            ));

            assert!(
                stop.wait(|event| retired.push(event))
                    .now_or_never()
                    .is_none()
            );
            commands_ready.try_recv().unwrap();
            let AudioCommand::StopAndSnapshot(reply) = receive.try_recv().unwrap() else {
                panic!("stop was not admitted after capacity became available");
            };
            assert!(
                stop.wait(|event| retired.push(event))
                    .now_or_never()
                    .is_none()
            );
            assert!(
                receive.is_empty(),
                "cancelled acknowledgement wait replayed stop"
            );

            events
                .try_send(AudioEvent::DecoderStopped { generation: 8 })
                .unwrap();
            let snapshot = AudioSnapshot {
                generation: 7,
                position_seconds: 3.5,
                listened_seconds: 2.5,
            };
            reply.send(snapshot).unwrap();
            assert_eq!(
                stop.wait(|event| retired.push(event)).await.unwrap(),
                snapshot
            );
            assert!(matches!(
                retired.as_slice(),
                [
                    AudioEvent::DecoderStopped { generation: 7 },
                    AudioEvent::DecoderStopped { generation: 8 },
                ]
            ));
            let event_receive = stop.into_events();
            events
                .try_send(AudioEvent::DecoderStopped { generation: 9 })
                .unwrap();
            assert!(matches!(
                event_receive.try_recv(),
                Ok(AudioEvent::DecoderStopped { generation: 9 })
            ));
        });
    }

    #[test]
    fn stop_deadline_covers_capacity_acknowledgement_and_closed_event_delivery() {
        futures::executor::block_on(async {
            for queue_full in [false, true] {
                let (commands, receive) = async_channel::bounded(1);
                let (command_wake, _commands_ready) = bounded(1);
                if queue_full {
                    commands.try_send(AudioCommand::Volume(0.4)).unwrap();
                }
                let (events, event_receive) = async_channel::bounded(1);
                let created = Instant::now();
                let mut stop = AudioStop::new(commands, command_wake, event_receive);
                assert!(stop.deadline >= created + Duration::from_secs(5));
                assert!(stop.deadline <= Instant::now() + Duration::from_secs(5));
                assert!(stop.wait(|_| {}).now_or_never().is_none());
                assert_eq!(receive.len(), 1);
                // After admission a closed event channel must not bypass the same
                // deadline while the worker still owns the unacknowledged reply.
                if !queue_full {
                    drop(events);
                }
                // Exercise a real timer wake after cancelling the first wait; the
                // production request keeps the five-second deadline checked above.
                stop.deadline = Instant::now() + Duration::from_millis(10);
                let error = stop.wait(|_| {}).await.unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("did not stop before the deadline")
                );
                assert_eq!(receive.len(), 1, "a timed-out stop was replayed");
                let _events = stop.into_events();
            }
        });
    }

    #[test]
    fn still_mode_discards_stopped_waveforms_without_changing_playback_policy() {
        let (_send, commands) = async_channel::bounded(1);
        let (_wake, commands_ready) = bounded(1);
        let (events, _receive) = async_channel::bounded(1);
        let (_stop, shutdown) = shutdown_channel();
        let analyzer = AnalysisWorker::new(Arc::new(RwLock::new(
            crate::analysis::AnalysisFrame::default(),
        )))
        .unwrap();
        let mut frame = WaveformFrame::default();
        frame.select(
            std::path::Path::new("stopped.wav"),
            None,
            DecoderHint::Auto,
            Some(1.0),
            8000,
        );
        frame.push(0.0, &[0.5; 8000], 8000, 1);
        frame.finish_scan(1.0);
        let completed = frame.clone();
        let waveform = Arc::new(RwLock::new(frame));
        let mut worker = Worker::new(
            commands,
            commands_ready,
            events,
            shutdown,
            analyzer,
            waveform.clone(),
            65536,
        );
        worker.command(AudioCommand::Volume(0.3));
        worker.analyzer.control.set_display_enabled(true);
        worker.command(AudioCommand::StillMode(true));
        let revision = waveform.read().revision;
        assert!(revision > completed.revision);
        for command in [
            AudioCommand::StillMode(true),
            AudioCommand::Seek(0.5),
            AudioCommand::Pause(true),
            AudioCommand::AnalysisSettings(crate::analysis::AnalysisSettings::from(
                &rivu_format::config::Config::default(),
            )),
        ] {
            worker.command(command);
            assert_eq!(waveform.read().revision, revision);
            assert_eq!(waveform.read().retained_capacity_bytes(), 0);
            assert_eq!(worker.volume, 0.3);
            assert!(worker.device.is_none());
            assert!(worker.playback.is_none());
            assert!(!worker.analyzer.control.accepts_audio());
        }
        worker.command(AudioCommand::StillMode(false));
        assert_eq!(waveform.read().retained_capacity_bytes(), 0);
        assert_eq!(waveform.read().revision, revision);
    }

    #[test]
    fn consecutive_seeks_stop_at_every_non_seek_barrier() {
        let (reply, _snapshot) = oneshot::channel();
        let barriers = [
            AudioCommand::Load {
                path: PathBuf::from("other.wav"),
                range: None,
                decoder_hint: DecoderHint::Auto,
                metadata: Arc::default(),
                generation: 2,
                start_seconds: 0.0,
                paused: true,
            },
            AudioCommand::Metadata {
                generation: 2,
                metadata: Arc::default(),
            },
            AudioCommand::Pause(true),
            AudioCommand::Stop,
            AudioCommand::Volume(0.5),
            AudioCommand::Equalizer(Arc::default()),
            AudioCommand::OutputSettings {
                request: 0,
                device: None,
                auto_mix: false,
            },
            AudioCommand::StillMode(true),
            AudioCommand::MediaReadBuffer(2),
            AudioCommand::FfmpegEnabled(false),
            AudioCommand::AnalysisSettings(crate::analysis::AnalysisSettings::from(
                &rivu_format::config::Config::default(),
            )),
            AudioCommand::StopAndSnapshot(reply),
            AudioCommand::Shutdown,
        ];
        for barrier in barriers {
            let kind = std::mem::discriminant(&barrier);
            let (send, receiver) = async_channel::bounded(4);
            for command in [
                AudioCommand::Seek(10.0),
                AudioCommand::Seek(20.0),
                barrier,
                AudioCommand::Seek(30.0),
            ] {
                send.try_send(command).unwrap();
            }
            let mut inbox = CommandInbox {
                receiver,
                pending: None,
            };
            let mut remaining = MAX_COMMANDS_PER_ROUND;
            assert!(matches!(
                inbox.try_recv_round(&mut remaining).unwrap(),
                AudioCommand::Seek(20.0)
            ));
            assert_eq!(
                std::mem::discriminant(&inbox.try_recv_round(&mut remaining).unwrap()),
                kind
            );
            assert!(matches!(
                inbox.try_recv_round(&mut remaining).unwrap(),
                AudioCommand::Seek(30.0)
            ));
            assert!(matches!(
                inbox.try_recv_round(&mut remaining),
                Err(TryRecvError::Empty)
            ));
        }
    }

    #[test]
    fn seek_batches_are_bounded_and_keep_the_remaining_targets() {
        let (send, receiver) = async_channel::bounded(MAX_COMMANDS_PER_ROUND + 2);
        for target in 1..=MAX_COMMANDS_PER_ROUND + 2 {
            send.try_send(AudioCommand::Seek(target as f64)).unwrap();
        }
        drop(send);
        let mut inbox = CommandInbox {
            receiver,
            pending: None,
        };
        let mut remaining = MAX_COMMANDS_PER_ROUND;
        assert!(matches!(
            inbox.try_recv_round(&mut remaining).unwrap(),
            AudioCommand::Seek(target) if target == MAX_COMMANDS_PER_ROUND as f64
        ));
        assert_eq!(remaining, 0);
        assert!(inbox.has_ready());
        assert!(matches!(
            inbox.try_recv_round(&mut remaining),
            Err(TryRecvError::Empty)
        ));
        remaining = MAX_COMMANDS_PER_ROUND;
        assert!(matches!(
            inbox.try_recv_round(&mut remaining).unwrap(),
            AudioCommand::Seek(target) if target == (MAX_COMMANDS_PER_ROUND + 2) as f64
        ));
        assert!(matches!(
            inbox.try_recv_round(&mut remaining),
            Err(TryRecvError::Closed)
        ));
    }

    #[test]
    fn replenished_commands_yield_each_round_without_reordering() {
        let (send, receiver) = async_channel::bounded(1);
        send.try_send(AudioCommand::Volume(0.0)).unwrap();
        let mut inbox = CommandInbox {
            receiver,
            pending: None,
        };
        let mut handled = 0;
        for _ in 0..3 {
            let mut remaining = MAX_COMMANDS_PER_ROUND;
            while let Ok(command) = inbox.try_recv_round(&mut remaining) {
                assert!(matches!(command, AudioCommand::Volume(value) if value == handled as f32));
                handled += 1;
                // Model a producer replenishing every consumed slot, without
                // thread timing or ever allowing the inbox to become empty.
                send.try_send(AudioCommand::Volume(handled as f32)).unwrap();
            }
            assert_eq!(remaining, 0);
            assert!(
                inbox.has_ready(),
                "refill must run despite continued commands"
            );
        }
        assert_eq!(handled, 3 * MAX_COMMANDS_PER_ROUND);
    }

    #[test]
    fn round_boundary_keeps_a_seek_read_ahead_barrier_before_later_seeks() {
        let (send, receiver) = async_channel::bounded(MAX_COMMANDS_PER_ROUND + 1);
        for target in 1..MAX_COMMANDS_PER_ROUND {
            send.try_send(AudioCommand::Seek(target as f64)).unwrap();
        }
        send.try_send(AudioCommand::Pause(true)).unwrap();
        send.try_send(AudioCommand::Seek(100.0)).unwrap();
        let mut inbox = CommandInbox {
            receiver,
            pending: None,
        };
        let mut remaining = MAX_COMMANDS_PER_ROUND;
        assert!(matches!(
            inbox.try_recv_round(&mut remaining).unwrap(),
            AudioCommand::Seek(value) if value == (MAX_COMMANDS_PER_ROUND - 1) as f64
        ));
        assert_eq!(remaining, 0);
        assert!(inbox.has_ready());
        remaining = 1;
        assert!(matches!(
            inbox.try_recv_round(&mut remaining).unwrap(),
            AudioCommand::Pause(true)
        ));
        assert_eq!(remaining, 0);
        remaining = MAX_COMMANDS_PER_ROUND;
        assert!(matches!(
            inbox.try_recv_round(&mut remaining).unwrap(),
            AudioCommand::Seek(100.0)
        ));
        assert!(!inbox.has_ready());
    }

    #[test]
    fn single_seek_does_not_wait_for_more_input() {
        let (send, receiver) = async_channel::bounded(1);
        send.try_send(AudioCommand::Seek(7.0)).unwrap();
        let (done, completed) = bounded(1);
        let worker = std::thread::spawn(move || {
            let mut inbox = CommandInbox {
                receiver,
                pending: None,
            };
            let mut remaining = MAX_COMMANDS_PER_ROUND;
            done.send(inbox.try_recv_round(&mut remaining).unwrap())
                .unwrap();
        });
        assert!(matches!(
            completed.recv_timeout(Duration::from_secs(2)).unwrap(),
            AudioCommand::Seek(7.0)
        ));
        drop(send);
        worker.join().unwrap();
    }

    #[test]
    fn engine_shutdown_and_drop_cancel_full_command_and_event_queues() {
        use crate::AudioEngine;
        use crate::analysis::AnalysisFrame;
        use parking_lot::Mutex;

        for explicit in [false, true] {
            for borrowed in [false, true] {
                let (commands, receive) = async_channel::bounded(1);
                let (command_wake, commands_ready) = bounded(1);
                commands.try_send(AudioCommand::Pause(true)).unwrap();
                let (events, event_receive) = async_channel::bounded(1);
                events
                    .try_send(AudioEvent::Ended {
                        generation: 1,
                        bitrate_bps: None,
                    })
                    .unwrap();
                let (shutdown, cancelled) = shutdown_channel();
                let analysis = Arc::new(RwLock::new(AnalysisFrame::default()));
                let waveform = Arc::new(RwLock::new(WaveformFrame::default()));
                let analyzer = AnalysisWorker::new(analysis.clone()).unwrap();
                let analysis_control = Arc::clone(&analyzer.control);
                let worker = Worker::new(
                    receive,
                    commands_ready,
                    events,
                    cancelled,
                    analyzer,
                    waveform.clone(),
                    1024,
                );
                let worker = std::thread::spawn(move || {
                    worker.event(AudioEvent::Ended {
                        generation: 2,
                        bitrate_bps: None,
                    });
                    worker.run();
                });
                let mut engine = AudioEngine {
                    commands,
                    command_wake,
                    events: Some(event_receive),
                    analysis,
                    analysis_control,
                    waveform,
                    worker: Some(worker),
                    shutdown: Mutex::new(Some(shutdown)),
                };
                let held_events = if borrowed { engine.events.take() } else { None };
                let (done, completed) = bounded(1);
                let closer = std::thread::spawn(move || {
                    if explicit {
                        engine.send(AudioCommand::Shutdown).unwrap();
                    }
                    drop(engine);
                    done.send(()).unwrap();
                });
                completed.recv_timeout(Duration::from_secs(2)).unwrap();
                closer.join().unwrap();
                if let Some(held_events) = held_events {
                    assert!(matches!(
                        held_events.try_recv(),
                        Ok(AudioEvent::Ended { generation: 1, .. })
                    ));
                }
            }
        }
    }

    #[test]
    fn full_event_queue_drops_progress_but_retains_lifecycle_events() {
        let (events, receive) = async_channel::bounded(1);
        let (_cancel, shutdown) = async_channel::bounded(1);
        events
            .try_send(AudioEvent::Ended {
                generation: 1,
                bitrate_bps: None,
            })
            .unwrap();
        send_event(
            &events,
            &shutdown,
            AudioEvent::Progress {
                generation: 1,
                position_seconds: 1.0,
                listened_seconds: 1.0,
            },
        );
        let sender = std::thread::spawn(move || {
            send_event(
                &events,
                &shutdown,
                AudioEvent::DecoderStopped { generation: 2 },
            );
            send_event(
                &events,
                &shutdown,
                AudioEvent::Failed {
                    generation: 3,
                    message: "decoder failed".into(),
                },
            );
        });
        assert!(matches!(
            receive.recv_blocking().unwrap(),
            AudioEvent::Ended { generation: 1, .. }
        ));
        assert!(matches!(
            receive.recv_blocking().unwrap(),
            AudioEvent::DecoderStopped { generation: 2 }
        ));
        assert!(matches!(
            receive.recv_blocking().unwrap(),
            AudioEvent::Failed { generation: 3, .. }
        ));
        sender.join().unwrap();
    }

    #[test]
    fn shutdown_cancels_a_full_event_queue() {
        let (events, _receive) = async_channel::bounded(1);
        let (stop, shutdown) = shutdown_channel();
        events
            .try_send(AudioEvent::Ended {
                generation: 1,
                bitrate_bps: None,
            })
            .unwrap();
        let sender = std::thread::spawn(move || {
            send_event(
                &events,
                &shutdown.events,
                AudioEvent::Ended {
                    generation: 2,
                    bitrate_bps: None,
                },
            );
        });
        drop(stop);
        sender.join().unwrap();
    }

    #[test]
    fn invalid_equalizer_settings_report_config_and_current_generation() {
        let (mut worker, _commands, events, _shutdown) = opening_worker();
        let invalid = Arc::new(EqualizerConfig {
            preamp_db: f64::NAN,
            ..Default::default()
        });
        worker.command(AudioCommand::Equalizer(invalid.clone()));
        match events.try_recv().unwrap() {
            AudioEvent::EqualizerSettingsFailed {
                generation, config, ..
            } => {
                assert_eq!(generation, None);
                assert!(Arc::ptr_eq(&config, &invalid));
            }
            _ => panic!("expected explicit idle equalizer settings failure"),
        }
        let file = crate::source::opening_prefix_file(1024, true);
        load_prefix(&mut worker, file.path(), DecoderHint::Native, 77);
        worker.command(AudioCommand::Equalizer(invalid.clone()));
        assert!(
            matches!(events.try_recv().unwrap(), AudioEvent::EqualizerSettingsFailed {
            generation: Some(77), config, ..
        } if Arc::ptr_eq(&config, &invalid))
        );
        assert_eq!(worker.opening.as_ref().unwrap().generation, 77);
    }

    #[test]
    fn cancelling_an_audition_opening_keeps_cumulative_heard_and_metadata() {
        let file = crate::source::opening_prefix_file(1024, true);
        let target = 1.0 / 8000.0;
        let info = Source::open(file.path(), 65_536, DecoderHint::Native, false)
            .unwrap()
            .info()
            .clone();
        let metadata = Arc::new(PlaybackMetadata {
            title: "Original title".into(),
            ..Default::default()
        });
        let (mut worker, _commands, events, _shutdown) = opening_worker();
        worker.command(AudioCommand::Audition {
            path: file.path().to_owned(),
            range: None,
            decoder_hint: DecoderHint::Native,
            source_info: Some(info),
            metadata: metadata.clone(),
            generation: 91,
            start_seconds: 0.0,
            paused: true,
            listened_seconds: 12.5,
        });
        worker.poll_opening();
        assert!(worker.opening.is_some(), "{:?}", events.try_recv());
        // Cancel an actual retained decoder after an in-range seek, before the
        // next opening poll can publish playback/output.
        worker.command(AudioCommand::Seek(target));
        assert!(worker.opening.is_some(), "{:?}", events.try_recv());
        let opening = worker.opening.as_ref().unwrap();
        assert_eq!(opening.start, target);
        assert!(opening.paused);
        assert!(Arc::ptr_eq(&opening.metadata, &metadata));
        assert_eq!(
            worker.stop(),
            AudioSnapshot {
                generation: 91,
                position_seconds: target,
                listened_seconds: 12.5
            }
        );
        assert!(events.is_empty());
    }
}
