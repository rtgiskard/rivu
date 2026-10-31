mod processing;
pub use processing::*;
mod duplicates;
pub use duplicates::{DuplicateGroup, DuplicateMember};

mod query;
pub use query::{Query, QueryIdentity, QueryStamp};

use rivu_format::decoder_hint::DecoderHint;
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
    pub configuration: ConfigurationState,
}

impl ClientSnapshot {
    pub fn current_track(&self) -> Option<&Track> {
        self.current_track.as_deref()
    }
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

/// Only the metadata needed to write a playlist export.
#[derive(Clone, Debug, PartialEq)]
pub struct PlaylistExportTrack {
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    pub duration: Option<f64>,
    pub cue: Option<CueSegment>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub id: i64,
    pub path: PathBuf,
    pub fingerprint: Option<String>,
    pub cue: Option<CueSegment>,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: Option<f64>,
    pub codec: String,
    pub channels: u16,
    pub sample_rate: u32,
    pub bitrate_bps: Option<u64>,
    pub bitrate_exact: bool,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub bits_per_sample: Option<u32>,
    pub release_date: Option<String>,
    pub favorite: bool,
    pub decoder_hint: DecoderHint,
    pub missing: bool,
    pub play_count: u64,
    pub last_played: Option<i64>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LibraryRow {
    pub id: i64,
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: Option<f64>,
    pub codec: String,
    pub channels: u16,
    pub sample_rate: u32,
    pub bits_per_sample: Option<u32>,
    pub cue_number: Option<u32>,
    pub bitrate_bps: Option<u64>,
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
    OtherTracks,
    Directory { path: PathBuf },
    Track(LibraryRow),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistSummary {
    pub id: i64,
    pub name: String,
    pub entry_count: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlaylistEntryRow {
    pub id: i64,
    pub track_id: i64,
    pub path: PathBuf,
    /// Original occurrence position, never a filtered row index.
    pub position: usize,
    /// Zero-based index among the current query's matches.
    pub offset: usize,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: Option<f64>,
    pub codec: String,
    pub channels: u16,
    pub sample_rate: u32,
    pub bits_per_sample: Option<u32>,
    pub bitrate_bps: Option<u64>,
    pub bitrate_exact: bool,
    pub cue_number: Option<u32>,
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

/// Catalog totals; CUE segments share bytes by source path, not fingerprint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryRootStats {
    pub path: PathBuf,
    pub total: usize,
    pub file_size: u64,
}

/// Maximum number of rows in a query batch or interactive page.
pub use rivu_format::config::MAX_QUERY_ROWS;

/// Quiet interval before interactive search submits its final query.
pub const SEARCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(150);

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

pub use rivu_format::config::RepeatMode;

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
    pub track_total: usize,
    pub playlist_total: usize,
    /// Changes whenever non-bitrate Track metadata changes.
    pub revision: u64,
    /// Changes only when the library tree or visible row ordering may change.
    pub structure_revision: u64,
    /// Core-owned album membership/order/query token, shared with transfers.
    pub album_revision: u64,
    /// Playlist summaries: identity, name, order, or member count. Not a content epoch.
    pub playlist_revision: u64,
    /// Content/lifetime tokens for every extant playlist; absence means deletion.
    pub playlist_entry_revisions: Arc<BTreeMap<i64, u64>>,
    pub metadata_revisions: Arc<MetadataRevisions>,
    /// Changes only after a committed bitrate pair actually changes.
    pub bitrate_revision: u64,
    pub duplicate_groups_revision: u64,
    pub duplicate_members_revision: u64,
    pub history: Arc<Vec<HistoryEntry>>,
}

/// Exact recent metadata scopes; unknown or overflowing batches reset all scopes.
/// The bound covers both maps, independently of library and playlist sizes.
pub const MAX_METADATA_SCOPES: usize = 256;

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

    pub fn track_revision(&self, id: i64, library: u64) -> u64 {
        if self.revision == library {
            self.tracks.get(&id).copied().unwrap_or(self.baseline)
        } else {
            library
        }
    }

    pub fn playlist_revision(&self, id: i64, library: u64) -> u64 {
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
    pub last_heard_at: Option<i64>,
    pub volume: f32,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub seek_revision: u64,
}

/// Identity of a task whose owner-committed terminal outcome can be awaited.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TaskTarget {
    Scan { id: u64 },
    Relocation { id: u64 },
    Processing { id: u64 },
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
    /// The library commit was confirmed, even if a later task step failed.
    /// On failure, false does not guarantee that no database changes occurred.
    pub library_committed: bool,
    /// A playlist was committed by this import, independent of refresh success.
    /// On failure, None does not prove that no playlist was created.
    pub playlist_id: Option<i64>,
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

/// Cold configuration state, independently requested and invalidated from runtime status.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConfigurationState {
    pub config: Arc<rivu_format::config::Config>,
    pub config_path: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SystemState {
    pub scanning: bool,
    pub scan_message: String,
    pub scan_progress: Option<ScanProgress>,
    pub scan_task_id: Option<u64>,
    pub last_error: Option<String>,
    pub devices: Arc<Vec<String>>,
    pub selected_device: Option<String>,
    /// In-process publication/wakeup sequence; not a query-domain invalidation token.
    pub revision: u64,
    pub mpris_status: String,
    pub ffmpeg_status: String,
    pub database_optimization: Option<DatabaseOptimization>,
    pub shutting_down: bool,
    pub relocation_task: Option<RelocationTaskStatus>,
    pub processing: Option<ProcessingTaskStatus>,
}
impl Default for SystemState {
    fn default() -> Self {
        Self {
            scanning: false,
            scan_message: String::new(),
            scan_progress: None,
            scan_task_id: None,
            last_error: None,
            devices: Arc::new(Vec::new()),
            selected_device: None,
            revision: 0,
            mpris_status: String::new(),
            ffmpeg_status: "disabled".into(),
            database_optimization: None,
            shutting_down: false,
            relocation_task: None,
            processing: None,
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
            album_revision: 0,
            playlist_revision: 0,
            playlist_entry_revisions: Arc::default(),
            metadata_revisions: Arc::default(),
            bitrate_revision: 0,
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

impl LibrarySnapshot {
    pub fn query_revisions(
        &self,
        queue_revision: u64,
        playlist_id: Option<i64>,
    ) -> crate::response::QueryRevisions {
        crate::response::QueryRevisions::from_library(
            self.revision,
            self.structure_revision,
            self.playlist_revision,
            queue_revision,
            self.duplicate_groups_revision,
            self.duplicate_members_revision,
        )
        .with_album_revision(self.album_revision)
        .with_bitrate_revision(self.bitrate_revision)
        .with_playlist_scope(
            playlist_id,
            &self.playlist_entry_revisions,
            &self.metadata_revisions,
        )
    }

    pub fn change_metadata(&mut self, tracks: Option<&[i64]>, playlists: Option<&[i64]>) {
        let previous = self.revision;
        self.revision = previous.wrapping_add(1);
        Arc::make_mut(&mut self.metadata_revisions).record(
            previous,
            self.revision,
            tracks,
            playlists,
        );
    }

    pub fn query_revisions_for(
        &self,
        query: &Query,
        queue_revision: u64,
    ) -> crate::response::QueryRevisions {
        self.query_revisions(queue_revision, None).for_query(
            query,
            &self.playlist_entry_revisions,
            &self.metadata_revisions,
        )
    }
}

/// A whole collection pinned to the membership/order version selected by its caller.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum CollectionSource {
    Album { key: AlbumKey, revision: u64 },
    Playlist { playlist_id: i64, revision: u64 },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct CollectionInsertion {
    pub index: u64,
    pub revision: u64,
}

/// Copies retain every source occurrence in order for both queue and playlist destinations.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum CollectionDestination {
    Queue {
        insertion: Option<CollectionInsertion>,
    },
    Playlist {
        playlist_id: i64,
        revision: u64,
        index: Option<u64>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    TransferCollection {
        source: CollectionSource,
        destination: CollectionDestination,
    },
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
        force: bool,
    },
    CheckOtherTracks {
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
    SeekRelative {
        queue_id: u64,
        delta: f64,
    },
    Volume {
        value: f32,
    },
    AdjustVolume {
        delta: f32,
    },
    Enqueue {
        track_ids: Vec<i64>,
    },
    QueueExternalPaths {
        paths: Vec<PathBuf>,
        index: Option<usize>,
        expected_revision: Option<u64>,
    },
    EnqueueSources {
        #[serde(default)]
        other_tracks: bool,
        directories: Vec<PathBuf>,
        track_ids: Vec<i64>,
    },
    InsertQueue {
        track_ids: Vec<i64>,
        index: usize,
        expected_revision: u64,
    },
    InsertQueueSources {
        #[serde(default)]
        other_tracks: bool,
        directories: Vec<PathBuf>,
        track_ids: Vec<i64>,
        index: usize,
        expected_revision: u64,
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
    CycleRepeat,
    ToggleShuffle,
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
    MovePlaylist {
        playlist_id: i64,
        before_playlist_id: Option<i64>,
        after: bool,
        expected_revision: u64,
    },
    AddPlaylist {
        playlist_id: i64,
        track_ids: Vec<i64>,
    },
    AddPlaylistSources {
        #[serde(default)]
        other_tracks: bool,
        playlist_id: i64,
        directories: Vec<PathBuf>,
        track_ids: Vec<i64>,
    },
    InsertPlaylist {
        playlist_id: i64,
        track_ids: Vec<i64>,
        index: usize,
        expected_revision: u64,
    },
    InsertPlaylistSources {
        #[serde(default)]
        other_tracks: bool,
        playlist_id: i64,
        directories: Vec<PathBuf>,
        track_ids: Vec<i64>,
        index: usize,
        expected_revision: u64,
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
        after: bool,
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
        config: Arc<rivu_format::config::Config>,
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
    /// Return current playback to its source and disable monitor EQ without
    /// changing stored effect parameters or cancelling offline work.
    ProcessingBypass,
    EqualizerEdit {
        edit: EqualizerEdit,
    },
    DenoiseEdit {
        patch: Box<DenoisePatch>,
    },
    NeuralEdit {
        patch: Box<NeuralPatch>,
    },
    ProcessingGraphEdit {
        expected_revision: u64,
        edit: Box<GraphEdit>,
    },
    ProcessingGraphRun {
        track_id: i64,
        target: GraphPort,
        mode: GraphExecutionMode,
    },
    ProcessingGraphAudition {
        id: u64,
        node: GraphPort,
    },
    ProcessingGraphExport {
        id: u64,
        node: GraphPort,
        path: PathBuf,
    },
    StartProcessing {
        track_id: i64,
        operation: ProcessingOperation,
        profile: Option<NoiseSelection>,
    },
    CancelProcessing {
        id: u64,
    },
    AuditionProcessing {
        id: u64,
        mode: AuditionMode,
        vocal_gain_db: f32,
        accompaniment_gain_db: f32,
    },
    DeduplicatePlaylist {
        playlist_id: i64,
        expected_revision: u64,
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
    fn query_scopes_require_playlist_presence_and_fall_back_to_library_metadata() {
        let mut library = LibrarySnapshot::default();
        Arc::make_mut(&mut library.playlist_entry_revisions).insert(7, 12);
        library.change_metadata(Some(&[3]), Some(&[7]));
        let query = Query::PlaylistEntryLocations {
            playlist_id: 7,
            query: None,
            entry_ids: vec![1],
        };
        let present = library.query_revisions_for(&query, 9);
        assert_eq!(present.playlist_entries.unwrap().revision, 12);
        assert_eq!(present.playlist_metadata.unwrap().revision, 1);
        assert_eq!(present.queue, 9);
        assert_eq!(present.track, None);

        Arc::make_mut(&mut library.playlist_entry_revisions).remove(&7);
        let deleted = library.query_revisions_for(&query, 9);
        assert_eq!(deleted.playlist_entries, None);
        assert_eq!(deleted.playlist_metadata, None);

        Arc::make_mut(&mut library.playlist_entry_revisions).insert(7, 13);
        // A snapshot without matching metadata history uses the global revision.
        library.revision = 20;
        let restored = library.query_revisions_for(&query, 9);
        assert_eq!(restored.playlist_entries.unwrap().revision, 13);
        assert_eq!(restored.playlist_metadata.unwrap().revision, 20);
        let track = library.query_revisions_for(&Query::Track { track_id: 3 }, 9);
        assert_eq!(track.track.unwrap().revision, 20);
        assert_eq!(track.playlist_entries, None);
        assert_eq!(track.playlist_metadata, None);
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

    #[test]
    fn bitrate_revision_changes_only_payload_query_stamps() {
        let before = crate::response::QueryRevisions::default();
        let after = crate::response::QueryRevisions {
            bitrate: 1,
            ..before
        };
        let key = AlbumKey {
            directory: "/music".into(),
            title: "Album".into(),
        };
        let payloads = [
            Query::LibraryPage {
                query: None,
                favorite: None,
                missing: None,
                sort: LibrarySort::Id,
                offset: 0,
                limit: 1,
            },
            Query::TrackPage {
                query: None,
                favorite: None,
                missing: None,
                offset: 0,
                limit: 1,
            },
            Query::TrackBatch {
                query: None,
                favorite: None,
                missing: None,
                after: None,
                limit: 1,
                expected: None,
            },
            Query::Track { track_id: 1 },
            Query::Albums {
                query: None,
                sort: AlbumSort::default(),
                offset: 0,
                limit: 1,
            },
            Query::AlbumTracks {
                key,
                query: None,
                offset: 0,
                limit: 1,
            },
            Query::DirectoryPage {
                path: "/music".into(),
                query: None,
                favorite: None,
                missing: None,
                offset: 0,
                limit: 1,
            },
            Query::OtherTracksPage {
                query: None,
                favorite: None,
                missing: None,
                sort: LibrarySort::Id,
                offset: 0,
                limit: 1,
            },
            Query::LibraryRootPage {
                path: "/music".into(),
                query: None,
                favorite: None,
                missing: None,
                sort: LibrarySort::Id,
                offset: 0,
                limit: 1,
            },
            Query::PlaylistEntries {
                playlist_id: 1,
                query: None,
                offset: 0,
                limit: 1,
            },
            Query::PlaylistEntryBatch {
                playlist_id: 1,
                query: None,
                after: None,
                limit: 1,
                expected: None,
            },
            Query::QueuePage {
                query: None,
                offset: 0,
                limit: 1,
            },
            Query::QueueBatch {
                query: None,
                after: None,
                limit: 1,
                expected: None,
            },
            Query::QueueEntries {
                query: None,
                queue_ids: vec![1],
            },
            Query::DuplicateMembers {
                fingerprint: "fingerprint".into(),
                offset: 0,
                limit: 1,
            },
            Query::DuplicateMemberSelection {
                fingerprint: "fingerprint".into(),
                track_ids: vec![1],
            },
        ];
        for query in payloads {
            assert_ne!(query.stamp(before), query.stamp(after), "{query:?}");
        }
        let identities_and_scalars = [
            Query::LibraryStats,
            Query::OtherTracksCount,
            Query::LibraryRootCounts {
                roots: vec!["/music".into()],
            },
            Query::PlaylistSummaries {
                query: None,
                offset: 0,
                limit: 1,
            },
            Query::PlaylistSummaryBatch {
                query: None,
                after: None,
                limit: 1,
                expected: None,
            },
            Query::PlaylistEntry { entry_id: 1 },
            Query::PlaylistEntryLocations {
                playlist_id: 1,
                query: Some("needle".into()),
                entry_ids: vec![1],
            },
            Query::LibraryLocate {
                track_id: 1,
                path: "/music".into(),
                tree: true,
                query: None,
                favorite: None,
                missing: None,
                sort: LibrarySort::Id,
            },
            Query::OtherTracksLocate {
                track_id: 1,
                query: None,
                favorite: None,
                missing: None,
                sort: LibrarySort::Id,
            },
            Query::DuplicateGroups {
                offset: 0,
                limit: 1,
            },
            Query::ScanTask { id: 1 },
            Query::RelocationTask { id: 1 },
            Query::ProcessingTask { id: 1 },
        ];
        for query in identities_and_scalars {
            assert_eq!(query.stamp(before), query.stamp(after), "{query:?}");
        }
    }

    #[test]
    fn occurrence_continuations_include_bitrate_payload_freshness() {
        let before = crate::response::QueryRevisions::default();
        let after = crate::response::QueryRevisions {
            bitrate: 1,
            ..before
        };
        for (query, playlist_id) in [
            (
                Query::QueueBatch {
                    query: None,
                    after: None,
                    limit: 1,
                    expected: None,
                },
                None,
            ),
            (
                Query::PlaylistEntryBatch {
                    playlist_id: 1,
                    query: None,
                    after: None,
                    limit: 1,
                    expected: None,
                },
                Some(1),
            ),
        ] {
            assert_ne!(
                query.stamp(before).continuation_scope(None, playlist_id),
                query.stamp(after).continuation_scope(None, playlist_id)
            );
        }
    }

    #[test]
    fn statistics_change_detail_freshness_without_changing_bitrate_revision() {
        let mut library = LibrarySnapshot {
            bitrate_revision: 7,
            ..LibrarySnapshot::default()
        };
        let detail = Query::Track { track_id: 1 };
        let before = detail.stamp(library.query_revisions_for(&detail, 0));
        library.change_metadata(Some(&[1]), Some(&[]));
        assert_ne!(
            before,
            detail.stamp(library.query_revisions_for(&detail, 0))
        );
        assert_eq!(library.bitrate_revision, 7);
    }
}
