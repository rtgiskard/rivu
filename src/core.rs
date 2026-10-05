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
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

type Wakeup = Arc<dyn Fn() + Send + Sync>;
enum CoreResponse {
    State(Response),
    Ack(Ack),
}
struct Request {
    command: Command,
    reply: Option<Sender<CoreResponse>>,
    ack: bool,
}

#[derive(Clone)]
pub struct AppHandle {
    pub state: Arc<RwLock<AppState>>,
    pub analysis: Arc<RwLock<AnalysisFrame>>,
    pub waveform: Arc<RwLock<audio::WaveformFrame>>,
    sender: Sender<Request>,
    wakeup: Arc<RwLock<Option<Wakeup>>>,
    gui_opener: Arc<RwLock<Option<Wakeup>>>,
    subscribers: Arc<RwLock<Vec<Sender<()>>>>,
    raise_requested: Arc<AtomicBool>,
}

impl AppHandle {
    pub fn send(&self, command: Command) -> Result<()> {
        self.sender
            .try_send(Request {
                command,
                reply: None,
                ack: false,
            })
            .context("Rivu is busy or stopped")
    }
    pub fn snapshot(&self) -> AppState {
        self.state.read().clone()
    }
    pub fn set_wakeup(&self, callback: impl Fn() + Send + Sync + 'static) {
        *self.wakeup.write() = Some(Arc::new(callback));
        self.notify_subscribers();
    }
    pub fn clear_wakeup(&self) {
        *self.wakeup.write() = None;
        self.notify_subscribers();
    }
    pub fn set_gui_opener(&self, callback: impl Fn() + Send + Sync + 'static) {
        *self.gui_opener.write() = Some(Arc::new(callback));
        self.notify_subscribers();
    }
    pub fn clear_gui_opener(&self) {
        *self.gui_opener.write() = None;
        self.notify_subscribers();
    }
    pub fn can_raise(&self) -> bool {
        self.gui_opener.read().is_some()
    }
    fn notify_subscribers(&self) {
        self.subscribers.write().retain(|sender| {
            !matches!(
                sender.try_send(()),
                Err(crossbeam_channel::TrySendError::Disconnected(_))
            )
        });
    }
    pub fn subscribe(&self) -> Receiver<()> {
        let (sender, receiver) = bounded(1);
        self.subscribers.write().push(sender);
        receiver
    }
    pub fn raise(&self) -> Result<()> {
        let opener = self.gui_opener.read();
        let callback = opener
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Rivu has no graphical host"))?;
        self.raise_requested.store(true, Ordering::Release);
        callback();
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
                ack: false,
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
        match response.unwrap_or_else(|error| {
            CoreResponse::State(Response {
                ok: false,
                error: Some(format!("Core response unavailable: {error}")),
                state: self.snapshot(),
            })
        }) {
            CoreResponse::State(response) => response,
            CoreResponse::Ack(_) => unreachable!("state request returned Ack"),
        }
    }

    pub fn request_ack(&self, command: Command) -> Ack {
        let (tx, rx) = bounded(1);
        if let Err(error) = self.sender.send_timeout(
            Request {
                command,
                reply: Some(tx),
                ack: true,
            },
            Duration::from_secs(2),
        ) {
            return Ack {
                ok: false,
                error: Some(format!("Core unavailable: {error}")),
                revision: self.snapshot().revision,
            };
        }
        match rx.recv_timeout(Duration::from_secs(12)) {
            Ok(CoreResponse::Ack(ack)) => ack,
            Ok(CoreResponse::State(_)) => Ack {
                ok: false,
                error: Some("Core returned state for Ack request".into()),
                revision: self.snapshot().revision,
            },
            Err(error) => Ack {
                ok: false,
                error: Some(format!("Core response unavailable: {error}")),
                revision: self.snapshot().revision,
            },
        }
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
        let gui_opener = Arc::new(RwLock::new(None));
        let subscribers = Arc::new(RwLock::new(Vec::new()));
        let ffmpeg_status = if config.ffmpeg_enabled {
            audio::ffmpeg_status().unwrap_or_else(|error| format!("unavailable: {error:#}"))
        } else {
            "disabled".to_owned()
        };
        let handle = AppHandle {
            state: shared.clone(),
            analysis: engine.analysis.clone(),
            waveform: Arc::clone(&engine.waveform),
            sender,
            wakeup: wakeup.clone(),
            gui_opener: gui_opener.clone(),
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
            ffmpeg_status,
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
        let raise_requested = handle.raise_requested.clone();
        let worker = thread::Builder::new()
            .name("rivu-core".into())
            .spawn(move || {
                let (scan_tx, scan_rx) = bounded(1);
                let next_queue_id = initial.queue.iter().map(|q| q.id).max().unwrap_or(0) + 1;
                let mut core = Core {
                    library: library::LibraryState::new(initial.library.as_ref().clone()),
                    store,
                    engine,
                    state: initial,
                    shared,
                    wakeup,
                    gui_opener,
                    subscribers,
                    raise_requested,
                    generation: 0,
                    next_queue_id,
                    playback: None,
                    scan_tx,
                    scan_rx,
                    scan_workers: Vec::new(),
                    shuffle_bag: Vec::new(),
                    played: Vec::new(),
                    played_cursor: 0,
                    queue_dirty: false,
                    config_dirty: false,
                    config_last_saved: Instant::now(),
                };
                let _ = core
                    .engine
                    .commands
                    .send(AudioCommand::Volume(core.state.volume));
                let _ = core.engine.commands.send(AudioCommand::FfmpegEnabled(
                    core.state.config.ffmpeg_enabled,
                ));
                let _ = core.engine.commands.send(AudioCommand::OutputSettings {
                    device: core.state.selected_device.clone(),
                    auto_mix: core.state.config.pipewire_auto_mix,
                });
                let _ = core.engine.commands.send(AudioCommand::AnalysisSettings(
                    core.state.config.as_ref().into(),
                ));
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
    pub fn join(&mut self) -> thread::Result<()> {
        if let Some(worker) = self.worker.take() {
            worker.join()
        } else {
            Ok(())
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
                ack: false,
            });
        }
        let _ = self.join();
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
    library: library::LibraryState,
    shared: Arc<RwLock<AppState>>,
    wakeup: Arc<RwLock<Option<Wakeup>>>,
    gui_opener: Arc<RwLock<Option<Wakeup>>>,
    subscribers: Arc<RwLock<Vec<Sender<()>>>>,
    raise_requested: Arc<AtomicBool>,
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
    queue_dirty: bool,
    config_dirty: bool,
    config_last_saved: Instant,
}
include!("core_impl.rs");

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
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
            library: library::LibraryState::new(state.library.as_ref().clone()),
            store,
            engine: AudioEngine::new(1).unwrap(),
            shared: Arc::new(RwLock::new(state.clone())),
            state,
            wakeup: Arc::new(RwLock::new(None)),
            gui_opener: Arc::new(RwLock::new(None)),
            subscribers: Arc::new(RwLock::new(Vec::new())),
            raise_requested: Arc::new(AtomicBool::new(false)),
            generation: 1,
            next_queue_id: 1,
            playback: None,
            scan_tx,
            scan_rx,
            scan_workers: Vec::new(),
            shuffle_bag: Vec::new(),
            played: Vec::new(),
            played_cursor: 0,
            queue_dirty: false,
            config_dirty: false,
            config_last_saved: Instant::now(),
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
            sender
                .send(AudioEvent::DecoderStopped { generation })
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
        core.reload(true).unwrap();
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
        let mut scanned = library::scan_paths(std::slice::from_ref(&path), &[], false).unwrap();
        let probed_title = scanned.records[0].media.title.clone();
        scanned.records[0].media.title = "Cached metadata".into();
        core.store.apply_scan(&scanned).unwrap();
        core.reload(true).unwrap();
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
    fn stopped_decoder_final_progress_counts_once_and_fences_late_events() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 1);
        progress(&mut core, 2.0, 2.0);
        progress(&mut core, 2.1, 2.1);
        let generation = core.generation;
        core.audio_event(AudioEvent::DecoderStopped { generation })
            .unwrap();
        core.audio_event(AudioEvent::Progress {
            generation,
            position_seconds: 10.0,
            listened_seconds: 10.0,
        })
        .unwrap();
        assert_eq!(core.state.status, PlaybackStatus::Stopped);
        assert_eq!(core.state.position, 0.0);
        assert!(core.playback.is_none());
        assert_eq!(play_count(&core, 1), 1);
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

    fn queue_entries(queue: &[QueueEntry]) -> Vec<(u64, i64)> {
        queue
            .iter()
            .map(|entry| (entry.id, entry.track_id))
            .collect()
    }

    fn assert_queue_saved(core: &Core) {
        let saved: Saved =
            serde_json::from_str(&core.store.get_setting("playback").unwrap().unwrap()).unwrap();
        assert_eq!(
            queue_entries(&saved.queue),
            queue_entries(&core.state.queue)
        );
        assert_eq!(saved.current, core.state.current_queue_id);
    }

    #[test]
    fn deduplicate_queue_keeps_current_duplicate_at_first_occurrence_and_prunes_history() {
        let (_directory, mut core) = fixture();
        // Separate the duplicates to exercise first-occurrence ordering.
        core.command(Command::MoveQueue {
            queue_id: 3,
            index: 4,
        })
        .unwrap();
        playing(&mut core, 3);
        progress(&mut core, 1.5, 1.5);
        core.state.shuffle = true;
        core.played = vec![1, 2, 4, 3, 2, 5];
        core.played_cursor = 4;
        core.shuffle_bag = vec![5, 2, 4];
        let generation = core.generation;
        let seek_revision = core.state.seek_revision;
        let next_queue_id = core.next_queue_id;
        let history = core.store.history(200).unwrap();
        let old_queue = Arc::clone(&core.state.queue);

        core.command(Command::DeduplicateQueue).unwrap();

        assert_eq!(
            queue_entries(&core.state.queue),
            [(1, 1), (3, 2), (4, 3), (5, 4)]
        );
        assert_eq!(
            queue_entries(&old_queue),
            [(1, 1), (2, 2), (4, 3), (5, 4), (3, 2)]
        );
        assert_eq!(core.played, [1, 4, 3, 5]);
        assert_eq!(core.played_cursor, 3);
        assert_eq!(core.shuffle_bag, [5, 4]);
        assert!(core.state.shuffle);
        assert_eq!(core.state.current_queue_id, Some(3));
        assert_eq!(core.state.status, PlaybackStatus::Playing);
        assert_eq!(core.state.position, 1.5);
        assert_eq!(core.state.duration, Some(10.0));
        assert_eq!(core.state.seek_revision, seek_revision);
        assert_eq!(core.generation, generation);
        assert_eq!(core.next_queue_id, next_queue_id);
        let playback = core.playback.as_ref().unwrap();
        assert_eq!(playback.track_id, 2);
        assert_eq!(playback.heard, 1.5);
        assert_eq!(playback.position, Some(1.5));
        assert!(playback.started);
        assert!(!playback.counted);
        assert_eq!(
            core.store.history(200).unwrap()[0].played_at,
            history[0].played_at
        );
        assert_queue_saved(&core);
        // Navigation still uses the retained history, including the forward gap.
        missing(&mut core, Command::Previous, 3);
        missing(&mut core, Command::Next, 4);
        assert_eq!(core.played_cursor, 3);
    }

    #[test]
    fn deduplicate_queue_preserves_first_entries_when_current_is_not_a_later_duplicate() {
        for current in [None, Some(2), Some(4)] {
            let (_directory, mut core) = fixture();
            core.state.current_queue_id = current;
            core.played = vec![1, 2, 3, 4, 5];
            core.played_cursor = 2;
            core.shuffle_bag = vec![5, 4, 3, 2];
            core.command(Command::DeduplicateQueue).unwrap();
            assert_eq!(
                queue_entries(&core.state.queue),
                [(1, 1), (2, 2), (4, 3), (5, 4)]
            );
            assert_eq!(core.state.current_queue_id, current);
            assert_eq!(core.played, [1, 2, 4, 5]);
            assert_eq!(core.played_cursor, 2);
            assert_eq!(core.shuffle_bag, [5, 4, 2]);
            assert_queue_saved(&core);
            core.command(Command::DeduplicateQueue).unwrap();
            assert_eq!(
                queue_entries(&core.state.queue),
                [(1, 1), (2, 2), (4, 3), (5, 4)]
            );
            assert_eq!(core.played_cursor, 2);
        }
    }

    #[test]
    fn randomize_queue_reorders_entries_without_changing_playback_or_shuffle_history() {
        use rand::SeedableRng;

        let (_directory, mut core) = fixture();
        core.enqueue(&[1, 2, 3, 4].repeat(8)).unwrap();
        playing(&mut core, 3);
        progress(&mut core, 1.5, 1.5);
        core.played = vec![1, 2, 3, 4];
        core.played_cursor = 3;
        core.shuffle_bag = vec![5, 4];
        let generation = core.generation;
        let seek_revision = core.state.seek_revision;
        let next_queue_id = core.next_queue_id;
        let original = Arc::clone(&core.state.queue);
        let mut expected = queue_entries(&original);

        core.randomize_queue(&mut rand::rngs::StdRng::seed_from_u64(42));
        assert_ne!(queue_entries(&core.state.queue), expected);
        let mut reordered = queue_entries(&core.state.queue);
        reordered.sort_unstable();
        expected.sort_unstable();
        assert_eq!(reordered, expected);

        for shuffle in [false, true] {
            core.state.shuffle = shuffle;
            core.command(Command::RandomizeQueue).unwrap();
            let mut reordered = queue_entries(&core.state.queue);
            reordered.sort_unstable();
            assert_eq!(reordered, expected);
            assert_eq!(core.state.shuffle, shuffle);
            assert_eq!(core.played, [1, 2, 3, 4]);
            assert_eq!(core.played_cursor, 3);
            assert_eq!(core.shuffle_bag, [5, 4]);
            assert_eq!(core.state.current_queue_id, Some(3));
            assert_eq!(core.state.status, PlaybackStatus::Playing);
            assert_eq!(core.state.position, 1.5);
            assert_eq!(core.state.seek_revision, seek_revision);
            assert_eq!(core.generation, generation);
            assert_eq!(core.next_queue_id, next_queue_id);
            assert_eq!(core.playback.as_ref().unwrap().track_id, 2);
            assert_eq!(core.playback.as_ref().unwrap().heard, 1.5);
            assert_queue_saved(&core);
        }
        assert_eq!(original.len(), 37);
        assert_eq!(original[0].id, 1);
    }

    #[test]
    fn queue_menu_commands_round_trip_and_allow_empty_or_single_entry_queues() {
        for (name, command) in [
            ("randomize_queue", Command::RandomizeQueue),
            ("deduplicate_queue", Command::DeduplicateQueue),
        ] {
            let json = serde_json::json!({ "command": name });
            assert_eq!(serde_json::to_value(&command).unwrap(), json);
            let (_directory, mut core) = fixture();
            for queue in [vec![], vec![QueueEntry { id: 7, track_id: 2 }]] {
                core.state.queue = Arc::new(queue);
                let expected = queue_entries(&core.state.queue);
                core.command(serde_json::from_value(json.clone()).unwrap())
                    .unwrap();
                assert_eq!(queue_entries(&core.state.queue), expected);
                assert_eq!(core.generation, 1);
                assert_eq!(core.state.status, PlaybackStatus::Stopped);
                assert_queue_saved(&core);
            }
        }
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
    fn moving_bulk_queue_entries_preserves_order_playback_history_and_persistence() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 3);
        progress(&mut core, 1.5, 1.5);
        core.played = vec![1, 3, 5];
        core.played_cursor = 2;
        core.shuffle_bag = vec![5, 4];
        let generation = core.generation;
        let history = core.store.history(200).unwrap();

        core.command(Command::MoveQueueEntries {
            queue_ids: vec![5, 1, 3, 1],
            index: 1,
        })
        .unwrap();

        assert_eq!(
            queue_entries(&core.state.queue),
            [(2, 2), (1, 1), (3, 2), (5, 4), (4, 3)]
        );
        assert_eq!(core.state.current_queue_id, Some(3));
        assert_eq!(core.state.status, PlaybackStatus::Playing);
        assert_eq!(core.generation, generation);
        assert_eq!(core.played, [1, 3, 5]);
        assert_eq!(core.played_cursor, 2);
        assert_eq!(core.shuffle_bag, [5, 4]);
        assert_eq!(core.state.position, 1.5);
        assert_eq!(core.playback.as_ref().unwrap().track_id, 2);
        let current_history = core.store.history(200).unwrap();
        assert_eq!(current_history.len(), history.len());
        assert_eq!(current_history[0].track_id, history[0].track_id);
        assert_eq!(current_history[0].played_at, history[0].played_at);
        assert_queue_saved(&core);

        core.command(Command::MoveQueueEntries {
            queue_ids: vec![1, 3, 5],
            index: usize::MAX,
        })
        .unwrap();
        assert_eq!(
            queue_entries(&core.state.queue),
            [(2, 2), (4, 3), (1, 1), (3, 2), (5, 4)]
        );
        assert_queue_saved(&core);
    }

    #[test]
    fn bulk_queue_invalid_ids_are_atomic_and_do_not_stop_current_playback() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 3);
        let queue = Arc::clone(&core.state.queue);
        let generation = core.generation;
        let status = core.state.status;
        let current = core.state.current_queue_id;
        let error = core
            .command(Command::MoveQueueEntries {
                queue_ids: vec![3, 99, 3],
                index: 0,
            })
            .unwrap_err();
        assert!(error.to_string().contains("99"));
        assert!(Arc::ptr_eq(&core.state.queue, &queue));
        assert_eq!(core.state.current_queue_id, current);
        assert_eq!(core.state.status, status);
        assert_eq!(core.generation, generation);

        let error = core
            .command(Command::RemoveQueueEntries {
                queue_ids: vec![3, 99],
            })
            .unwrap_err();
        assert!(error.to_string().contains("99"));
        assert!(Arc::ptr_eq(&core.state.queue, &queue));
        assert_eq!(core.state.current_queue_id, current);
        assert_eq!(core.state.status, status);
        assert_eq!(core.generation, generation);
    }

    #[test]
    fn bulk_remove_stops_only_when_current_entry_is_removed_and_handles_duplicates() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 3);
        core.played = vec![1, 3, 5];
        core.played_cursor = 2;
        let generation = core.generation;
        core.command(Command::RemoveQueueEntries {
            queue_ids: vec![1, 1],
        })
        .unwrap();
        assert_eq!(
            queue_entries(&core.state.queue),
            [(2, 2), (3, 2), (4, 3), (5, 4)]
        );
        assert_eq!(core.state.current_queue_id, Some(3));
        assert_eq!(core.state.status, PlaybackStatus::Playing);
        assert_eq!(core.generation, generation);
        assert_eq!(core.played, [3, 5]);
        assert_eq!(core.played_cursor, 1);
        assert_queue_saved(&core);

        core.command(Command::RemoveQueueEntries {
            queue_ids: vec![3, 3],
        })
        .unwrap();
        assert!(!core.state.queue.iter().any(|entry| entry.id == 3));
        assert_eq!(core.state.current_queue_id, None);
        assert_eq!(core.state.status, PlaybackStatus::Stopped);
        assert_ne!(core.generation, generation);
        assert!(core.playback.is_none());
        assert_eq!(core.played, [5]);
        assert_eq!(core.played_cursor, 0);
        assert_queue_saved(&core);
    }

    #[test]
    fn empty_bulk_queue_ids_are_no_ops_and_commands_round_trip() {
        for (command, expected) in [
            (
                Command::RemoveQueueEntries { queue_ids: vec![] },
                serde_json::json!({
                    "command": "remove_queue_entries",
                    "queue_ids": []
                }),
            ),
            (
                Command::MoveQueueEntries {
                    queue_ids: vec![],
                    index: 9,
                },
                serde_json::json!({
                    "command": "move_queue_entries",
                    "queue_ids": [],
                    "index": 9
                }),
            ),
        ] {
            assert_eq!(serde_json::to_value(&command).unwrap(), expected);
            let (_directory, mut core) = fixture();
            let queue = Arc::clone(&core.state.queue);
            let generation = core.generation;
            core.command(command).unwrap();
            assert!(Arc::ptr_eq(&core.state.queue, &queue));
            assert_eq!(core.generation, generation);
            assert_queue_saved(&core);
        }
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
        core.reload(true).unwrap();
        let error = core
            .command(Command::PlayPlaylist { playlist_id })
            .unwrap_err();
        assert!(error.to_string().contains("2.wav"));
        assert_eq!(core.state.status, PlaybackStatus::Stopped);
        assert_eq!(core.state.current_queue_id, None);
        missing(&mut core, Command::Resume, 2);
    }
    #[test]
    fn show_window_rejects_without_host_and_preserves_playback_state() {
        let (_directory, mut core) = fixture();
        core.state.status = PlaybackStatus::Playing;
        core.state.position = 12.5;
        let queue = core
            .state
            .queue
            .iter()
            .map(|entry| (entry.id, entry.track_id))
            .collect::<Vec<_>>();
        let error = core.command(Command::ShowWindow).unwrap_err();
        assert!(error.to_string().contains("graphical host"));
        assert_eq!(core.state.status, PlaybackStatus::Playing);
        assert_eq!(core.state.position, 12.5);
        assert_eq!(
            core.state
                .queue
                .iter()
                .map(|entry| (entry.id, entry.track_id))
                .collect::<Vec<_>>(),
            queue
        );
    }

    #[test]
    fn show_window_notifies_registered_host_without_changing_playback() {
        let (_directory, mut core) = fixture();
        core.state.status = PlaybackStatus::Paused;
        core.state.position = 7.25;
        let queue = core
            .state
            .queue
            .iter()
            .map(|entry| (entry.id, entry.track_id))
            .collect::<Vec<_>>();
        let called = Arc::new(AtomicBool::new(false));
        let signal = called.clone();
        *core.gui_opener.write() = Some(Arc::new(move || {
            signal.store(true, Ordering::Release);
        }));
        core.command(Command::ShowWindow).unwrap();
        assert!(called.load(Ordering::Acquire));
        assert!(core.raise_requested.swap(false, Ordering::AcqRel));
        assert_eq!(core.state.status, PlaybackStatus::Paused);
        assert_eq!(core.state.position, 7.25);
        assert_eq!(
            core.state
                .queue
                .iter()
                .map(|entry| (entry.id, entry.track_id))
                .collect::<Vec<_>>(),
            queue
        );
    }

    #[test]
    fn shutdown_wakes_registered_host_after_publishing_stopped_state() {
        let (_directory, mut core) = fixture();
        let shared = core.shared.clone();
        let observed = Arc::new(AtomicBool::new(false));
        let signal = observed.clone();
        *core.gui_opener.write() = Some(Arc::new(move || {
            let state = shared.read();
            if state.shutting_down && state.status == PlaybackStatus::Stopped {
                signal.store(true, Ordering::Release);
            }
        }));
        let (sender, receiver) = bounded(1);
        sender
            .send(Request {
                command: Command::Shutdown,
                reply: None,
                ack: false,
            })
            .unwrap();
        core.run(receiver);
        assert!(observed.load(Ordering::Acquire));
    }
}
