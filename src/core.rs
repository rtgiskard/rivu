use crate::{
    analysis::AnalysisFrame,
    audio::{self, AudioCommand, AudioEngine, AudioEvent},
    config::Config,
    library::{self, M3uItem, ScanResult},
    model::*,
    store::Store,
};
use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender, bounded, select};
use parking_lot::RwLock;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

type Wakeup = Arc<dyn Fn() + Send + Sync>;
struct Request {
    command: Command,
    reply: Option<Sender<Response>>,
}

#[derive(Clone)]
pub struct AppHandle {
    pub state: Arc<RwLock<AppState>>,
    pub analysis: Arc<RwLock<AnalysisFrame>>,
    sender: Sender<Request>,
    wakeup: Arc<RwLock<Option<Wakeup>>>,
    subscribers: Arc<RwLock<Vec<Sender<()>>>>,
    raise_requested: Arc<AtomicBool>,
}

impl AppHandle {
    pub fn send(&self, command: Command) -> Result<()> {
        self.sender
            .try_send(Request {
                command,
                reply: None,
            })
            .context("Rivu is busy or stopped")
    }
    pub fn snapshot(&self) -> AppState {
        self.state.read().clone()
    }
    pub fn set_wakeup(&self, callback: impl Fn() + Send + Sync + 'static) {
        *self.wakeup.write() = Some(Arc::new(callback));
    }
    pub fn subscribe(&self) -> Receiver<()> {
        let (sender, receiver) = bounded(1);
        self.subscribers.write().push(sender);
        receiver
    }
    pub fn raise(&self) -> Result<()> {
        self.raise_requested.store(true, Ordering::Release);
        if let Some(callback) = self.wakeup.read().clone() {
            callback();
        }
        Ok(())
    }
    pub fn take_raise_request(&self) -> bool {
        self.raise_requested.swap(false, Ordering::AcqRel)
    }
    pub fn request(&self, command: Command) -> Response {
        let (tx, rx) = bounded(1);
        let maintenance = matches!(command, Command::OptimizeDatabase);
        let result = self.sender.send_timeout(
            Request {
                command,
                reply: Some(tx),
            },
            Duration::from_secs(2),
        );
        if let Err(error) = result {
            return Response {
                ok: false,
                error: Some(format!("Core unavailable: {error}")),
                state: self.snapshot(),
            };
        }
        let response = if maintenance {
            rx.recv().map_err(|error| error.to_string())
        } else {
            rx.recv_timeout(Duration::from_secs(12))
                .map_err(|error| error.to_string())
        };
        response.unwrap_or_else(|error| Response {
            ok: false,
            error: Some(format!("Core response unavailable: {error}")),
            state: self.snapshot(),
        })
    }
}

pub struct Runtime {
    pub handle: AppHandle,
    worker: Option<JoinHandle<()>>,
}
impl Runtime {
    pub fn start(data_dir: &Path, config_path: &Path) -> Result<Self> {
        let config = Config::load(config_path)?;
        config.save(config_path)?;
        std::fs::create_dir_all(data_dir)?;
        let store = Store::open(&data_dir.join("library.db"))?;
        let engine = AudioEngine::new(config.media_read_buffer_mb)?;
        let (sender, receiver) = bounded(64);
        let shared = Arc::new(RwLock::new(AppState::default()));
        let wakeup = Arc::new(RwLock::new(None));
        let subscribers = Arc::new(RwLock::new(Vec::new()));
        let handle = AppHandle {
            state: shared.clone(),
            analysis: engine.analysis.clone(),
            sender,
            wakeup: wakeup.clone(),
            subscribers: subscribers.clone(),
            raise_requested: Arc::new(AtomicBool::new(false)),
        };
        let mut initial = AppState {
            library: Arc::new(store.tracks()?),
            playlists: Arc::new(store.playlists()?),
            history: Arc::new(store.history(200)?),
            devices: Arc::new(audio::devices().unwrap_or_default()),
            volume: config.volume,
            shuffle: config.shuffle,
            repeat: config.repeat,
            selected_device: config.output_device.clone(),
            config: Arc::new(config),
            config_path: config_path.to_path_buf(),
            ..AppState::default()
        };
        if let Some(json) = store.get_setting("playback")? {
            let saved: Saved =
                serde_json::from_str(&json).context("Reading saved playback settings")?;
            initial.queue = Arc::new(
                saved
                    .queue
                    .into_iter()
                    .filter(|entry| {
                        initial
                            .library
                            .binary_search_by_key(&entry.track_id, |track| track.id)
                            .is_ok()
                    })
                    .collect(),
            );
            initial.current_queue_id = saved
                .current
                .filter(|id| initial.queue.iter().any(|entry| entry.id == *id));
        }
        *shared.write() = initial.clone();
        let worker = thread::Builder::new()
            .name("rivu-core".into())
            .spawn(move || {
                let (scan_tx, scan_rx) = bounded(1);
                let next_queue_id = initial.queue.iter().map(|q| q.id).max().unwrap_or(0) + 1;
                let mut core = Core {
                    store,
                    engine,
                    state: initial,
                    shared,
                    wakeup,
                    subscribers,
                    generation: 0,
                    next_queue_id,
                    playback: None,
                    scan_tx,
                    scan_rx,
                    scan_workers: Vec::new(),
                    shuffle_bag: Vec::new(),
                    played: Vec::new(),
                    played_cursor: 0,
                };
                let _ = core
                    .engine
                    .commands
                    .send(AudioCommand::Volume(core.state.volume));
                let _ = core
                    .engine
                    .commands
                    .send(AudioCommand::AnalysisRate(core.state.config.analysis_fps));
                if core.state.selected_device.is_some() {
                    let _ = core
                        .engine
                        .commands
                        .send(AudioCommand::Device(core.state.selected_device.clone()));
                }
                core.run(receiver);
            })?;
        Ok(Self {
            handle,
            worker: Some(worker),
        })
    }
    pub fn is_finished(&self) -> bool {
        self.worker
            .as_ref()
            .is_none_or(|worker| worker.is_finished())
    }
    pub fn join(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        if self
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
        {
            let _ = self.handle.sender.send(Request {
                command: Command::Shutdown,
                reply: None,
            });
        }
        self.join();
    }
}

#[derive(Serialize, Deserialize)]
struct Saved {
    queue: Vec<QueueEntry>,
    current: Option<u64>,
}
struct PlaybackStats {
    track_id: i64,
    heard: f64,
    position: Option<f64>,
    started: bool,
    counted: bool,
}
struct ScanFinished {
    result: Result<ScanResult>,
    import: Option<(String, Vec<M3uItem>)>,
}
struct Core {
    store: Store,
    engine: AudioEngine,
    state: AppState,
    shared: Arc<RwLock<AppState>>,
    wakeup: Arc<RwLock<Option<Wakeup>>>,
    subscribers: Arc<RwLock<Vec<Sender<()>>>>,
    generation: u64,
    next_queue_id: u64,
    playback: Option<PlaybackStats>,
    scan_tx: Sender<ScanFinished>,
    scan_rx: Receiver<ScanFinished>,
    scan_workers: Vec<JoinHandle<()>>,
    shuffle_bag: Vec<u64>,
    played: Vec<u64>,
    // Number of history entries through the current (or just removed) entry.
    played_cursor: usize,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
impl Core {
    fn run(&mut self, requests: Receiver<Request>) {
        loop {
            select! {
                recv(requests) -> request => {
                    let Ok(request) = request else { break; };
                    let shutdown = matches!(request.command, Command::Shutdown);
                    let changed = !matches!(request.command, Command::Status | Command::Overview);
                    let result = self.command(request.command);
                    if let Err(error) = &result { self.state.last_error = Some(format!("{error:#}")); }
                    if changed { self.publish(); }
                    if let Some(reply) = request.reply {
                        let _ = reply.send(Response { ok: result.is_ok(), error: result.err().map(|e| format!("{e:#}")), state: self.state.clone() });
                    }
                    if shutdown { break; }
                }
                recv(self.engine.events) -> event => {
                    let Ok(event) = event else { self.state.last_error = Some("Audio worker stopped unexpectedly".into()); self.publish(); break; };
                    if let Err(error) = self.audio_event(event) { self.state.last_error = Some(format!("{error:#}")); }
                    self.publish();
                }
                recv(self.scan_rx) -> scan => {
                    if let Ok(scan) = scan {
                        if let Err(error) = self.finish_scan(scan) { self.state.last_error = Some(format!("{error:#}")); }
                        self.state.scanning = false;
                        self.publish();
                    }
                }
            }
        }
        if let Err(error) = self.stop() {
            self.state.last_error = Some(format!("Saving play count: {error:#}"));
        }
        let _ = self.save();
        let _ = self.engine.commands.send(AudioCommand::Shutdown);
        for worker in self.scan_workers.drain(..) {
            let _ = worker.join();
        }
        self.state.status = PlaybackStatus::Stopped;
        self.publish();
    }

    fn publish(&mut self) {
        self.state.revision = self.state.revision.wrapping_add(1);
        *self.shared.write() = self.state.clone();
        self.subscribers.write().retain(|sender| {
            !matches!(
                sender.try_send(()),
                Err(crossbeam_channel::TrySendError::Disconnected(_))
            )
        });
        let callback = self.wakeup.read().clone();
        if let Some(callback) = callback {
            callback();
        }
    }
    fn save(&mut self) -> Result<()> {
        if self.state.config.volume != self.state.volume
            || self.state.config.shuffle != self.state.shuffle
            || self.state.config.repeat != self.state.repeat
            || self.state.config.output_device != self.state.selected_device
        {
            let mut config = self.state.config.as_ref().clone();
            config.volume = self.state.volume;
            config.shuffle = self.state.shuffle;
            config.repeat = self.state.repeat;
            config.output_device.clone_from(&self.state.selected_device);
            config.save(&self.state.config_path)?;
            self.state.config = Arc::new(config);
        }
        let saved = Saved {
            queue: self.state.queue.as_ref().clone(),
            current: self.state.current_queue_id,
        };
        self.store
            .set_setting("playback", &serde_json::to_string(&saved)?)
    }
    fn reload(&mut self) -> Result<()> {
        self.state.library = Arc::new(self.store.tracks()?);
        self.state.playlists = Arc::new(self.store.playlists()?);
        self.state.history = Arc::new(self.store.history(200)?);
        Ok(())
    }
    fn audio(&self, command: AudioCommand) -> Result<()> {
        self.engine
            .commands
            .send(command)
            .context("Audio worker unavailable")
    }
    fn track(&self, id: i64) -> Result<&Track> {
        let index = self
            .state
            .library
            .binary_search_by_key(&id, |track| track.id)
            .ok()
            .context("Track not found")?;
        Ok(&self.state.library[index])
    }
    fn enqueue(&mut self, ids: &[i64]) -> Result<()> {
        for id in ids {
            self.track(*id)?;
        }
        let queue = Arc::make_mut(&mut self.state.queue);
        for id in ids {
            queue.push(QueueEntry {
                id: self.next_queue_id,
                track_id: *id,
            });
            self.next_queue_id += 1;
        }
        self.shuffle_bag.clear();
        Ok(())
    }
    fn prune_queue_history(&mut self) {
        let queue = &self.state.queue;
        self.shuffle_bag
            .retain(|id| queue.iter().any(|entry| entry.id == *id));
        let mut index = 0;
        let mut cursor = 0;
        self.played.retain(|id| {
            let keep = queue.iter().any(|entry| entry.id == *id);
            if keep && index < self.played_cursor {
                cursor += 1;
            }
            index += 1;
            keep
        });
        self.played_cursor = cursor;
    }
    fn start(&mut self, queue_id: u64, record_history: bool) -> Result<()> {
        let entry = self
            .state
            .queue
            .iter()
            .find(|entry| entry.id == queue_id)
            .context("Queue entry not found")?;
        let track = self.track(entry.track_id)?.clone();
        if !track.path.is_file() {
            bail!("Missing audio file: {}", track.path.display());
        }
        self.stop()?;
        self.audio(AudioCommand::Load {
            path: track.path,
            range: track.cue.as_ref().map(|cue| audio::PlaybackRange {
                start_seconds: cue.start_seconds(),
                end_seconds: cue.end_seconds(),
            }),
            generation: self.generation,
            start_seconds: 0.0,
            paused: false,
        })?;
        self.playback = Some(PlaybackStats {
            track_id: track.id,
            heard: 0.0,
            position: None,
            started: false,
            counted: false,
        });
        self.state.current_queue_id = Some(queue_id);
        self.state.duration = track.duration;
        self.state.status = PlaybackStatus::Playing;
        self.state.last_error = None;
        if record_history {
            self.played.truncate(self.played_cursor);
            self.played.push(queue_id);
            self.played_cursor = self.played.len();
        }
        self.shuffle_bag.retain(|id| *id != queue_id);
        self.save()
    }
    fn count_play(&mut self) -> Result<()> {
        let Some(duration) = self
            .state
            .duration
            .filter(|duration| duration.is_finite() && *duration > 0.0)
        else {
            return Ok(());
        };
        let Some(playback) = &mut self.playback else {
            return Ok(());
        };
        if playback.started
            && !playback.counted
            && playback.heard > duration * self.state.config.play_count_threshold_percent / 100.0
        {
            self.store.increment_play_count(playback.track_id)?;
            playback.counted = true;
            self.state.library = Arc::new(self.store.tracks()?);
        }
        Ok(())
    }
    fn playback_started(&mut self, info: library::MediaInfo) -> Result<()> {
        let Some(playback) = &mut self.playback else {
            return Ok(());
        };
        if playback.started {
            return Ok(());
        }
        if let Err(error) = self.store.mark_played(playback.track_id, now()) {
            // Rejected bookkeeping must not be revived by another queued start
            // or counted from the successful audio output's final snapshot.
            self.playback = None;
            return Err(error);
        }
        playback.started = true;
        self.state.duration = info
            .duration
            .filter(|duration| duration.is_finite() && *duration > 0.0)
            .or(self
                .state
                .duration
                .filter(|duration| duration.is_finite() && *duration > 0.0));
        self.state.library = Arc::new(self.store.tracks()?);
        self.state.history = Arc::new(self.store.history(200)?);
        self.count_play()
    }
    fn playback_progress(&mut self, position: f64, heard: f64) -> Result<()> {
        if let Some(playback) = &mut self.playback
            && playback.started
        {
            if position.is_finite() {
                self.state.position = position;
                playback.position = Some(position);
            }
            if heard.is_finite() {
                playback.heard = playback.heard.max(heard);
            }
            self.count_play()?;
        }
        Ok(())
    }
    fn drain_playback_events(&mut self, generation: u64) -> Result<()> {
        while let Ok(event) = self.engine.events.try_recv() {
            match event {
                AudioEvent::Started {
                    generation: event_generation,
                    info,
                } if event_generation == generation => self.playback_started(info)?,
                AudioEvent::Progress {
                    generation: event_generation,
                    position_seconds,
                    listened_seconds,
                } if event_generation == generation => {
                    self.playback_progress(position_seconds, listened_seconds)?;
                }
                // The output is retiring: terminal events must never advance
                // the queue, recursively stop the worker, or revive playback.
                _ => {}
            }
        }
        Ok(())
    }
    fn finish_playback(
        &mut self,
        snapshot: Result<Option<(u64, f64)>>,
        natural: bool,
    ) -> Result<()> {
        let generation = self.generation;
        // A request can beat a queued Started even though the worker has already
        // played audio. Acknowledged stops make that queued bookkeeping safe to
        // consume before generation fencing; errors still fence below.
        let bookkeeping = if snapshot.is_ok() {
            self.drain_playback_events(generation)
        } else {
            Ok(())
        };
        // Fence queued events even when stopping or saving the count fails.
        self.generation = self.generation.wrapping_add(1);
        self.state.status = PlaybackStatus::Stopped;
        self.state.position = 0.0;
        self.state.seek_revision = self.state.seek_revision.wrapping_add(1);
        let snapshot = snapshot?;
        if let Some(playback) = &mut self.playback {
            if let Some((snapshot_generation, heard)) = snapshot
                && snapshot_generation == generation
                && heard.is_finite()
            {
                playback.heard = playback.heard.max(heard);
            }
            if natural
                && !self
                    .state
                    .duration
                    .is_some_and(|duration| duration.is_finite() && duration > 0.0)
            {
                // Only backend-confirmed position at EOF can resolve an unknown
                // duration; a seek target or accumulated heard time cannot.
                self.state.duration = playback.position.filter(|position| *position > 0.0);
            }
        }
        bookkeeping?;
        self.count_play()?;
        self.playback = None;
        Ok(())
    }
    fn stop(&mut self) -> Result<()> {
        let snapshot = self.engine.stop_and_snapshot().map(Some);
        self.finish_playback(snapshot, false)
    }
    fn advance(&mut self, natural: bool) -> Result<()> {
        if self.state.queue.is_empty() {
            return self.stop();
        }
        let current = self.state.current_queue_id;
        if natural
            && self.state.repeat == RepeatMode::One
            && let Some(id) = current
        {
            return self.start(id, true);
        }
        if self.state.shuffle {
            if !natural && self.played_cursor < self.played.len() {
                let id = self.played[self.played_cursor];
                self.start(id, false)?;
                self.played_cursor += 1;
                return Ok(());
            }
            if self.shuffle_bag.is_empty() {
                let visited = self
                    .played
                    .iter()
                    .copied()
                    .collect::<std::collections::HashSet<_>>();
                self.shuffle_bag = self
                    .state
                    .queue
                    .iter()
                    .filter(|q| Some(q.id) != current && !visited.contains(&q.id))
                    .map(|q| q.id)
                    .collect();
                if self.shuffle_bag.is_empty() && self.state.repeat == RepeatMode::All {
                    self.played.clear();
                    self.played_cursor = 0;
                    self.shuffle_bag = self
                        .state
                        .queue
                        .iter()
                        .filter(|q| Some(q.id) != current)
                        .map(|q| q.id)
                        .collect();
                    if self.shuffle_bag.is_empty() {
                        self.shuffle_bag = self.state.queue.iter().map(|q| q.id).collect();
                    }
                }
                self.shuffle_bag.shuffle(&mut rand::rng());
            }
            if let Some(&id) = self.shuffle_bag.last() {
                return self.start(id, true);
            }
        } else {
            let index = current.and_then(|id| self.state.queue.iter().position(|q| q.id == id));
            let next = index.map_or(0, |index| index + 1);
            if let Some(entry) = self.state.queue.get(next) {
                return self.start(entry.id, true);
            }
            if self.state.repeat == RepeatMode::All {
                return self.start(self.state.queue[0].id, true);
            }
        }
        self.stop()
    }
    fn previous(&mut self) -> Result<()> {
        if self.state.position > 3.0 {
            return self.seek(0.0);
        }
        if self.state.shuffle
            && let Some(index) =
                self.played_cursor
                    .checked_sub(if self.state.current_queue_id.is_some() {
                        2
                    } else {
                        1
                    })
        {
            self.start(self.played[index], false)?;
            self.played_cursor = index + 1;
            return Ok(());
        }
        let index = self
            .state
            .current_queue_id
            .and_then(|id| self.state.queue.iter().position(|q| q.id == id));
        if let Some(index) = index {
            if index > 0 {
                return self.start(self.state.queue[index - 1].id, true);
            }
            return self.seek(0.0);
        }
        if let Some(entry) = self.state.queue.first() {
            return self.start(entry.id, true);
        }
        bail!("Queue is empty")
    }
    fn seek(&mut self, seconds: f64) -> Result<()> {
        if !seconds.is_finite() || seconds < 0.0 {
            bail!("Seek must be a finite nonnegative number");
        }
        if self.state.status == PlaybackStatus::Stopped {
            bail!("Nothing is playing");
        }
        let seconds = self
            .state
            .duration
            .map_or(seconds, |duration| seconds.min(duration));
        self.audio(AudioCommand::Seek(seconds))?;
        self.state.position = seconds;
        self.state.seek_revision = self.state.seek_revision.wrapping_add(1);
        Ok(())
    }
    fn scan(
        &mut self,
        paths: Vec<PathBuf>,
        import: Option<(String, Vec<M3uItem>)>,
        force: bool,
    ) -> Result<()> {
        if self.state.scanning {
            bail!("A library scan is already running");
        }
        if paths.is_empty() {
            bail!("No files or directories supplied");
        }
        let known = if force {
            Vec::new()
        } else {
            self.store.known_files()?
        };
        let sender = self.scan_tx.clone();
        let worker = thread::Builder::new()
            .name("rivu-scan".into())
            .spawn(move || {
                let result = library::scan_paths(&paths, &known);
                let _ = sender.send(ScanFinished { result, import });
            })?;
        self.scan_workers.retain(|worker| !worker.is_finished());
        self.scan_workers.push(worker);
        self.state.scanning = true;
        self.state.last_error = None;
        self.state.scan_message = "Reading audio metadata…".into();
        Ok(())
    }
    fn finish_scan(&mut self, scan: ScanFinished) -> Result<()> {
        let result = scan.result?;
        self.store.apply_scan(&result)?;
        self.state.scan_message = result.summary();
        if !result.errors().is_empty() {
            self.state.last_error = Some(result.errors().join("\n"));
        }
        self.reload()?;
        if let Some((name, items)) = scan.import {
            let mut ids = Vec::with_capacity(items.len());
            let by_source: std::collections::HashMap<(&Path, Option<u32>), i64> = self
                .state
                .library
                .iter()
                .filter(|track| !track.missing)
                .map(|track| {
                    let key = track
                        .cue
                        .as_ref()
                        .map_or((track.path.as_path(), None), |cue| {
                            (cue.sheet.as_path(), Some(cue.number))
                        });
                    (key, track.id)
                })
                .collect();
            for item in items {
                if let Ok(path) = item.path.canonicalize()
                    && let Some(id) = by_source.get(&(path.as_path(), item.cue_track))
                {
                    ids.push(*id);
                }
            }
            if ids.is_empty() {
                bail!("Playlist contains no supported, accessible audio tracks");
            }
            let playlist_id = self.store.create_playlist(&name)?;
            self.store.add_playlist(playlist_id, &ids)?;
            self.state.playlists = Arc::new(self.store.playlists()?);
        }
        Ok(())
    }
    fn audio_event(&mut self, event: AudioEvent) -> Result<()> {
        match event {
            AudioEvent::Started { generation, info } if generation == self.generation => {
                if let Err(error) = self.playback_started(info) {
                    self.stop().with_context(|| format!("{error:#}"))?;
                    return Err(error);
                }
            }
            AudioEvent::Progress {
                generation,
                position_seconds,
                listened_seconds,
            } if generation == self.generation => {
                self.playback_progress(position_seconds, listened_seconds)?;
            }
            AudioEvent::Ended { generation } if generation == self.generation => {
                let snapshot = self.engine.stop_and_snapshot().map(Some);
                self.finish_playback(snapshot, true)?;
                self.advance(true)?;
            }
            AudioEvent::Failed {
                generation,
                message,
            } if generation == self.generation => {
                self.stop().with_context(|| message.clone())?;
                self.state.last_error = Some(message);
            }
            _ => {}
        }
        Ok(())
    }
    fn command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Status | Command::Overview => return Ok(()),
            Command::Scan { paths, force } => self.scan(paths, None, force)?,
            Command::Play { track_id } => {
                self.track(track_id)?;
                let existing = self
                    .state
                    .queue
                    .iter()
                    .find(|q| q.track_id == track_id)
                    .map(|q| q.id);
                let id = if let Some(id) = existing {
                    id
                } else {
                    self.enqueue(&[track_id])?;
                    self.state.queue.last().unwrap().id
                };
                self.start(id, true)?;
            }
            Command::PlayQueue { queue_id } => self.start(queue_id, true)?,
            Command::PlayPlaylist { playlist_id } => {
                let ids = self
                    .state
                    .playlists
                    .iter()
                    .find(|p| p.id == playlist_id)
                    .context("Playlist not found")?
                    .entries
                    .iter()
                    .map(|e| e.track_id)
                    .collect::<Vec<_>>();
                if ids.is_empty() {
                    bail!("Playlist is empty");
                }
                self.stop()?;
                self.state.queue = Arc::new(Vec::new());
                self.state.current_queue_id = None;
                self.played.clear();
                self.played_cursor = 0;
                self.enqueue(&ids)?;
                self.start(self.state.queue[0].id, true)?;
            }
            Command::Resume => {
                if self.state.status == PlaybackStatus::Stopped {
                    let id = self
                        .state
                        .current_queue_id
                        .or_else(|| self.state.queue.first().map(|q| q.id))
                        .context("Queue is empty")?;
                    self.start(id, true)?;
                } else {
                    self.audio(AudioCommand::Pause(false))?;
                    self.state.status = PlaybackStatus::Playing;
                }
            }
            Command::Pause => {
                if self.state.status == PlaybackStatus::Playing {
                    self.audio(AudioCommand::Pause(true))?;
                    self.state.status = PlaybackStatus::Paused;
                }
            }
            Command::Toggle => {
                return self.command(if self.state.status == PlaybackStatus::Playing {
                    Command::Pause
                } else {
                    Command::Resume
                });
            }
            Command::Stop => self.stop()?,
            Command::Next => self.advance(false)?,
            Command::Previous => self.previous()?,
            Command::Seek { seconds } => self.seek(seconds)?,
            Command::Volume { value } => {
                if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                    bail!("Volume must be between 0 and 1");
                }
                self.audio(AudioCommand::Volume(value))?;
                self.state.volume = value;
            }
            Command::Enqueue { track_ids } => self.enqueue(&track_ids)?,
            Command::RemoveQueue { queue_id } => {
                if !self.state.queue.iter().any(|q| q.id == queue_id) {
                    bail!("Queue entry not found");
                }
                if self.state.current_queue_id == Some(queue_id) {
                    self.stop()?;
                    self.state.current_queue_id = None;
                }
                Arc::make_mut(&mut self.state.queue).retain(|q| q.id != queue_id);
                self.prune_queue_history();
            }
            Command::MoveQueue { queue_id, index } => {
                let queue = Arc::make_mut(&mut self.state.queue);
                let old = queue
                    .iter()
                    .position(|q| q.id == queue_id)
                    .context("Queue entry not found")?;
                if index >= queue.len() {
                    bail!("Queue target position out of range");
                }
                let entry = queue.remove(old);
                queue.insert(index, entry);
            }
            Command::ClearQueue => {
                self.stop()?;
                self.state.queue = Arc::new(Vec::new());
                self.state.current_queue_id = None;
                self.played.clear();
                self.played_cursor = 0;
                self.shuffle_bag.clear();
            }
            Command::Shuffle { enabled } => {
                self.state.shuffle = enabled;
                self.shuffle_bag.clear();
                self.played.clear();
                self.played_cursor = 0;
                if let Some(id) = self.state.current_queue_id {
                    self.played.push(id);
                    self.played_cursor = 1;
                }
            }
            Command::Repeat { mode } => self.state.repeat = mode,
            Command::CreatePlaylist { name } => {
                self.store.create_playlist(&name)?;
                self.state.playlists = Arc::new(self.store.playlists()?);
            }
            Command::RenamePlaylist { playlist_id, name } => {
                self.store.rename_playlist(playlist_id, &name)?;
                self.state.playlists = Arc::new(self.store.playlists()?);
            }
            Command::DeletePlaylist { playlist_id } => {
                self.store.delete_playlist(playlist_id)?;
                self.state.playlists = Arc::new(self.store.playlists()?);
            }
            Command::AddPlaylist {
                playlist_id,
                track_ids,
            } => {
                self.store.add_playlist(playlist_id, &track_ids)?;
                self.state.playlists = Arc::new(self.store.playlists()?);
            }
            Command::RemovePlaylistEntry { entry_id } => {
                self.store.remove_playlist_entry(entry_id)?;
                self.state.playlists = Arc::new(self.store.playlists()?);
            }
            Command::MovePlaylistEntry { entry_id, index } => {
                self.store.move_playlist_entry(entry_id, index)?;
                self.state.playlists = Arc::new(self.store.playlists()?);
            }
            Command::ImportPlaylist { path, name } => {
                if self.state.scanning {
                    bail!("A library scan is already running");
                }
                let items = library::import_playlist(&path)?;
                let paths = items
                    .iter()
                    .map(|item| item.path.clone())
                    .collect::<Vec<_>>();
                let name = name.unwrap_or_else(|| {
                    path.file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                });
                self.scan(paths, Some((name, items)), false)?;
            }
            Command::ExportPlaylist { playlist_id, path } => {
                let playlist = self
                    .state
                    .playlists
                    .iter()
                    .find(|p| p.id == playlist_id)
                    .context("Playlist not found")?;
                library::export_m3u(&path, playlist, &self.state.library)?;
            }
            Command::EditTrack {
                track_id,
                title,
                artist,
                album,
            } => {
                self.store.edit_track(track_id, &title, &artist, &album)?;
                self.state.library = Arc::new(self.store.tracks()?);
            }
            Command::RemoveTracks { track_ids } => {
                for id in &track_ids {
                    self.track(*id)?;
                }
                if self
                    .state
                    .current_track()
                    .is_some_and(|track| track_ids.contains(&track.id))
                {
                    self.stop()?;
                    self.state.current_queue_id = None;
                }
                self.store.remove_tracks(&track_ids)?;
                Arc::make_mut(&mut self.state.queue).retain(|q| !track_ids.contains(&q.track_id));
                self.prune_queue_history();
                self.reload()?;
            }
            Command::SetFavorite {
                track_ids,
                favorite,
            } => {
                self.store.set_favorite(&track_ids, favorite)?;
                self.state.library = Arc::new(self.store.tracks()?);
            }
            Command::RemoveMissingTracks => {
                let track_ids = self
                    .state
                    .library
                    .iter()
                    .filter(|track| track.missing)
                    .map(|track| track.id)
                    .collect();
                return self.command(Command::RemoveTracks { track_ids });
            }
            Command::Device { name } => {
                if let Some(name) = &name
                    && !self.state.devices.contains(name)
                {
                    bail!("Output device not found: {name}");
                }
                self.audio(AudioCommand::Device(name.clone()))?;
                self.state.selected_device = name;
            }
            Command::Analysis { enabled } => {
                self.audio(AudioCommand::Analysis(enabled))?;
                return Ok(());
            }
            Command::DismissError => {
                self.state.last_error = None;
                return Ok(());
            }
            Command::Configure { config } => {
                config.validate()?;
                if let Some(device) = &config.output_device
                    && !self.state.devices.contains(device)
                {
                    bail!("Output device not found: {device}");
                }
                config.save(&self.state.config_path)?;
                if self.state.selected_device != config.output_device {
                    self.audio(AudioCommand::Device(config.output_device.clone()))?;
                }
                self.audio(AudioCommand::MediaReadBuffer(config.media_read_buffer_mb))?;
                self.audio(AudioCommand::Volume(config.volume))?;
                self.audio(AudioCommand::AnalysisRate(config.analysis_fps))?;
                self.state.selected_device.clone_from(&config.output_device);
                self.state.volume = config.volume;
                if self.state.shuffle != config.shuffle {
                    self.shuffle_bag.clear();
                    self.played.clear();
                    self.played_cursor = 0;
                }
                self.state.shuffle = config.shuffle;
                self.state.repeat = config.repeat;
                self.state.config = Arc::new(config);
            }
            Command::MprisStatus { status } => {
                self.state.mpris_status = status;
                return Ok(());
            }
            Command::SeekQueue { queue_id, seconds } => {
                if self.state.current_queue_id == Some(queue_id) {
                    self.seek(seconds)?;
                }
                return Ok(());
            }
            Command::OptimizeDatabase => {
                self.state.database_optimization = None;
                if self.state.status != PlaybackStatus::Stopped || self.state.scanning {
                    bail!("Database optimization requires stopped playback and no active scan");
                }
                self.state.database_optimization = Some(self.store.optimize()?);
                // Saving playback here would immediately create new WAL pages
                // after maintenance has truncated them.
                return Ok(());
            }
            Command::Shutdown => {
                self.state.shutting_down = true;
                self.stop()?;
            }
        }
        self.save()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{MediaInfo, ScanRecord};

    // An idle real audio worker handles stop/snapshot, but these tests never load
    // audio. Missing-file errors expose the selected entry without opening CPAL.
    fn fixture() -> (tempfile::TempDir, Core) {
        fixture_with_duration(Some(10.0))
    }

    fn fixture_with_duration(duration: Option<f64>) -> (tempfile::TempDir, Core) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(Path::new(":memory:")).unwrap();
        store
            .apply_scan(&ScanResult {
                roots: vec![directory.path().to_path_buf()],
                suppressed_sources: Vec::new(),
                records: (1..=4)
                    .map(|id| ScanRecord {
                        path: directory.path().join(format!("{id}.wav")),
                        cue: None,
                        size: 1,
                        modified_ns: 1,
                        fingerprint: None,
                        media: MediaInfo {
                            title: format!("Track {id}"),
                            artist: String::new(),
                            album: String::new(),
                            duration,
                            codec: "pcm".into(),
                            channels: 2,
                            sample_rate: 48000,
                            bitrate_bps: None,
                            track_number: None,
                            disc_number: None,
                            bits_per_sample: None,
                            release_date: None,
                        },
                    })
                    .collect(),
                errors: Vec::new(),
            })
            .unwrap();
        let state = AppState {
            library: Arc::new(store.tracks().unwrap()),
            config_path: directory.path().join("config.toml"),
            ..AppState::default()
        };
        let (scan_tx, scan_rx) = bounded(1);
        let mut core = Core {
            store,
            engine: AudioEngine::new(1).unwrap(),
            shared: Arc::new(RwLock::new(state.clone())),
            state,
            wakeup: Arc::new(RwLock::new(None)),
            subscribers: Arc::new(RwLock::new(Vec::new())),
            generation: 1,
            next_queue_id: 1,
            playback: None,
            scan_tx,
            scan_rx,
            scan_workers: Vec::new(),
            shuffle_bag: Vec::new(),
            played: Vec::new(),
            played_cursor: 0,
        };
        core.enqueue(&[1, 2, 2, 3, 4]).unwrap();
        (directory, core)
    }

    fn pending(core: &mut Core, queue_id: u64) {
        core.state.current_queue_id = Some(queue_id);
        core.state.status = PlaybackStatus::Playing;
        let (track_id, duration) = {
            let track = core.state.current_track().unwrap();
            (track.id, track.duration)
        };
        core.state.duration = duration;
        core.playback = Some(PlaybackStats {
            track_id,
            heard: 0.0,
            position: None,
            started: false,
            counted: false,
        });
    }

    fn started_event(core: &Core, duration: Option<f64>) -> AudioEvent {
        let track = core.state.current_track().unwrap();
        AudioEvent::Started {
            generation: core.generation,
            info: MediaInfo {
                title: track.title.clone(),
                artist: track.artist.clone(),
                album: track.album.clone(),
                duration,
                codec: track.codec.clone(),
                channels: track.channels,
                sample_rate: track.sample_rate,
                bitrate_bps: None,
                track_number: None,
                disc_number: None,
                bits_per_sample: None,
                release_date: None,
            },
        }
    }

    fn started(core: &mut Core, duration: Option<f64>) -> Result<()> {
        core.audio_event(started_event(core, duration))
    }

    fn playing(core: &mut Core, queue_id: u64) {
        pending(core, queue_id);
        started(core, Some(10.0)).unwrap();
    }

    fn progress(core: &mut Core, position_seconds: f64, listened_seconds: f64) {
        core.audio_event(AudioEvent::Progress {
            generation: core.generation,
            position_seconds,
            listened_seconds,
        })
        .unwrap();
    }

    fn play_count(core: &Core, track_id: i64) -> u64 {
        core.store
            .tracks()
            .unwrap()
            .iter()
            .find(|track| track.id == track_id)
            .unwrap()
            .play_count
    }

    fn missing(core: &mut Core, command: Command, track_id: i64) {
        let path = core.track(track_id).unwrap().path.clone();
        let error = core.command(command).unwrap_err();
        assert!(format!("{error:#}").contains(&path.display().to_string()));
    }

    #[test]
    fn successful_start_records_recent_once_and_twenty_percent_is_strict() {
        let (_directory, mut core) = fixture();
        pending(&mut core, 1);
        assert!(core.store.history(200).unwrap().is_empty());
        started(&mut core, Some(10.0)).unwrap();
        assert_eq!(core.store.history(200).unwrap()[0].track_id, 1);
        assert_eq!(play_count(&core, 1), 0);

        // A duplicate Started must not refresh last-played a second time.
        core.store.mark_played(1, 7).unwrap();
        started(&mut core, Some(10.0)).unwrap();
        assert_eq!(core.store.history(200).unwrap()[0].played_at, 7);
        progress(&mut core, 9.0, 2.0);
        progress(&mut core, 9.0, f64::NAN);
        progress(&mut core, 9.0, f64::INFINITY);
        assert_eq!(play_count(&core, 1), 0);
        progress(&mut core, 9.0, 2.000_001);
        assert_eq!(play_count(&core, 1), 1);
    }

    #[test]
    fn configured_threshold_controls_counting_including_zero_percent() {
        for percent in [0.0, 50.0, 99.0] {
            let (_directory, mut core) = fixture();
            Arc::make_mut(&mut core.state.config).play_count_threshold_percent = percent;
            playing(&mut core, 1);
            let boundary = 10.0 * percent / 100.0;
            progress(&mut core, 10.0, boundary);
            assert_eq!(play_count(&core, 1), 0);
            progress(&mut core, 10.0, boundary + 0.000_001);
            assert_eq!(play_count(&core, 1), 1);
        }
    }

    #[test]
    fn repeated_progress_pause_seek_and_resume_count_only_once() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 1);
        core.store.mark_played(1, 7).unwrap();
        progress(&mut core, 1.0, 1.0);
        core.command(Command::Pause).unwrap();
        progress(&mut core, 1.0, 1.0);
        core.command(Command::Seek { seconds: 9.0 }).unwrap();
        assert_eq!(play_count(&core, 1), 0);
        core.command(Command::Resume).unwrap();
        progress(&mut core, 9.5, 1.5);
        core.command(Command::Seek { seconds: 0.0 }).unwrap();
        progress(&mut core, 0.5, 2.0);
        assert_eq!(play_count(&core, 1), 0);
        progress(&mut core, 1.0, 2.5);
        for _ in 0..3 {
            core.command(Command::Pause).unwrap();
            core.command(Command::Resume).unwrap();
            core.command(Command::Seek { seconds: 5.0 }).unwrap();
            progress(&mut core, 5.0, 2.0);
            progress(&mut core, 8.0, 8.0);
        }
        core.stop().unwrap();
        assert_eq!(play_count(&core, 1), 1);
        assert_eq!(core.store.history(200).unwrap()[0].played_at, 7);
    }

    #[test]
    fn failed_load_and_stale_events_neither_count_nor_record_recent() {
        let (_directory, mut core) = fixture();
        pending(&mut core, 1);
        let generation = core.generation;
        let track = core.state.current_track().unwrap();
        core.audio_event(AudioEvent::Started {
            generation: generation.wrapping_sub(1),
            info: MediaInfo {
                title: track.title.clone(),
                artist: track.artist.clone(),
                album: track.album.clone(),
                duration: Some(10.0),
                codec: track.codec.clone(),
                channels: track.channels,
                sample_rate: track.sample_rate,
                bitrate_bps: None,
                track_number: None,
                disc_number: None,
                bits_per_sample: None,
                release_date: None,
            },
        })
        .unwrap();
        progress(&mut core, 10.0, 10.0);
        core.audio_event(AudioEvent::Failed {
            generation,
            message: "Decoder failed".into(),
        })
        .unwrap();
        assert_eq!(core.state.status, PlaybackStatus::Stopped);
        assert_eq!(core.state.last_error.as_deref(), Some("Decoder failed"));
        assert_ne!(core.generation, generation);
        core.audio_event(AudioEvent::Progress {
            generation,
            position_seconds: 10.0,
            listened_seconds: 10.0,
        })
        .unwrap();
        assert_eq!(core.state.position, 0.0);
        assert_eq!(play_count(&core, 1), 0);
        assert!(core.store.history(200).unwrap().is_empty());
    }

    #[test]
    fn final_stop_snapshot_counts_only_matching_generation_above_boundary() {
        for (heard, stale, expected) in [(2.0, false, 0), (2.01, false, 1), (10.0, true, 0)] {
            let (_directory, mut core) = fixture();
            playing(&mut core, 1);
            progress(&mut core, 2.0, 2.0);
            let generation = core.generation;
            let snapshot_generation = if stale { generation - 1 } else { generation };
            core.finish_playback(Ok(Some((snapshot_generation, heard))), false)
                .unwrap();
            assert_eq!(play_count(&core, 1), expected);
            assert_eq!(core.state.status, PlaybackStatus::Stopped);
            assert_ne!(core.generation, generation);
            assert!(core.playback.is_none());
            core.audio_event(AudioEvent::Progress {
                generation,
                position_seconds: 10.0,
                listened_seconds: 10.0,
            })
            .unwrap();
            assert_eq!(play_count(&core, 1), expected);
        }
    }

    #[test]
    fn stop_ack_records_queued_start_and_snapshot_without_terminal_navigation() {
        for (heard, expected) in [(2.0, 0), (2.01, 1)] {
            let (_directory, mut core) = fixture();
            pending(&mut core, 1);
            let generation = core.generation;
            let (sender, events) = bounded(8);
            core.engine.events = events;
            sender.send(started_event(&core, Some(10.0))).unwrap();
            sender.send(started_event(&core, Some(10.0))).unwrap();
            sender
                .send(AudioEvent::Progress {
                    generation,
                    position_seconds: 9.0,
                    listened_seconds: 2.0,
                })
                .unwrap();
            sender
                .send(AudioEvent::Progress {
                    generation: generation - 1,
                    position_seconds: 10.0,
                    listened_seconds: 10.0,
                })
                .unwrap();
            sender.send(AudioEvent::Ended { generation }).unwrap();
            sender
                .send(AudioEvent::Failed {
                    generation,
                    message: "Retired failure".into(),
                })
                .unwrap();
            core.finish_playback(Ok(Some((generation, heard))), false)
                .unwrap();
            assert_eq!(play_count(&core, 1), expected);
            assert_eq!(core.store.history(200).unwrap().len(), 1);
            assert_eq!(core.store.history(200).unwrap()[0].track_id, 1);
            assert_eq!(core.state.status, PlaybackStatus::Stopped);
            assert_eq!(core.state.current_queue_id, Some(1));
            assert_eq!(core.state.position, 0.0);
            assert!(core.state.last_error.is_none());
            assert_eq!(core.generation, generation.wrapping_add(1));
            assert!(core.playback.is_none());
        }
    }

    #[test]
    fn queued_start_rejection_fences_once_without_recursive_stop_or_count() {
        let (_directory, mut core) = fixture();
        pending(&mut core, 1);
        let generation = core.generation;
        let (sender, events) = bounded(4);
        core.engine.events = events;
        sender.send(started_event(&core, Some(10.0))).unwrap();
        sender
            .send(AudioEvent::Progress {
                generation,
                position_seconds: 10.0,
                listened_seconds: 10.0,
            })
            .unwrap();
        sender.send(AudioEvent::Ended { generation }).unwrap();
        core.store.remove_tracks(&[1]).unwrap();
        assert!(
            core.finish_playback(Ok(Some((generation, 10.0))), false)
                .is_err()
        );
        assert_eq!(core.generation, generation.wrapping_add(1));
        assert_eq!(core.state.status, PlaybackStatus::Stopped);
        assert!(core.playback.is_none());
        while let Ok(event) = core.engine.events.try_recv() {
            core.audio_event(event).unwrap();
        }
        assert!(core.store.history(200).unwrap().is_empty());
        assert_eq!(play_count(&core, 2), 0);
    }

    #[test]
    fn database_optimization_requires_stopped_playback_and_no_scan() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 1);
        core.state.database_optimization = Some(DatabaseOptimization {
            database_bytes_before: 1,
            database_bytes_after: 1,
            wal_bytes_before: 1,
            wal_bytes_after: 0,
        });
        assert!(core.command(Command::OptimizeDatabase).is_err());
        assert!(core.state.database_optimization.is_none());
        core.command(Command::Pause).unwrap();
        assert!(core.command(Command::OptimizeDatabase).is_err());
        core.stop().unwrap();
        core.state.scanning = true;
        assert!(core.command(Command::OptimizeDatabase).is_err());
        core.state.scanning = false;
        let saved = core.store.get_setting("playback").unwrap();
        core.enqueue(&[4]).unwrap();
        core.command(Command::OptimizeDatabase).unwrap();
        assert!(core.state.database_optimization.is_some());
        assert_eq!(core.store.get_setting("playback").unwrap(), saved);
        assert_eq!(core.store.history(200).unwrap()[0].track_id, 1);
        assert_eq!(play_count(&core, 1), 0);
    }

    #[test]
    fn favorite_updates_preserve_counts_and_recent_history() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 1);
        progress(&mut core, 3.0, 3.0);
        let played_at = core.store.history(200).unwrap()[0].played_at;
        core.command(Command::SetFavorite {
            track_ids: vec![1, 2],
            favorite: true,
        })
        .unwrap();
        assert!(core.track(1).unwrap().favorite);
        assert!(core.track(2).unwrap().favorite);
        assert!(!core.track(3).unwrap().favorite);
        core.command(Command::SetFavorite {
            track_ids: vec![1],
            favorite: false,
        })
        .unwrap();
        assert!(!core.track(1).unwrap().favorite);
        assert!(core.track(2).unwrap().favorite);
        assert_eq!(play_count(&core, 1), 1);
        assert_eq!(core.store.history(200).unwrap()[0].played_at, played_at);
        assert_eq!(core.state.status, PlaybackStatus::Playing);
    }

    #[test]
    fn removing_missing_tracks_reuses_queue_playlist_and_recent_cleanup_without_disk_deletes() {
        let (_directory, mut core) = fixture();
        let kept_path = core.track(1).unwrap().path.clone();
        std::fs::write(&kept_path, b"Do not delete").unwrap();
        let playlist_id = core.store.create_playlist("Mixed availability").unwrap();
        core.store.add_playlist(playlist_id, &[1, 2]).unwrap();
        core.store
            .apply_scan(&ScanResult {
                roots: vec![core.track(2).unwrap().path.clone()],
                ..ScanResult::default()
            })
            .unwrap();
        core.reload().unwrap();
        playing(&mut core, 2);
        core.played = vec![1, 2, 3];
        core.played_cursor = 2;
        core.command(Command::RemoveMissingTracks).unwrap();
        assert_eq!(
            core.state
                .library
                .iter()
                .map(|track| track.id)
                .collect::<Vec<_>>(),
            [1, 3, 4]
        );
        assert_eq!(
            core.state
                .queue
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            [1, 4, 5]
        );
        assert_eq!(
            core.state.playlists[0]
                .entries
                .iter()
                .map(|entry| entry.track_id)
                .collect::<Vec<_>>(),
            [1]
        );
        assert_eq!(core.played, [1]);
        assert_eq!(core.played_cursor, 1);
        assert_eq!(core.state.current_queue_id, None);
        assert_eq!(core.state.status, PlaybackStatus::Stopped);
        assert!(core.state.history.is_empty());
        assert_eq!(std::fs::read(&kept_path).unwrap(), b"Do not delete");
    }

    #[test]
    fn forced_scan_reprobes_unchanged_files_instead_of_reusing_cached_metadata() {
        use std::io::Write;
        let (_directory, mut core) = fixture();
        let path = core.track(1).unwrap().path.clone();
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(b"RIFF").unwrap();
        file.write_all(&236_u32.to_le_bytes()).unwrap();
        file.write_all(b"WAVEfmt ").unwrap();
        file.write_all(&16_u32.to_le_bytes()).unwrap();
        file.write_all(&1_u16.to_le_bytes()).unwrap();
        file.write_all(&1_u16.to_le_bytes()).unwrap();
        file.write_all(&7500_u32.to_le_bytes()).unwrap();
        file.write_all(&15000_u32.to_le_bytes()).unwrap();
        file.write_all(&2_u16.to_le_bytes()).unwrap();
        file.write_all(&16_u16.to_le_bytes()).unwrap();
        file.write_all(b"data").unwrap();
        file.write_all(&200_u32.to_le_bytes()).unwrap();
        file.write_all(&[0; 200]).unwrap();
        drop(file);
        let mut scanned = library::scan_paths(&[path.clone()], &[]).unwrap();
        let probed_title = scanned.records[0].media.title.clone();
        scanned.records[0].media.title = "Cached metadata".into();
        core.store.apply_scan(&scanned).unwrap();
        core.reload().unwrap();
        for (force, expected) in [(false, "Cached metadata"), (true, probed_title.as_str())] {
            core.command(Command::Scan {
                paths: vec![path.clone()],
                force,
            })
            .unwrap();
            let finished = core.scan_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            core.finish_scan(finished).unwrap();
            core.state.scanning = false;
            assert_eq!(core.track(1).unwrap().title, expected);
        }
    }

    #[test]
    fn stale_events_cannot_count_or_stop_new_playback() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 1);
        let generation = core.generation;
        core.stop().unwrap();
        playing(&mut core, 2);
        core.audio_event(AudioEvent::Progress {
            generation,
            position_seconds: 10.0,
            listened_seconds: 10.0,
        })
        .unwrap();
        core.audio_event(AudioEvent::Ended { generation }).unwrap();
        core.audio_event(AudioEvent::Failed {
            generation,
            message: "Old failure".into(),
        })
        .unwrap();
        assert_eq!(core.state.status, PlaybackStatus::Playing);
        assert_eq!(core.state.position, 0.0);
        assert!(core.state.last_error.is_none());
        assert_eq!(play_count(&core, 1), 0);
        assert_eq!(play_count(&core, 2), 0);
        progress(&mut core, 3.0, 3.0);
        assert_eq!(play_count(&core, 2), 1);
    }

    #[test]
    fn started_uses_valid_decoder_duration_or_keeps_known_metadata_duration() {
        for duration in [
            None,
            Some(f64::NAN),
            Some(0.0),
            Some(f64::INFINITY),
            Some(5.0),
        ] {
            let (_directory, mut core) = fixture();
            pending(&mut core, 1);
            started(&mut core, duration).unwrap();
            let expected_duration = if duration == Some(5.0) { 5.0 } else { 10.0 };
            assert_eq!(core.state.duration, Some(expected_duration));
            let boundary = expected_duration * 20.0 / 100.0;
            progress(&mut core, expected_duration, boundary);
            assert_eq!(play_count(&core, 1), 0);
            progress(&mut core, expected_duration, boundary + 0.000_001);
            assert_eq!(play_count(&core, 1), 1);
        }
    }

    #[test]
    fn rejected_recent_update_stops_without_counting_the_start() {
        let (_directory, mut core) = fixture();
        pending(&mut core, 1);
        let generation = core.generation;
        core.store.remove_tracks(&[1]).unwrap();
        assert!(started(&mut core, Some(10.0)).is_err());
        assert_eq!(core.state.status, PlaybackStatus::Stopped);
        assert_ne!(core.generation, generation);
        assert!(core.playback.is_none());
        assert!(core.store.history(200).unwrap().is_empty());
        assert_eq!(play_count(&core, 2), 0);
    }

    #[test]
    fn unknown_duration_waits_for_natural_eof_and_uses_confirmed_position() {
        for (heard, expected) in [(2.0, 0), (2.01, 1), (8.0, 1)] {
            let (_directory, mut core) = fixture_with_duration(None);
            pending(&mut core, 1);
            started(&mut core, None).unwrap();
            progress(&mut core, 10.0, heard);
            assert_eq!(play_count(&core, 1), 0);
            let generation = core.generation;
            assert!(core.audio_event(AudioEvent::Ended { generation }).is_err());
            assert_eq!(play_count(&core, 1), expected);
            assert_eq!(core.state.duration, Some(10.0));
            assert_eq!(core.state.status, PlaybackStatus::Stopped);
        }
    }

    #[test]
    fn unknown_duration_never_uses_seek_targets_heard_time_or_manual_stop() {
        let (_directory, mut core) = fixture_with_duration(None);
        pending(&mut core, 1);
        started(&mut core, None).unwrap();
        core.command(Command::Seek { seconds: 10.0 }).unwrap();
        progress(&mut core, f64::NAN, 8.0);
        let generation = core.generation;
        assert!(core.audio_event(AudioEvent::Ended { generation }).is_err());
        assert_eq!(play_count(&core, 1), 0);
        assert_eq!(core.state.duration, None);

        pending(&mut core, 1);
        started(&mut core, None).unwrap();
        progress(&mut core, 10.0, 8.0);
        core.stop().unwrap();
        assert_eq!(play_count(&core, 1), 0);
    }

    #[test]
    fn m3u_playlist_deduplicates_in_first_occurrence_order_without_deduplicating_queue() {
        let (directory, mut core) = fixture();
        for track_id in [1, 2, 3] {
            std::fs::write(&core.track(track_id).unwrap().path, []).unwrap();
        }
        let path = directory.path().join("duplicates.m3u");
        std::fs::write(&path, "2.wav\n1.wav\n2.wav\n3.wav\n1.wav\n").unwrap();
        core.finish_scan(ScanFinished {
            result: Ok(ScanResult::default()),
            import: Some((
                "First occurrences".into(),
                library::import_playlist(&path).unwrap(),
            )),
        })
        .unwrap();
        assert_eq!(
            core.state.playlists[0]
                .entries
                .iter()
                .map(|entry| entry.track_id)
                .collect::<Vec<_>>(),
            [2, 1, 3]
        );
        core.command(Command::Enqueue {
            track_ids: vec![2, 1, 2, 3, 1],
        })
        .unwrap();
        assert_eq!(
            core.state
                .queue
                .iter()
                .skip(5)
                .map(|entry| entry.track_id)
                .collect::<Vec<_>>(),
            [2, 1, 2, 3, 1]
        );
    }

    #[test]
    fn cue_playlist_import_deduplicates_segments_in_first_occurrence_order() {
        let (directory, mut core) = fixture();
        let sheet = directory.path().join("album.cue");
        std::fs::write(&sheet, "FILE album.wav WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 00:05:00\n").unwrap();
        let items = library::import_playlist(&sheet).unwrap();
        let records = (1..=2)
            .map(|number| ScanRecord {
                path: directory.path().join("album.wav"),
                cue: Some(CueSegment {
                    sheet: sheet.clone(),
                    number,
                    start_frame: u64::from(number - 1) * 375,
                    end_frame: if number == 1 { Some(375) } else { None },
                }),
                size: 1,
                modified_ns: 1,
                fingerprint: None,
                media: MediaInfo {
                    title: format!("Part {number}"),
                    artist: String::new(),
                    album: "Album".into(),
                    duration: Some(5.0),
                    codec: "pcm".into(),
                    channels: 2,
                    sample_rate: 48_000,
                    bitrate_bps: None,
                    track_number: None,
                    disc_number: None,
                    bits_per_sample: None,
                    release_date: None,
                },
            })
            .collect();
        core.finish_scan(ScanFinished {
            result: Ok(ScanResult {
                roots: vec![sheet],
                records,
                ..ScanResult::default()
            }),
            import: Some((
                "CUE occurrences".into(),
                vec![items[1].clone(), items[0].clone(), items[1].clone()],
            )),
        })
        .unwrap();
        let playlist = &core.state.playlists[0];
        let tracks: Vec<_> = playlist
            .entries
            .iter()
            .map(|entry| core.track(entry.track_id).unwrap())
            .collect();
        assert_eq!(
            tracks
                .iter()
                .map(|track| track.title.as_str())
                .collect::<Vec<_>>(),
            ["Part 2", "Part 1"]
        );
        assert_ne!(tracks[0].id, tracks[1].id);
        assert_ne!(playlist.entries[0].id, playlist.entries[1].id);
    }

    #[test]
    fn ended_missing_next_stops_and_resume_attempts_a_load() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 1);
        core.state.position = 10.0;
        let generation = core.generation;
        let error = core
            .audio_event(AudioEvent::Ended { generation })
            .unwrap_err();
        assert!(error.to_string().contains("2.wav"));
        assert_eq!(core.state.status, PlaybackStatus::Stopped);
        assert_eq!(core.state.position, 0.0);
        assert!(core.playback.is_none());
        assert_eq!(core.store.history(1).unwrap()[0].track_id, 1);
        assert_ne!(core.generation, generation);
        core.audio_event(AudioEvent::Progress {
            generation,
            position_seconds: 10.0,
            listened_seconds: 10.0,
        })
        .unwrap();
        assert_eq!(core.state.position, 0.0);
        // Resume must try to reopen the selected track, not send Pause(false)
        // to an empty worker and falsely report playing.
        missing(&mut core, Command::Resume, 1);
        assert_eq!(core.state.status, PlaybackStatus::Stopped);
    }

    #[test]
    fn missing_manual_next_preserves_current_playback() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 1);
        core.state.position = 4.0;
        let generation = core.generation;
        let played_at = core.store.history(1).unwrap()[0].played_at;
        missing(&mut core, Command::Next, 2);
        assert_eq!(core.state.status, PlaybackStatus::Playing);
        assert_eq!(core.state.current_queue_id, Some(1));
        assert_eq!(core.state.position, 4.0);
        assert_eq!(core.generation, generation);
        assert_eq!(core.playback.as_ref().unwrap().track_id, 1);
        assert_eq!(core.store.history(1).unwrap()[0].played_at, played_at);
    }

    #[test]
    fn count_save_failure_still_stops_and_preserves_audio_error() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 1);
        let playback = core.playback.as_mut().unwrap();
        playback.track_id = i64::MAX;
        playback.heard = 3.0;
        let generation = core.generation;
        let error = core
            .audio_event(AudioEvent::Failed {
                generation,
                message: "Decoder failed".into(),
            })
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("Decoder failed"));
        assert!(message.contains("does not exist"));
        assert_eq!(core.state.status, PlaybackStatus::Stopped);
        assert_ne!(core.generation, generation);
    }

    #[test]
    fn removing_tracks_prunes_unplayed_shuffle_entries_and_duplicates() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 1);
        core.state.shuffle = true;
        core.played = vec![1];
        core.played_cursor = 1;
        core.shuffle_bag = vec![5, 4, 3, 2];
        core.command(Command::RemoveTracks { track_ids: vec![2] })
            .unwrap();
        assert_eq!(core.shuffle_bag, [5, 4]);
        assert_eq!(
            core.state.queue.iter().map(|q| q.id).collect::<Vec<_>>(),
            [1, 4, 5]
        );
        missing(&mut core, Command::Next, 3);
        assert_eq!(core.shuffle_bag, [5, 4]);
        assert_eq!(core.state.current_queue_id, Some(1));
        assert_eq!(core.state.status, PlaybackStatus::Playing);
    }

    #[test]
    fn removing_history_rebases_cursor_without_consuming_failed_navigation() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 4);
        core.state.shuffle = true;
        core.played = vec![1, 2, 3, 4, 5];
        core.played_cursor = 4;
        core.command(Command::RemoveTracks { track_ids: vec![2] })
            .unwrap();
        assert_eq!(core.played, [1, 4, 5]);
        assert_eq!(core.played_cursor, 2);
        missing(&mut core, Command::Previous, 1);
        assert_eq!(core.played_cursor, 2);
        missing(&mut core, Command::Next, 4);
        assert_eq!(core.played_cursor, 2);
        core.command(Command::RemoveQueue { queue_id: 5 }).unwrap();
        core.command(Command::Next).unwrap();
        assert_eq!(core.state.status, PlaybackStatus::Stopped);
    }

    #[test]
    fn removing_current_queue_entry_keeps_other_copy_and_history_gap() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 2);
        core.state.shuffle = true;
        core.played = vec![1, 2, 3];
        core.played_cursor = 2;
        core.command(Command::RemoveQueue { queue_id: 2 }).unwrap();
        assert_eq!(core.played, [1, 3]);
        assert_eq!(core.played_cursor, 1);
        assert_eq!(core.state.current_queue_id, None);
        assert_eq!(core.state.status, PlaybackStatus::Stopped);
        assert!(
            core.state
                .queue
                .iter()
                .any(|q| q.id == 3 && q.track_id == 2)
        );
        missing(&mut core, Command::Previous, 1);
        missing(&mut core, Command::Next, 2);
        core.command(Command::ClearQueue).unwrap();
        assert!(core.played.is_empty());
        assert_eq!(core.played_cursor, 0);
        assert!(core.shuffle_bag.is_empty());
        core.command(Command::Next).unwrap();
        assert_eq!(core.state.status, PlaybackStatus::Stopped);
    }

    #[test]
    fn repeat_one_only_repeats_natural_end_and_repeat_all_revisits_singleton() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 1);
        core.state.repeat = RepeatMode::One;
        missing(&mut core, Command::Next, 2);
        let generation = core.generation;
        let error = core
            .audio_event(AudioEvent::Ended { generation })
            .unwrap_err();
        assert!(error.to_string().contains("1.wav"));
        assert_eq!(core.state.status, PlaybackStatus::Stopped);
        core.command(Command::RemoveTracks {
            track_ids: vec![2, 3, 4],
        })
        .unwrap();
        core.state.shuffle = true;
        core.state.repeat = RepeatMode::All;
        core.played = vec![1];
        core.played_cursor = 1;
        missing(&mut core, Command::Next, 1);
    }

    #[test]
    fn duplicate_queue_entries_count_as_separate_plays_but_share_recent_history() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 2);
        progress(&mut core, 3.0, 3.0);
        assert_eq!(play_count(&core, 2), 1);
        core.stop().unwrap();
        playing(&mut core, 3);
        progress(&mut core, 3.0, 3.0);
        progress(&mut core, 8.0, 8.0);
        assert_eq!(play_count(&core, 2), 2);
        let history = core.store.history(200).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].track_id, 2);
        assert_eq!(core.state.queue[1].track_id, core.state.queue[2].track_id);
        assert_ne!(core.state.queue[1].id, core.state.queue[2].id);
    }

    #[test]
    fn failed_playlist_start_does_not_keep_an_obsolete_queue_selection() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 1);
        let playlist_id = core.store.create_playlist("missing").unwrap();
        core.store.add_playlist(playlist_id, &[2]).unwrap();
        core.reload().unwrap();
        let error = core
            .command(Command::PlayPlaylist { playlist_id })
            .unwrap_err();
        assert!(error.to_string().contains("2.wav"));
        assert_eq!(core.state.status, PlaybackStatus::Stopped);
        assert_eq!(core.state.current_queue_id, None);
        missing(&mut core, Command::Resume, 2);
    }
}
