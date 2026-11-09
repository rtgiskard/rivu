pub(crate) mod endpoint;
pub use endpoint::{address, connect, verify_peer};
#[macro_use]
mod config_fields;
mod config_patch;
pub use config_patch::WireConfigMutation;

use crate::{
    AppCommand,
    model::{Command, ConfigurationState, Query, SystemState},
    response::{Ack, QueryResponse, StateResponse, StateRevisions, StateSections, StateWatch},
};
use anyhow::{Context, Result, bail};
use rivu_format::decoder_hint::DecoderHint;
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc, time::Duration};

pub const PROTOCOL_VERSION: u8 = 21;
pub const MAX_REQUEST: usize = 4 * 1024 * 1024;
pub const MAX_RESPONSE: usize = 16 * 1024 * 1024;
pub const MAX_CLIENTS: usize = 16;
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(1);
pub const UNKNOWN_INSTANCE: u32 = 0;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestTag {
    Hello = 0,
    State = 1,
    Query = 2,
    Command = 3,
    Watch = 4,
    WaitTask = 5,
    QueryRetained = 6,
    RegisterDesktop = 7,
    QueryCancel = 8,
}

impl TryFrom<u8> for RequestTag {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self> {
        Ok(match value {
            0 => Self::Hello,
            1 => Self::State,
            2 => Self::Query,
            3 => Self::Command,
            4 => Self::Watch,
            5 => Self::WaitTask,
            6 => Self::QueryRetained,
            7 => Self::RegisterDesktop,
            8 => Self::QueryCancel,
            _ => bail!("Unknown IPC request type {value}"),
        })
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseTag {
    Hello = 0,
    State = 1,
    Query = 2,
    Ack = 3,
    Watch = 4,
    Task = 5,
    Error = 255,
}

impl TryFrom<u8> for ResponseTag {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self> {
        Ok(match value {
            0 => Self::Hello,
            1 => Self::State,
            2 => Self::Query,
            3 => Self::Ack,
            4 => Self::Watch,
            5 => Self::Task,
            255 => Self::Error,
            _ => bail!("Unknown IPC response type {value}"),
        })
    }
}
pub enum SessionState {
    Unbound,
    Bound(u32),
}

impl SessionState {
    pub fn is_bound(&self) -> bool {
        matches!(self, Self::Bound(_))
    }
}

pub enum WireRequest {
    Hello(StateSections),
    State(StateSections),
    Query(WireQuery),
    Command(WireCommand),
    Watch(StateWatch),
    WaitTask(crate::model::TaskTarget),
    QueryRetained(WireRetainedQuery),
    RegisterDesktop,
    /// Serial control frame: cancels only the matching read and has no reply.
    QueryCancel(u64),
}

#[derive(Serialize, Deserialize)]
pub struct WireRetainedQuery {
    pub id: u64,
    pub query: WireQuery,
}

/// Binary query wire format. Variant order belongs to the versioned wire schema.
#[derive(Serialize, Deserialize)]
pub enum WireQuery {
    LibraryPage {
        query: Option<String>,
        favorite: Option<bool>,
        missing: Option<bool>,
        sort: crate::model::LibrarySort,
        offset: u64,
        limit: u64,
    },
    TrackPage {
        query: Option<String>,
        favorite: Option<bool>,
        missing: Option<bool>,
        offset: u64,
        limit: u64,
    },
    DirectoryPage {
        path: PathBuf,
        query: Option<String>,
        favorite: Option<bool>,
        missing: Option<bool>,
        offset: u64,
        limit: u64,
    },
    PlaylistSummaries {
        query: Option<String>,
        offset: u64,
        limit: u64,
    },
    PlaylistEntries {
        query: Option<String>,
        playlist_id: i64,
        offset: u64,
        limit: u64,
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
        limit: u64,
        expected: Option<crate::response::QueryRevisions>,
    },
    PlaylistSummaryBatch {
        query: Option<String>,
        after: Option<i64>,
        limit: u64,
        expected: Option<crate::response::QueryRevisions>,
    },
    PlaylistEntryBatch {
        query: Option<String>,
        playlist_id: i64,
        after: Option<crate::model::BatchContinuation<crate::model::PlaylistEntryCursor>>,
        limit: u64,
        expected: Option<crate::response::QueryRevisions>,
    },
    LibraryRootPage {
        path: PathBuf,
        query: Option<String>,
        favorite: Option<bool>,
        missing: Option<bool>,
        sort: crate::model::LibrarySort,
        offset: u64,
        limit: u64,
    },
    LibraryLocate {
        track_id: i64,
        path: PathBuf,
        tree: bool,
        query: Option<String>,
        favorite: Option<bool>,
        missing: Option<bool>,
        sort: crate::model::LibrarySort,
    },
    LibraryRootCounts {
        roots: Vec<PathBuf>,
    },
    LibraryRootStats {
        roots: Vec<PathBuf>,
    },
    ScanTask {
        id: u64,
    },
    QueuePage {
        query: Option<String>,
        offset: u64,
        limit: u64,
    },
    QueueBatch {
        query: Option<String>,
        after: Option<crate::model::BatchContinuation<crate::model::QueueCursor>>,
        limit: u64,
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
        offset: u64,
        limit: u64,
    },
    DuplicateMembers {
        fingerprint: String,
        offset: u64,
        limit: u64,
    },
    RelocationTask {
        id: u64,
    },
    ProcessingTask {
        id: u64,
    },
    PlaylistEntryLocations {
        query: Option<String>,
        playlist_id: i64,
        entry_ids: Vec<i64>,
    },
    Albums {
        query: Option<String>,
        sort: crate::model::AlbumSort,
        offset: u64,
        limit: u64,
    },
    AlbumTracks {
        query: Option<String>,
        key: crate::model::AlbumKey,
        offset: u64,
        limit: u64,
    },
    DuplicateMemberSelection {
        fingerprint: String,
        track_ids: Vec<i64>,
    },
    OtherTracksPage {
        query: Option<String>,
        favorite: Option<bool>,
        missing: Option<bool>,
        sort: crate::model::LibrarySort,
        offset: u64,
        limit: u64,
    },
    OtherTracksLocate {
        track_id: i64,
        query: Option<String>,
        favorite: Option<bool>,
        missing: Option<bool>,
        sort: crate::model::LibrarySort,
    },
    OtherTracksCount,
    PlaylistSummary {
        playlist_id: i64,
    },
}

/// Binary command wire format. Variant order belongs to the versioned wire schema.
#[derive(Serialize, Deserialize)]
pub enum WireCommand {
    TransferCollection {
        source: crate::model::CollectionSource,
        destination: crate::model::CollectionDestination,
    },
    ShowWindow,
    OptimizeDatabase,
    SetFavorite {
        track_ids: Vec<i64>,
        favorite: bool,
    },
    RemoveMissingTracks,
    Scan {
        paths: Vec<PathBuf>,
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
        #[serde(default)]
        other_tracks: bool,
    },
    InsertQueue {
        track_ids: Vec<i64>,
        index: u64,
        expected_revision: u64,
    },
    InsertQueueSources {
        directories: Vec<PathBuf>,
        track_ids: Vec<i64>,
        #[serde(default)]
        other_tracks: bool,
        index: u64,
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
        index: u64,
    },
    MoveQueueEntries {
        queue_ids: Vec<u64>,
        index: u64,
    },
    ClearQueue,
    RandomizeQueue,
    DeduplicateQueue,
    Shuffle {
        enabled: bool,
    },
    Repeat {
        mode: crate::model::RepeatMode,
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
        #[serde(default)]
        other_tracks: bool,
    },
    InsertPlaylist {
        playlist_id: i64,
        track_ids: Vec<i64>,
        index: u64,
        expected_revision: u64,
    },
    InsertPlaylistSources {
        playlist_id: i64,
        directories: Vec<PathBuf>,
        track_ids: Vec<i64>,
        #[serde(default)]
        other_tracks: bool,
        index: u64,
        expected_revision: u64,
    },
    RemovePlaylistEntry {
        entry_id: i64,
    },
    MovePlaylistEntry {
        entry_id: i64,
        index: u64,
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
        title: Option<String>,
        artist: Option<String>,
        album: Option<String>,
        decoder_hint: Option<DecoderHint>,
    },
    RemoveTracks {
        track_ids: Vec<i64>,
    },
    RemoveHistory {
        track_ids: Vec<i64>,
    },
    Device {
        name: Option<String>,
    },
    DismissError,
    Configure {
        config: Box<WireConfig>,
    },
    MprisStatus {
        status: String,
    },
    Shutdown,
    DismissScanResult,
    CancelScan {
        id: u64,
    },
    CreatePlaylistFromQueue {
        name: String,
    },
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
    PlayAlbum {
        key: crate::model::AlbumKey,
    },
    EnqueueAlbum {
        key: crate::model::AlbumKey,
    },
    ToggleFavorite {
        track_ids: Vec<i64>,
    },
    AdjustVolume {
        delta: f32,
    },
    CycleRepeat,
    ToggleShuffle,
    SeekRelative {
        queue_id: u64,
        delta: f64,
    },
    EqualizerEdit {
        edit: crate::model::EqualizerEdit,
    },
    DenoiseEdit {
        patch: Box<crate::model::DenoisePatch>,
    },
    NeuralEdit {
        patch: Box<crate::model::NeuralPatch>,
    },
    StartProcessing {
        track_id: i64,
        operation: crate::model::ProcessingOperation,
        profile: Option<crate::model::NoiseSelection>,
    },
    CancelProcessing {
        id: u64,
    },
    AuditionProcessing {
        id: u64,
        mode: crate::model::AuditionMode,
        vocal_gain_db: f32,
        accompaniment_gain_db: f32,
    },
    ProcessingGraphEdit {
        expected_revision: u64,
        edit: Box<crate::model::GraphEdit>,
    },
    ProcessingGraphRun {
        track_id: i64,
        target: crate::model::GraphPort,
        mode: crate::model::GraphExecutionMode,
    },
    ProcessingGraphAudition {
        id: u64,
        node: crate::model::GraphPort,
    },
    ProcessingGraphExport {
        id: u64,
        node: crate::model::GraphPort,
        path: PathBuf,
    },
    CheckOtherTracks {
        force: bool,
    },
    QueueExternalPaths {
        paths: Vec<PathBuf>,
        index: Option<u64>,
        expected_revision: Option<u64>,
    },
    ProcessingBypass {
        bypassed: bool,
    },
    DeduplicatePlaylist {
        playlist_id: i64,
        expected_revision: u64,
    },
    MovePlaylist {
        playlist_id: i64,
        before_playlist_id: Option<i64>,
        after: bool,
        expected_revision: u64,
    },
    SetPlayCount {
        track_id: i64,
        play_count: u64,
    },
    MutateConfig {
        change: WireConfigMutation,
    },
}

pub fn wire_usize(value: u64, field: &str) -> Result<usize> {
    usize::try_from(value).with_context(|| format!("{field} does not fit in usize"))
}

impl From<Query> for WireQuery {
    fn from(value: Query) -> Self {
        match value {
            Query::OtherTracksPage {
                query,
                favorite,
                missing,
                sort,
                offset,
                limit,
            } => Self::OtherTracksPage {
                query,
                favorite,
                missing,
                sort,
                offset: offset as u64,
                limit: limit as u64,
            },
            Query::OtherTracksLocate {
                track_id,
                query,
                favorite,
                missing,
                sort,
            } => Self::OtherTracksLocate {
                track_id,
                query,
                favorite,
                missing,
                sort,
            },
            Query::OtherTracksCount => Self::OtherTracksCount,
            Query::Albums {
                query,
                sort,
                offset,
                limit,
            } => Self::Albums {
                query,
                sort,
                offset: offset as u64,
                limit: limit as u64,
            },
            Query::PlaylistSummary { playlist_id } => Self::PlaylistSummary { playlist_id },
            Query::AlbumTracks {
                key,
                query,
                offset,
                limit,
            } => Self::AlbumTracks {
                query,
                key,
                offset: offset as u64,
                limit: limit as u64,
            },
            Query::LibraryPage {
                query,
                favorite,
                missing,
                sort,
                offset,
                limit,
            } => Self::LibraryPage {
                query,
                favorite,
                missing,
                sort,
                offset: offset as u64,
                limit: limit as u64,
            },
            Query::TrackPage {
                query,
                favorite,
                missing,
                offset,
                limit,
            } => Self::TrackPage {
                query,
                favorite,
                missing,
                offset: offset as u64,
                limit: limit as u64,
            },
            Query::DirectoryPage {
                path,
                query,
                favorite,
                missing,
                offset,
                limit,
            } => Self::DirectoryPage {
                path,
                query,
                favorite,
                missing,
                offset: offset as u64,
                limit: limit as u64,
            },
            Query::PlaylistSummaries {
                query,
                offset,
                limit,
            } => Self::PlaylistSummaries {
                query,
                offset: offset as u64,
                limit: limit as u64,
            },
            Query::PlaylistEntries {
                query,
                playlist_id,
                offset,
                limit,
            } => Self::PlaylistEntries {
                query,
                playlist_id,
                offset: offset as u64,
                limit: limit as u64,
            },
            Query::PlaylistEntryLocations {
                query,
                playlist_id,
                entry_ids,
            } => Self::PlaylistEntryLocations {
                query,
                playlist_id,
                entry_ids,
            },
            Query::Track { track_id } => Self::Track { track_id },
            Query::LibraryStats => Self::LibraryStats,
            Query::TrackBatch {
                query,
                favorite,
                missing,
                after,
                limit,
                expected,
            } => Self::TrackBatch {
                query,
                favorite,
                missing,
                after,
                limit: limit as u64,
                expected,
            },
            Query::PlaylistSummaryBatch {
                query,
                after,
                limit,
                expected,
            } => Self::PlaylistSummaryBatch {
                query,
                after,
                limit: limit as u64,
                expected,
            },
            Query::PlaylistEntryBatch {
                query,
                playlist_id,
                after,
                limit,
                expected,
            } => Self::PlaylistEntryBatch {
                query,
                playlist_id,
                after,
                limit: limit as u64,
                expected,
            },
            Query::LibraryRootPage {
                path,
                query,
                favorite,
                missing,
                sort,
                offset,
                limit,
            } => Self::LibraryRootPage {
                path,
                query,
                favorite,
                missing,
                sort,
                offset: offset as u64,
                limit: limit as u64,
            },
            Query::LibraryLocate {
                track_id,
                path,
                tree,
                query,
                favorite,
                missing,
                sort,
            } => Self::LibraryLocate {
                track_id,
                path,
                tree,
                query,
                favorite,
                missing,
                sort,
            },
            Query::LibraryRootCounts { roots } => Self::LibraryRootCounts { roots },
            Query::LibraryRootStats { roots } => Self::LibraryRootStats { roots },
            Query::ScanTask { id } => Self::ScanTask { id },
            Query::QueuePage {
                query,
                offset,
                limit,
            } => Self::QueuePage {
                query,
                offset: offset as u64,
                limit: limit as u64,
            },
            Query::QueueBatch {
                query,
                after,
                limit,
                expected,
            } => Self::QueueBatch {
                query,
                after,
                limit: limit as u64,
                expected,
            },
            Query::QueueEntries { query, queue_ids } => Self::QueueEntries { query, queue_ids },
            Query::PlaylistEntry { entry_id } => Self::PlaylistEntry { entry_id },
            Query::DuplicateGroups { offset, limit } => Self::DuplicateGroups {
                offset: offset as u64,
                limit: limit as u64,
            },
            Query::DuplicateMembers {
                fingerprint,
                offset,
                limit,
            } => Self::DuplicateMembers {
                fingerprint,
                offset: offset as u64,
                limit: limit as u64,
            },
            Query::DuplicateMemberSelection {
                fingerprint,
                track_ids,
            } => Self::DuplicateMemberSelection {
                fingerprint,
                track_ids,
            },
            Query::RelocationTask { id } => Self::RelocationTask { id },
            Query::ProcessingTask { id } => Self::ProcessingTask { id },
        }
    }
}

impl TryFrom<WireQuery> for Query {
    type Error = anyhow::Error;

    fn try_from(value: WireQuery) -> Result<Self> {
        Ok(match value {
            WireQuery::OtherTracksPage {
                query,
                favorite,
                missing,
                sort,
                offset,
                limit,
            } => Self::OtherTracksPage {
                query,
                favorite,
                missing,
                sort,
                offset: wire_usize(offset, "offset")?,
                limit: wire_usize(limit, "limit")?,
            },
            WireQuery::OtherTracksLocate {
                track_id,
                query,
                favorite,
                missing,
                sort,
            } => Self::OtherTracksLocate {
                track_id,
                query,
                favorite,
                missing,
                sort,
            },
            WireQuery::OtherTracksCount => Self::OtherTracksCount,
            WireQuery::Albums {
                query,
                sort,
                offset,
                limit,
            } => Self::Albums {
                query,
                sort,
                offset: wire_usize(offset, "offset")?,
                limit: wire_usize(limit, "limit")?,
            },
            WireQuery::AlbumTracks {
                key,
                query,
                offset,
                limit,
            } => Self::AlbumTracks {
                query,
                key,
                offset: wire_usize(offset, "offset")?,
                limit: wire_usize(limit, "limit")?,
            },
            WireQuery::LibraryPage {
                query,
                favorite,
                missing,
                sort,
                offset,
                limit,
            } => Self::LibraryPage {
                query,
                favorite,
                missing,
                sort,
                offset: wire_usize(offset, "offset")?,
                limit: wire_usize(limit, "limit")?,
            },
            WireQuery::TrackPage {
                query,
                favorite,
                missing,
                offset,
                limit,
            } => Self::TrackPage {
                query,
                favorite,
                missing,
                offset: wire_usize(offset, "offset")?,
                limit: wire_usize(limit, "limit")?,
            },
            WireQuery::DirectoryPage {
                path,
                query,
                favorite,
                missing,
                offset,
                limit,
            } => Self::DirectoryPage {
                path,
                query,
                favorite,
                missing,
                offset: wire_usize(offset, "offset")?,
                limit: wire_usize(limit, "limit")?,
            },
            WireQuery::PlaylistSummaries {
                query,
                offset,
                limit,
            } => Self::PlaylistSummaries {
                query,
                offset: wire_usize(offset, "offset")?,
                limit: wire_usize(limit, "limit")?,
            },
            WireQuery::PlaylistEntries {
                query,
                playlist_id,
                offset,
                limit,
            } => Self::PlaylistEntries {
                query,
                playlist_id,
                offset: wire_usize(offset, "offset")?,
                limit: wire_usize(limit, "limit")?,
            },
            WireQuery::PlaylistEntryLocations {
                query,
                playlist_id,
                entry_ids,
            } => Self::PlaylistEntryLocations {
                query,
                playlist_id,
                entry_ids,
            },
            WireQuery::Track { track_id } => Self::Track { track_id },
            WireQuery::LibraryStats => Self::LibraryStats,
            WireQuery::TrackBatch {
                query,
                favorite,
                missing,
                after,
                limit,
                expected,
            } => Self::TrackBatch {
                query,
                favorite,
                missing,
                after,
                limit: wire_usize(limit, "limit")?,
                expected,
            },
            WireQuery::PlaylistSummaryBatch {
                query,
                after,
                limit,
                expected,
            } => Self::PlaylistSummaryBatch {
                query,
                after,
                limit: wire_usize(limit, "limit")?,
                expected,
            },
            WireQuery::PlaylistEntryBatch {
                query,
                playlist_id,
                after,
                limit,
                expected,
            } => Self::PlaylistEntryBatch {
                query,
                playlist_id,
                after,
                limit: wire_usize(limit, "limit")?,
                expected,
            },
            WireQuery::LibraryRootPage {
                path,
                query,
                favorite,
                missing,
                sort,
                offset,
                limit,
            } => Self::LibraryRootPage {
                path,
                query,
                favorite,
                missing,
                sort,
                offset: wire_usize(offset, "offset")?,
                limit: wire_usize(limit, "limit")?,
            },
            WireQuery::LibraryLocate {
                track_id,
                path,
                tree,
                query,
                favorite,
                missing,
                sort,
            } => Self::LibraryLocate {
                track_id,
                path,
                tree,
                query,
                favorite,
                missing,
                sort,
            },
            WireQuery::LibraryRootCounts { roots } => Self::LibraryRootCounts { roots },
            WireQuery::LibraryRootStats { roots } => Self::LibraryRootStats { roots },
            WireQuery::ScanTask { id } => Self::ScanTask { id },
            WireQuery::QueuePage {
                query,
                offset,
                limit,
            } => Self::QueuePage {
                query,
                offset: wire_usize(offset, "offset")?,
                limit: wire_usize(limit, "limit")?,
            },
            WireQuery::QueueBatch {
                query,
                after,
                limit,
                expected,
            } => Self::QueueBatch {
                query,
                after,
                limit: wire_usize(limit, "limit")?,
                expected,
            },
            WireQuery::QueueEntries { query, queue_ids } => Self::QueueEntries { query, queue_ids },
            WireQuery::PlaylistEntry { entry_id } => Self::PlaylistEntry { entry_id },
            WireQuery::DuplicateGroups { offset, limit } => Self::DuplicateGroups {
                offset: wire_usize(offset, "offset")?,
                limit: wire_usize(limit, "limit")?,
            },
            WireQuery::DuplicateMembers {
                fingerprint,
                offset,
                limit,
            } => Self::DuplicateMembers {
                fingerprint,
                offset: wire_usize(offset, "offset")?,
                limit: wire_usize(limit, "limit")?,
            },
            WireQuery::DuplicateMemberSelection {
                fingerprint,
                track_ids,
            } => Self::DuplicateMemberSelection {
                fingerprint,
                track_ids,
            },
            WireQuery::RelocationTask { id } => Self::RelocationTask { id },
            WireQuery::ProcessingTask { id } => Self::ProcessingTask { id },
            WireQuery::PlaylistSummary { playlist_id } => Self::PlaylistSummary { playlist_id },
        })
    }
}

impl From<AppCommand> for WireCommand {
    fn from(value: AppCommand) -> Self {
        match value {
            AppCommand::ShowWindow => Self::ShowWindow,
            AppCommand::Shutdown => Self::Shutdown,
            AppCommand::Core(command) => command.into(),
        }
    }
}

impl TryFrom<WireCommand> for AppCommand {
    type Error = anyhow::Error;
    fn try_from(value: WireCommand) -> Result<Self> {
        Ok(match value {
            WireCommand::ShowWindow => Self::ShowWindow,
            WireCommand::Shutdown => Self::Shutdown,
            command => Self::Core(Command::try_from(command)?),
        })
    }
}

impl From<Command> for WireCommand {
    fn from(value: Command) -> Self {
        match value {
            Command::TransferCollection {
                source,
                destination,
            } => Self::TransferCollection {
                source,
                destination,
            },
            Command::CheckOtherTracks { force } => Self::CheckOtherTracks { force },
            Command::QueueExternalPaths {
                paths,
                index,
                expected_revision,
            } => Self::QueueExternalPaths {
                paths,
                index: index.map(|index| index as u64),
                expected_revision,
            },
            Command::PlayAlbum { key } => Self::PlayAlbum { key },
            Command::EnqueueAlbum { key } => Self::EnqueueAlbum { key },
            Command::OptimizeDatabase => Self::OptimizeDatabase,
            Command::SetPlayCount {
                track_id,
                play_count,
            } => Self::SetPlayCount {
                track_id,
                play_count,
            },
            Command::SetFavorite {
                track_ids,
                favorite,
            } => Self::SetFavorite {
                track_ids,
                favorite,
            },
            Command::ToggleFavorite { track_ids } => Self::ToggleFavorite { track_ids },
            Command::RemoveMissingTracks => Self::RemoveMissingTracks,
            Command::Scan { paths, force } => Self::Scan { paths, force },
            Command::Play { track_id } => Self::Play { track_id },
            Command::PlayQueue { queue_id } => Self::PlayQueue { queue_id },
            Command::PlayPlaylist { playlist_id } => Self::PlayPlaylist { playlist_id },
            Command::Resume => Self::Resume,
            Command::Pause => Self::Pause,
            Command::Toggle => Self::Toggle,
            Command::Stop => Self::Stop,
            Command::Next => Self::Next,
            Command::Previous => Self::Previous,
            Command::Seek { seconds } => Self::Seek { seconds },
            Command::SeekQueue { queue_id, seconds } => Self::SeekQueue { queue_id, seconds },
            Command::Volume { value } => Self::Volume { value },
            Command::AdjustVolume { delta } => Self::AdjustVolume { delta },
            Command::CycleRepeat => Self::CycleRepeat,
            Command::ToggleShuffle => Self::ToggleShuffle,
            Command::SeekRelative { queue_id, delta } => Self::SeekRelative { queue_id, delta },
            Command::Enqueue { track_ids } => Self::Enqueue { track_ids },
            Command::EnqueueSources {
                directories,
                track_ids,
                other_tracks,
            } => Self::EnqueueSources {
                directories,
                track_ids,
                other_tracks,
            },
            Command::InsertQueue {
                track_ids,
                index,
                expected_revision,
            } => Self::InsertQueue {
                track_ids,
                index: index as u64,
                expected_revision,
            },
            Command::InsertQueueSources {
                directories,
                track_ids,
                other_tracks,
                index,
                expected_revision,
            } => Self::InsertQueueSources {
                directories,
                track_ids,
                other_tracks,
                index: index as u64,
                expected_revision,
            },
            Command::RemoveQueue { queue_id } => Self::RemoveQueue { queue_id },
            Command::RemoveQueueEntries { queue_ids } => Self::RemoveQueueEntries { queue_ids },
            Command::MoveQueue { queue_id, index } => Self::MoveQueue {
                queue_id,
                index: index as u64,
            },
            Command::MoveQueueEntries { queue_ids, index } => Self::MoveQueueEntries {
                queue_ids,
                index: index as u64,
            },
            Command::ClearQueue => Self::ClearQueue,
            Command::RandomizeQueue => Self::RandomizeQueue,
            Command::DeduplicateQueue => Self::DeduplicateQueue,
            Command::Shuffle { enabled } => Self::Shuffle { enabled },
            Command::Repeat { mode } => Self::Repeat { mode },
            Command::CreatePlaylist { name } => Self::CreatePlaylist { name },
            Command::CreatePlaylistWithTracks { name, track_ids } => {
                Self::CreatePlaylistWithTracks { name, track_ids }
            }
            Command::RenamePlaylist { playlist_id, name } => {
                Self::RenamePlaylist { playlist_id, name }
            }
            Command::DeletePlaylist { playlist_id } => Self::DeletePlaylist { playlist_id },
            Command::MovePlaylist {
                playlist_id,
                before_playlist_id,
                after,
                expected_revision,
            } => Self::MovePlaylist {
                playlist_id,
                before_playlist_id,
                after,
                expected_revision,
            },
            Command::AddPlaylist {
                playlist_id,
                track_ids,
            } => Self::AddPlaylist {
                playlist_id,
                track_ids,
            },
            Command::AddPlaylistSources {
                playlist_id,
                directories,
                track_ids,
                other_tracks,
            } => Self::AddPlaylistSources {
                playlist_id,
                directories,
                track_ids,
                other_tracks,
            },
            Command::InsertPlaylist {
                playlist_id,
                track_ids,
                index,
                expected_revision,
            } => Self::InsertPlaylist {
                playlist_id,
                track_ids,
                index: index as u64,
                expected_revision,
            },
            Command::InsertPlaylistSources {
                playlist_id,
                directories,
                track_ids,
                other_tracks,
                index,
                expected_revision,
            } => Self::InsertPlaylistSources {
                playlist_id,
                directories,
                track_ids,
                other_tracks,
                index: index as u64,
                expected_revision,
            },
            Command::RemovePlaylistEntry { entry_id } => Self::RemovePlaylistEntry { entry_id },
            Command::MovePlaylistEntry { entry_id, index } => Self::MovePlaylistEntry {
                entry_id,
                index: index as u64,
            },
            Command::RemovePlaylistEntries {
                playlist_id,
                entry_ids,
                expected_revision,
            } => Self::RemovePlaylistEntries {
                playlist_id,
                entry_ids,
                expected_revision,
            },
            Command::MovePlaylistEntries {
                playlist_id,
                entry_ids,
                before_entry_id,
                after,
                expected_revision,
            } => Self::MovePlaylistEntries {
                playlist_id,
                entry_ids,
                before_entry_id,
                after,
                expected_revision,
            },
            Command::ImportPlaylist { path, name } => Self::ImportPlaylist { path, name },
            Command::ExportPlaylist { playlist_id, path } => {
                Self::ExportPlaylist { playlist_id, path }
            }
            Command::EditTrack {
                track_id,
                title,
                artist,
                album,
                decoder_hint,
            } => Self::EditTrack {
                track_id,
                title,
                artist,
                album,
                decoder_hint,
            },
            Command::RemoveTracks { track_ids } => Self::RemoveTracks { track_ids },
            Command::RemoveHistory { track_ids } => Self::RemoveHistory { track_ids },
            Command::Device { name } => Self::Device { name },
            Command::DismissError => Self::DismissError,
            Command::DismissScanResult => Self::DismissScanResult,
            Command::CancelScan { id } => Self::CancelScan { id },
            Command::Configure { config } => Self::Configure {
                config: Box::new(WireConfig::from(config)),
            },
            Command::MutateConfig { change } => Self::MutateConfig {
                change: change.into(),
            },
            Command::MprisStatus { status } => Self::MprisStatus { status },
            Command::CreatePlaylistFromQueue { name } => Self::CreatePlaylistFromQueue { name },
            Command::RelocateTrack {
                source_id,
                target_id,
            } => Self::RelocateTrack {
                source_id,
                target_id,
            },
            Command::CancelRelocation { id } => Self::CancelRelocation { id },
            Command::KeepTrackIndependent { track_id } => Self::KeepTrackIndependent { track_id },
            Command::ProcessingBypass { bypassed } => Self::ProcessingBypass { bypassed },
            Command::EqualizerEdit { edit } => Self::EqualizerEdit { edit },
            Command::DenoiseEdit { patch } => Self::DenoiseEdit { patch },
            Command::NeuralEdit { patch } => Self::NeuralEdit { patch },
            Command::ProcessingGraphEdit {
                expected_revision,
                edit,
            } => Self::ProcessingGraphEdit {
                expected_revision,
                edit,
            },
            Command::ProcessingGraphRun {
                track_id,
                target,
                mode,
            } => Self::ProcessingGraphRun {
                track_id,
                target,
                mode,
            },
            Command::ProcessingGraphAudition { id, node } => {
                Self::ProcessingGraphAudition { id, node }
            }
            Command::ProcessingGraphExport { id, node, path } => {
                Self::ProcessingGraphExport { id, node, path }
            }
            Command::StartProcessing {
                track_id,
                operation,
                profile,
            } => Self::StartProcessing {
                track_id,
                operation,
                profile,
            },
            Command::CancelProcessing { id } => Self::CancelProcessing { id },
            Command::AuditionProcessing {
                id,
                mode,
                vocal_gain_db,
                accompaniment_gain_db,
            } => Self::AuditionProcessing {
                id,
                mode,
                vocal_gain_db,
                accompaniment_gain_db,
            },
            Command::DeduplicatePlaylist {
                playlist_id,
                expected_revision,
            } => Self::DeduplicatePlaylist {
                playlist_id,
                expected_revision,
            },
        }
    }
}

impl TryFrom<WireCommand> for Command {
    type Error = anyhow::Error;

    fn try_from(value: WireCommand) -> Result<Self> {
        Ok(match value {
            WireCommand::TransferCollection {
                source,
                destination,
            } => Self::TransferCollection {
                source,
                destination,
            },
            WireCommand::CheckOtherTracks { force } => Self::CheckOtherTracks { force },
            WireCommand::QueueExternalPaths {
                paths,
                index,
                expected_revision,
            } => Self::QueueExternalPaths {
                paths,
                index: index.map(|index| wire_usize(index, "index")).transpose()?,
                expected_revision,
            },
            WireCommand::PlayAlbum { key } => Self::PlayAlbum { key },
            WireCommand::EnqueueAlbum { key } => Self::EnqueueAlbum { key },
            WireCommand::ShowWindow => bail!("ShowWindow is an application request"),
            WireCommand::OptimizeDatabase => Self::OptimizeDatabase,
            WireCommand::SetPlayCount {
                track_id,
                play_count,
            } => Self::SetPlayCount {
                track_id,
                play_count,
            },
            WireCommand::SetFavorite {
                track_ids,
                favorite,
            } => Self::SetFavorite {
                track_ids,
                favorite,
            },
            WireCommand::ToggleFavorite { track_ids } => Self::ToggleFavorite { track_ids },
            WireCommand::RemoveMissingTracks => Self::RemoveMissingTracks,
            WireCommand::Scan { paths, force } => Self::Scan { paths, force },
            WireCommand::Play { track_id } => Self::Play { track_id },
            WireCommand::PlayQueue { queue_id } => Self::PlayQueue { queue_id },
            WireCommand::PlayPlaylist { playlist_id } => Self::PlayPlaylist { playlist_id },
            WireCommand::Resume => Self::Resume,
            WireCommand::Pause => Self::Pause,
            WireCommand::Toggle => Self::Toggle,
            WireCommand::Stop => Self::Stop,
            WireCommand::Next => Self::Next,
            WireCommand::Previous => Self::Previous,
            WireCommand::Seek { seconds } => Self::Seek { seconds },
            WireCommand::SeekQueue { queue_id, seconds } => Self::SeekQueue { queue_id, seconds },
            WireCommand::Volume { value } => Self::Volume { value },
            WireCommand::AdjustVolume { delta } => Self::AdjustVolume { delta },
            WireCommand::CycleRepeat => Self::CycleRepeat,
            WireCommand::ToggleShuffle => Self::ToggleShuffle,
            WireCommand::SeekRelative { queue_id, delta } => Self::SeekRelative { queue_id, delta },
            WireCommand::Enqueue { track_ids } => Self::Enqueue { track_ids },
            WireCommand::EnqueueSources {
                directories,
                track_ids,
                other_tracks,
            } => Self::EnqueueSources {
                directories,
                track_ids,
                other_tracks,
            },
            WireCommand::InsertQueue {
                track_ids,
                index,
                expected_revision,
            } => Self::InsertQueue {
                track_ids,
                index: wire_usize(index, "index")?,
                expected_revision,
            },
            WireCommand::InsertQueueSources {
                directories,
                track_ids,
                other_tracks,
                index,
                expected_revision,
            } => Self::InsertQueueSources {
                directories,
                track_ids,
                other_tracks,
                index: wire_usize(index, "index")?,
                expected_revision,
            },
            WireCommand::RemoveQueue { queue_id } => Self::RemoveQueue { queue_id },
            WireCommand::RemoveQueueEntries { queue_ids } => Self::RemoveQueueEntries { queue_ids },
            WireCommand::MoveQueue { queue_id, index } => Self::MoveQueue {
                queue_id,
                index: wire_usize(index, "index")?,
            },
            WireCommand::MoveQueueEntries { queue_ids, index } => Self::MoveQueueEntries {
                queue_ids,
                index: wire_usize(index, "index")?,
            },
            WireCommand::ClearQueue => Self::ClearQueue,
            WireCommand::RandomizeQueue => Self::RandomizeQueue,
            WireCommand::DeduplicateQueue => Self::DeduplicateQueue,
            WireCommand::Shuffle { enabled } => Self::Shuffle { enabled },
            WireCommand::Repeat { mode } => Self::Repeat { mode },
            WireCommand::CreatePlaylist { name } => Self::CreatePlaylist { name },
            WireCommand::CreatePlaylistWithTracks { name, track_ids } => {
                Self::CreatePlaylistWithTracks { name, track_ids }
            }
            WireCommand::RenamePlaylist { playlist_id, name } => {
                Self::RenamePlaylist { playlist_id, name }
            }
            WireCommand::DeletePlaylist { playlist_id } => Self::DeletePlaylist { playlist_id },
            WireCommand::MovePlaylist {
                playlist_id,
                before_playlist_id,
                after,
                expected_revision,
            } => Self::MovePlaylist {
                playlist_id,
                before_playlist_id,
                after,
                expected_revision,
            },
            WireCommand::AddPlaylist {
                playlist_id,
                track_ids,
            } => Self::AddPlaylist {
                playlist_id,
                track_ids,
            },
            WireCommand::AddPlaylistSources {
                playlist_id,
                directories,
                track_ids,
                other_tracks,
            } => Self::AddPlaylistSources {
                playlist_id,
                directories,
                track_ids,
                other_tracks,
            },
            WireCommand::InsertPlaylist {
                playlist_id,
                track_ids,
                index,
                expected_revision,
            } => Self::InsertPlaylist {
                playlist_id,
                track_ids,
                index: wire_usize(index, "index")?,
                expected_revision,
            },
            WireCommand::InsertPlaylistSources {
                playlist_id,
                directories,
                track_ids,
                other_tracks,
                index,
                expected_revision,
            } => Self::InsertPlaylistSources {
                playlist_id,
                directories,
                track_ids,
                other_tracks,
                index: wire_usize(index, "index")?,
                expected_revision,
            },
            WireCommand::RemovePlaylistEntry { entry_id } => Self::RemovePlaylistEntry { entry_id },
            WireCommand::MovePlaylistEntry { entry_id, index } => Self::MovePlaylistEntry {
                entry_id,
                index: wire_usize(index, "index")?,
            },
            WireCommand::RemovePlaylistEntries {
                playlist_id,
                entry_ids,
                expected_revision,
            } => Self::RemovePlaylistEntries {
                playlist_id,
                entry_ids,
                expected_revision,
            },
            WireCommand::MovePlaylistEntries {
                playlist_id,
                entry_ids,
                before_entry_id,
                after,
                expected_revision,
            } => Self::MovePlaylistEntries {
                playlist_id,
                entry_ids,
                before_entry_id,
                after,
                expected_revision,
            },
            WireCommand::ImportPlaylist { path, name } => Self::ImportPlaylist { path, name },
            WireCommand::ExportPlaylist { playlist_id, path } => {
                Self::ExportPlaylist { playlist_id, path }
            }
            WireCommand::EditTrack {
                track_id,
                title,
                artist,
                album,
                decoder_hint,
            } => Self::EditTrack {
                track_id,
                title,
                artist,
                album,
                decoder_hint,
            },
            WireCommand::RemoveTracks { track_ids } => Self::RemoveTracks { track_ids },
            WireCommand::RemoveHistory { track_ids } => Self::RemoveHistory { track_ids },
            WireCommand::Device { name } => Self::Device { name },
            WireCommand::DismissError => Self::DismissError,
            WireCommand::DismissScanResult => Self::DismissScanResult,
            WireCommand::CancelScan { id } => Self::CancelScan { id },
            WireCommand::Configure { config } => Self::Configure {
                config: (*config).into(),
            },
            WireCommand::MutateConfig { change } => Self::MutateConfig {
                change: change.into(),
            },
            WireCommand::MprisStatus { status } => Self::MprisStatus { status },
            WireCommand::Shutdown => bail!("Shutdown is an application request"),
            WireCommand::CreatePlaylistFromQueue { name } => Self::CreatePlaylistFromQueue { name },
            WireCommand::RelocateTrack {
                source_id,
                target_id,
            } => Self::RelocateTrack {
                source_id,
                target_id,
            },
            WireCommand::CancelRelocation { id } => Self::CancelRelocation { id },
            WireCommand::KeepTrackIndependent { track_id } => {
                Self::KeepTrackIndependent { track_id }
            }
            WireCommand::ProcessingBypass { bypassed } => Self::ProcessingBypass { bypassed },
            WireCommand::EqualizerEdit { edit } => Self::EqualizerEdit { edit },
            WireCommand::DenoiseEdit { patch } => Self::DenoiseEdit { patch },
            WireCommand::NeuralEdit { patch } => Self::NeuralEdit { patch },
            WireCommand::ProcessingGraphEdit {
                expected_revision,
                edit,
            } => Self::ProcessingGraphEdit {
                expected_revision,
                edit,
            },
            WireCommand::ProcessingGraphRun {
                track_id,
                target,
                mode,
            } => Self::ProcessingGraphRun {
                track_id,
                target,
                mode,
            },
            WireCommand::ProcessingGraphAudition { id, node } => {
                Self::ProcessingGraphAudition { id, node }
            }
            WireCommand::ProcessingGraphExport { id, node, path } => {
                Self::ProcessingGraphExport { id, node, path }
            }
            WireCommand::StartProcessing {
                track_id,
                operation,
                profile,
            } => Self::StartProcessing {
                track_id,
                operation,
                profile,
            },
            WireCommand::CancelProcessing { id } => Self::CancelProcessing { id },
            WireCommand::AuditionProcessing {
                id,
                mode,
                vocal_gain_db,
                accompaniment_gain_db,
            } => Self::AuditionProcessing {
                id,
                mode,
                vocal_gain_db,
                accompaniment_gain_db,
            },
            WireCommand::DeduplicatePlaylist {
                playlist_id,
                expected_revision,
            } => Self::DeduplicatePlaylist {
                playlist_id,
                expected_revision,
            },
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HelloResponse {
    pub instance_id: [u8; 4],
    pub state: WireStateResponse,
}

#[derive(Clone, Debug)]
pub enum WireResponse {
    Hello(HelloResponse),
    Query(QueryResponse),
    State(WireStateResponse),
    Ack(Ack),
    Watch(StateRevisions),
    Task(Result<crate::response::TaskOutcome, crate::response::QueryError>),
    Error(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WireStateResponse {
    revisions: StateRevisions,
    playback: Option<crate::response::PlaybackSnapshot>,
    queue: Option<crate::model::QueueState>,
    library: Option<crate::model::LibrarySnapshot>,
    system: Option<SystemState>,
    configuration: Option<WireConfigurationState>,
    processing: Option<crate::response::ProcessingState>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum WireSpectrumBarLayout {
    Automatic { width: f32 },
    Fixed { count: u32 },
}

impl From<rivu_format::config::SpectrumBarLayout> for WireSpectrumBarLayout {
    fn from(value: rivu_format::config::SpectrumBarLayout) -> Self {
        match value {
            rivu_format::config::SpectrumBarLayout::Automatic { width } => {
                Self::Automatic { width }
            }
            rivu_format::config::SpectrumBarLayout::Fixed { count } => Self::Fixed { count },
        }
    }
}

impl From<WireSpectrumBarLayout> for rivu_format::config::SpectrumBarLayout {
    fn from(value: WireSpectrumBarLayout) -> Self {
        match value {
            WireSpectrumBarLayout::Automatic { width } => Self::Automatic { width },
            WireSpectrumBarLayout::Fixed { count } => Self::Fixed { count },
        }
    }
}

/// Shared domain storage with an explicit positional binary serialization schema.
#[derive(Clone, Debug)]
pub struct WireConfig(Arc<rivu_format::config::Config>);

impl From<Arc<rivu_format::config::Config>> for WireConfig {
    fn from(value: Arc<rivu_format::config::Config>) -> Self {
        Self(value)
    }
}

impl From<WireConfig> for Arc<rivu_format::config::Config> {
    fn from(value: WireConfig) -> Self {
        value.0
    }
}

impl From<WireConfig> for rivu_format::config::Config {
    fn from(value: WireConfig) -> Self {
        Arc::unwrap_or_clone(value.0)
    }
}

macro_rules! declare_config {
    ($( $(#[$attr:meta])* $field:ident: $ty:ty, )*) => {
        impl Serialize for WireConfig {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                use serde::ser::SerializeStruct;
                let field_count = [$(stringify!($field)),*].len();
                let mut state = serializer.serialize_struct("WireConfig", field_count)?;
                $(state.serialize_field(
                    stringify!($field),
                    &config_wire_borrow!($field, self.0.$field),
                )?;)*
                state.end()
            }
        }

        impl<'de> Deserialize<'de> for WireConfig {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                #[derive(Deserialize)]
                struct OwnedConfig {
                    $( $(#[$attr])* $field: $ty, )*
                }
                let value = OwnedConfig::deserialize(deserializer)?;
                Ok(Self(Arc::new(rivu_format::config::Config {
                    $( $field: config_wire_convert!(from_wire, $field, value.$field), )*
                })))
            }
        }
    };
}

config_wire_fields!(declare_config);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WireConfigurationState {
    config: WireConfig,
    config_path: PathBuf,
}

impl From<ConfigurationState> for WireConfigurationState {
    fn from(value: ConfigurationState) -> Self {
        Self {
            config: WireConfig::from(value.config),
            config_path: value.config_path,
        }
    }
}

impl From<WireConfigurationState> for ConfigurationState {
    fn from(value: WireConfigurationState) -> Self {
        Self {
            config: value.config.into(),
            config_path: value.config_path,
        }
    }
}

pub fn wire_state(response: StateResponse) -> WireStateResponse {
    WireStateResponse {
        revisions: response.revisions,
        playback: response.playback,
        queue: response.queue,
        library: response.library,
        system: response.system,
        configuration: response.configuration.map(WireConfigurationState::from),
        processing: response.processing,
    }
}

pub fn state_response(response: WireStateResponse) -> StateResponse {
    StateResponse {
        revisions: response.revisions,
        playback: response.playback,
        queue: response.queue,
        library: response.library,
        system: response.system,
        configuration: response.configuration.map(ConfigurationState::from),
        processing: response.processing,
    }
}
