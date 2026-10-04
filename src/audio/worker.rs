//! Command handling, playback lifecycle, refill scheduling, and analyzer policy.

use super::{
    AudioCommand, AudioEvent, WaveformFrame,
    output::validate_device,
    playback::{Playback, PlaybackOptions},
};
use crate::analysis::AnalysisWorker;
use anyhow::anyhow;
use crossbeam_channel::{Receiver, Sender, TryRecvError, select};
use parking_lot::RwLock;
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

pub(super) struct Worker {
    commands: Receiver<AudioCommand>,
    events: Sender<AudioEvent>,
    shutdown: Receiver<()>,
    analyzer: AnalysisWorker,
    waveform: Arc<RwLock<WaveformFrame>>,
    playback: Option<Playback>,
    device: Option<String>,
    volume: f32,
    media_read_buffer_len: usize,
    wanted_analysis: bool,
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
            commands,
            events,
            shutdown,
            analyzer,
            waveform,
            playback: None,
            device: None,
            volume: 0.7,
            media_read_buffer_len,
            wanted_analysis: false,
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
        self.analyzer.set_enabled(false);
        if let Some(mut playback) = self.playback.take() {
            self.last_snapshot = (playback.generation(), playback.close());
        }
        self.last_snapshot
    }
    fn refresh_analysis(&self) {
        self.analyzer.set_enabled(
            self.wanted_analysis
                && self
                    .playback
                    .as_ref()
                    .is_some_and(|playback| !playback.paused()),
        );
    }
    fn playback_options(
        &self,
        path: PathBuf,
        range: Option<super::PlaybackRange>,
        generation: u64,
        start: f64,
        paused: bool,
        prior_heard: f64,
    ) -> PlaybackOptions {
        PlaybackOptions {
            path,
            range,
            generation,
            start,
            paused,
            device: self.device.clone(),
            volume: self.volume,
            prior_heard,
            media_read_buffer_len: self.media_read_buffer_len,
            ffmpeg_enabled: self.ffmpeg_enabled,
            pipewire_auto_mix: self.pipewire_auto_mix,
        }
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
            AudioCommand::Analysis(enabled) => {
                self.wanted_analysis = enabled;
                self.refresh_analysis();
            }
            AudioCommand::AnalysisSettings(settings) => {
                self.analyzer.configure(settings);
            }
            AudioCommand::FfmpegEnabled(enabled) => {
                self.ffmpeg_enabled = enabled;
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
                generation,
                start_seconds,
                paused,
            } => {
                self.stop();
                match Playback::new(
                    self.playback_options(path, range, generation, start_seconds, paused, 0.0),
                    &self.analyzer,
                    self.waveform.clone(),
                ) {
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
            AudioCommand::Pause(paused) => {
                if let Some(playback) = self.playback.as_mut() {
                    playback.pause(paused);
                }
                self.refresh_analysis();
            }
            AudioCommand::Seek(seconds) => self.reopen(Some(seconds)),
            AudioCommand::OutputSettings { device, auto_mix } => {
                let device_changed = self.device != device;
                if device_changed || self.pipewire_auto_mix != auto_mix {
                    if device_changed && let Err(error) = validate_device(device.as_deref()) {
                        self.fail(error);
                    } else {
                        self.device = device;
                        self.pipewire_auto_mix = auto_mix;
                        self.reopen(None);
                    }
                }
            }
        }
        true
    }
    fn reopen(&mut self, target: Option<f64>) {
        if let Some(mut old) = self.playback.take() {
            let position = target.unwrap_or_else(|| old.position());
            let heard = old.close();
            let generation = old.generation();
            match Playback::new(
                self.playback_options(
                    old.path().to_owned(),
                    old.range(),
                    generation,
                    position,
                    old.paused(),
                    heard,
                ),
                &self.analyzer,
                self.waveform.clone(),
            ) {
                Ok(playback) => self.playback = Some(playback),
                Err(error) => {
                    self.last_snapshot = (generation, heard);
                    self.event(AudioEvent::Failed {
                        generation,
                        message: format!("{error:#}"),
                    });
                }
            }
            self.refresh_analysis();
        }
    }
    fn fail(&mut self, error: anyhow::Error) {
        let (generation, _) = self.stop();
        self.event(AudioEvent::Failed {
            generation,
            message: format!("{error:#}"),
        });
    }
    pub(super) fn run(mut self) {
        loop {
            if matches!(self.shutdown.try_recv(), Err(TryRecvError::Disconnected)) {
                break;
            }
            while let Ok(command) = self.commands.try_recv() {
                if matches!(self.shutdown.try_recv(), Err(TryRecvError::Disconnected)) {
                    self.stop();
                    return;
                }
                if !self.command(command) {
                    return;
                }
            }
            if self.playback.is_none() {
                let command = select! {
                    recv(self.shutdown) -> _ => break,
                    recv(self.commands) -> command => match command {
                        Ok(command) => command,
                        Err(_) => break,
                    },
                };
                if !self.command(command) {
                    break;
                }
                continue;
            }
            // Fatal backend errors must still be delivered while software-paused,
            // and must not be hidden by an otherwise completed drain.
            if let Ok(error) = self.playback.as_ref().unwrap().errors().try_recv() {
                self.fail(anyhow!(error));
                continue;
            }
            let result = self.playback.as_mut().unwrap().fill();
            if let Err(error) = result {
                self.fail(error);
                continue;
            }
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
                let (_, heard) = self.stop();
                self.event(AudioEvent::Progress {
                    generation,
                    position_seconds: position,
                    listened_seconds: heard,
                });
                self.event(AudioEvent::Ended { generation });
                continue;
            }
            let playback = self.playback.as_ref().unwrap();
            let errors = playback.errors().clone();
            let notifications = playback.notifications().clone();
            // Pause waits for commands, errors or the final device-clock tail.
            // Stable paused playback never schedules a status/GUI polling tick.
            let command = if playback.paused() {
                select! {
                    recv(self.shutdown) -> _ => break,
                    recv(self.commands) -> command => Some(command),
                    recv(errors) -> error => {
                        if let Ok(error) = error { self.fail(anyhow!(error)); }
                        None
                    },
                    recv(notifications) -> _ => None,
                }
            } else {
                select! {
                    recv(self.shutdown) -> _ => break,
                    recv(self.commands) -> command => Some(command),
                    recv(errors) -> error => {
                        if let Ok(error) = error { self.fail(anyhow!(error)); }
                        None
                    },
                    recv(notifications) -> _ => None,
                    default(Duration::from_millis(10)) => None,
                }
            };
            if let Some(command) = command {
                let Ok(command) = command else {
                    break;
                };
                if !self.command(command) {
                    break;
                }
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

    #[test]
    fn engine_shutdown_and_drop_cancel_full_command_and_event_queues() {
        use crate::analysis::AnalysisFrame;
        use crate::audio::AudioEngine;
        use parking_lot::Mutex;

        for explicit in [false, true] {
            let (commands, receive) = bounded(1);
            commands.send(AudioCommand::Pause(true)).unwrap();
            let (events, event_receive) = bounded(1);
            events.send(AudioEvent::Ended { generation: 1 }).unwrap();
            let held_events = event_receive.clone();
            let (shutdown, cancelled) = bounded(0);
            let analysis = Arc::new(RwLock::new(AnalysisFrame::default()));
            let waveform = Arc::new(RwLock::new(WaveformFrame::default()));
            let worker = Worker::new(
                receive,
                events,
                cancelled,
                AnalysisWorker::new(analysis.clone()).unwrap(),
                waveform.clone(),
                1024,
            );
            let worker = std::thread::spawn(move || {
                worker.event(AudioEvent::Ended { generation: 2 });
                worker.run();
            });
            let engine = AudioEngine {
                commands,
                events: event_receive,
                analysis,
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
                Ok(AudioEvent::Ended { generation: 1 })
            ));
        }
    }

    #[test]
    fn full_event_queue_drops_progress_but_retains_lifecycle_events() {
        let (events, receive) = bounded(1);
        let (_stop, shutdown) = bounded(0);
        events.send(AudioEvent::Ended { generation: 1 }).unwrap();
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
            AudioEvent::Ended { generation: 1 }
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
        events.send(AudioEvent::Ended { generation: 1 }).unwrap();
        let sender = std::thread::spawn(move || {
            send_event(&events, &shutdown, AudioEvent::Ended { generation: 2 });
        });
        drop(stop);
        sender.join().unwrap();
    }
}
