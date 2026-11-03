//! In-process display publications, independent of frontend toolkit and engine.
use crate::model::{
    Config, HistoryEntry, MediaTaskOwner, PlaybackState, ProcessingAudition, QueueState, Track,
};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone, Debug, Default)]
pub struct DisplayState {
    pub library: DisplayLibraryState,
    pub queue: QueueState,
    pub current_track: Option<Arc<Track>>,
    pub playback: PlaybackState,
    pub source_generation: u64,
    pub system: DisplaySystemState,
    pub revisions: crate::response::StateRevisions,
}

#[derive(Clone, Debug, Default)]
pub struct DisplayLibraryState {
    pub track_total: usize,
    pub playlist_total: usize,
    pub revision: u64,
    pub structure_revision: u64,
    pub album_revision: u64,
    pub playlist_revision: u64,
    pub playlist_entry_revisions: Arc<BTreeMap<i64, u64>>,
    pub metadata_revisions: Arc<crate::model::MetadataRevisions>,
    pub bitrate_revision: u64,
    pub duplicate_groups_revision: u64,
    pub duplicate_members_revision: u64,
    pub history: Arc<Vec<HistoryEntry>>,
}

#[derive(Clone, Debug, Default)]
pub struct DisplaySystemState {
    pub scanning: bool,
    pub scan_message: String,
    pub scan_progress: Option<crate::model::ScanProgress>,
    pub scan_task_id: Option<u64>,
    pub scan_owner: Option<MediaTaskOwner>,
    pub last_error: Option<String>,
    pub devices: Arc<Vec<String>>,
    /// Mirrors `SystemState::revision`: publication/wakeup sequence, not query freshness.
    pub revision: u64,
    pub config: Arc<Config>,
    pub mpris_status: String,
    pub ffmpeg_status: String,
    pub shutting_down: bool,
    pub relocation_task: Option<crate::model::RelocationTaskStatus>,
    pub processing: Option<crate::model::ProcessingTaskStatus>,
    pub processing_audition: Option<ProcessingAudition>,
}
