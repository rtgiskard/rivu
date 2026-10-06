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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LibraryRow {
    pub id: i64,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: Option<f64>,
    pub favorite: bool,
    pub missing: bool,
    pub play_count: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LibraryPage {
    pub total: usize,
    pub rows: Vec<LibraryRow>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrackPage {
    pub total: usize,
    pub rows: Vec<Track>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibrarySort {
    #[default]
    Id,
    Album,
    MostPlayed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DirectoryRow {
    Directory { path: PathBuf },
    Track(LibraryRow),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DirectoryPage {
    pub total: usize,
    pub rows: Vec<DirectoryRow>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistSummary {
    pub id: i64,
    pub name: String,
    pub entry_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistSummaryPage {
    pub total: usize,
    pub rows: Vec<PlaylistSummary>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistEntryRow {
    pub id: i64,
    pub track_id: i64,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub missing: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistEntryPage {
    pub total: usize,
    pub rows: Vec<PlaylistEntryRow>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryStats {
    pub total: usize,
    pub play_count: u64,
}

/// Maximum number of rows retained by any in-memory view page.
pub const PAGE_SIZE: usize = 256;

/// Quiet interval before interactive search submits its final query.
pub(crate) const SEARCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(150);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    #[serde(default)]
    pub track_total: usize,
    #[serde(default)]
    pub playlist_total: usize,
    /// Changes whenever any persisted track data changes.
    #[serde(default)]
    pub revision: u64,
    /// Changes only when the library tree or visible row ordering may change.
    #[serde(default)]
    pub structure_revision: u64,
    #[serde(default)]
    pub playlist_revision: u64,
    #[serde(default)]
    pub history: Arc<Vec<HistoryEntry>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueueState {
    pub entries: Arc<Vec<QueueEntry>>,
    /// ID-sorted minimal metadata for distinct queued tracks.
    #[serde(default)]
    pub tracks: Arc<Vec<LibraryRow>>,
    pub current_id: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlaybackState {
    pub status: PlaybackStatus,
    pub position: f64,
    pub duration: Option<f64>,
    /// Unix timestamp of the most recent confirmed playback activity.
    /// This is session state; durable track history is updated when the session ends.
    #[serde(default)]
    pub last_heard_at: Option<i64>,
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

impl Default for LibrarySnapshot {
    fn default() -> Self {
        Self {
            track_total: 0,
            playlist_total: 0,
            revision: 0,
            structure_revision: 0,
            playlist_revision: 0,
            history: Arc::new(Vec::new()),
        }
    }
}

impl Default for QueueState {
    fn default() -> Self {
        Self {
            entries: Arc::new(Vec::new()),
            tracks: Arc::new(Vec::new()),
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
            last_heard_at: None,
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

///
/// This is shared by the terminal and GUI frontends so playback controls keep
/// identical semantics regardless of input surface.
pub(crate) fn playback_key_command(key: &str, playback: &PlaybackState) -> Option<Command> {
    let seek = |seconds: f64| {
        (playback.status != PlaybackStatus::Stopped).then_some(Command::Seek { seconds })
    };
    match key {
        "space" => Some(Command::Toggle),
        "n" => Some(Command::Next),
        "p" => Some(Command::Previous),
        "left" => seek((playback.position - 5.0).max(0.0)),
        "right" => seek((playback.position + 5.0).max(0.0)),
        "home" => seek(0.0),
        "end" => playback
            .duration
            .filter(|duration| duration.is_finite())
            .and_then(seek),
        "r" => Some(Command::Repeat {
            mode: match playback.repeat {
                RepeatMode::Off => RepeatMode::All,
                RepeatMode::All => RepeatMode::One,
                RepeatMode::One => RepeatMode::Off,
            },
        }),
        "s" => Some(Command::Shuffle {
            enabled: !playback.shuffle,
        }),
        "]" => Some(Command::Volume {
            value: (playback.volume + 0.05).min(1.0),
        }),
        "[" => Some(Command::Volume {
            value: (playback.volume - 0.05).max(0.0),
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
    LibraryPage {
        query: Option<String>,
        favorite: Option<bool>,
        missing: Option<bool>,
        sort: LibrarySort,
        offset: usize,
        limit: usize,
    },
    TrackPage {
        query: Option<String>,
        favorite: Option<bool>,
        missing: Option<bool>,
        offset: usize,
        limit: usize,
    },
    DirectoryPage {
        path: PathBuf,
        offset: usize,
        limit: usize,
    },
    PlaylistSummaries {
        offset: usize,
        limit: usize,
    },
    PlaylistEntries {
        playlist_id: i64,
        offset: usize,
        limit: usize,
    },
    Track {
        track_id: i64,
    },
    LibraryStats,
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
    EnqueueSources {
        directories: Vec<PathBuf>,
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
    AddPlaylistSources {
        playlist_id: i64,
        directories: Vec<PathBuf>,
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn playback_seek_respects_status_and_boundaries() {
        let mut state = PlaybackState {
            position: 2.0,
            ..PlaybackState::default()
        };
        assert!(playback_key_command("left", &state).is_none());
        state.status = PlaybackStatus::Playing;
        assert!(matches!(
            playback_key_command("left", &state),
            Some(Command::Seek { seconds }) if seconds == 0.0
        ));
        state.position = 12.0;
        assert!(matches!(
            playback_key_command("right", &state),
            Some(Command::Seek { seconds }) if seconds == 17.0
        ));
        state.duration = Some(f64::INFINITY);
        assert!(playback_key_command("end", &state).is_none());
        state.duration = Some(42.0);
        assert!(matches!(
            playback_key_command("end", &state),
            Some(Command::Seek { seconds }) if seconds == 42.0
        ));
        assert!(playback_key_command("unknown", &state).is_none());
    }

    #[test]
    fn playback_volume_and_repeat_cycle_are_bounded() {
        let mut state = PlaybackState {
            volume: 1.0,
            ..PlaybackState::default()
        };
        assert!(matches!(
            playback_key_command("]", &state),
            Some(Command::Volume { value }) if value == 1.0
        ));
        state.volume = 0.0;
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
        state.repeat = RepeatMode::All;
        assert!(matches!(
            playback_key_command("r", &state),
            Some(Command::Repeat {
                mode: RepeatMode::One
            })
        ));
        state.repeat = RepeatMode::One;
        assert!(matches!(
            playback_key_command("r", &state),
            Some(Command::Repeat {
                mode: RepeatMode::Off
            })
        ));
    }
}
