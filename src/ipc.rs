use crate::{
    core::AppHandle,
    model::{Command, DatabaseOptimization, Query, ScanProgress, SystemState},
    response::{Ack, QueryResponse, StateResponse, StateRevisions, StateSections},
};
use anyhow::{Context, Error, Result, bail, ensure};
use bincode::{
    config,
    serde::{decode_from_slice, encode_into_std_write},
};
use bytes::{Buf, Bytes, BytesMut};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Write},
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    runtime::Builder,
    sync::watch,
    time::timeout,
};

const PROTOCOL_VERSION: u8 = 2;
const MAX_REQUEST: usize = 64 * 1024;
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const MAX_CLIENTS: usize = 16;
const HELLO_TIMEOUT: Duration = Duration::from_secs(1);
const UNKNOWN_INSTANCE: u32 = 0;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestType {
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
enum ResponseType {
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
enum SessionState {
    Unbound,
    Bound,
}

impl SessionState {
    fn is_bound(&self) -> bool {
        matches!(self, Self::Bound)
    }
}

enum WireRequest {
    Hello(StateSections),
    State(StateSections),
    Query(WireQuery),
    Command(WireCommand),
    Watch(StateRevisions),
}

/// Binary query wire format. Variant order is a protocol contract; append only.
#[derive(Serialize, Deserialize)]
enum WireQuery {
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
}

/// Binary command wire format. Variant order is a protocol contract; append only.
#[derive(Serialize, Deserialize)]
enum WireCommand {
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
        config: WireConfig,
    },
    MprisStatus {
        status: String,
    },
    Shutdown,
}

fn wire_usize(value: u64, field: &str) -> Result<usize> {
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
                offset,
                limit,
            } => Self::DirectoryPage {
                path,
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
                offset,
                limit,
            } => Self::DirectoryPage {
                path,
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
            Command::Configure { config } => Self::Configure {
                config: WireConfig::from(&config),
            },
            Command::MprisStatus { status } => Self::MprisStatus { status },
            Command::Shutdown => Self::Shutdown,
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
            WireCommand::Configure { config } => Self::Configure {
                config: config.into(),
            },
            WireCommand::MprisStatus { status } => Self::MprisStatus { status },
            WireCommand::Shutdown => Self::Shutdown,
        })
    }
}

fn decode_message_header(bytes: &[u8]) -> Result<(u8, u8, &[u8])> {
    ensure!(bytes.len() >= 2, "IPC message header is truncated");
    Ok((bytes[0], bytes[1], &bytes[2..]))
}

fn decode_body<T: for<'de> Deserialize<'de>>(
    bytes: &[u8],
    codec: impl bincode::config::Config,
    context: &'static str,
) -> Result<T> {
    let (value, consumed) = decode_from_slice(bytes, codec).context(context)?;
    ensure!(
        consumed == bytes.len(),
        "Trailing bytes in IPC message body"
    );
    Ok(value)
}

fn decode_request_frame(bytes: &[u8]) -> Result<(u8, WireRequest)> {
    let (version, message_type, body) = decode_message_header(bytes)?;
    let message_type = RequestType::try_from(message_type)?;
    let request = match message_type {
        RequestType::Hello => WireRequest::Hello(decode_body(
            body,
            request_config(),
            "Decoding IPC Hello request",
        )?),
        RequestType::State => WireRequest::State(decode_body(
            body,
            request_config(),
            "Decoding IPC state request",
        )?),
        RequestType::Query => WireRequest::Query(decode_body(
            body,
            request_config(),
            "Decoding IPC query request",
        )?),
        RequestType::Command => WireRequest::Command(decode_body(
            body,
            request_config(),
            "Decoding IPC command request",
        )?),
        RequestType::Watch => WireRequest::Watch(decode_body(
            body,
            request_config(),
            "Decoding IPC watch request",
        )?),
    };
    Ok((version, request))
}

fn decode_response_frame(bytes: &[u8]) -> Result<(u8, WireResponse)> {
    let (version, message_type, body) = decode_message_header(bytes)?;
    let message_type = ResponseType::try_from(message_type)?;
    let response = match message_type {
        ResponseType::Hello => WireResponse::Hello(decode_body(
            body,
            config::standard(),
            "Decoding IPC Hello response",
        )?),
        ResponseType::State => WireResponse::State(decode_body(
            body,
            config::standard(),
            "Decoding IPC state response",
        )?),
        ResponseType::Query => WireResponse::Query(decode_body(
            body,
            config::standard(),
            "Decoding IPC query response",
        )?),
        ResponseType::Ack => WireResponse::Ack(decode_body(
            body,
            config::standard(),
            "Decoding IPC acknowledgement",
        )?),
        ResponseType::Watch => WireResponse::Watch(decode_body(
            body,
            config::standard(),
            "Decoding IPC watch response",
        )?),
        ResponseType::Error => WireResponse::Error(decode_body(
            body,
            config::standard(),
            "Decoding IPC error response",
        )?),
    };
    Ok((version, response))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct HelloResponse {
    instance_id: [u8; 4],
    state: WireStateResponse,
}

#[derive(Clone, Debug)]
enum WireResponse {
    Hello(HelloResponse),
    Query(QueryResponse),
    State(WireStateResponse),
    Ack(Ack),
    Watch(StateRevisions),
    Error(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WireStateResponse {
    revisions: StateRevisions,
    playback: Option<crate::response::PlaybackSnapshot>,
    queue: Option<crate::model::QueueState>,
    library: Option<crate::model::LibrarySnapshot>,
    #[serde(with = "wire_system_option")]
    system: Option<WireSystemState>,
}

/// Complete positional IPC schema for `Config`.
///
/// Keep this explicit instead of serializing `Config` directly: its human-readable
/// serde attributes are not a stable binary wire contract.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct WireConfig {
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
    queue_limit: u32,
    page_size: u32,
    ui_font: String,
    ui_scale: f32,
    analysis_fps: u32,
    visual_background: crate::config::RgbColor,
    visual_palette: crate::config::VisualizationPalette,
    spectrum_style: crate::config::SpectrumStyle,
    spectrum_fft_size: u32,
    spectrum_window: crate::config::SpectrumWindow,
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
    radial_spectrum_style: crate::config::RadialSpectrumStyle,
    radial_spectrum_sensitivity: f32,
    radial_spectrum_rotation_speed: f32,
    radial_spectrum_bar_width: f32,
    radial_spectrum_bar_glow_layers: u32,
    radial_spectrum_ring_opacity: f32,
    radial_spectrum_bloom_intensity: f32,
    radial_spectrum_inner_diameter: f32,
    radial_spectrum_fade_when_idle: bool,
    radial_spectrum_primary_color: crate::config::RgbColor,
    radial_spectrum_secondary_color: crate::config::RgbColor,
    spectrogram_labels: bool,
    spectrogram_interpolate: bool,
    spectrogram_sampling_points_scale: f32,
    spectrogram_interpolation_points: u32,
    waveform_labels: bool,
    spectrum_db_range: f32,
    spectrogram_db_range: f32,
    spectrogram_history_seconds: u32,
    waveform_cursor_color: crate::config::RgbColor,
    waveform_glow: f32,
    media_read_buffer_mb: u32,
    nerd_symbols: bool,
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
            queue_limit: value.queue_limit,
            page_size: value.page_size,
            ui_font: value.ui_font.clone(),
            ui_scale: value.ui_scale,
            analysis_fps: value.analysis_fps,
            visual_background: value.visual_background,
            visual_palette: value.visual_palette,
            spectrum_style: value.spectrum_style,
            spectrum_fft_size: value.spectrum_fft_size,
            spectrum_window: value.spectrum_window,
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
            radial_spectrum_style: value.radial_spectrum_style,
            radial_spectrum_sensitivity: value.radial_spectrum_sensitivity,
            radial_spectrum_rotation_speed: value.radial_spectrum_rotation_speed,
            radial_spectrum_bar_width: value.radial_spectrum_bar_width,
            radial_spectrum_bar_glow_layers: value.radial_spectrum_bar_glow_layers,
            radial_spectrum_ring_opacity: value.radial_spectrum_ring_opacity,
            radial_spectrum_bloom_intensity: value.radial_spectrum_bloom_intensity,
            radial_spectrum_inner_diameter: value.radial_spectrum_inner_diameter,
            radial_spectrum_fade_when_idle: value.radial_spectrum_fade_when_idle,
            radial_spectrum_primary_color: value.radial_spectrum_primary_color,
            radial_spectrum_secondary_color: value.radial_spectrum_secondary_color,
            spectrogram_labels: value.spectrogram_labels,
            spectrogram_interpolate: value.spectrogram_interpolate,
            spectrogram_sampling_points_scale: value.spectrogram_sampling_points_scale,
            spectrogram_interpolation_points: value.spectrogram_interpolation_points,
            waveform_labels: value.waveform_labels,
            spectrum_db_range: value.spectrum_db_range,
            spectrogram_db_range: value.spectrogram_db_range,
            spectrogram_history_seconds: value.spectrogram_history_seconds,
            waveform_cursor_color: value.waveform_cursor_color,
            waveform_glow: value.waveform_glow,
            media_read_buffer_mb: value.media_read_buffer_mb,
            nerd_symbols: value.nerd_symbols,
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
            queue_limit: value.queue_limit,
            page_size: value.page_size,
            ui_font: value.ui_font,
            ui_scale: value.ui_scale,
            analysis_fps: value.analysis_fps,
            visual_background: value.visual_background,
            visual_palette: value.visual_palette,
            spectrum_style: value.spectrum_style,
            spectrum_fft_size: value.spectrum_fft_size,
            spectrum_window: value.spectrum_window,
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
            radial_spectrum_style: value.radial_spectrum_style,
            radial_spectrum_sensitivity: value.radial_spectrum_sensitivity,
            radial_spectrum_rotation_speed: value.radial_spectrum_rotation_speed,
            radial_spectrum_bar_width: value.radial_spectrum_bar_width,
            radial_spectrum_bar_glow_layers: value.radial_spectrum_bar_glow_layers,
            radial_spectrum_ring_opacity: value.radial_spectrum_ring_opacity,
            radial_spectrum_bloom_intensity: value.radial_spectrum_bloom_intensity,
            radial_spectrum_inner_diameter: value.radial_spectrum_inner_diameter,
            radial_spectrum_fade_when_idle: value.radial_spectrum_fade_when_idle,
            radial_spectrum_primary_color: value.radial_spectrum_primary_color,
            radial_spectrum_secondary_color: value.radial_spectrum_secondary_color,
            spectrogram_labels: value.spectrogram_labels,
            spectrogram_interpolate: value.spectrogram_interpolate,
            spectrogram_sampling_points_scale: value.spectrogram_sampling_points_scale,
            spectrogram_interpolation_points: value.spectrogram_interpolation_points,
            waveform_labels: value.waveform_labels,
            spectrum_db_range: value.spectrum_db_range,
            spectrogram_db_range: value.spectrogram_db_range,
            spectrogram_history_seconds: value.spectrogram_history_seconds,
            waveform_cursor_color: value.waveform_cursor_color,
            waveform_glow: value.waveform_glow,
            media_read_buffer_mb: value.media_read_buffer_mb,
            nerd_symbols: value.nerd_symbols,
            ffmpeg_enabled: value.ffmpeg_enabled,
            pipewire_auto_mix: value.pipewire_auto_mix,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WireSystemState {
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

impl From<&SystemState> for WireSystemState {
    fn from(value: &SystemState) -> Self {
        Self {
            scanning: value.scanning,
            scan_message: value.scan_message.clone(),
            scan_progress: value.scan_progress.clone(),
            last_error: value.last_error.clone(),
            devices: value.devices.clone(),
            selected_device: value.selected_device.clone(),
            revision: value.revision,
            config: WireConfig::from(value.config.as_ref()),
            config_path: value.config_path.clone(),
            mpris_status: value.mpris_status.clone(),
            ffmpeg_status: value.ffmpeg_status.clone(),
            database_optimization: value.database_optimization.clone(),
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

mod wire_system_option {
    use super::WireSystemState;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S>(value: &Option<WireSystemState>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        value.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<WireSystemState>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<WireSystemState>::deserialize(deserializer)
    }
}

fn wire_state(response: StateResponse) -> WireStateResponse {
    WireStateResponse {
        revisions: response.revisions,
        playback: response.playback,
        queue: response.queue,
        library: response.library,
        system: response.system.as_ref().map(WireSystemState::from),
    }
}

fn state_response(response: WireStateResponse) -> StateResponse {
    StateResponse {
        revisions: response.revisions,
        playback: response.playback,
        queue: response.queue,
        library: response.library,
        system: response.system.map(SystemState::from),
    }
}

struct FrameWriter<'a> {
    bytes: &'a mut Vec<u8>,
    limit: usize,
}

impl Write for FrameWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > 4 + self.limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "IPC frame exceeds maximum size",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encode_message_into_with<T, C>(
    version: u8,
    message_type: u8,
    value: &T,
    bytes: &mut Vec<u8>,
    codec: C,
    limit: usize,
) -> Result<()>
where
    T: Serialize,
    C: config::Config,
{
    bytes.clear();
    bytes.resize(6, 0);
    bytes[4] = version;
    bytes[5] = message_type;
    let mut writer = FrameWriter { bytes, limit };
    encode_into_std_write(value, &mut writer, codec).context("Encoding IPC message")?;
    let length = u32::try_from(writer.bytes.len() - 4).context("IPC frame is too large")?;
    writer.bytes[..4].copy_from_slice(&length.to_le_bytes());
    Ok(())
}

fn encode_request_frame_into(request: &WireRequest, bytes: &mut Vec<u8>) -> Result<()> {
    match request {
        WireRequest::Hello(value) => encode_message_into_with(
            PROTOCOL_VERSION,
            RequestType::Hello as u8,
            value,
            bytes,
            request_config(),
            MAX_REQUEST,
        ),
        WireRequest::State(value) => encode_message_into_with(
            PROTOCOL_VERSION,
            RequestType::State as u8,
            value,
            bytes,
            request_config(),
            MAX_REQUEST,
        ),
        WireRequest::Query(value) => encode_message_into_with(
            PROTOCOL_VERSION,
            RequestType::Query as u8,
            value,
            bytes,
            request_config(),
            MAX_REQUEST,
        ),
        WireRequest::Command(value) => encode_message_into_with(
            PROTOCOL_VERSION,
            RequestType::Command as u8,
            value,
            bytes,
            request_config(),
            MAX_REQUEST,
        ),
        WireRequest::Watch(value) => encode_message_into_with(
            PROTOCOL_VERSION,
            RequestType::Watch as u8,
            value,
            bytes,
            request_config(),
            MAX_REQUEST,
        ),
    }
}

fn encode_response_frame_into(response: &WireResponse, bytes: &mut Vec<u8>) -> Result<()> {
    match response {
        WireResponse::Hello(value) => encode_message_into_with(
            PROTOCOL_VERSION,
            ResponseType::Hello as u8,
            value,
            bytes,
            config::standard(),
            MAX_RESPONSE,
        ),
        WireResponse::State(value) => encode_message_into_with(
            PROTOCOL_VERSION,
            ResponseType::State as u8,
            value,
            bytes,
            config::standard(),
            MAX_RESPONSE,
        ),
        WireResponse::Query(value) => encode_message_into_with(
            PROTOCOL_VERSION,
            ResponseType::Query as u8,
            value,
            bytes,
            config::standard(),
            MAX_RESPONSE,
        ),
        WireResponse::Ack(value) => encode_message_into_with(
            PROTOCOL_VERSION,
            ResponseType::Ack as u8,
            value,
            bytes,
            config::standard(),
            MAX_RESPONSE,
        ),
        WireResponse::Watch(value) => encode_message_into_with(
            PROTOCOL_VERSION,
            ResponseType::Watch as u8,
            value,
            bytes,
            config::standard(),
            MAX_RESPONSE,
        ),
        WireResponse::Error(value) => encode_message_into_with(
            PROTOCOL_VERSION,
            ResponseType::Error as u8,
            value,
            bytes,
            config::standard(),
            MAX_RESPONSE,
        ),
    }
}

pub struct Server {
    path: PathBuf,
    shutdown: watch::Sender<bool>,
    bridge_stop: crossbeam_channel::Sender<()>,
    worker: Option<JoinHandle<()>>,
}

pub fn bind(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if !path.exists() {
        return Ok(());
    }
    if !fs::symlink_metadata(path)?.file_type().is_socket() {
        bail!("Refusing to replace non-socket {}", path.display());
    }
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => bail!(
            "Rivu is already running at {}. Use its CLI or TUI.",
            path.display()
        ),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            fs::remove_file(path)?;
            Ok(())
        }
        Err(error) => Err(error).context("Checking existing Rivu socket"),
    }
}

impl Server {
    pub fn start(path: PathBuf, handle: AppHandle) -> Result<Self> {
        let (shutdown, shutdown_rx) = watch::channel(false);
        let (bridge_stop, bridge_stop_rx) = crossbeam_channel::bounded(1);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let worker_path = path.clone();
        let worker = thread::Builder::new().name("rivu-ipc".into()).spawn(move || {
            let runtime = match Builder::new_current_thread().enable_io().enable_time().build() {
                Ok(runtime) => runtime,
                Err(error) => { let _ = ready_tx.send(Err(anyhow::Error::from(error))); return; }
            };
            let listener = { let _guard = runtime.enter(); UnixListener::bind(&worker_path) };
            let listener = match listener {
                Ok(listener) => listener,
                Err(error) => { let _ = ready_tx.send(Err(anyhow::Error::from(error))); return; }
            };
            if let Err(error) = fs::set_permissions(&worker_path, fs::Permissions::from_mode(0o600)) {
                let _ = ready_tx.send(Err(anyhow::Error::from(error))); return;
            }
            let (revision_tx, revision_rx) = watch::channel(handle.state_revisions());
            let updates = handle.subscribe();
            let bridge_handle = handle.clone();
            let bridge = thread::Builder::new().name("rivu-ipc-revisions".into()).spawn(move || {
                loop {
                    crossbeam_channel::select! {
                        recv(updates) -> message => {
                            if message.is_ok() { let _ = revision_tx.send(bridge_handle.state_revisions()); } else { break; }
                        }
                        recv(bridge_stop_rx) -> _ => break,
                    }
                }
            });
            let Ok(bridge) = bridge else {
                let _ = ready_tx.send(Err(anyhow::anyhow!("starting IPC revision bridge"))); return;
            };
            let _ = ready_tx.send(Ok(()));
            runtime.block_on(run_server(listener, handle, revision_rx, shutdown_rx));
            let _ = bridge.join();
        })?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                path,
                shutdown,
                bridge_stop,
                worker: Some(worker),
            }),
            Ok(Err(error)) => {
                let _ = worker.join();
                Err(error)
            }
            Err(error) => {
                let _ = worker.join();
                Err(anyhow::Error::from(error))
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        let _ = self.bridge_stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let _ = fs::remove_file(&self.path);
    }
}

async fn read_frame<S: AsyncRead + Unpin>(
    stream: &mut S,
    limit: usize,
    pending: &mut BytesMut,
) -> Result<Bytes> {
    let mut scratch = [0u8; 8192];
    while pending.len() < 4 {
        let need = 4 - pending.len();
        let read = stream
            .read(&mut scratch[..need])
            .await
            .context("Reading IPC frame")?;
        if read == 0 {
            bail!("IPC connection closed while reading frame");
        }
        pending.extend_from_slice(&scratch[..read]);
    }
    let length = u32::from_le_bytes(pending[..4].try_into().unwrap()) as usize;
    if length > limit {
        bail!("IPC frame exceeds maximum size");
    }
    pending.reserve((4 + length).saturating_sub(pending.len()));
    while pending.len() < 4 + length {
        let need = (4 + length - pending.len()).min(scratch.len());
        let read = stream
            .read(&mut scratch[..need])
            .await
            .context("Reading IPC frame")?;
        if read == 0 {
            bail!("IPC connection closed while reading frame");
        }
        pending.extend_from_slice(&scratch[..read]);
    }
    pending.advance(4);
    Ok(pending.split_to(length).freeze())
}

async fn write_frame_buffered<S: AsyncWrite + Unpin>(
    stream: &mut S,
    value: &WireResponse,
    buffer: &mut Vec<u8>,
) -> Result<()> {
    encode_response_frame_into(value, buffer)?;
    stream
        .write_all(buffer)
        .await
        .context("Writing IPC frame")?;
    stream.flush().await.context("Flushing IPC frame")?;
    Ok(())
}

async fn write_error_frame<S: AsyncWrite + Unpin>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
    error: impl Into<String>,
) -> Result<()> {
    let response = error_frame(error);
    write_frame_buffered(stream, &response, buffer).await
}

async fn write_request_frame_buffered<S: AsyncWrite + Unpin>(
    stream: &mut S,
    request: &WireRequest,
    buffer: &mut Vec<u8>,
) -> Result<()> {
    encode_request_frame_into(request, buffer)?;
    stream
        .write_all(buffer)
        .await
        .context("Writing IPC frame")?;
    stream.flush().await.context("Flushing IPC frame")?;
    Ok(())
}

async fn read_response_frame<S: AsyncRead + Unpin>(
    stream: &mut S,
    pending: &mut BytesMut,
) -> Result<WireResponse> {
    let bytes = read_frame(stream, MAX_RESPONSE, pending).await?;
    let (version, response) = decode_response_frame(&bytes)?;
    ensure!(
        version == PROTOCOL_VERSION,
        "Unsupported IPC protocol version {version}"
    );
    Ok(response)
}

fn request_config() -> impl bincode::config::Config {
    config::standard().with_limit::<MAX_REQUEST>()
}

fn error_frame(error: impl Into<String>) -> WireResponse {
    WireResponse::Error(error.into())
}

fn unpack_error(response: WireResponse) -> Result<WireResponse> {
    if let WireResponse::Error(error) = response {
        bail!("IPC server error: {error}");
    }
    Ok(response)
}

fn validate_hello(response: HelloResponse) -> Result<HelloResponse> {
    ensure!(
        u32::from_le_bytes(response.instance_id) != UNKNOWN_INSTANCE,
        "IPC Hello returned instance 0"
    );
    Ok(response)
}

fn unpack_hello(response: WireResponse) -> Result<HelloResponse> {
    let WireResponse::Hello(response) = unpack_error(response)? else {
        bail!("IPC response was not a Hello response")
    };
    validate_hello(response)
}

async fn run_command(handle: &AppHandle, command: Command) -> Result<Ack> {
    let duration = if matches!(command, Command::OptimizeDatabase) {
        Duration::from_secs(120)
    } else {
        Duration::from_secs(15)
    };
    timeout(
        duration,
        tokio::task::spawn_blocking({
            let handle = handle.clone();
            move || handle.request_ack(command)
        }),
    )
    .await
    .context("Reading IPC acknowledgement timed out")?
    .map_err(|error| anyhow::anyhow!("Core command task failed: {error}"))
}

async fn run_query(handle: &AppHandle, query: Query) -> Result<QueryResponse> {
    timeout(
        Duration::from_secs(15),
        tokio::task::spawn_blocking({
            let handle = handle.clone();
            move || handle.query(query)
        }),
    )
    .await
    .context("Reading IPC query timed out")?
    .map_err(|error| anyhow::anyhow!("Core query task failed: {error}"))
}

async fn wait_for_revisions(
    stream: &mut UnixStream,
    pending: &mut BytesMut,
    handle: &AppHandle,
    expected: StateRevisions,
    updates: &mut watch::Receiver<StateRevisions>,
    shutdown: &mut watch::Receiver<bool>,
) -> Option<Option<StateRevisions>> {
    let mut scratch = [0u8; 8192];
    loop {
        let current = handle.state_revisions();
        if current != expected {
            return Some(Some(current));
        }
        tokio::select! {
            changed = updates.changed() => { if changed.is_err() { return None; } }
            changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { return None; } }
            ready = stream.readable() => {
                if ready.is_err() { return None; }
                match stream.try_read(&mut scratch) {
                    Ok(0) => return None,
                    Ok(read) => {
                        pending.extend_from_slice(&scratch[..read]);
                        if pending.len() >= 4 {
                            let length = u32::from_le_bytes(pending[..4].try_into().unwrap()) as usize;
                            if length > MAX_REQUEST { return None; }
                            if pending.len() >= 4 + length { return Some(None); }
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => return None,
                }
            }
        }
    }
}

async fn serve_connection(
    mut stream: UnixStream,
    handle: AppHandle,
    instance_id: u32,
    mut updates: watch::Receiver<StateRevisions>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut pending = BytesMut::new();
    let mut write_buffer = Vec::new();
    let mut session = SessionState::Unbound;
    loop {
        let bytes = if session.is_bound() {
            tokio::select! {
                frame = read_frame(&mut stream, MAX_REQUEST, &mut pending) => frame?,
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return Ok(()); }
                    continue;
                }
            }
        } else {
            tokio::select! {
                frame = timeout(HELLO_TIMEOUT, read_frame(&mut stream, MAX_REQUEST, &mut pending)) => {
                    match frame {
                        Ok(frame) => frame?,
                        Err(_) => return Ok(()),
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return Ok(()); }
                    continue;
                }
            }
        };
        let (version, request) = match decode_request_frame(&bytes) {
            Ok(frame) => frame,
            Err(error) => {
                write_error_frame(
                    &mut stream,
                    &mut write_buffer,
                    format!("Invalid IPC request: {error}"),
                )
                .await?;
                return Ok(());
            }
        };
        if version != PROTOCOL_VERSION {
            write_error_frame(
                &mut stream,
                &mut write_buffer,
                format!("Unsupported IPC protocol version {version}"),
            )
            .await?;
            return Ok(());
        }
        if !session.is_bound() && !matches!(request, WireRequest::Hello(_)) {
            write_error_frame(
                &mut stream,
                &mut write_buffer,
                "IPC session requires a Hello handshake",
            )
            .await?;
            continue;
        }
        match request {
            WireRequest::Hello(sections) => {
                if session.is_bound() {
                    write_error_frame(
                        &mut stream,
                        &mut write_buffer,
                        "IPC session is already handshaken",
                    )
                    .await?;
                    continue;
                }
                let response = WireResponse::Hello(HelloResponse {
                    instance_id: instance_id.to_le_bytes(),
                    state: wire_state(handle.state(sections)),
                });
                write_frame_buffered(&mut stream, &response, &mut write_buffer).await?;
                session = SessionState::Bound;
            }
            WireRequest::State(sections) => {
                let response = WireResponse::State(wire_state(handle.state(sections)));
                write_frame_buffered(&mut stream, &response, &mut write_buffer).await?;
            }
            WireRequest::Query(query) => {
                let query = match Query::try_from(query) {
                    Ok(query) => query,
                    Err(error) => {
                        write_error_frame(
                            &mut stream,
                            &mut write_buffer,
                            format!("Invalid IPC query: {error}"),
                        )
                        .await?;
                        continue;
                    }
                };
                let response = WireResponse::Query(run_query(&handle, query).await?);
                write_frame_buffered(&mut stream, &response, &mut write_buffer).await?;
            }
            WireRequest::Command(command) => {
                let command = match Command::try_from(command) {
                    Ok(command) => command,
                    Err(error) => {
                        write_error_frame(
                            &mut stream,
                            &mut write_buffer,
                            format!("Invalid IPC command: {error}"),
                        )
                        .await?;
                        continue;
                    }
                };
                let response = WireResponse::Ack(run_command(&handle, command).await?);
                write_frame_buffered(&mut stream, &response, &mut write_buffer).await?;
            }
            WireRequest::Watch(expected) => {
                let Some(result) = wait_for_revisions(
                    &mut stream,
                    &mut pending,
                    &handle,
                    expected,
                    &mut updates,
                    &mut shutdown,
                )
                .await
                else {
                    return Ok(());
                };
                let Some(revisions) = result else {
                    continue;
                };
                let response = WireResponse::Watch(revisions);
                write_frame_buffered(&mut stream, &response, &mut write_buffer).await?;
            }
        }
    }
}

async fn run_server(
    listener: UnixListener,
    handle: AppHandle,
    updates: watch::Receiver<StateRevisions>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut rng = rand::rng();
    let mut instance_id = UNKNOWN_INSTANCE;
    while instance_id == UNKNOWN_INSTANCE {
        instance_id = rng.random();
    }
    let mut clients = Vec::new();
    let revisions = updates;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue; };
                clients.retain(|task: &tokio::task::JoinHandle<Result<()>>| !task.is_finished());
                if clients.len() < MAX_CLIENTS { clients.push(tokio::spawn(serve_connection(stream, handle.clone(), instance_id, revisions.clone(), shutdown.clone()))); }
            }
            changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { break; } }
        }
    }
    for task in clients {
        task.abort();
        let _ = task.await;
    }
}

pub fn is_no_instance(error: &Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            )
        })
    })
}

pub struct WatcherSession {
    runtime: tokio::runtime::Runtime,
    stream: UnixStream,
    cancelled: Arc<AtomicBool>,
    cancel_notify: Arc<tokio::sync::Notify>,
    read_buffer: BytesMut,
    write_buffer: Vec<u8>,
    session: SessionState,
}

fn client_runtime() -> Result<tokio::runtime::Runtime> {
    Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("creating IPC runtime")
}

pub fn watch_session(path: &Path) -> Result<WatcherSession> {
    watch_session_with_cancel(
        path,
        Arc::new(AtomicBool::new(false)),
        Arc::new(tokio::sync::Notify::new()),
    )
}

pub fn watch_session_with_cancel(
    path: &Path,
    cancelled: Arc<AtomicBool>,
    cancel_notify: Arc<tokio::sync::Notify>,
) -> Result<WatcherSession> {
    let runtime = client_runtime()?;
    let stream = runtime
        .block_on(UnixStream::connect(path))
        .with_context(|| {
            format!(
                "Cannot connect to Rivu at {}. Start `rivu` or `rivu serve` first.",
                path.display()
            )
        })?;
    Ok(WatcherSession {
        runtime,
        stream,
        cancelled,
        cancel_notify,
        read_buffer: BytesMut::new(),
        write_buffer: Vec::new(),
        session: SessionState::Unbound,
    })
}
impl WatcherSession {
    pub fn cancellation(&self) -> (Arc<AtomicBool>, Arc<tokio::sync::Notify>) {
        (self.cancelled.clone(), self.cancel_notify.clone())
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.cancel_notify.notify_waiters();
    }

    fn ensure_handshake(&self) -> Result<()> {
        if self.session.is_bound() {
            Ok(())
        } else {
            bail!("Watcher session requires a Hello handshake")
        }
    }

    fn exchange(&mut self, request: &WireRequest) -> Result<WireResponse> {
        self.runtime.block_on(async {
            let notified = self.cancel_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.cancelled.load(Ordering::Acquire) {
                bail!("IPC request cancelled");
            }
            tokio::select! {
                _ = &mut notified => bail!("IPC request cancelled"),
                response = async {
                    write_request_frame_buffered(&mut self.stream, request, &mut self.write_buffer).await?;
                    read_response_frame(&mut self.stream, &mut self.read_buffer).await
                } => {
                    let response = response?;
                    if self.cancelled.load(Ordering::Acquire) {
                        bail!("IPC request cancelled");
                    }
                    Ok(response)
                }
            }
        })
    }

    pub fn get_state(&mut self, sections: StateSections) -> Result<StateResponse> {
        let request = if self.session.is_bound() {
            WireRequest::State(sections)
        } else {
            WireRequest::Hello(sections)
        };
        match unpack_error(self.exchange(&request)?)? {
            WireResponse::Hello(response) => {
                let response = validate_hello(response)?;
                self.session = SessionState::Bound;
                Ok(state_response(response.state))
            }
            WireResponse::State(state) if self.session.is_bound() => Ok(state_response(state)),
            _ => bail!("IPC response was not a state or Hello response"),
        }
    }

    pub fn query(&mut self, query: &Query) -> Result<QueryResponse> {
        self.ensure_handshake()?;
        let request = WireRequest::Query(WireQuery::from(query.clone()));
        match unpack_error(self.exchange(&request)?)? {
            WireResponse::Query(response) => Ok(response),
            _ => bail!("IPC response was not a query response"),
        }
    }

    pub fn watch(&mut self, revisions: StateRevisions) -> Result<StateRevisions> {
        self.watch_until(revisions)?
            .ok_or_else(|| anyhow::anyhow!("Watcher cancelled"))
    }

    pub(crate) fn watch_until(
        &mut self,
        revisions: StateRevisions,
    ) -> Result<Option<StateRevisions>> {
        self.ensure_handshake()?;
        let request = WireRequest::Watch(revisions);
        let response = match self.exchange(&request) {
            Ok(response) => response,
            Err(_) if self.cancelled.load(Ordering::Acquire) => return Ok(None),
            Err(error) => return Err(error),
        };
        match unpack_error(response)? {
            WireResponse::Watch(revisions) => Ok(Some(revisions)),
            _ => bail!("IPC response was not a watch response"),
        }
    }
}

async fn one_shot(
    path: &Path,
    request: WireRequest,
    cancellation: Option<(Arc<AtomicBool>, Arc<tokio::sync::Notify>)>,
) -> Result<WireResponse> {
    let operation = async {
        let mut stream = UnixStream::connect(path).await.with_context(|| {
            format!(
                "Cannot connect to Rivu at {}. Start `rivu` or `rivu serve` first.",
                path.display()
            )
        })?;
        let mut pending = BytesMut::new();
        let mut write_buffer = Vec::new();
        let state_sections = match &request {
            WireRequest::State(sections) => Some(*sections),
            _ => None,
        };
        write_request_frame_buffered(
            &mut stream,
            &WireRequest::Hello(state_sections.unwrap_or_default()),
            &mut write_buffer,
        )
        .await?;
        let response = timeout(
            Duration::from_secs(120),
            read_response_frame(&mut stream, &mut pending),
        )
        .await
        .context("Reading IPC Hello response timed out")??;
        let hello = unpack_hello(response)?;
        if state_sections.is_some() {
            return Ok(WireResponse::State(hello.state));
        }
        write_request_frame_buffered(&mut stream, &request, &mut write_buffer).await?;
        let response = timeout(
            Duration::from_secs(120),
            read_response_frame(&mut stream, &mut pending),
        )
        .await
        .context("Reading IPC response timed out")??;
        unpack_error(response)
    };
    let Some((cancelled, notify)) = cancellation else {
        return operation.await;
    };
    let notified = notify.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    if cancelled.load(Ordering::Acquire) {
        bail!("IPC request cancelled");
    }
    tokio::select! { _ = &mut notified => bail!("IPC request cancelled"), response = operation => response }
}

pub fn query(path: &Path, query: &Query) -> Result<QueryResponse> {
    let runtime = client_runtime()?;
    let frame = runtime.block_on(one_shot(
        path,
        WireRequest::Query(WireQuery::from(query.clone())),
        None,
    ))?;
    match frame {
        WireResponse::Query(response) => Ok(response),
        _ => bail!("IPC response was not a query response"),
    }
}

pub(crate) fn query_with_cancel(
    path: &Path,
    query: &Query,
    cancelled: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
) -> Result<QueryResponse> {
    let runtime = client_runtime()?;
    let frame = runtime.block_on(one_shot(
        path,
        WireRequest::Query(WireQuery::from(query.clone())),
        Some((cancelled, notify)),
    ))?;
    match frame {
        WireResponse::Query(response) => Ok(response),
        _ => bail!("IPC response was not a query response"),
    }
}

pub fn get_state(path: &Path, sections: StateSections) -> Result<StateResponse> {
    let runtime = client_runtime()?;
    let frame = runtime.block_on(one_shot(path, WireRequest::State(sections), None))?;
    match frame {
        WireResponse::State(response) => Ok(state_response(response)),
        _ => bail!("IPC response was not a state response"),
    }
}

pub fn request_ack(path: &Path, command: &Command) -> Result<Ack> {
    let runtime = client_runtime()?;
    let frame = runtime.block_on(one_shot(
        path,
        WireRequest::Command(WireCommand::from(command.clone())),
        None,
    ))?;
    match frame {
        WireResponse::Ack(response) => Ok(response),
        _ => bail!("IPC response was not an acknowledgement"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    include!("ipc_tests.rs");
}
