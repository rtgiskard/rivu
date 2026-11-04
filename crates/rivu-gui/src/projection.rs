use rivu_format::config::Config;
use rivu_protocol::model::{
    ClientSnapshot, HistoryEntry, MediaTaskOwner, PlaybackState, ProcessingAudition, QueueState,
    Track,
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
    /// GUI publication/wakeup sequence, independent of query-domain freshness.
    pub revision: u64,
    pub config: Arc<Config>,
    pub mpris_status: String,
    pub ffmpeg_status: String,
    pub shutting_down: bool,
    pub relocation_task: Option<rivu_protocol::model::RelocationTaskStatus>,
    pub processing: Option<Arc<rivu_protocol::model::ProcessingTaskStatus>>,
    pub processing_audition: Option<ProcessingAudition>,
}

impl GuiSystemSnapshot {
    pub(crate) fn scan_visible(&self, owner: MediaTaskOwner) -> bool {
        self.scan_owner == Some(owner) && (self.scanning || self.scan_progress.is_some())
    }
}

impl GuiSnapshot {
    pub(crate) fn from_client(
        state: &ClientSnapshot,
        revisions: rivu_protocol::response::StateRevisions,
    ) -> Self {
        Self {
            revisions,
            library: GuiLibrarySnapshot {
                track_total: state.library.track_total,
                playlist_total: state.library.playlist_total,
                revision: state.library.revision,
                structure_revision: state.library.structure_revision,
                album_revision: state.library.album_revision,
                playlist_revision: state.library.playlist_revision,
                playlist_entry_revisions: Arc::clone(&state.library.playlist_entry_revisions),
                metadata_revisions: Arc::clone(&state.library.metadata_revisions),
                bitrate_revision: state.library.bitrate_revision,
                duplicate_groups_revision: state.library.duplicate_groups_revision,
                duplicate_members_revision: state.library.duplicate_members_revision,
                history: Arc::clone(&state.library.history),
            },
            queue: state.queue.clone(),
            current_track: state.current_track.clone(),
            playback: state.playback.clone(),
            source_generation: state.source_generation,
            system: GuiSystemSnapshot {
                scanning: state.system.scanning,
                scan_message: state.system.scan_message.clone(),
                scan_progress: state.system.scan_progress.clone(),
                scan_task_id: state.system.scan_task_id,
                scan_owner: state.system.scan_owner,
                last_error: state.system.last_error.clone(),
                devices: Arc::clone(&state.system.devices),
                revision: state.system.revision,
                config: Arc::clone(&state.configuration.config),
                mpris_status: state.system.mpris_status.clone(),
                ffmpeg_status: state.system.ffmpeg_status.clone(),
                shutting_down: state.system.shutting_down,
                relocation_task: state.system.relocation_task.clone(),
                processing: state.system.processing.clone(),
                processing_audition: state.processing_audition,
            },
        }
    }
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
