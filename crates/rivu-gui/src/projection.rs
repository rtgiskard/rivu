use rivu_protocol::model::Config;
use rivu_protocol::{
    display::{DisplayLibraryState, DisplayState, DisplaySystemState},
    model::*,
};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone, Debug, Default)]
pub struct GuiSnapshot {
    pub library: GuiLibrarySnapshot,
    pub queue: QueueState,
    pub current_track: Option<Arc<Track>>,
    pub playback: PlaybackState,
    pub(crate) source_generation: u64,
    pub system: GuiSystemSnapshot,
    pub revisions: rivu_protocol::response::StateRevisions,
}

#[derive(Clone, Debug, Default)]
pub struct GuiLibrarySnapshot {
    pub track_total: usize,
    pub playlist_total: usize,
    pub revision: u64,
    pub structure_revision: u64,
    pub album_revision: u64,
    pub playlist_revision: u64,
    pub playlist_entry_revisions: Arc<BTreeMap<i64, u64>>,
    pub metadata_revisions: Arc<rivu_protocol::model::MetadataRevisions>,
    pub bitrate_revision: u64,
    pub duplicate_groups_revision: u64,
    pub duplicate_members_revision: u64,
    pub history: Arc<Vec<HistoryEntry>>,
}

#[derive(Clone, Debug, Default)]
pub struct GuiSystemSnapshot {
    pub scanning: bool,
    pub scan_message: String,
    pub scan_progress: Option<rivu_protocol::model::ScanProgress>,
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
    pub relocation_task: Option<rivu_protocol::model::RelocationTaskStatus>,
    pub processing: Option<rivu_protocol::model::ProcessingTaskStatus>,
    pub processing_audition: Option<ProcessingAudition>,
}

impl GuiSystemSnapshot {
    pub(crate) fn scan_visible(&self, owner: MediaTaskOwner) -> bool {
        self.scan_owner == Some(owner) && (self.scanning || self.scan_progress.is_some())
    }
}

impl GuiSnapshot {
    pub fn current_track(&self) -> Option<&Track> {
        self.current_track.as_deref()
    }
}

impl GuiLibrarySnapshot {
    pub(crate) fn query_revisions(
        &self,
        playlist_id: Option<i64>,
    ) -> rivu_protocol::response::QueryRevisions {
        let mut revisions = rivu_protocol::response::QueryRevisions::from_library(
            self.revision,
            self.structure_revision,
            self.playlist_revision,
            0,
            self.duplicate_groups_revision,
            self.duplicate_members_revision,
        )
        .with_album_revision(self.album_revision)
        .with_playlist_scope(
            playlist_id,
            &self.playlist_entry_revisions,
            &self.metadata_revisions,
        );
        revisions.bitrate = self.bitrate_revision;
        revisions
    }
}

impl GuiSnapshot {
    pub(crate) fn query_revisions(
        &self,
        playlist_id: Option<i64>,
    ) -> rivu_protocol::response::QueryRevisions {
        let mut revisions = self.library.query_revisions(playlist_id);
        revisions.queue = self.queue.revision;
        revisions
    }

    pub(crate) fn query_revisions_for(
        &self,
        query: &rivu_protocol::model::Query,
    ) -> rivu_protocol::response::QueryRevisions {
        self.query_revisions(None).for_query(
            query,
            &self.library.playlist_entry_revisions,
            &self.library.metadata_revisions,
        )
    }
}

impl From<DisplayState> for GuiSnapshot {
    fn from(state: DisplayState) -> Self {
        Self {
            library: GuiLibrarySnapshot::from(state.library),
            queue: state.queue,
            current_track: state.current_track,
            playback: state.playback,
            source_generation: state.source_generation,
            system: GuiSystemSnapshot::from(state.system),
            revisions: state.revisions,
        }
    }
}

impl From<DisplayLibraryState> for GuiLibrarySnapshot {
    fn from(state: DisplayLibraryState) -> Self {
        Self {
            track_total: state.track_total,
            playlist_total: state.playlist_total,
            revision: state.revision,
            structure_revision: state.structure_revision,
            album_revision: state.album_revision,
            playlist_revision: state.playlist_revision,
            playlist_entry_revisions: state.playlist_entry_revisions,
            metadata_revisions: state.metadata_revisions,
            bitrate_revision: state.bitrate_revision,
            duplicate_groups_revision: state.duplicate_groups_revision,
            duplicate_members_revision: state.duplicate_members_revision,
            history: state.history,
        }
    }
}

impl From<DisplaySystemState> for GuiSystemSnapshot {
    fn from(state: DisplaySystemState) -> Self {
        Self {
            scanning: state.scanning,
            scan_message: state.scan_message,
            scan_progress: state.scan_progress,
            scan_task_id: state.scan_task_id,
            scan_owner: state.scan_owner,
            last_error: state.last_error,
            devices: state.devices,
            revision: state.revision,
            config: state.config,
            mpris_status: state.mpris_status,
            ffmpeg_status: state.ffmpeg_status,
            shutting_down: state.shutting_down,
            relocation_task: state.relocation_task,
            processing: state.processing,
            processing_audition: state.processing_audition,
        }
    }
}
