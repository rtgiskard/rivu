//! Command handling, playback lifecycle, refill scheduling, and analyzer policy.

use super::{
    AudioCommand, AudioEvent,
    output::validate_device,
    playback::{Playback, PlaybackOptions},
};
use crate::analysis::AnalysisWorker;
use anyhow::anyhow;
use crossbeam_channel::{Receiver, Sender, select};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

pub(super) struct Worker {
    commands: Receiver<AudioCommand>,
    events: Sender<AudioEvent>,
    analyzer: AnalysisWorker,
    playback: Option<Playback>,
    device: Option<String>,
    volume: f32,
    media_read_buffer_len: usize,
    wanted_analysis: bool,
    last_snapshot: (u64, f64),
    progress: Instant,
}
impl Worker {
    pub(super) fn new(
        commands: Receiver<AudioCommand>,
        events: Sender<AudioEvent>,
        analyzer: AnalysisWorker,
        media_read_buffer_len: usize,
    ) -> Self {
        Self {
            commands,
            events,
            analyzer,
            playback: None,
            device: None,
            volume: 0.7,
            media_read_buffer_len,
            wanted_analysis: false,
            last_snapshot: (0, 0.0),
            progress: Instant::now(),
        }
    }
    fn event(&self, event: AudioEvent) {
        let _ = self.events.send_timeout(event, Duration::from_secs(1));
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
        generation: u64,
        start: f64,
        paused: bool,
        prior_heard: f64,
    ) -> PlaybackOptions {
        PlaybackOptions {
            path,
            generation,
            start,
            paused,
            device: self.device.clone(),
            volume: self.volume,
            prior_heard,
            media_read_buffer_len: self.media_read_buffer_len,
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
            AudioCommand::AnalysisRate(rate) if (5..=60).contains(&rate) => {
                self.analyzer.set_rate(rate);
            }
            AudioCommand::AnalysisRate(_) => {}
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
                generation,
                start_seconds,
                paused,
            } => {
                self.stop();
                match Playback::new(
                    self.playback_options(path, generation, start_seconds, paused, 0.0),
                    &self.analyzer,
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
            AudioCommand::Device(name) => {
                if let Err(error) = validate_device(name.as_deref()) {
                    self.fail(error);
                } else {
                    self.device = name;
                    self.reopen(None);
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
                    generation,
                    position,
                    old.paused(),
                    heard,
                ),
                &self.analyzer,
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
            while let Ok(command) = self.commands.try_recv() {
                if !self.command(command) {
                    return;
                }
            }
            if self.playback.is_none() {
                let Ok(command) = self.commands.recv() else {
                    break;
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
            if self.progress.elapsed() >= Duration::from_millis(100) {
                let playback = self.playback.as_ref().unwrap();
                self.event(AudioEvent::Progress {
                    generation: playback.generation(),
                    position_seconds: playback.position(),
                    listened_seconds: playback.heard(),
                });
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
            let interval = Duration::from_millis(if playback.paused() { 100 } else { 10 });
            select! {
                recv(self.commands) -> command => {
                    let Ok(command) = command else { break; };
                    if !self.command(command) { break; }
                }
                recv(errors) -> error => if let Ok(error) = error { self.fail(anyhow!(error)); },
                // The software-paused stream still runs silent callbacks so its
                // device clock and fatal errors continue to be observed.
                default(interval) => (),
            }
        }
        self.stop();
    }
}
