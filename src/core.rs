use crate::{
    analysis::AnalysisFrame,
    audio::{self, AudioCommand, AudioEngine, AudioEvent},
    config::Config,
    library::{self, ScanResult},
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
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
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
        rx.recv_timeout(Duration::from_secs(12))
            .unwrap_or_else(|error| Response {
                ok: false,
                error: Some(format!("Core response timed out: {error}")),
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
                    session: None,
                    stats_tick: Instant::now(),
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
struct Session {
    id: i64,
    heard: f64,
}
struct ScanFinished {
    result: Result<ScanResult>,
    import: Option<(String, Vec<PathBuf>)>,
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
    session: Option<Session>,
    stats_tick: Instant,
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
        if let Err(error) = self.finish_session("shutdown") {
            self.state.last_error = Some(format!("Saving listening session: {error:#}"));
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
        self.finish_session("changed")?;
        self.session = Some(Session {
            id: self.store.begin_session(track.id, now())?,
            heard: 0.0,
        });
        self.audio(AudioCommand::Load {
            path: track.path,
            generation: self.generation,
            start_seconds: 0.0,
            paused: false,
        })
        .or_else(|error| {
            self.finish_session("failed")
                .with_context(|| format!("{error:#}"))?;
            Err(error)
        })?;
        self.stats_tick = Instant::now();
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
    fn finish_session(&mut self, reason: &str) -> Result<()> {
        let snapshot = self.engine.stop_and_snapshot();
        let generation = self.generation;
        // Once output has been stopped, later database/load errors must not leave
        // a resumable-looking state or allow queued events to revive old playback.
        self.generation = self.generation.wrapping_add(1);
        self.state.status = PlaybackStatus::Stopped;
        self.state.position = 0.0;
        self.state.seek_revision = self.state.seek_revision.wrapping_add(1);
        let (snapshot_generation, heard) = snapshot?;
        if snapshot_generation == generation
            && let Some(session) = &mut self.session
        {
            session.heard = session.heard.max(heard);
        }
        if let Some(session) = &self.session {
            self.store
                .update_session(session.id, session.heard, Some(now()), reason)?;
            self.session = None;
            self.state.library = Arc::new(self.store.tracks()?);
            self.state.history = Arc::new(self.store.history(200)?);
        }
        Ok(())
    }
    fn stop(&mut self, reason: &str) -> Result<()> {
        self.finish_session(reason)
    }
    fn advance(&mut self, natural: bool) -> Result<()> {
        if self.state.queue.is_empty() {
            return self.stop("empty_queue");
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
        self.stop("queue_finished")
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
    fn scan(&mut self, paths: Vec<PathBuf>, import: Option<(String, Vec<PathBuf>)>) -> Result<()> {
        if self.state.scanning {
            bail!("A library scan is already running");
        }
        if paths.is_empty() {
            bail!("No files or directories supplied");
        }
        let known = self.store.known_files()?;
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
        if let Some((name, paths)) = scan.import {
            let mut ids = Vec::with_capacity(paths.len());
            let by_path: std::collections::HashMap<&Path, i64> = self
                .state
                .library
                .iter()
                .map(|track| (track.path.as_path(), track.id))
                .collect();
            for path in paths {
                if let Ok(path) = path.canonicalize()
                    && let Some(id) = by_path.get(path.as_path())
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
                self.state.duration = info.duration;
            }
            AudioEvent::Progress {
                generation,
                position_seconds,
                listened_seconds,
            } if generation == self.generation => {
                if position_seconds.is_finite() {
                    self.state.position = position_seconds;
                }
                if let Some(session) = &mut self.session {
                    if listened_seconds.is_finite() {
                        session.heard = session.heard.max(listened_seconds);
                    }
                    if self.stats_tick.elapsed() >= Duration::from_secs(1) {
                        self.store
                            .update_session(session.id, session.heard, None, "playing")?;
                        self.stats_tick = Instant::now();
                    }
                }
            }
            AudioEvent::Ended { generation } if generation == self.generation => {
                self.finish_session("completed")?;
                self.advance(true)?;
            }
            AudioEvent::Failed {
                generation,
                message,
            } if generation == self.generation => {
                self.stop("failed").with_context(|| message.clone())?;
                self.state.last_error = Some(message);
            }
            _ => {}
        }
        Ok(())
    }
    fn command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Status | Command::Overview => return Ok(()),
            Command::Scan { paths } => self.scan(paths, None)?,
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
                self.stop("playlist_changed")?;
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
            Command::Stop => self.stop("stopped")?,
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
                    self.stop("removed_from_queue")?;
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
                self.stop("queue_cleared")?;
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
                let items = library::import_m3u(&path)?;
                let paths = items.into_iter().map(|item| item.path).collect::<Vec<_>>();
                let name = name.unwrap_or_else(|| {
                    path.file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                });
                self.scan(paths.clone(), Some((name, paths)))?;
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
                    self.stop("removed_from_library")?;
                    self.state.current_queue_id = None;
                }
                self.store.remove_tracks(&track_ids)?;
                Arc::make_mut(&mut self.state.queue).retain(|q| !track_ids.contains(&q.track_id));
                self.prune_queue_history();
                self.reload()?;
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
            Command::Shutdown => {
                self.state.shutting_down = true;
                self.stop("shutdown")?;
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
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(Path::new(":memory:")).unwrap();
        store
            .apply_scan(&ScanResult {
                roots: vec![directory.path().to_path_buf()],
                records: (1..=4)
                    .map(|id| ScanRecord {
                        path: directory.path().join(format!("{id}.wav")),
                        size: 1,
                        modified_ns: 1,
                        fingerprint: None,
                        media: MediaInfo {
                            title: format!("Track {id}"),
                            artist: String::new(),
                            album: String::new(),
                            duration: Some(10.0),
                            codec: "pcm".into(),
                            channels: 2,
                            sample_rate: 48000,
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
            session: None,
            stats_tick: Instant::now(),
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

    fn playing(core: &mut Core, queue_id: u64) {
        core.state.current_queue_id = Some(queue_id);
        core.state.status = PlaybackStatus::Playing;
        let track_id = core.state.current_track().unwrap().id;
        core.session = Some(Session {
            id: core.store.begin_session(track_id, now()).unwrap(),
            heard: 2.0,
        });
    }

    fn missing(core: &mut Core, command: Command, track_id: i64) {
        let path = core.track(track_id).unwrap().path.clone();
        assert_eq!(
            core.command(command).unwrap_err().to_string(),
            format!("Missing audio file: {}", path.display())
        );
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
        assert!(core.session.is_none());
        assert_eq!(core.store.history(1).unwrap()[0].reason, "completed");
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
        let session = core.session.as_ref().unwrap().id;
        missing(&mut core, Command::Next, 2);
        assert_eq!(core.state.status, PlaybackStatus::Playing);
        assert_eq!(core.state.current_queue_id, Some(1));
        assert_eq!(core.state.position, 4.0);
        assert_eq!(core.generation, generation);
        assert_eq!(core.session.as_ref().unwrap().id, session);
        assert!(core.store.history(1).unwrap()[0].ended_at.is_none());
    }

    #[test]
    fn session_save_failure_still_stops_and_preserves_audio_error() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 1);
        core.session.as_mut().unwrap().id = i64::MAX;
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
    fn failed_session_creation_after_stopping_does_not_report_playing() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 1);
        std::fs::write(&core.track(2).unwrap().path, []).unwrap();
        // The selected track vanishes from storage before its session starts.
        core.store.remove_tracks(&[2]).unwrap();
        let error = core.command(Command::Next).unwrap_err();
        assert!(error.to_string().contains("track 2 does not exist"));
        assert_eq!(core.state.status, PlaybackStatus::Stopped);
        assert!(core.session.is_none());
        assert_eq!(core.store.history(1).unwrap()[0].reason, "changed");
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
