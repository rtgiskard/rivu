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
    pub cue: Option<CueSegment>,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: Option<f64>,
    pub codec: String,
    pub channels: u16,
    pub sample_rate: u32,
    pub missing: bool,
    pub play_count: u64,
    pub listen_seconds: f64,
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
    pub id: i64,
    pub track_id: i64,
    pub title: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub listened_seconds: f64,
    pub counted: bool,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppState {
    pub library: Arc<Vec<Track>>,
    pub playlists: Arc<Vec<Playlist>>,
    pub queue: Arc<Vec<QueueEntry>>,
    pub history: Arc<Vec<HistoryEntry>>,
    pub current_queue_id: Option<u64>,
    pub status: PlaybackStatus,
    pub position: f64,
    pub duration: Option<f64>,
    pub volume: f32,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub scanning: bool,
    pub scan_message: String,
    pub last_error: Option<String>,
    pub devices: Arc<Vec<String>>,
    pub selected_device: Option<String>,
    pub revision: u64,
    pub seek_revision: u64,
    pub config: Arc<crate::config::Config>,
    pub config_path: PathBuf,
    pub mpris_status: String,
    pub shutting_down: bool,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            library: Arc::new(Vec::new()),
            playlists: Arc::new(Vec::new()),
            queue: Arc::new(Vec::new()),
            history: Arc::new(Vec::new()),
            current_queue_id: None,
            status: PlaybackStatus::Stopped,
            position: 0.0,
            duration: None,
            volume: 0.7,
            shuffle: false,
            repeat: RepeatMode::Off,
            scanning: false,
            scan_message: String::new(),
            last_error: None,
            devices: Arc::new(Vec::new()),
            selected_device: None,
            revision: 0,
            seek_revision: 0,
            config: Arc::new(crate::config::Config::default()),
            config_path: PathBuf::new(),
            mpris_status: String::new(),
            shutting_down: false,
        }
    }
}

impl AppState {
    pub fn current_track(&self) -> Option<&Track> {
        let queue_id = self.current_queue_id?;
        let track_id = self
            .queue
            .iter()
            .find(|entry| entry.id == queue_id)?
            .track_id;
        let index = self
            .library
            .binary_search_by_key(&track_id, |track| track.id)
            .ok()?;
        self.library.get(index)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    Status,
    Overview,
    Scan {
        paths: Vec<PathBuf>,
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
    MoveQueue {
        queue_id: u64,
        index: usize,
    },
    ClearQueue,
    Shuffle {
        enabled: bool,
    },
    Repeat {
        mode: RepeatMode,
    },
    CreatePlaylist {
        name: String,
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
pub struct Response {
    pub ok: bool,
    pub error: Option<String>,
    pub state: AppState,
}
