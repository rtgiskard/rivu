use super::*;
use crate::library::ScanRecord;
use std::sync::atomic::Ordering;

mod admission;
mod catalog;
mod display;
mod lifecycle;
mod long_operations;
mod navigation;
mod other_tracks;
mod playback;
mod processing;
mod query;
mod queue_playlists;
mod relocation;
mod settings;
mod tracks;

pub(super) fn executor() -> tokio::runtime::Handle {
    static TASKS: std::sync::LazyLock<TaskRuntime> =
        std::sync::LazyLock::new(|| TaskRuntime::start().unwrap());
    TASKS.handle.clone()
}

// Bound synchronous test waits while exercising the production Tokio ports.
trait TokioReceiverExt<T> {
    fn recv_timeout(&mut self, timeout: Duration) -> Result<T>;
}

impl<T> TokioReceiverExt<T> for tokio::sync::mpsc::Receiver<T> {
    fn recv_timeout(&mut self, timeout: Duration) -> Result<T> {
        wait_on(&executor(), async {
            tokio::time::timeout(timeout, self.recv())
                .await
                .context("Timed out waiting for task event")?
                .context("Task event channel disconnected")
        })
    }
}

// An idle real audio worker handles stop/snapshot, but these tests never load
// audio. Missing-file errors expose the selected entry without opening output.
fn fixture() -> (tempfile::TempDir, Core) {
    fixture_with_duration(Some(10.0))
}

fn fixture_with_duration(duration: Option<f64>) -> (tempfile::TempDir, Core) {
    fixture_with_database(duration, false)
}

fn fixture_with_database(duration: Option<f64>, persistent: bool) -> (tempfile::TempDir, Core) {
    let directory = tempfile::tempdir().unwrap();
    let database_path = if persistent {
        directory.path().join("library.db")
    } else {
        Path::new(":memory:").to_path_buf()
    };
    let store = Store::open(&database_path).unwrap();
    store
        .apply_scan(&ScanResult {
            roots: vec![directory.path().to_path_buf()],
            min_duration_secs: 0,
            suppressed_sources: Vec::new(),
            skipped_directories: Vec::new(),
            skipped_directories_truncated: false,
            scan_incomplete: false,
            uncertainties: Vec::new(),
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
                        bitrate_exact: false,
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
    let mut state = CoreState::default();
    state.library.track_total = store.track_count().unwrap();
    state.system.config_path = directory.path().join("config.toml");
    Arc::make_mut(&mut state.system.config)
        .library_roots
        .push(directory.path().to_path_buf());
    let (scan_tx, scan_rx) = mpsc::channel(3);
    let (relocation_tx, relocation_rx) = mpsc::channel(1);
    let tasks = TaskRuntime::start().unwrap();
    let mut core = Core {
        store,
        engine: AudioEngine::new(1).unwrap(),
        logging: None,
        executor: tasks.handle.clone(),
        revision_updates: watch::channel(state.revisions).0,
        shared: Arc::new(RwLock::new(state.clone())),
        state,
        wakeup: Arc::new(RwLock::new(None)),
        subscribers: Arc::new(RwLock::new(Vec::new())),
        generation: 1,
        output_settings_request: 0,
        queue_ids: Vec::new(),
        playback: None,
        retired_playback: None,
        playback_write_deferred: false,
        pending_bitrate: None,
        pending_navigation: None,
        active_export: None,
        active_external_admission: None,
        active_optimization: None,
        processing: crate::processing::Service::new(directory.path().to_path_buf()),
        processing_results: VecDeque::new(),
        scan_tx,
        scan_rx,
        active_scan: None,
        scan_results: VecDeque::new(),
        next_scan_id: 1,
        relocation_tx,
        relocation_rx,
        active_relocation: None,
        relocation_results: VecDeque::new(),
        next_relocation_id: 1,
        played: Vec::new(),
        played_cursor: 0,
        saved_config: None,
        config_save_error: None,
        _test_tasks: Some(tasks),
    };
    core.enqueue(&[1, 2, 2, 3, 4]).unwrap();
    (directory, core)
}

// Capture the commands Core really sends without opening a platform device.
// The existing idle worker remains alive for its event/analysis ownership.
fn recording_audio(core: &mut Core) -> Receiver<AudioCommand> {
    let (commands, mut receive) = mpsc::channel(64);
    let (record, recorded) = crossbeam_channel::unbounded();
    let idle_commands = core.engine.replace_commands(commands);
    thread::spawn(move || {
        let mut final_snapshot = audio::AudioSnapshot::default();
        while let Some(command) = receive.blocking_recv() {
            match &command {
                AudioCommand::Load {
                    generation,
                    start_seconds,
                    ..
                } => {
                    final_snapshot = audio::AudioSnapshot {
                        generation: *generation,
                        position_seconds: *start_seconds,
                        listened_seconds: 0.0,
                    };
                }
                AudioCommand::Audition {
                    generation,
                    start_seconds,
                    listened_seconds,
                    ..
                }
                | AudioCommand::GraphAudition {
                    generation,
                    start_seconds,
                    listened_seconds,
                    ..
                } => {
                    final_snapshot = audio::AudioSnapshot {
                        generation: *generation,
                        position_seconds: *start_seconds,
                        listened_seconds: *listened_seconds,
                    };
                }
                AudioCommand::Seek(position) => final_snapshot.position_seconds = *position,
                _ => {}
            }
            match command {
                AudioCommand::StopAndSnapshot(reply) => {
                    let (recorded_reply, _) = oneshot::channel();
                    let _ = record.send(AudioCommand::StopAndSnapshot(recorded_reply));
                    let _ = reply.send(final_snapshot);
                }
                command => {
                    let _ = record.send(command);
                }
            }
        }
        drop(idle_commands);
    });
    recorded
}

fn navigation_runtime(
    mut core: Core,
) -> (
    CoreHandle,
    RuntimeControl,
    JoinHandle<Core>,
    Arc<Mutex<Vec<MprisSnapshot>>>,
) {
    core.publish();
    let (sender, requests) = mpsc::channel(64);
    let handle = CoreHandle {
        state: core.shared.clone(),
        analysis: core.engine.analysis.clone(),
        analysis_control: core.engine.analysis_control.clone(),
        waveform: core.engine.waveform.clone(),
        sender,
        executor: core.executor.clone(),
        revision_updates: core.revision_updates.subscribe(),
        wakeup: core.wakeup.clone(),
        subscribers: core.subscribers.clone(),
        query_timeout: None,
        _test_tasks: None,
    };
    let publications = Arc::new(Mutex::new(Vec::new()));
    let observed = publications.clone();
    let state = core.shared.clone();
    handle.set_wakeup(move || {
        observed
            .lock()
            .push(MprisSnapshot::from_core(&state.read()));
    });
    let (control, shutdown) = RuntimeControl::new();
    let worker_control = control.clone();
    let worker = thread::spawn(move || run_test_driver(core, requests, shutdown, &worker_control));
    (handle, control, worker, publications)
}

fn run_test_driver(
    mut core: Core,
    requests: mpsc::Receiver<Request>,
    shutdown: mpsc::Receiver<()>,
    control: &RuntimeControl,
) -> Core {
    core.run(requests, shutdown, control);
    core
}

fn run_until_disconnected(core: Core, requests: mpsc::Receiver<Request>) -> Core {
    let (control, shutdown) = RuntimeControl::new();
    run_test_driver(core, requests, shutdown, &control)
}

fn display_fixture() -> (
    CoreHandle,
    rivu_audio::analysis::AnalysisWorker,
    mpsc::Receiver<Request>,
) {
    let analysis = Arc::new(RwLock::new(AnalysisFrame::default()));
    let worker = rivu_audio::analysis::AnalysisWorker::new(analysis.clone()).unwrap();
    worker.configure(rivu_audio::analysis::AnalysisSettings {
        fps: 60,
        fft_size: 512,
        window: crate::config::AnalysisWindow::None,
        signal: crate::config::VisualizationSignal::After,
    });
    worker.set_playback_active(true);
    let (sender, receiver) = mpsc::channel(1);
    let tasks = Arc::new(TaskRuntime::start().unwrap());
    let handle = CoreHandle {
        state: Arc::new(RwLock::new(CoreState::default())),
        analysis,
        analysis_control: worker.control.clone(),
        waveform: Arc::new(RwLock::new(audio::WaveformFrame::default())),
        sender,
        executor: tasks.handle.clone(),
        revision_updates: watch::channel(StateRevisions::default()).1,
        wakeup: Arc::new(RwLock::new(None)),
        subscribers: Arc::new(RwLock::new(Vec::new())),
        query_timeout: None,
        _test_tasks: Some(tasks),
    };
    (handle, worker, receiver)
}

fn write_pcm(path: &Path) {
    write_pcm_frames(path, 1);
}

fn write_pcm_frames(path: &Path, frames: u32) {
    use std::io::Write;
    let data_len = frames * 200;
    let mut file = std::fs::File::create(path).unwrap();
    file.write_all(b"RIFF").unwrap();
    file.write_all(&(36 + data_len).to_le_bytes()).unwrap();
    file.write_all(b"WAVEfmt ").unwrap();
    file.write_all(&16_u32.to_le_bytes()).unwrap();
    file.write_all(&1_u16.to_le_bytes()).unwrap();
    file.write_all(&1_u16.to_le_bytes()).unwrap();
    file.write_all(&7500_u32.to_le_bytes()).unwrap();
    file.write_all(&15000_u32.to_le_bytes()).unwrap();
    file.write_all(&2_u16.to_le_bytes()).unwrap();
    file.write_all(&16_u16.to_le_bytes()).unwrap();
    file.write_all(b"data").unwrap();
    file.write_all(&data_len.to_le_bytes()).unwrap();
    file.write_all(&vec![0; data_len as usize]).unwrap();
}

fn pending(core: &mut Core, queue_id: u64) {
    core.store.set_queue_current(Some(queue_id)).unwrap();

    core.refresh_queue_tracks().unwrap();
    core.state.playback.status = PlaybackStatus::Playing;
    let (track_id, duration) = {
        let track = core.state.current_track().unwrap();
        (track.id, track.duration)
    };
    core.state.playback.duration = duration;
    core.playback = Some(PlaybackStats {
        track_id,
        source: core
            .playback_source(core.state.current_track().unwrap())
            .unwrap(),
        heard: 0.0,
        position: None,
        started: false,
        counted: false,
        last_heard_at: None,
        activity_updated: false,
        paused_at: None,
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
            bitrate_exact: false,
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
    core.store.track(track_id).unwrap().unwrap().play_count
}

fn missing(core: &mut Core, command: Command, track_id: i64) {
    let path = core.track(track_id).unwrap().path.clone();
    let error = core.command(command).unwrap_err();
    assert!(format!("{error:#}").contains(&path.display().to_string()));
}

fn queue_entries(core: &Core) -> Vec<(u64, i64)> {
    core.store
        .queue_page(None, 0, MAX_QUERY_ROWS)
        .unwrap()
        .rows
        .into_iter()
        .map(|row| (row.id, row.track_id))
        .collect()
}

fn assert_queue_saved(core: &Core) {
    assert_eq!(core.store.queue_ids().unwrap(), core.queue_ids);
    assert_eq!(core.store.queue_state().unwrap(), core.state.queue);
}
