use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::Arc,
};

/// Shared client state assembled from sectioned IPC responses.
/// UI-specific projections borrow this contract; it contains no view state.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ClientSnapshot {
    pub library: LibrarySnapshot,
    pub queue: QueueState,
    pub current_track: Option<Arc<Track>>,
    pub playback: PlaybackState,
    pub system: SystemState,
}

impl ClientSnapshot {
    pub fn current_track(&self) -> Option<&Track> {
        self.current_track.as_deref()
    }
}

/// Decoded file metadata shared by scanning, persistence, and playback.
///
/// This value contains no decoder or audio-output state; probing supplies it and
/// the library caches it independently of the playback engine.
/// Unlike [`Track`], this carries no library identity or user-maintained state.
#[derive(Clone, Debug)]
pub struct MediaInfo {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: Option<f64>,
    /// Encoded audio bitrate in bits/s; a scan estimate unless `bitrate_exact` is true.
    pub bitrate_bps: Option<u64>,
    /// Reliable fixed-encoding calculation or complete-playback measured average;
    /// decoder overlap/delay/padding handling can introduce small backend differences.
    pub bitrate_exact: bool,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    /// Source PCM/lossless precision, never the decoder's output sample width.
    pub bits_per_sample: Option<u32>,
    /// Original date precision is retained (e.g. year, year-month, or full date).
    pub release_date: Option<String>,
    pub codec: String,
    pub channels: u16,
    pub sample_rate: u32,
}

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

/// User-selected decoder routing, applied when opening a track for playback.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecoderHint {
    #[default]
    Auto,
    Native,
    Ffmpeg,
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
    #[serde(default)]
    pub bitrate_exact: bool,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub bits_per_sample: Option<u32>,
    pub release_date: Option<String>,
    pub favorite: bool,
    #[serde(default)]
    pub decoder_hint: DecoderHint,
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
    pub codec: String,
    pub bitrate_bps: Option<u64>,
    #[serde(default)]
    pub bitrate_exact: bool,
    pub favorite: bool,
    pub missing: bool,
    pub play_count: u64,
}

/// A bounded interactive result with an exact filtered total.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page<T> {
    pub total: usize,
    pub rows: Vec<T>,
}

/// Count-free sequential result. `next` is present only when more rows exist.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Batch<T, C> {
    pub rows: Vec<T>,
    pub next: Option<C>,
}

/// Sequential occurrence traversal: the keyset stays separate from matching rank.
/// `scope` binds both fields to the query, collection and selected data revisions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchContinuation<C> {
    pub cursor: C,
    /// Matching offset of the next row, not a queue or playlist position.
    pub offset: usize,
    pub scope: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistEntryCursor {
    pub position: i64,
    pub id: i64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibrarySort {
    #[default]
    Id,
    Album,
    MostPlayed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlbumSort {
    #[default]
    Title,
    Artist,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DirectoryRow {
    Directory { path: PathBuf },
    Track(LibraryRow),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistSummary {
    pub id: i64,
    pub name: String,
    pub entry_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistEntryRow {
    pub id: i64,
    pub track_id: i64,
    /// Original occurrence position, never a filtered row index.
    pub position: usize,
    /// Zero-based index among the current query's matches.
    pub offset: usize,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub favorite: bool,
    pub missing: bool,
}

/// Stable entry identity and current absolute position, without metadata or page residency.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistEntryLocation {
    pub id: i64,
    pub playlist_id: i64,
    pub track_id: i64,
    pub position: usize,
    pub offset: usize,
    pub total: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryStats {
    pub total: usize,
    pub play_count: u64,
}

/// Total scanned tracks under a configured root, independent of view filters.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryRootCount {
    pub path: PathBuf,
    pub total: usize,
}

/// Maximum number of rows in a query batch or interactive page.
pub const MAX_QUERY_ROWS: usize = 256;

/// Quiet interval before interactive search submits its final query.
pub(crate) const SEARCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(150);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueueEntry {
    pub id: u64,
    pub track_id: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueueRow {
    pub id: u64,
    pub track_id: i64,
    pub position: usize,
    /// Zero-based index among the current query's matches.
    pub offset: usize,
    pub track: LibraryRow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueCursor {
    pub position: usize,
    pub id: u64,
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
    pub play_count: u64,
    pub played_at: i64,
    pub favorite: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Changes whenever the effective Track projection changes.
    #[serde(default)]
    pub revision: u64,
    /// Changes only when the library tree or visible row ordering may change.
    #[serde(default)]
    pub structure_revision: u64,
    /// Playlist summaries: identity, name, or member count. Not a content epoch.
    #[serde(default)]
    pub playlist_revision: u64,
    /// Content/lifetime tokens for every extant playlist; absence means deletion.
    #[serde(default)]
    pub playlist_entry_revisions: Arc<BTreeMap<i64, u64>>,
    #[serde(default)]
    pub metadata_revisions: Arc<MetadataRevisions>,
    #[serde(default)]
    pub duplicate_groups_revision: u64,
    #[serde(default)]
    pub duplicate_members_revision: u64,
    #[serde(default)]
    pub history: Arc<Vec<HistoryEntry>>,
}

/// Exact recent metadata scopes; unknown or overflowing batches reset all scopes.
/// The bound covers both maps, independently of library and playlist sizes.
pub(crate) const MAX_METADATA_SCOPES: usize = 256;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MetadataRevisions {
    revision: u64,
    baseline: u64,
    tracks: HashMap<i64, u64>,
    playlists: HashMap<i64, u64>,
}

impl MetadataRevisions {
    fn reset(&mut self, revision: u64) {
        self.revision = revision;
        self.baseline = revision;
        self.tracks.clear();
        self.playlists.clear();
    }

    fn record(
        &mut self,
        previous: u64,
        revision: u64,
        tracks: Option<&[i64]>,
        playlists: Option<&[i64]>,
    ) {
        if self.revision != previous {
            self.reset(previous);
        }
        let (Some(tracks), Some(playlists)) = (tracks, playlists) else {
            self.reset(revision);
            return;
        };
        if tracks.len().saturating_add(playlists.len()) > MAX_METADATA_SCOPES {
            self.reset(revision);
            return;
        }
        self.revision = revision;
        for &id in tracks {
            if !self.tracks.contains_key(&id)
                && self.tracks.len() + self.playlists.len() == MAX_METADATA_SCOPES
            {
                self.reset(revision);
                return;
            }
            self.tracks.insert(id, revision);
        }
        for &id in playlists {
            if !self.playlists.contains_key(&id)
                && self.tracks.len() + self.playlists.len() == MAX_METADATA_SCOPES
            {
                self.reset(revision);
                return;
            }
            self.playlists.insert(id, revision);
        }
    }

    pub(crate) fn track_revision(&self, id: i64, library: u64) -> u64 {
        if self.revision == library {
            self.tracks.get(&id).copied().unwrap_or(self.baseline)
        } else {
            library
        }
    }

    pub(crate) fn playlist_revision(&self, id: i64, library: u64) -> u64 {
        if self.revision == library {
            self.playlists.get(&id).copied().unwrap_or(self.baseline)
        } else {
            library
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueState {
    pub total: usize,
    pub current_id: Option<u64>,
    pub current_position: Option<usize>,
    pub revision: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanPhase {
    Discovering,
    ReadingMetadata,
    Saving,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanProgress {
    pub phase: ScanPhase,
    pub processed: usize,
    pub total: Option<usize>,
    pub errors: usize,
    pub path: Option<PathBuf>,
}

/// Result of one scan/import task, independent of global UI errors or later tasks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanTaskStatus {
    pub id: u64,
    pub progress: ScanProgress,
    pub message: String,
    pub error: Option<String>,
}

/// A manually confirmed ordinary-file identity relocation, owned by Core.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelocationPhase {
    Verifying,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelocationTaskStatus {
    pub id: u64,
    pub source_id: i64,
    pub target_id: i64,
    pub phase: RelocationPhase,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SystemState {
    pub scanning: bool,
    pub scan_message: String,
    #[serde(default)]
    pub scan_progress: Option<ScanProgress>,
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
    #[serde(default)]
    pub relocation_task: Option<RelocationTaskStatus>,
}
impl Default for SystemState {
    fn default() -> Self {
        Self {
            scanning: false,
            scan_message: String::new(),
            scan_progress: None,
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
            relocation_task: None,
        }
    }
}

impl Default for LibrarySnapshot {
    fn default() -> Self {
        Self {
            track_total: 0,
            playlist_total: 0,
            revision: 0,
            structure_revision: 0,
            playlist_revision: 0,
            playlist_entry_revisions: Arc::default(),
            metadata_revisions: Arc::default(),
            duplicate_groups_revision: 0,
            duplicate_members_revision: 0,
            history: Arc::new(Vec::new()),
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

/// Exact effective album title scoped to its immediate parent directory.
/// Directory has no trailing separator, except for the filesystem root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlbumKey {
    pub directory: PathBuf,
    pub title: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AlbumSummary {
    pub key: AlbumKey,
    pub artist: String,
    pub track_count: usize,
    pub representative: Track,
}

/// One exact sibling position; no library payload or unbounded ID list is returned.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryLocation {
    pub offset: usize,
    pub total: usize,
    pub child: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LibraryLocateResult {
    Located(LibraryLocation),
    Filtered,
    Unavailable,
    OutsideRoot,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Query {
    Albums {
        query: Option<String>,
        sort: AlbumSort,
        offset: usize,
        limit: usize,
    },
    AlbumTracks {
        key: AlbumKey,
        query: Option<String>,
        offset: usize,
        limit: usize,
    },
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
        query: Option<String>,
        favorite: Option<bool>,
        missing: Option<bool>,
        offset: usize,
        limit: usize,
    },
    PlaylistSummaries {
        query: Option<String>,
        offset: usize,
        limit: usize,
    },
    PlaylistEntries {
        playlist_id: i64,
        query: Option<String>,
        offset: usize,
        limit: usize,
    },
    PlaylistEntryLocations {
        playlist_id: i64,
        query: Option<String>,
        entry_ids: Vec<i64>,
    },
    Track {
        track_id: i64,
    },
    LibraryStats,
    TrackBatch {
        query: Option<String>,
        favorite: Option<bool>,
        missing: Option<bool>,
        after: Option<i64>,
        limit: usize,
        expected: Option<crate::response::QueryRevisions>,
    },
    PlaylistSummaryBatch {
        query: Option<String>,
        after: Option<i64>,
        limit: usize,
        expected: Option<crate::response::QueryRevisions>,
    },
    PlaylistEntryBatch {
        playlist_id: i64,
        query: Option<String>,
        after: Option<BatchContinuation<PlaylistEntryCursor>>,
        limit: usize,
        expected: Option<crate::response::QueryRevisions>,
    },
    LibraryRootPage {
        path: PathBuf,
        query: Option<String>,
        favorite: Option<bool>,
        missing: Option<bool>,
        sort: LibrarySort,
        offset: usize,
        limit: usize,
    },
    LibraryLocate {
        track_id: i64,
        path: PathBuf,
        tree: bool,
        query: Option<String>,
        favorite: Option<bool>,
        missing: Option<bool>,
        sort: LibrarySort,
    },
    LibraryRootCounts {
        roots: Vec<PathBuf>,
    },
    ScanTask {
        id: u64,
    },
    QueuePage {
        query: Option<String>,
        offset: usize,
        limit: usize,
    },
    QueueBatch {
        query: Option<String>,
        after: Option<BatchContinuation<QueueCursor>>,
        limit: usize,
        expected: Option<crate::response::QueryRevisions>,
    },
    QueueEntries {
        query: Option<String>,
        queue_ids: Vec<u64>,
    },
    PlaylistEntry {
        entry_id: i64,
    },
    DuplicateGroups {
        offset: usize,
        limit: usize,
    },
    DuplicateMembers {
        fingerprint: String,
        offset: usize,
        limit: usize,
    },
    RelocationTask {
        id: u64,
    },
    DuplicateMemberSelection {
        fingerprint: String,
        track_ids: Vec<i64>,
    },
}

/// Selected equality tokens, owned by the query rather than its consumers.
/// Unselected domains are zeroed, so scheduling and acceptance share one contract.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueryStamp(crate::response::QueryRevisions);

impl QueryStamp {
    pub fn revisions(self) -> crate::response::QueryRevisions {
        self.0
    }

    pub(crate) fn continuation_scope(
        self,
        query: Option<&str>,
        playlist_id: Option<i64>,
    ) -> [u8; 32] {
        let mut hash = blake3::Hasher::new();
        hash.update(b"rivu occurrence continuation v1");
        hash.update(&[u8::from(playlist_id.is_some())]);
        hash.update(&playlist_id.unwrap_or_default().to_le_bytes());
        // Callers supply the same normalized search text used by SQL.
        hash.update(query.unwrap_or_default().as_bytes());
        hash.update(&self.0.library.to_le_bytes());
        hash.update(&self.0.queue.to_le_bytes());
        for scope in [self.0.playlist_entries, self.0.playlist_metadata] {
            hash.update(&[u8::from(scope.is_some())]);
            let scope = scope.unwrap_or_default();
            hash.update(&scope.playlist_id.to_le_bytes());
            hash.update(&scope.revision.to_le_bytes());
        }
        *hash.finalize().as_bytes()
    }
}

/// Borrowed projection identity: demand never allocates or changes the data key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryIdentity<'a> {
    Albums(&'a Option<String>, AlbumSort),
    AlbumTracks(&'a AlbumKey, &'a Option<String>),
    LibraryPage(&'a Option<String>, Option<bool>, Option<bool>, LibrarySort),
    TrackPage(&'a Option<String>, Option<bool>, Option<bool>),
    DirectoryPage(
        &'a std::path::Path,
        &'a Option<String>,
        Option<bool>,
        Option<bool>,
    ),
    PlaylistSummaries(&'a Option<String>),
    PlaylistEntries(i64, &'a Option<String>),
    PlaylistEntryLocations(i64, &'a [i64], &'a Option<String>),
    Track(i64),
    LibraryStats,
    TrackBatch(&'a Option<String>, Option<bool>, Option<bool>),
    PlaylistSummaryBatch(&'a Option<String>),
    PlaylistEntryBatch(i64, &'a Option<String>),
    LibraryRootPage(
        &'a std::path::Path,
        &'a Option<String>,
        Option<bool>,
        Option<bool>,
        LibrarySort,
    ),
    LibraryLocate(
        i64,
        &'a std::path::Path,
        bool,
        &'a Option<String>,
        Option<bool>,
        Option<bool>,
        LibrarySort,
    ),
    LibraryRootCounts(&'a [PathBuf]),
    ScanTask(u64),
    QueuePage(&'a Option<String>),
    QueueBatch(&'a Option<String>),
    QueueEntries(&'a [u64], &'a Option<String>),
    PlaylistEntry(i64),
    DuplicateGroups,
    DuplicateMembers(&'a str),
    RelocationTask(u64),
    DuplicateMemberSelection(&'a str, &'a [i64]),
}

impl Query {
    pub fn stamp(&self, revisions: crate::response::QueryRevisions) -> QueryStamp {
        use crate::response::QueryRevisions;
        let mut stamp = QueryRevisions::default();
        match self {
            Self::LibraryPage { .. }
            | Self::TrackPage { .. }
            | Self::TrackBatch { .. }
            | Self::LibraryStats => stamp.library = revisions.library,
            Self::Track { track_id } => {
                stamp.track = revisions.track.filter(|scope| scope.track_id == *track_id);
                if stamp.track.is_none() {
                    stamp.library = revisions.library;
                }
            }
            Self::Albums { .. }
            | Self::AlbumTracks { .. }
            | Self::DirectoryPage { .. }
            | Self::LibraryRootPage { .. }
            | Self::LibraryLocate { .. } => {
                stamp.library = revisions.library;
                stamp.structure = revisions.structure;
            }
            Self::LibraryRootCounts { .. } => stamp.structure = revisions.structure,
            Self::PlaylistSummaries { .. } | Self::PlaylistSummaryBatch { .. } => {
                stamp.playlist = revisions.playlist;
            }
            Self::PlaylistEntries { playlist_id, .. }
            | Self::PlaylistEntryBatch { playlist_id, .. } => {
                stamp.playlist_metadata = revisions
                    .playlist_metadata
                    .filter(|scope| scope.playlist_id == *playlist_id);
                if stamp.playlist_metadata.is_none() {
                    stamp.library = revisions.library;
                }
                stamp.playlist_entries = revisions
                    .playlist_entries
                    .filter(|scope| scope.playlist_id == *playlist_id);
            }
            Self::PlaylistEntryLocations {
                playlist_id, query, ..
            } => {
                stamp.playlist_entries = revisions
                    .playlist_entries
                    .filter(|scope| scope.playlist_id == *playlist_id);
                if query.is_some() {
                    stamp.playlist_metadata = revisions
                        .playlist_metadata
                        .filter(|scope| scope.playlist_id == *playlist_id);
                    if stamp.playlist_metadata.is_none() {
                        stamp.library = revisions.library;
                    }
                }
            }
            Self::PlaylistEntry { .. } => stamp.playlist_entries = revisions.playlist_entries,
            Self::QueuePage { query, .. }
            | Self::QueueBatch { query, .. }
            | Self::QueueEntries { query, .. } => {
                stamp.queue = revisions.queue;
                if query.is_some() {
                    stamp.library = revisions.library;
                }
            }
            Self::DuplicateGroups { .. } => stamp.duplicate_groups = revisions.duplicate_groups,
            Self::DuplicateMembers { .. } | Self::DuplicateMemberSelection { .. } => {
                stamp.library = revisions.library;
                stamp.duplicate_members = revisions.duplicate_members;
                stamp.queue = revisions.queue;
            }
            Self::ScanTask { .. } | Self::RelocationTask { .. } => {}
        }
        QueryStamp(stamp)
    }

    /// Offsets, cursors, page sizes and expected baselines are demand, not identity.
    pub fn identity(&self) -> QueryIdentity<'_> {
        match self {
            Self::Albums { query, sort, .. } => QueryIdentity::Albums(query, *sort),
            Self::AlbumTracks { key, query, .. } => QueryIdentity::AlbumTracks(key, query),
            Self::LibraryPage {
                query,
                favorite,
                missing,
                sort,
                ..
            } => QueryIdentity::LibraryPage(query, *favorite, *missing, *sort),
            Self::TrackPage {
                query,
                favorite,
                missing,
                ..
            } => QueryIdentity::TrackPage(query, *favorite, *missing),
            Self::DirectoryPage {
                path,
                query,
                favorite,
                missing,
                ..
            } => QueryIdentity::DirectoryPage(path, query, *favorite, *missing),
            Self::PlaylistSummaries { query, .. } => QueryIdentity::PlaylistSummaries(query),
            Self::PlaylistEntries {
                playlist_id, query, ..
            } => QueryIdentity::PlaylistEntries(*playlist_id, query),
            Self::PlaylistEntryLocations {
                playlist_id,
                entry_ids,
                query,
            } => QueryIdentity::PlaylistEntryLocations(*playlist_id, entry_ids, query),
            Self::Track { track_id } => QueryIdentity::Track(*track_id),
            Self::LibraryStats => QueryIdentity::LibraryStats,
            Self::TrackBatch {
                query,
                favorite,
                missing,
                ..
            } => QueryIdentity::TrackBatch(query, *favorite, *missing),
            Self::PlaylistSummaryBatch { query, .. } => QueryIdentity::PlaylistSummaryBatch(query),
            Self::PlaylistEntryBatch {
                playlist_id, query, ..
            } => QueryIdentity::PlaylistEntryBatch(*playlist_id, query),
            Self::LibraryRootPage {
                path,
                query,
                favorite,
                missing,
                sort,
                ..
            } => QueryIdentity::LibraryRootPage(path, query, *favorite, *missing, *sort),
            Self::LibraryLocate {
                track_id,
                path,
                tree,
                query,
                favorite,
                missing,
                sort,
            } => QueryIdentity::LibraryLocate(
                *track_id, path, *tree, query, *favorite, *missing, *sort,
            ),
            Self::LibraryRootCounts { roots } => QueryIdentity::LibraryRootCounts(roots),
            Self::ScanTask { id } => QueryIdentity::ScanTask(*id),
            Self::QueuePage { query, .. } => QueryIdentity::QueuePage(query),
            Self::QueueBatch { query, .. } => QueryIdentity::QueueBatch(query),
            Self::QueueEntries { queue_ids, query } => {
                QueryIdentity::QueueEntries(queue_ids, query)
            }
            Self::PlaylistEntry { entry_id } => QueryIdentity::PlaylistEntry(*entry_id),
            Self::DuplicateGroups { .. } => QueryIdentity::DuplicateGroups,
            Self::DuplicateMembers { fingerprint, .. } => {
                QueryIdentity::DuplicateMembers(fingerprint)
            }
            Self::RelocationTask { id } => QueryIdentity::RelocationTask(*id),
            Self::DuplicateMemberSelection {
                fingerprint,
                track_ids,
            } => QueryIdentity::DuplicateMemberSelection(fingerprint, track_ids),
        }
    }
}

impl LibrarySnapshot {
    pub fn query_revisions(
        &self,
        queue_revision: u64,
        playlist_id: Option<i64>,
    ) -> crate::response::QueryRevisions {
        crate::response::QueryRevisions {
            library: self.revision,
            structure: self.structure_revision,
            playlist: self.playlist_revision,
            queue: queue_revision,
            duplicate_groups: self.duplicate_groups_revision,
            duplicate_members: self.duplicate_members_revision,
            track: None,
            playlist_metadata: playlist_id
                .filter(|id| self.playlist_entry_revisions.contains_key(id))
                .map(|id| crate::response::PlaylistEntryRevision {
                    playlist_id: id,
                    revision: self.metadata_revisions.playlist_revision(id, self.revision),
                }),
            playlist_entries: playlist_id.and_then(|id| {
                self.playlist_entry_revisions.get(&id).map(|revision| {
                    crate::response::PlaylistEntryRevision {
                        playlist_id: id,
                        revision: *revision,
                    }
                })
            }),
        }
    }

    pub(crate) fn change_metadata(&mut self, tracks: Option<&[i64]>, playlists: Option<&[i64]>) {
        let previous = self.revision;
        self.revision = previous.wrapping_add(1);
        Arc::make_mut(&mut self.metadata_revisions).record(
            previous,
            self.revision,
            tracks,
            playlists,
        );
    }

    pub(crate) fn query_revisions_for(
        &self,
        query: &Query,
        queue_revision: u64,
    ) -> crate::response::QueryRevisions {
        let playlist_id = match query {
            Query::PlaylistEntries { playlist_id, .. }
            | Query::PlaylistEntryBatch { playlist_id, .. }
            | Query::PlaylistEntryLocations { playlist_id, .. } => Some(*playlist_id),
            _ => None,
        };
        let mut revisions = self.query_revisions(queue_revision, playlist_id);
        if let Query::Track { track_id } = query {
            revisions.track = Some(crate::response::TrackRevision {
                track_id: *track_id,
                revision: self
                    .metadata_revisions
                    .track_revision(*track_id, self.revision),
            });
        }
        revisions
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    PlayAlbum {
        key: AlbumKey,
    },
    EnqueueAlbum {
        key: AlbumKey,
    },
    OptimizeDatabase,
    SetFavorite {
        track_ids: Vec<i64>,
        favorite: bool,
    },
    ToggleFavorite {
        track_ids: Vec<i64>,
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
    CreatePlaylistFromQueue {
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
    RemovePlaylistEntries {
        playlist_id: i64,
        entry_ids: Vec<i64>,
        expected_revision: u64,
    },
    MovePlaylistEntries {
        playlist_id: i64,
        entry_ids: Vec<i64>,
        before_entry_id: Option<i64>,
        expected_revision: u64,
    },
    ImportPlaylist {
        path: PathBuf,
        name: Option<String>,
    },
    ExportPlaylist {
        playlist_id: i64,
        path: PathBuf,
    },
    /// Only supplied fields are changed; an empty string explicitly clears text.
    EditTrack {
        track_id: i64,
        title: Option<String>,
        artist: Option<String>,
        album: Option<String>,
        decoder_hint: Option<DecoderHint>,
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
        config: Arc<crate::config::Config>,
    },
    MprisStatus {
        status: String,
    },
    DismissScanResult,
    CancelScan {
        id: u64,
    },
    /// Explicitly retain source identity/state at a freshly verified target path.
    RelocateTrack {
        source_id: i64,
        target_id: i64,
    },
    CancelRelocation {
        id: u64,
    },
    KeepTrackIndependent {
        track_id: i64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn search_projection_identity_changes_with_query_not_page_demand() {
        for mut query in [
            Query::AlbumTracks {
                key: AlbumKey {
                    directory: "/music".into(),
                    title: "Album".into(),
                },
                query: None,
                offset: 0,
                limit: 2,
            },
            Query::PlaylistSummaries {
                query: None,
                offset: 0,
                limit: 2,
            },
            Query::PlaylistEntries {
                playlist_id: 1,
                query: None,
                offset: 0,
                limit: 2,
            },
            Query::PlaylistEntryLocations {
                playlist_id: 1,
                query: None,
                entry_ids: vec![1],
            },
            Query::PlaylistSummaryBatch {
                query: None,
                after: None,
                limit: 2,
                expected: None,
            },
            Query::PlaylistEntryBatch {
                playlist_id: 1,
                query: None,
                after: None,
                limit: 2,
                expected: None,
            },
            Query::QueuePage {
                query: None,
                offset: 0,
                limit: 2,
            },
            Query::QueueBatch {
                query: None,
                after: None,
                limit: 2,
                expected: None,
            },
            Query::QueueEntries {
                query: None,
                queue_ids: vec![1],
            },
        ] {
            let unfiltered = query.clone();
            let (Query::AlbumTracks { query: filter, .. }
            | Query::PlaylistSummaries { query: filter, .. }
            | Query::PlaylistEntries { query: filter, .. }
            | Query::PlaylistEntryLocations { query: filter, .. }
            | Query::PlaylistSummaryBatch { query: filter, .. }
            | Query::PlaylistEntryBatch { query: filter, .. }
            | Query::QueuePage { query: filter, .. }
            | Query::QueueBatch { query: filter, .. }
            | Query::QueueEntries { query: filter, .. }) = &mut query
            else {
                unreachable!()
            };
            *filter = Some("needle".into());
            assert_ne!(unfiltered.identity(), query.identity());
            let filtered = query.clone();
            match &mut query {
                Query::AlbumTracks { offset, limit, .. }
                | Query::PlaylistSummaries { offset, limit, .. }
                | Query::PlaylistEntries { offset, limit, .. }
                | Query::QueuePage { offset, limit, .. } => {
                    *offset = 200;
                    *limit = 20;
                }
                Query::PlaylistSummaryBatch { after, limit, .. } => {
                    *after = Some(100);
                    *limit = 20;
                }
                Query::PlaylistEntryBatch { after, limit, .. } => {
                    *after = Some(BatchContinuation {
                        cursor: PlaylistEntryCursor {
                            position: 100,
                            id: 200,
                        },
                        offset: 50,
                        scope: [0; 32],
                    });
                    *limit = 20;
                }
                Query::QueueBatch { after, limit, .. } => {
                    *after = Some(BatchContinuation {
                        cursor: QueueCursor {
                            position: 100,
                            id: 200,
                        },
                        offset: 50,
                        scope: [0; 32],
                    });
                    *limit = 20;
                }
                _ => {}
            }
            assert_eq!(filtered.identity(), query.identity());
        }
    }

    #[test]
    fn filtered_occurrence_lookup_stamps_include_match_metadata() {
        let before = crate::response::QueryRevisions {
            library: 1,
            queue: 1,
            playlist_entries: Some(crate::response::PlaylistEntryRevision {
                playlist_id: 1,
                revision: 1,
            }),
            playlist_metadata: Some(crate::response::PlaylistEntryRevision {
                playlist_id: 1,
                revision: 1,
            }),
            ..Default::default()
        };
        let after = crate::response::QueryRevisions {
            library: 2,
            playlist_metadata: Some(crate::response::PlaylistEntryRevision {
                playlist_id: 1,
                revision: 2,
            }),
            ..before
        };
        for query in [None, Some("needle".into())] {
            let queue = Query::QueueEntries {
                query: query.clone(),
                queue_ids: vec![1],
            };
            let playlist = Query::PlaylistEntryLocations {
                playlist_id: 1,
                query: query.clone(),
                entry_ids: vec![1],
            };
            assert_eq!(queue.stamp(before) == queue.stamp(after), query.is_none());
            assert_eq!(
                playlist.stamp(before) == playlist.stamp(after),
                query.is_none()
            );
        }
    }

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

    #[test]
    fn metadata_scope_overflow_and_unknown_batches_invalidate_all_prior_scopes() {
        let mut library = LibrarySnapshot::default();
        let query = Query::Track { track_id: 10_000 };
        let initial = query.stamp(library.query_revisions_for(&query, 0));
        for id in 0..MAX_METADATA_SCOPES as i64 {
            library.change_metadata(Some(&[id]), Some(&[]));
        }
        assert_eq!(query.stamp(library.query_revisions_for(&query, 0)), initial);
        library.change_metadata(Some(&[MAX_METADATA_SCOPES as i64]), Some(&[]));
        let overflow = query.stamp(library.query_revisions_for(&query, 0));
        assert_ne!(overflow, initial);
        library.change_metadata(Some(&[1]), Some(&[]));
        assert_eq!(
            query.stamp(library.query_revisions_for(&query, 0)),
            overflow
        );
        library.change_metadata(None, None);
        assert_ne!(
            query.stamp(library.query_revisions_for(&query, 0)),
            overflow
        );
    }
}
