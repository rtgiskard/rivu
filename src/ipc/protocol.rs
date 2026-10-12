use crate::{
    model::{Command, DatabaseOptimization, Query, ScanProgress, SystemState},
    response::{Ack, QueryResponse, StateResponse, StateRevisions, StateSections},
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc, time::Duration};

pub(super) const PROTOCOL_VERSION: u8 = 9;
pub(super) const MAX_REQUEST: usize = 4 * 1024 * 1024;
pub(super) const MAX_RESPONSE: usize = 16 * 1024 * 1024;
pub(super) const MAX_CLIENTS: usize = 16;
pub(super) const HELLO_TIMEOUT: Duration = Duration::from_secs(1);
pub(super) const UNKNOWN_INSTANCE: u32 = 0;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RequestType {
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
pub(super) enum ResponseType {
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
pub(super) enum SessionState {
    Unbound,
    Bound,
}

impl SessionState {
    pub(super) fn is_bound(&self) -> bool {
        matches!(self, Self::Bound)
    }
}

pub(super) enum WireRequest {
    Hello(StateSections),
    State(StateSections),
    Query(WireQuery),
    Command(WireCommand),
    Watch(StateRevisions),
}

/// Binary query wire format. Variant order is a protocol contract; append only.
#[derive(Serialize, Deserialize)]
pub(super) enum WireQuery {
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
        offset: u64,
        limit: u64,
    },
    PlaylistSummaries {
        offset: u64,
        limit: u64,
    },
    PlaylistEntries {
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
        after: Option<i64>,
        limit: u64,
        expected: Option<crate::response::QueryRevisions>,
    },
    PlaylistEntryBatch {
        playlist_id: i64,
        after: Option<crate::model::PlaylistEntryCursor>,
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
    LibraryRootCounts {
        roots: Vec<PathBuf>,
    },
    ScanTask {
        id: u64,
    },
    QueuePage {
        offset: u64,
        limit: u64,
    },
    QueueBatch {
        after: Option<crate::model::QueueCursor>,
        limit: u64,
        expected: Option<crate::response::QueryRevisions>,
    },
    QueueEntries {
        queue_ids: Vec<u64>,
    },
}

/// Binary command wire format. Variant order is a protocol contract; append only.
#[derive(Serialize, Deserialize)]
pub(super) enum WireCommand {
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
        config: WireConfig,
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
}

pub(super) fn wire_usize(value: u64, field: &str) -> Result<usize> {
    usize::try_from(value).with_context(|| format!("{field} does not fit in usize"))
}

impl From<Query> for WireQuery {
    fn from(value: Query) -> Self {
        match value {
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
                offset,
                limit,
            } => Self::DirectoryPage {
                path,
                query,
                favorite,
                offset: offset as u64,
                limit: limit as u64,
            },
            Query::PlaylistSummaries { offset, limit } => Self::PlaylistSummaries {
                offset: offset as u64,
                limit: limit as u64,
            },
            Query::PlaylistEntries {
                playlist_id,
                offset,
                limit,
            } => Self::PlaylistEntries {
                playlist_id,
                offset: offset as u64,
                limit: limit as u64,
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
                after,
                limit,
                expected,
            } => Self::PlaylistSummaryBatch {
                after,
                limit: limit as u64,
                expected,
            },
            Query::PlaylistEntryBatch {
                playlist_id,
                after,
                limit,
                expected,
            } => Self::PlaylistEntryBatch {
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
            Query::LibraryRootCounts { roots } => Self::LibraryRootCounts { roots },
            Query::ScanTask { id } => Self::ScanTask { id },
            Query::QueuePage { offset, limit } => Self::QueuePage {
                offset: offset as u64,
                limit: limit as u64,
            },
            Query::QueueBatch {
                after,
                limit,
                expected,
            } => Self::QueueBatch {
                after,
                limit: limit as u64,
                expected,
            },
            Query::QueueEntries { queue_ids } => Self::QueueEntries { queue_ids },
        }
    }
}

impl TryFrom<WireQuery> for Query {
    type Error = anyhow::Error;

    fn try_from(value: WireQuery) -> Result<Self> {
        Ok(match value {
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
                offset,
                limit,
            } => Self::DirectoryPage {
                path,
                query,
                favorite,
                offset: wire_usize(offset, "offset")?,
                limit: wire_usize(limit, "limit")?,
            },
            WireQuery::PlaylistSummaries { offset, limit } => Self::PlaylistSummaries {
                offset: wire_usize(offset, "offset")?,
                limit: wire_usize(limit, "limit")?,
            },
            WireQuery::PlaylistEntries {
                playlist_id,
                offset,
                limit,
            } => Self::PlaylistEntries {
                playlist_id,
                offset: wire_usize(offset, "offset")?,
                limit: wire_usize(limit, "limit")?,
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
                after,
                limit,
                expected,
            } => Self::PlaylistSummaryBatch {
                after,
                limit: wire_usize(limit, "limit")?,
                expected,
            },
            WireQuery::PlaylistEntryBatch {
                playlist_id,
                after,
                limit,
                expected,
            } => Self::PlaylistEntryBatch {
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
            WireQuery::LibraryRootCounts { roots } => Self::LibraryRootCounts { roots },
            WireQuery::ScanTask { id } => Self::ScanTask { id },
            WireQuery::QueuePage { offset, limit } => Self::QueuePage {
                offset: wire_usize(offset, "offset")?,
                limit: wire_usize(limit, "limit")?,
            },
            WireQuery::QueueBatch {
                after,
                limit,
                expected,
            } => Self::QueueBatch {
                after,
                limit: wire_usize(limit, "limit")?,
                expected,
            },
            WireQuery::QueueEntries { queue_ids } => Self::QueueEntries { queue_ids },
        })
    }
}

impl From<Command> for WireCommand {
    fn from(value: Command) -> Self {
        match value {
            Command::ShowWindow => Self::ShowWindow,
            Command::OptimizeDatabase => Self::OptimizeDatabase,
            Command::SetFavorite {
                track_ids,
                favorite,
            } => Self::SetFavorite {
                track_ids,
                favorite,
            },
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
            Command::Enqueue { track_ids } => Self::Enqueue { track_ids },
            Command::EnqueueSources {
                directories,
                track_ids,
            } => Self::EnqueueSources {
                directories,
                track_ids,
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
            } => Self::AddPlaylistSources {
                playlist_id,
                directories,
                track_ids,
            },
            Command::RemovePlaylistEntry { entry_id } => Self::RemovePlaylistEntry { entry_id },
            Command::MovePlaylistEntry { entry_id, index } => Self::MovePlaylistEntry {
                entry_id,
                index: index as u64,
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
            } => Self::EditTrack {
                track_id,
                title,
                artist,
                album,
            },
            Command::RemoveTracks { track_ids } => Self::RemoveTracks { track_ids },
            Command::Device { name } => Self::Device { name },
            Command::Analysis { enabled } => Self::Analysis { enabled },
            Command::DismissError => Self::DismissError,
            Command::DismissScanResult => Self::DismissScanResult,
            Command::CancelScan { id } => Self::CancelScan { id },
            Command::Configure { config } => Self::Configure {
                config: WireConfig::from(&config),
            },
            Command::MprisStatus { status } => Self::MprisStatus { status },
            Command::Shutdown => Self::Shutdown,
            Command::CreatePlaylistFromQueue { name } => Self::CreatePlaylistFromQueue { name },
        }
    }
}

impl TryFrom<WireCommand> for Command {
    type Error = anyhow::Error;

    fn try_from(value: WireCommand) -> Result<Self> {
        Ok(match value {
            WireCommand::ShowWindow => Self::ShowWindow,
            WireCommand::OptimizeDatabase => Self::OptimizeDatabase,
            WireCommand::SetFavorite {
                track_ids,
                favorite,
            } => Self::SetFavorite {
                track_ids,
                favorite,
            },
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
            WireCommand::Enqueue { track_ids } => Self::Enqueue { track_ids },
            WireCommand::EnqueueSources {
                directories,
                track_ids,
            } => Self::EnqueueSources {
                directories,
                track_ids,
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
            } => Self::AddPlaylistSources {
                playlist_id,
                directories,
                track_ids,
            },
            WireCommand::RemovePlaylistEntry { entry_id } => Self::RemovePlaylistEntry { entry_id },
            WireCommand::MovePlaylistEntry { entry_id, index } => Self::MovePlaylistEntry {
                entry_id,
                index: wire_usize(index, "index")?,
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
            } => Self::EditTrack {
                track_id,
                title,
                artist,
                album,
            },
            WireCommand::RemoveTracks { track_ids } => Self::RemoveTracks { track_ids },
            WireCommand::Device { name } => Self::Device { name },
            WireCommand::Analysis { enabled } => Self::Analysis { enabled },
            WireCommand::DismissError => Self::DismissError,
            WireCommand::DismissScanResult => Self::DismissScanResult,
            WireCommand::CancelScan { id } => Self::CancelScan { id },
            WireCommand::Configure { config } => Self::Configure {
                config: config.into(),
            },
            WireCommand::MprisStatus { status } => Self::MprisStatus { status },
            WireCommand::Shutdown => Self::Shutdown,
            WireCommand::CreatePlaylistFromQueue { name } => Self::CreatePlaylistFromQueue { name },
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct HelloResponse {
    pub(super) instance_id: [u8; 4],
    pub(super) state: WireStateResponse,
}

#[derive(Clone, Debug)]
pub(super) enum WireResponse {
    Hello(HelloResponse),
    Query(QueryResponse),
    State(WireStateResponse),
    Ack(Ack),
    Watch(StateRevisions),
    Error(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct WireStateResponse {
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
pub(super) struct WireConfig {
    library_roots: Vec<PathBuf>,
    scan_max_depth: u32,
    output_device: Option<String>,
    volume: f32,
    shuffle: bool,
    repeat: crate::model::RepeatMode,
    play_count_threshold_percent: f64,
    tray_enabled: bool,
    mpris_enabled: bool,
    log_level: crate::config::LogLevel,
    log_to_file: bool,
    log_retention_weeks: u32,
    page_size: u32,
    ui_font: String,
    ui_scale: f32,
    layout_snap_enabled: bool,
    layout_snap_grid_px: u32,
    still_mode: bool,
    analysis_fps: u32,
    visual_background: crate::config::RgbColor,
    visual_palette: crate::config::VisualizationPalette,
    spectrum_style: crate::config::SpectrumStyle,
    analysis_fft_size: u32,
    analysis_window: crate::config::AnalysisWindow,
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
    audio_visualizer_style: crate::config::AudioVisualizerStyle,
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
    audio_visualizer_primary_color: crate::config::RgbColor,
    audio_visualizer_secondary_color: crate::config::RgbColor,
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
    waveform_cursor_color: crate::config::RgbColor,
    waveform_glow: f32,
    waveform_rms_gain: f32,
    waveform_peak_gain: f32,
    waveform_peak_gamma: f32,
    visualization_cache: bool,
    media_read_buffer_mb: u32,
    ffmpeg_enabled: bool,
    pipewire_auto_mix: bool,
}

impl From<&crate::config::Config> for WireConfig {
    fn from(value: &crate::config::Config) -> Self {
        Self {
            library_roots: value.library_roots.clone(),
            scan_max_depth: value.scan_max_depth,
            output_device: value.output_device.clone(),
            volume: value.volume,
            shuffle: value.shuffle,
            repeat: value.repeat,
            play_count_threshold_percent: value.play_count_threshold_percent,
            tray_enabled: value.tray_enabled,
            mpris_enabled: value.mpris_enabled,
            log_level: value.log_level,
            log_to_file: value.log_to_file,
            log_retention_weeks: value.log_retention_weeks,
            page_size: value.page_size,
            ui_font: value.ui_font.clone(),
            ui_scale: value.ui_scale,
            layout_snap_enabled: value.layout_snap_enabled,
            layout_snap_grid_px: value.layout_snap_grid_px,
            still_mode: value.still_mode,
            analysis_fps: value.analysis_fps,
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
            waveform_cursor_color: value.waveform_cursor_color,
            waveform_glow: value.waveform_glow,
            waveform_rms_gain: value.waveform_rms_gain,
            waveform_peak_gain: value.waveform_peak_gain,
            waveform_peak_gamma: value.waveform_peak_gamma,
            visualization_cache: value.visualization_cache,
            media_read_buffer_mb: value.media_read_buffer_mb,
            ffmpeg_enabled: value.ffmpeg_enabled,
            pipewire_auto_mix: value.pipewire_auto_mix,
        }
    }
}

impl From<WireConfig> for crate::config::Config {
    fn from(value: WireConfig) -> Self {
        Self {
            library_roots: value.library_roots,
            scan_max_depth: value.scan_max_depth,
            output_device: value.output_device,
            volume: value.volume,
            shuffle: value.shuffle,
            repeat: value.repeat,
            play_count_threshold_percent: value.play_count_threshold_percent,
            tray_enabled: value.tray_enabled,
            mpris_enabled: value.mpris_enabled,
            log_level: value.log_level,
            log_to_file: value.log_to_file,
            log_retention_weeks: value.log_retention_weeks,
            page_size: value.page_size,
            ui_font: value.ui_font,
            ui_scale: value.ui_scale,
            layout_snap_enabled: value.layout_snap_enabled,
            layout_snap_grid_px: value.layout_snap_grid_px,
            still_mode: value.still_mode,
            analysis_fps: value.analysis_fps,
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
            waveform_cursor_color: value.waveform_cursor_color,
            waveform_glow: value.waveform_glow,
            waveform_rms_gain: value.waveform_rms_gain,
            waveform_peak_gain: value.waveform_peak_gain,
            waveform_peak_gamma: value.waveform_peak_gamma,
            visualization_cache: value.visualization_cache,
            media_read_buffer_mb: value.media_read_buffer_mb,
            ffmpeg_enabled: value.ffmpeg_enabled,
            pipewire_auto_mix: value.pipewire_auto_mix,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct WireSystemState {
    scanning: bool,
    scan_message: String,
    #[serde(default)]
    scan_progress: Option<ScanProgress>,
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
}

impl From<SystemState> for WireSystemState {
    fn from(value: SystemState) -> Self {
        Self {
            scanning: value.scanning,
            scan_message: value.scan_message,
            scan_progress: value.scan_progress,
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
        }
    }
}

impl From<WireSystemState> for SystemState {
    fn from(value: WireSystemState) -> Self {
        Self {
            scanning: value.scanning,
            scan_message: value.scan_message,
            scan_progress: value.scan_progress,
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
        }
    }
}

pub(super) fn wire_state(response: StateResponse) -> WireStateResponse {
    WireStateResponse {
        revisions: response.revisions,
        playback: response.playback,
        queue: response.queue,
        library: response.library,
        system: response.system.map(WireSystemState::from),
    }
}

pub(super) fn state_response(response: WireStateResponse) -> StateResponse {
    StateResponse {
        revisions: response.revisions,
        playback: response.playback,
        queue: response.queue,
        library: response.library,
        system: response.system.map(SystemState::from),
    }
}
