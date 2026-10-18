//! Command handling, playback lifecycle, refill scheduling, and analyzer policy.

use super::{
    AudioCommand, AudioEvent, PlaybackMetadata, WaveformFrame,
    output::validate_device,
    playback::{Playback, PlaybackOptions},
    source::Source,
};
use crate::analysis::AnalysisWorker;
use crate::model::DecoderHint;
use anyhow::anyhow;
use crossbeam_channel::{Receiver, Sender, TryRecvError, select};
use parking_lot::RwLock;
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

// Include coalesced seeks and read-ahead barriers in each scheduling round.
const MAX_COMMANDS_PER_ROUND: usize = 64;

struct CommandInbox {
    receiver: Receiver<AudioCommand>,
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
// published until its first-frame prefix has completed in finite rounds.
struct Opening {
    path: PathBuf,
    range: Option<super::PlaybackRange>,
    decoder_hint: DecoderHint,
    generation: u64,
    metadata: Arc<PlaybackMetadata>,
    start: f64,
    paused: bool,
    source: Option<Source>,
}

impl Opening {
    fn poll(&mut self, buffer_len: usize, ffmpeg_enabled: bool) -> anyhow::Result<bool> {
        if self.source.is_none() {
            self.source = Some(Source::begin_open(
                &self.path,
                buffer_len,
                self.decoder_hint,
                ffmpeg_enabled,
            )?);
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
    events: Sender<AudioEvent>,
    shutdown: Receiver<()>,
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
    last_snapshot: (u64, f64),
    progress: Instant,
    last_progress: Option<(u64, f64, f64)>,
}
impl Worker {
    pub(super) fn new(
        commands: Receiver<AudioCommand>,
        events: Sender<AudioEvent>,
        shutdown: Receiver<()>,
        analyzer: AnalysisWorker,
        waveform: Arc<RwLock<WaveformFrame>>,
        media_read_buffer_len: usize,
    ) -> Self {
        Self {
            commands: CommandInbox {
                receiver: commands,
                pending: None,
            },
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
            last_snapshot: (0, 0.0),
            progress: Instant::now(),
            last_progress: None,
        }
    }
    fn event(&self, event: AudioEvent) {
        send_event(&self.events, &self.shutdown, event);
    }
    fn stop(&mut self) -> (u64, f64) {
        self.analyzer.set_playback_active(false);
        if let Some(opening) = self.opening.take() {
            self.last_snapshot = (opening.generation, 0.0);
        }
        if let Some(mut playback) = self.playback.take() {
            self.last_snapshot = (playback.generation(), playback.close());
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
                    let position = self.opening.as_ref().unwrap().start;
                    let (generation, heard) = self.stop();
                    self.event(AudioEvent::Progress {
                        generation,
                        position_seconds: position,
                        listened_seconds: heard,
                    });
                    self.event(AudioEvent::DecoderStopped { generation });
                }
                if !enabled && self.playback.as_ref().is_some_and(Playback::uses_ffmpeg) {
                    let position = self.playback.as_ref().unwrap().position();
                    let (generation, heard) = self.stop();
                    self.event(AudioEvent::Progress {
                        generation,
                        position_seconds: position,
                        listened_seconds: heard,
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
                if let Some(opening) = self.opening.as_mut() {
                    opening.start = seconds.max(0.0);
                }
                if let Err(error) = self.reopen(Some(seconds)) {
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
        let (generation, _) = self.stop();
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
        if matches!(self.shutdown.try_recv(), Err(TryRecvError::Disconnected)) {
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
                    },
                    &self.analyzer,
                    self.waveform.clone(),
                );
                if matches!(self.shutdown.try_recv(), Err(TryRecvError::Disconnected)) {
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
        if matches!(self.shutdown.try_recv(), Err(TryRecvError::Disconnected)) {
            self.stop();
            return false;
        }
        let mut remaining = MAX_COMMANDS_PER_ROUND;
        while let Ok(command) = self.commands.try_recv_round(&mut remaining) {
            if matches!(self.shutdown.try_recv(), Err(TryRecvError::Disconnected)) {
                self.stop();
                return false;
            }
            if !self.command(command) {
                return false;
            }
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
                let command = select! {
                    recv(self.shutdown) -> _ => break,
                    recv(self.commands.receiver) -> command => match command {
                        Ok(command) => command,
                        Err(_) => break,
                    },
                };
                self.commands.pending = Some(command);
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
                let generation = playback.generation();
                let position = playback.position();
                let bitrate_bps = playback.completed_bitrate();
                let (_, heard) = self.stop();
                self.event(AudioEvent::Progress {
                    generation,
                    position_seconds: position,
                    listened_seconds: heard,
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
            let command = if playback.paused() {
                select! {
                    recv(self.shutdown) -> _ => break,
                    recv(self.commands.receiver) -> command => Some(command),
                    recv(errors) -> error => {
                        if let Ok(error) = error { self.fail(anyhow!(error)); }
                        None
                    },
                    recv(notifications) -> _ => None,
                }
            } else {
                select! {
                    recv(self.shutdown) -> _ => break,
                    recv(self.commands.receiver) -> command => Some(command),
                    recv(errors) -> error => {
                        if let Ok(error) = error { self.fail(anyhow!(error)); }
                        None
                    },
                    recv(notifications) -> _ => None,
                    default(deadline) => None,
                }
            };
            if let Some(command) = command {
                let Ok(command) = command else {
                    break;
                };
                self.commands.pending = Some(command);
            }
        }
        self.stop();
    }
}

// Backpressure stays on the scheduling thread, never the real-time callback.
// The bounded channel cannot silently lose lifecycle events while a receiver
// remains connected. Shutdown explicitly cancels a blocked send.
fn send_event(events: &Sender<AudioEvent>, shutdown: &Receiver<()>, event: AudioEvent) {
    if matches!(event, AudioEvent::Progress { .. }) {
        let _ = events.try_send(event);
    } else {
        select! {
            send(events, event) -> _ => {},
            recv(shutdown) -> _ => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::bounded;

    fn opening_worker() -> (
        Worker,
        Sender<AudioCommand>,
        Receiver<AudioEvent>,
        Sender<()>,
    ) {
        let (send, commands) = bounded(MAX_COMMANDS_PER_ROUND);
        let (events, receive) = bounded(16);
        let (stop, shutdown) = bounded(0);
        let analyzer = AnalysisWorker::new(Arc::new(RwLock::new(
            crate::analysis::AnalysisFrame::default(),
        )))
        .unwrap();
        let worker = Worker::new(
            commands,
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
    fn paused_opening_retains_metadata_and_rejects_retired_generation_updates() {
        let file = super::super::source::opening_prefix_file(1024, true);
        let (mut worker, commands, events, _shutdown) = opening_worker();
        load_prefix(&mut worker, file.path(), DecoderHint::Native, 41);
        worker.poll_opening();
        assert!(worker.opening.as_ref().unwrap().source.is_some());

        let edited = Arc::new(PlaybackMetadata {
            title: "Edited CUE title".into(),
            artist: "Library artist".into(),
            album: "Library album".into(),
        });
        for command in [
            AudioCommand::Pause(true),
            AudioCommand::Metadata {
                generation: 41,
                metadata: Arc::clone(&edited),
            },
            AudioCommand::Seek(0.125),
            AudioCommand::Metadata {
                generation: 40,
                metadata: Arc::default(),
            },
        ] {
            commands.send(command).unwrap();
        }
        assert!(worker.control_round());
        let opening = worker.opening.as_ref().unwrap();
        assert!(opening.paused);
        assert_eq!(opening.start, 0.125);
        assert!(
            opening.source.is_some(),
            "metadata must not reopen the source"
        );
        assert!(Arc::ptr_eq(&opening.metadata, &edited));
        worker.poll_opening();
        assert!(worker.playback.is_none());
        assert!(events.is_empty());

        let replacement = Arc::new(PlaybackMetadata {
            title: "Replacement".into(),
            ..PlaybackMetadata::default()
        });
        commands
            .send(AudioCommand::Load {
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
            .send(AudioCommand::Metadata {
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
        let file = super::super::source::opening_prefix_file(1024, true);
        for hint in opening_hints() {
            let (mut worker, commands, events, _shutdown) = opening_worker();
            load_prefix(&mut worker, file.path(), hint, 41);
            for _ in 0..2 {
                worker.poll_opening();
                assert!(worker.opening.is_some());
                assert!(worker.playback.is_none());
                assert!(events.is_empty());
            }
            let (reply, snapshot) = bounded(1);
            commands.send(AudioCommand::StopAndSnapshot(reply)).unwrap();
            assert!(worker.control_round());
            assert_eq!(snapshot.try_recv().unwrap(), (41, 0.0));
            assert!(worker.opening.is_none());
            worker.poll_opening();
            assert!(events.is_empty());
        }
    }

    #[test]
    fn replacing_a_real_first_frame_prefix_reports_only_the_new_generation() {
        let old = super::super::source::opening_prefix_file(1024, true);
        let empty = super::super::source::opening_prefix_file(0, false);
        for hint in opening_hints() {
            let (mut worker, commands, events, _shutdown) = opening_worker();
            load_prefix(&mut worker, old.path(), hint, 41);
            worker.poll_opening();
            assert!(worker.opening.is_some());
            commands
                .send(AudioCommand::Load {
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
        let file = super::super::source::opening_prefix_file(1024, true);
        for hint in opening_hints() {
            for disconnect in [false, true] {
                let (mut worker, commands, events, shutdown) = opening_worker();
                load_prefix(&mut worker, file.path(), hint, 41);
                worker.poll_opening();
                assert!(worker.opening.is_some());
                if disconnect {
                    drop(shutdown);
                } else {
                    commands.send(AudioCommand::Shutdown).unwrap();
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
        let file = super::super::source::opening_prefix_file(1024, true);
        let (mut worker, commands, events, _shutdown) = opening_worker();
        load_prefix(&mut worker, file.path(), DecoderHint::Native, 41);
        worker.poll_opening();
        assert!(worker.opening.is_some());
        commands
            .send(AudioCommand::OutputSettings {
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
        commands.send(AudioCommand::FfmpegEnabled(false)).unwrap();
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
        let file = super::super::source::opening_prefix_file(1024, true);
        let (mut worker, commands, events, _shutdown) = opening_worker();
        load_prefix(&mut worker, file.path(), DecoderHint::Ffmpeg, 41);
        worker.poll_opening();
        assert!(worker.opening.is_some());
        commands.send(AudioCommand::FfmpegEnabled(false)).unwrap();
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
        let (_send, commands) = bounded(1);
        let (events, receive) = bounded(2);
        let (_stop, shutdown) = bounded(0);
        let analyzer = AnalysisWorker::new(Arc::new(RwLock::new(
            crate::analysis::AnalysisFrame::default(),
        )))
        .unwrap();
        let mut worker = Worker::new(
            commands,
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
        let (commands, receive) = bounded(1);
        commands.send(AudioCommand::Volume(0.4)).unwrap();
        let (events, event_receive) = bounded(1);
        events
            .send(AudioEvent::Ended {
                generation: 1,
                bitrate_bps: None,
            })
            .unwrap();
        let (shutdown, cancelled) = bounded(0);
        let analyzer = AnalysisWorker::new(Arc::new(RwLock::new(
            crate::analysis::AnalysisFrame::default(),
        )))
        .unwrap();
        let worker = Worker::new(
            receive,
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
        let snapshot = crate::audio::stop_and_snapshot(&commands, &event_receive, |event| {
            if let AudioEvent::Ended { generation, .. } = event {
                retired.push(generation);
            }
        });
        drop(shutdown);
        worker.join().unwrap();
        assert_eq!(snapshot.unwrap(), (0, 0.0));
        assert_eq!(retired, [1, 2]);
    }

    #[test]
    fn still_mode_discards_stopped_waveforms_without_changing_playback_policy() {
        let (_send, commands) = bounded(1);
        let (events, _receive) = bounded(1);
        let (_stop, shutdown) = bounded(0);
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
                &crate::config::Config::default(),
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
        let (reply, _snapshot) = bounded(1);
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
            AudioCommand::OutputSettings {
                request: 0,
                device: None,
                auto_mix: false,
            },
            AudioCommand::StillMode(true),
            AudioCommand::MediaReadBuffer(2),
            AudioCommand::FfmpegEnabled(false),
            AudioCommand::AnalysisSettings(crate::analysis::AnalysisSettings::from(
                &crate::config::Config::default(),
            )),
            AudioCommand::StopAndSnapshot(reply),
            AudioCommand::Shutdown,
        ];
        for barrier in barriers {
            let kind = std::mem::discriminant(&barrier);
            let (send, receiver) = bounded(4);
            for command in [
                AudioCommand::Seek(10.0),
                AudioCommand::Seek(20.0),
                barrier,
                AudioCommand::Seek(30.0),
            ] {
                send.send(command).unwrap();
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
        let (send, receiver) = bounded(MAX_COMMANDS_PER_ROUND + 2);
        for target in 1..=MAX_COMMANDS_PER_ROUND + 2 {
            send.send(AudioCommand::Seek(target as f64)).unwrap();
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
            Err(TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn replenished_commands_yield_each_round_without_reordering() {
        let (send, receiver) = bounded(1);
        send.send(AudioCommand::Volume(0.0)).unwrap();
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
                send.send(AudioCommand::Volume(handled as f32)).unwrap();
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
        let (send, receiver) = bounded(MAX_COMMANDS_PER_ROUND + 1);
        for target in 1..MAX_COMMANDS_PER_ROUND {
            send.send(AudioCommand::Seek(target as f64)).unwrap();
        }
        send.send(AudioCommand::Pause(true)).unwrap();
        send.send(AudioCommand::Seek(100.0)).unwrap();
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
        let (send, receiver) = bounded(1);
        send.send(AudioCommand::Seek(7.0)).unwrap();
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
        use crate::analysis::AnalysisFrame;
        use crate::audio::AudioEngine;
        use parking_lot::Mutex;

        for explicit in [false, true] {
            let (commands, receive) = bounded(1);
            commands.send(AudioCommand::Pause(true)).unwrap();
            let (events, event_receive) = bounded(1);
            events
                .send(AudioEvent::Ended {
                    generation: 1,
                    bitrate_bps: None,
                })
                .unwrap();
            let held_events = event_receive.clone();
            let (shutdown, cancelled) = bounded(0);
            let analysis = Arc::new(RwLock::new(AnalysisFrame::default()));
            let waveform = Arc::new(RwLock::new(WaveformFrame::default()));
            let analyzer = AnalysisWorker::new(analysis.clone()).unwrap();
            let analysis_control = Arc::clone(&analyzer.control);
            let worker = Worker::new(receive, events, cancelled, analyzer, waveform.clone(), 1024);
            let worker = std::thread::spawn(move || {
                worker.event(AudioEvent::Ended {
                    generation: 2,
                    bitrate_bps: None,
                });
                worker.run();
            });
            let engine = AudioEngine {
                commands,
                events: event_receive,
                analysis,
                analysis_control,
                waveform,
                worker: Some(worker),
                shutdown: Mutex::new(Some(shutdown)),
            };
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
            assert!(matches!(
                held_events.try_recv(),
                Ok(AudioEvent::Ended { generation: 1, .. })
            ));
        }
    }

    #[test]
    fn full_event_queue_drops_progress_but_retains_lifecycle_events() {
        let (events, receive) = bounded(1);
        let (_stop, shutdown) = bounded(0);
        events
            .send(AudioEvent::Ended {
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
            receive.recv().unwrap(),
            AudioEvent::Ended { generation: 1, .. }
        ));
        assert!(matches!(
            receive.recv().unwrap(),
            AudioEvent::DecoderStopped { generation: 2 }
        ));
        assert!(matches!(
            receive.recv().unwrap(),
            AudioEvent::Failed { generation: 3, .. }
        ));
        sender.join().unwrap();
    }

    #[test]
    fn shutdown_cancels_a_full_event_queue() {
        let (events, _receive) = bounded(1);
        let (stop, shutdown) = bounded::<()>(0);
        events
            .send(AudioEvent::Ended {
                generation: 1,
                bitrate_bps: None,
            })
            .unwrap();
        let sender = std::thread::spawn(move || {
            send_event(
                &events,
                &shutdown,
                AudioEvent::Ended {
                    generation: 2,
                    bitrate_bps: None,
                },
            );
        });
        drop(stop);
        sender.join().unwrap();
    }
}
