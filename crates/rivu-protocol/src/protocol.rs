mod endpoint;
pub use endpoint::{address, connect, verify_peer};

use crate::{
    AppCommand,
    model::{Command, DatabaseOptimization, Query, ScanProgress, SystemState},
    response::{Ack, QueryResponse, StateResponse, StateRevisions, StateSections},
};
use anyhow::{Context, Result, bail};
use rivu_format::media::DecoderHint;
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc, time::Duration};

pub const PROTOCOL_VERSION: u8 = 9;
pub const MAX_REQUEST: usize = 4 * 1024 * 1024;
pub const MAX_RESPONSE: usize = 16 * 1024 * 1024;
pub const MAX_CLIENTS: usize = 16;
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(1);
pub const UNKNOWN_INSTANCE: u32 = 0;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestType {
    Hello = 0,
    State = 1,
    Query = 2,
    Command = 3,
    Watch = 4,
}

impl TryFrom<u8> for RequestType {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self> {
        Ok(match value {
            0 => Self::Hello,
            1 => Self::State,
            2 => Self::Query,
            3 => Self::Command,
            4 => Self::Watch,
            _ => bail!("Unknown IPC request type {value}"),
        })
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseType {
    Hello = 0,
    State = 1,
    Query = 2,
    Ack = 3,
    Watch = 4,
    Error = 255,
}

impl TryFrom<u8> for ResponseType {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self> {
        Ok(match value {
            0 => Self::Hello,
            1 => Self::State,
            2 => Self::Query,
            3 => Self::Ack,
            4 => Self::Watch,
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
    Watch(StateRevisions),
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
    Device {
        name: Option<String>,
    },
    Analysis {
        enabled: bool,
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
        revision: u64,
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
    ProcessingBypass,
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
            Command::Device { name } => Self::Device { name },
            Command::Analysis { enabled } => Self::Analysis { enabled },
            Command::DismissError => Self::DismissError,
            Command::DismissScanResult => Self::DismissScanResult,
            Command::CancelScan { id } => Self::CancelScan { id },
            Command::Configure { config } => Self::Configure {
                config: Box::new(WireConfig::from(config.as_ref())),
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
            Command::ProcessingBypass => Self::ProcessingBypass,
            Command::EqualizerEdit { edit } => Self::EqualizerEdit { edit },
            Command::DenoiseEdit { patch } => Self::DenoiseEdit { patch },
            Command::NeuralEdit { patch } => Self::NeuralEdit { patch },
            Command::ProcessingGraphEdit { revision, edit } => {
                Self::ProcessingGraphEdit { revision, edit }
            }
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
            WireCommand::Device { name } => Self::Device { name },
            WireCommand::Analysis { enabled } => Self::Analysis { enabled },
            WireCommand::DismissError => Self::DismissError,
            WireCommand::DismissScanResult => Self::DismissScanResult,
            WireCommand::CancelScan { id } => Self::CancelScan { id },
            WireCommand::Configure { config } => Self::Configure {
                config: Arc::new((*config).into()),
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
            WireCommand::ProcessingBypass => Self::ProcessingBypass,
            WireCommand::EqualizerEdit { edit } => Self::EqualizerEdit { edit },
            WireCommand::DenoiseEdit { patch } => Self::DenoiseEdit { patch },
            WireCommand::NeuralEdit { patch } => Self::NeuralEdit { patch },
            WireCommand::ProcessingGraphEdit { revision, edit } => {
                Self::ProcessingGraphEdit { revision, edit }
            }
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
    Error(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WireStateResponse {
    revisions: StateRevisions,
    playback: Option<crate::response::PlaybackSnapshot>,
    queue: Option<crate::model::QueueState>,
    library: Option<crate::model::LibrarySnapshot>,
    system: Option<WireSystemState>,
}

/// Complete positional IPC schema for `Config`.
///
/// Keep this explicit instead of serializing `Config` directly: its human-readable
/// serde attributes are not a stable binary wire contract.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WireConfig {
    library_roots: Vec<PathBuf>,
    scan_max_depth: u32,
    scan_min_duration_secs: u32,
    output_device: Option<String>,
    volume: f32,
    shuffle: bool,
    repeat: crate::model::RepeatMode,
    play_count_threshold_percent: f64,
    tray_enabled: bool,
    confirm_queue_replace: bool,
    mpris_enabled: bool,
    log_level: rivu_format::config::LogLevel,
    log_to_file: bool,
    log_retention_weeks: u32,
    page_size: u32,
    /// Artist, album, duration, codec, bitrate in fixed wire order.
    library_columns: [bool; 5],
    ui_font: String,
    ui_font_size: u32,
    ui_language: String,
    ui_theme: rivu_format::config::Theme,
    ui_scale: f32,
    layout_snap_enabled: bool,
    layout_snap_grid_px: u32,
    still_mode: bool,
    visualization_signal: rivu_format::config::VisualizationSignal,
    analysis_fps: u32,
    visualization_fps: u16,
    visual_background: rivu_format::config::RgbColor,
    visual_palette: rivu_format::config::VisualizationPalette,
    spectrum_style: rivu_format::config::SpectrumStyle,
    analysis_fft_size: u32,
    analysis_window: rivu_format::config::AnalysisWindow,
    spectrum_interpolate: bool,
    spectrum_bar_width: f32,
    spectrum_bars: u32,
    spectrum_gap: f32,
    spectrum_peaks: bool,
    spectrum_peak_hold_ms: u32,
    spectrum_peak_gravity: f32,
    spectrum_bar_hold_ms: u32,
    spectrum_bar_gravity: f32,
    spectrum_smoothing_ms: u32,
    spectrum_grid: bool,
    spectrum_labels: bool,
    audio_visualizer_style: rivu_format::config::AudioVisualizerStyle,
    audio_visualizer_grid: bool,
    audio_visualizer_labels: bool,
    audio_visualizer_sensitivity: f32,
    audio_visualizer_rotation_speed: f32,
    audio_visualizer_bar_width: f32,
    audio_visualizer_bar_glow_layers: u32,
    audio_visualizer_ring_opacity: f32,
    audio_visualizer_bloom_intensity: f32,
    audio_visualizer_inner_diameter: f32,
    audio_visualizer_fade_when_idle: bool,
    audio_visualizer_primary_color: rivu_format::config::RgbColor,
    audio_visualizer_secondary_color: rivu_format::config::RgbColor,
    album_artwork_style: rivu_format::config::AlbumArtworkStyle,
    album_artwork_mode: rivu_format::config::AlbumArtworkMode,
    album_artwork_period_seconds: f32,
    album_artwork_direction: rivu_format::config::AlbumArtworkDirection,
    album_artwork_fps: u16,
    artwork_decode_budget_mb: u32,
    gpu_material_budget_mb: u32,
    spectrogram_labels: bool,
    spectrogram_interpolate: bool,
    spectrogram_sampling_points_scale: f32,
    spectrogram_interpolation_points: u32,
    waveform_labels: bool,
    waveform_seek_enabled: bool,
    visualization_db_range: f32,
    spectrum_db_range: Option<f32>,
    spectrogram_db_range: Option<f32>,
    audio_visualizer_db_range: Option<f32>,
    spectrogram_history_seconds: u32,
    spectrogram_flow_speed: f32,
    spectrogram_accumulation: u32,
    waveform_cursor_color: rivu_format::config::RgbColor,
    waveform_glow: f32,
    waveform_rms_gain: f32,
    waveform_peak_gain: f32,
    waveform_peak_gamma: f32,
    visualization_cache: bool,
    media_read_buffer_mb: u32,
    ffmpeg_enabled: bool,
    pipewire_auto_mix: bool,
    equalizer: rivu_dsp::equalizer::config::EqualizerConfig,
    denoise: rivu_dsp::denoise::DenoiseConfig,
    neural: rivu_format::config::NeuralConfig,
    equalizer_presets: Vec<rivu_format::config::EqualizerPreset>,
    equalizer_ab: Option<rivu_dsp::equalizer::config::EqualizerConfig>,
    processing_graph: crate::model::ProcessingGraph,
}

impl From<&rivu_format::config::Config> for WireConfig {
    fn from(value: &rivu_format::config::Config) -> Self {
        Self {
            library_roots: value.library_roots.clone(),
            scan_max_depth: value.scan_max_depth,
            scan_min_duration_secs: value.scan_min_duration_secs,
            output_device: value.output_device.clone(),
            volume: value.volume,
            shuffle: value.shuffle,
            repeat: value.repeat,
            play_count_threshold_percent: value.play_count_threshold_percent,
            tray_enabled: value.tray_enabled,
            confirm_queue_replace: value.confirm_queue_replace,
            mpris_enabled: value.mpris_enabled,
            log_level: value.log_level,
            log_to_file: value.log_to_file,
            log_retention_weeks: value.log_retention_weeks,
            page_size: value.page_size,
            library_columns: [
                value.library_columns.artist,
                value.library_columns.album,
                value.library_columns.duration,
                value.library_columns.codec,
                value.library_columns.bitrate,
            ],
            ui_font: value.ui_font.clone(),
            ui_font_size: value.ui_font_size,
            ui_language: value.ui_language.clone(),
            ui_theme: value.ui_theme,
            ui_scale: value.ui_scale,
            layout_snap_enabled: value.layout_snap_enabled,
            layout_snap_grid_px: value.layout_snap_grid_px,
            still_mode: value.still_mode,
            visualization_signal: value.visualization_signal,
            analysis_fps: value.analysis_fps,
            visualization_fps: value.visualization_fps,
            visual_background: value.visual_background,
            visual_palette: value.visual_palette,
            spectrum_style: value.spectrum_style,
            analysis_fft_size: value.analysis_fft_size,
            analysis_window: value.analysis_window,
            spectrum_interpolate: value.spectrum_interpolate,
            spectrum_bar_width: value.spectrum_bar_width,
            spectrum_bars: value.spectrum_bars,
            spectrum_gap: value.spectrum_gap,
            spectrum_peaks: value.spectrum_peaks,
            spectrum_peak_hold_ms: value.spectrum_peak_hold_ms,
            spectrum_peak_gravity: value.spectrum_peak_gravity,
            spectrum_bar_hold_ms: value.spectrum_bar_hold_ms,
            spectrum_bar_gravity: value.spectrum_bar_gravity,
            spectrum_smoothing_ms: value.spectrum_smoothing_ms,
            spectrum_grid: value.spectrum_grid,
            spectrum_labels: value.spectrum_labels,
            audio_visualizer_style: value.audio_visualizer_style,
            audio_visualizer_grid: value.audio_visualizer_grid,
            audio_visualizer_labels: value.audio_visualizer_labels,
            audio_visualizer_sensitivity: value.audio_visualizer_sensitivity,
            audio_visualizer_rotation_speed: value.audio_visualizer_rotation_speed,
            audio_visualizer_bar_width: value.audio_visualizer_bar_width,
            audio_visualizer_bar_glow_layers: value.audio_visualizer_bar_glow_layers,
            audio_visualizer_ring_opacity: value.audio_visualizer_ring_opacity,
            audio_visualizer_bloom_intensity: value.audio_visualizer_bloom_intensity,
            audio_visualizer_inner_diameter: value.audio_visualizer_inner_diameter,
            audio_visualizer_fade_when_idle: value.audio_visualizer_fade_when_idle,
            audio_visualizer_primary_color: value.audio_visualizer_primary_color,
            audio_visualizer_secondary_color: value.audio_visualizer_secondary_color,
            album_artwork_style: value.album_artwork_style,
            album_artwork_mode: value.album_artwork_mode,
            album_artwork_period_seconds: value.album_artwork_period_seconds,
            album_artwork_direction: value.album_artwork_direction,
            album_artwork_fps: value.album_artwork_fps,
            artwork_decode_budget_mb: value.artwork_decode_budget_mb,
            gpu_material_budget_mb: value.gpu_material_budget_mb,
            spectrogram_labels: value.spectrogram_labels,
            spectrogram_interpolate: value.spectrogram_interpolate,
            spectrogram_sampling_points_scale: value.spectrogram_sampling_points_scale,
            spectrogram_interpolation_points: value.spectrogram_interpolation_points,
            waveform_labels: value.waveform_labels,
            waveform_seek_enabled: value.waveform_seek_enabled,
            visualization_db_range: value.visualization_db_range,
            spectrum_db_range: value.spectrum_db_range,
            spectrogram_db_range: value.spectrogram_db_range,
            audio_visualizer_db_range: value.audio_visualizer_db_range,
            spectrogram_history_seconds: value.spectrogram_history_seconds,
            spectrogram_flow_speed: value.spectrogram_flow_speed,
            spectrogram_accumulation: value.spectrogram_accumulation,
            waveform_cursor_color: value.waveform_cursor_color,
            waveform_glow: value.waveform_glow,
            waveform_rms_gain: value.waveform_rms_gain,
            waveform_peak_gain: value.waveform_peak_gain,
            waveform_peak_gamma: value.waveform_peak_gamma,
            visualization_cache: value.visualization_cache,
            media_read_buffer_mb: value.media_read_buffer_mb,
            ffmpeg_enabled: value.ffmpeg_enabled,
            pipewire_auto_mix: value.pipewire_auto_mix,
            equalizer: value.equalizer.clone(),
            denoise: value.denoise.clone(),
            neural: value.neural.clone(),
            equalizer_presets: value.equalizer_presets.clone(),
            equalizer_ab: value.equalizer_ab.clone(),
            processing_graph: value.processing_graph.clone(),
        }
    }
}

impl From<WireConfig> for rivu_format::config::Config {
    fn from(value: WireConfig) -> Self {
        Self {
            library_roots: value.library_roots,
            scan_max_depth: value.scan_max_depth,
            scan_min_duration_secs: value.scan_min_duration_secs,
            output_device: value.output_device,
            volume: value.volume,
            shuffle: value.shuffle,
            repeat: value.repeat,
            play_count_threshold_percent: value.play_count_threshold_percent,
            tray_enabled: value.tray_enabled,
            confirm_queue_replace: value.confirm_queue_replace,
            mpris_enabled: value.mpris_enabled,
            log_level: value.log_level,
            log_to_file: value.log_to_file,
            log_retention_weeks: value.log_retention_weeks,
            page_size: value.page_size,
            library_columns: rivu_format::config::LibraryColumns {
                artist: value.library_columns[0],
                album: value.library_columns[1],
                duration: value.library_columns[2],
                codec: value.library_columns[3],
                bitrate: value.library_columns[4],
            },
            ui_font: value.ui_font,
            ui_font_size: value.ui_font_size,
            ui_language: value.ui_language,
            ui_theme: value.ui_theme,
            ui_scale: value.ui_scale,
            layout_snap_enabled: value.layout_snap_enabled,
            layout_snap_grid_px: value.layout_snap_grid_px,
            still_mode: value.still_mode,
            visualization_signal: value.visualization_signal,
            analysis_fps: value.analysis_fps,
            visualization_fps: value.visualization_fps,
            visual_background: value.visual_background,
            visual_palette: value.visual_palette,
            spectrum_style: value.spectrum_style,
            analysis_fft_size: value.analysis_fft_size,
            analysis_window: value.analysis_window,
            spectrum_interpolate: value.spectrum_interpolate,
            spectrum_bar_width: value.spectrum_bar_width,
            spectrum_bars: value.spectrum_bars,
            spectrum_gap: value.spectrum_gap,
            spectrum_peaks: value.spectrum_peaks,
            spectrum_peak_hold_ms: value.spectrum_peak_hold_ms,
            spectrum_peak_gravity: value.spectrum_peak_gravity,
            spectrum_bar_hold_ms: value.spectrum_bar_hold_ms,
            spectrum_bar_gravity: value.spectrum_bar_gravity,
            spectrum_smoothing_ms: value.spectrum_smoothing_ms,
            spectrum_grid: value.spectrum_grid,
            spectrum_labels: value.spectrum_labels,
            audio_visualizer_style: value.audio_visualizer_style,
            audio_visualizer_grid: value.audio_visualizer_grid,
            audio_visualizer_labels: value.audio_visualizer_labels,
            audio_visualizer_sensitivity: value.audio_visualizer_sensitivity,
            audio_visualizer_rotation_speed: value.audio_visualizer_rotation_speed,
            audio_visualizer_bar_width: value.audio_visualizer_bar_width,
            audio_visualizer_bar_glow_layers: value.audio_visualizer_bar_glow_layers,
            audio_visualizer_ring_opacity: value.audio_visualizer_ring_opacity,
            audio_visualizer_bloom_intensity: value.audio_visualizer_bloom_intensity,
            audio_visualizer_inner_diameter: value.audio_visualizer_inner_diameter,
            audio_visualizer_fade_when_idle: value.audio_visualizer_fade_when_idle,
            audio_visualizer_primary_color: value.audio_visualizer_primary_color,
            audio_visualizer_secondary_color: value.audio_visualizer_secondary_color,
            album_artwork_style: value.album_artwork_style,
            album_artwork_mode: value.album_artwork_mode,
            album_artwork_period_seconds: value.album_artwork_period_seconds,
            album_artwork_direction: value.album_artwork_direction,
            album_artwork_fps: value.album_artwork_fps,
            artwork_decode_budget_mb: value.artwork_decode_budget_mb,
            gpu_material_budget_mb: value.gpu_material_budget_mb,
            spectrogram_labels: value.spectrogram_labels,
            spectrogram_interpolate: value.spectrogram_interpolate,
            spectrogram_sampling_points_scale: value.spectrogram_sampling_points_scale,
            spectrogram_interpolation_points: value.spectrogram_interpolation_points,
            waveform_labels: value.waveform_labels,
            waveform_seek_enabled: value.waveform_seek_enabled,
            visualization_db_range: value.visualization_db_range,
            spectrum_db_range: value.spectrum_db_range,
            spectrogram_db_range: value.spectrogram_db_range,
            audio_visualizer_db_range: value.audio_visualizer_db_range,
            spectrogram_history_seconds: value.spectrogram_history_seconds,
            spectrogram_flow_speed: value.spectrogram_flow_speed,
            spectrogram_accumulation: value.spectrogram_accumulation,
            waveform_cursor_color: value.waveform_cursor_color,
            waveform_glow: value.waveform_glow,
            waveform_rms_gain: value.waveform_rms_gain,
            waveform_peak_gain: value.waveform_peak_gain,
            waveform_peak_gamma: value.waveform_peak_gamma,
            visualization_cache: value.visualization_cache,
            media_read_buffer_mb: value.media_read_buffer_mb,
            ffmpeg_enabled: value.ffmpeg_enabled,
            pipewire_auto_mix: value.pipewire_auto_mix,
            equalizer: value.equalizer,
            denoise: value.denoise,
            neural: value.neural,
            equalizer_presets: value.equalizer_presets,
            equalizer_ab: value.equalizer_ab,
            processing_graph: value.processing_graph,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WireSystemState {
    scanning: bool,
    scan_message: String,
    scan_progress: Option<ScanProgress>,
    scan_task_id: Option<u64>,
    last_error: Option<String>,
    devices: Arc<Vec<String>>,
    selected_device: Option<String>,
    revision: u64,
    config: WireConfig,
    config_path: PathBuf,
    mpris_status: String,
    ffmpeg_status: String,
    database_optimization: Option<DatabaseOptimization>,
    shutting_down: bool,
    relocation_task: Option<crate::model::RelocationTaskStatus>,
    processing: Option<crate::model::ProcessingTaskStatus>,
}

impl From<SystemState> for WireSystemState {
    fn from(value: SystemState) -> Self {
        Self {
            scanning: value.scanning,
            scan_message: value.scan_message,
            scan_progress: value.scan_progress,
            scan_task_id: value.scan_task_id,
            last_error: value.last_error,
            devices: value.devices,
            selected_device: value.selected_device,
            revision: value.revision,
            config: WireConfig::from(value.config.as_ref()),
            config_path: value.config_path,
            mpris_status: value.mpris_status,
            ffmpeg_status: value.ffmpeg_status,
            database_optimization: value.database_optimization,
            shutting_down: value.shutting_down,
            relocation_task: value.relocation_task,
            processing: value.processing,
        }
    }
}

impl From<WireSystemState> for SystemState {
    fn from(value: WireSystemState) -> Self {
        Self {
            scanning: value.scanning,
            scan_message: value.scan_message,
            scan_progress: value.scan_progress,
            scan_task_id: value.scan_task_id,
            last_error: value.last_error,
            devices: value.devices,
            selected_device: value.selected_device,
            revision: value.revision,
            config: Arc::new(value.config.into()),
            config_path: value.config_path,
            mpris_status: value.mpris_status,
            ffmpeg_status: value.ffmpeg_status,
            database_optimization: value.database_optimization,
            shutting_down: value.shutting_down,
            relocation_task: value.relocation_task,
            processing: value.processing,
        }
    }
}

pub fn wire_state(response: StateResponse) -> WireStateResponse {
    WireStateResponse {
        revisions: response.revisions,
        playback: response.playback,
        queue: response.queue,
        library: response.library,
        system: response.system.map(WireSystemState::from),
    }
}

pub fn state_response(response: WireStateResponse) -> StateResponse {
    StateResponse {
        revisions: response.revisions,
        playback: response.playback,
        queue: response.queue,
        library: response.library,
        system: response.system.map(SystemState::from),
    }
}
