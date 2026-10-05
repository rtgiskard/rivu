use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CueSegment {
    pub sheet: PathBuf,
    pub number: u32,
    pub start_frame: u64,
    pub end_frame: Option<u64>,
}

impl CueSegment {
    pub fn start_seconds(&self) -> f64 {
        self.start_frame as f64 / 75.0
    }

    pub fn end_seconds(&self) -> Option<f64> {
        self.end_frame.map(|frame| frame as f64 / 75.0)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Track {
    pub id: i64,
    pub path: PathBuf,
    #[serde(default)]
    pub fingerprint: Option<String>,
    #[serde(default)]
    pub cue: Option<CueSegment>,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: Option<f64>,
    pub codec: String,
    pub channels: u16,
    pub sample_rate: u32,
    pub bitrate_bps: Option<u64>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub bits_per_sample: Option<u32>,
    pub release_date: Option<String>,
    pub favorite: bool,
    pub missing: bool,
    pub play_count: u64,
    pub last_played: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlaylistEntry {
    pub id: i64,
    pub track_id: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Playlist {
    pub id: i64,
    pub name: String,
    pub entries: Vec<PlaylistEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueueEntry {
    pub id: u64,
    pub track_id: i64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackStatus {
    #[default]
    Stopped,
    Playing,
    Paused,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepeatMode {
    #[default]
    Off,
    All,
    One,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub track_id: i64,
    pub title: String,
    pub played_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DatabaseOptimization {
    pub database_bytes_before: u64,
    pub database_bytes_after: u64,
    pub wal_bytes_before: u64,
    pub wal_bytes_after: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LibrarySnapshot {
    pub tracks: Arc<Vec<Track>>,
    /// Changes whenever any persisted track data changes.
    #[serde(default)]
    pub revision: u64,
    /// Changes only when the library tree or visible row ordering may change.
    #[serde(default)]
    pub structure_revision: u64,
    pub playlists: Arc<Vec<Playlist>>,
    pub history: Arc<Vec<HistoryEntry>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueueState {
    pub entries: Arc<Vec<QueueEntry>>,
    pub current_id: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlaybackState {
    pub status: PlaybackStatus,
    pub position: f64,
    pub duration: Option<f64>,
    pub volume: f32,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub seek_revision: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SystemState {
    pub scanning: bool,
    pub scan_message: String,
    pub last_error: Option<String>,
    pub devices: Arc<Vec<String>>,
    pub selected_device: Option<String>,
    pub revision: u64,
    pub config: Arc<crate::config::Config>,
    pub config_path: PathBuf,
    pub mpris_status: String,
    pub ffmpeg_status: String,
    pub database_optimization: Option<DatabaseOptimization>,
    pub shutting_down: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppState {
    pub library: LibrarySnapshot,
    pub queue: QueueState,
    pub playback: PlaybackState,
    pub system: SystemState,
}

impl Default for LibrarySnapshot {
    fn default() -> Self {
        Self {
            tracks: Arc::new(Vec::new()),
            revision: 0,
            structure_revision: 0,
            playlists: Arc::new(Vec::new()),
            history: Arc::new(Vec::new()),
        }
    }
}

impl Default for QueueState {
    fn default() -> Self {
        Self {
            entries: Arc::new(Vec::new()),
            current_id: None,
        }
    }
}

impl Default for PlaybackState {
    fn default() -> Self {
        Self {
            status: PlaybackStatus::Stopped,
            position: 0.0,
            duration: None,
            volume: 0.7,
            shuffle: false,
            repeat: RepeatMode::Off,
            seek_revision: 0,
        }
    }
}

impl Default for SystemState {
    fn default() -> Self {
        Self {
            scanning: false,
            scan_message: String::new(),
            last_error: None,
            devices: Arc::new(Vec::new()),
            selected_device: None,
            revision: 0,
            config: Arc::new(crate::config::Config::default()),
            config_path: PathBuf::new(),
            mpris_status: String::new(),
            ffmpeg_status: "disabled".into(),
            database_optimization: None,
            shutting_down: false,
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            library: LibrarySnapshot::default(),
            queue: QueueState::default(),
            playback: PlaybackState::default(),
            system: SystemState::default(),
        }
    }
}

impl AppState {
    pub fn current_track(&self) -> Option<&Track> {
        let queue_id = self.queue.current_id?;
        let track_id = self
            .queue
            .entries
            .iter()
            .find(|entry| entry.id == queue_id)?
            .track_id;
        let index = self
            .library
            .tracks
            .binary_search_by_key(&track_id, |track| track.id)
            .ok()?;
        self.library.tracks.get(index)
    }
}

#[derive(Clone, Debug)]
pub struct MprisSnapshot {
    pub tracks: Arc<Vec<Track>>,
    pub queue: QueueState,
    pub playback: PlaybackState,
    pub shutting_down: bool,
}

impl MprisSnapshot {
    pub fn from_state(state: &AppState) -> Self {
        Self {
            tracks: Arc::clone(&state.library.tracks),
            queue: state.queue.clone(),
            playback: state.playback.clone(),
            shutting_down: state.system.shutting_down,
        }
    }

    pub fn current_track(&self) -> Option<&Track> {
        let queue_id = self.queue.current_id?;
        let track_id = self
            .queue
            .entries
            .iter()
            .find(|entry| entry.id == queue_id)?
            .track_id;
        let index = self
            .tracks
            .binary_search_by_key(&track_id, |track| track.id)
            .ok()?;
        self.tracks.get(index)
    }
}
/// Translate a normalized playback key name into the corresponding command.

#[derive(Clone, Debug)]
pub struct GuiSnapshot(AppState);

impl GuiSnapshot {
    pub fn from_state(state: &AppState) -> Self {
        Self(state.clone())
    }
}

impl std::ops::Deref for GuiSnapshot {
    type Target = AppState;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
///
/// This is shared by the terminal and GUI frontends so playback controls keep
/// identical semantics regardless of input surface.
pub(crate) fn playback_key_command(key: &str, state: &AppState) -> Option<Command> {
    let seek = |seconds: f64| {
        (state.playback.status != PlaybackStatus::Stopped).then_some(Command::Seek { seconds })
    };
    match key {
        "space" => Some(Command::Toggle),
        "n" => Some(Command::Next),
        "p" => Some(Command::Previous),
        "left" => seek((state.playback.position - 5.0).max(0.0)),
        "right" => seek((state.playback.position + 5.0).max(0.0)),
        "home" => seek(0.0),
        "end" => state
            .playback
            .duration
            .filter(|duration| duration.is_finite())
            .and_then(seek),
        "r" => Some(Command::Repeat {
            mode: match state.playback.repeat {
                RepeatMode::Off => RepeatMode::All,
                RepeatMode::All => RepeatMode::One,
                RepeatMode::One => RepeatMode::Off,
            },
        }),
        "s" => Some(Command::Shuffle {
            enabled: !state.playback.shuffle,
        }),
        "]" => Some(Command::Volume {
            value: (state.playback.volume + 0.05).min(1.0),
        }),
        "[" => Some(Command::Volume {
            value: (state.playback.volume - 0.05).max(0.0),
        }),
        _ => None,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    Status,
    Overview,
    ShowWindow,
    OptimizeDatabase,
    SetFavorite {
        track_ids: Vec<i64>,
        favorite: bool,
    },
    RemoveMissingTracks,
    Scan {
        paths: Vec<PathBuf>,
        #[serde(default)]
        force: bool,
    },
    Play {
        track_id: i64,
    },
    PlayQueue {
        queue_id: u64,
    },
    PlayPlaylist {
        playlist_id: i64,
    },
    Resume,
    Pause,
    Toggle,
    Stop,
    Next,
    Previous,
    Seek {
        seconds: f64,
    },
    SeekQueue {
        queue_id: u64,
        seconds: f64,
    },
    Volume {
        value: f32,
    },
    Enqueue {
        track_ids: Vec<i64>,
    },
    RemoveQueue {
        queue_id: u64,
    },
    RemoveQueueEntries {
        queue_ids: Vec<u64>,
    },
    MoveQueue {
        queue_id: u64,
        index: usize,
    },
    MoveQueueEntries {
        queue_ids: Vec<u64>,
        index: usize,
    },
    ClearQueue,
    RandomizeQueue,
    DeduplicateQueue,
    Shuffle {
        enabled: bool,
    },
    Repeat {
        mode: RepeatMode,
    },
    CreatePlaylist {
        name: String,
    },
    CreatePlaylistWithTracks {
        name: String,
        track_ids: Vec<i64>,
    },
    RenamePlaylist {
        playlist_id: i64,
        name: String,
    },
    DeletePlaylist {
        playlist_id: i64,
    },
    AddPlaylist {
        playlist_id: i64,
        track_ids: Vec<i64>,
    },
    RemovePlaylistEntry {
        entry_id: i64,
    },
    MovePlaylistEntry {
        entry_id: i64,
        index: usize,
    },
    ImportPlaylist {
        path: PathBuf,
        name: Option<String>,
    },
    ExportPlaylist {
        playlist_id: i64,
        path: PathBuf,
    },
    EditTrack {
        track_id: i64,
        title: String,
        artist: String,
        album: String,
    },
    RemoveTracks {
        track_ids: Vec<i64>,
    },
    Device {
        name: Option<String>,
    },
    Analysis {
        enabled: bool,
    },
    DismissError,
    Configure {
        config: crate::config::Config,
    },
    MprisStatus {
        status: String,
    },
    Shutdown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StateResponse {
    pub ok: bool,
    pub error: Option<String>,
    pub state: AppState,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Ack {
    pub ok: bool,
    pub error: Option<String>,
    pub revision: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_seek_respects_status_and_boundaries() {
        let mut state = AppState::default();
        state.playback.position = 2.0;
        assert!(playback_key_command("left", &state).is_none());
        state.playback.status = PlaybackStatus::Playing;
        assert!(matches!(
            playback_key_command("left", &state),
            Some(Command::Seek { seconds }) if seconds == 0.0
        ));
        state.playback.position = 12.0;
        assert!(matches!(
            playback_key_command("right", &state),
            Some(Command::Seek { seconds }) if seconds == 17.0
        ));
        state.playback.duration = Some(f64::INFINITY);
        assert!(playback_key_command("end", &state).is_none());
        state.playback.duration = Some(42.0);
        assert!(matches!(
            playback_key_command("end", &state),
            Some(Command::Seek { seconds }) if seconds == 42.0
        ));
        assert!(playback_key_command("unknown", &state).is_none());
    }

    #[test]
    fn playback_volume_and_repeat_cycle_are_bounded() {
        let mut state = AppState::default();
        state.playback.volume = 1.0;
        assert!(matches!(
            playback_key_command("]", &state),
            Some(Command::Volume { value }) if value == 1.0
        ));
        state.playback.volume = 0.0;
        assert!(matches!(
            playback_key_command("[", &state),
            Some(Command::Volume { value }) if value == 0.0
        ));
        assert!(matches!(
            playback_key_command("r", &state),
            Some(Command::Repeat {
                mode: RepeatMode::All
            })
        ));
        state.playback.repeat = RepeatMode::All;
        assert!(matches!(
            playback_key_command("r", &state),
            Some(Command::Repeat {
                mode: RepeatMode::One
            })
        ));
        state.playback.repeat = RepeatMode::One;
        assert!(matches!(
            playback_key_command("r", &state),
            Some(Command::Repeat {
                mode: RepeatMode::Off
            })
        ));
    }
}
