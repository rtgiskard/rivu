use rivu_protocol::model::{GraphExecutionMode, GraphPort};

use super::{
    Arc, ConfigurationState, LibrarySnapshot, PlaybackState, Query, QueryRevisions, QueueState,
    StateRevisions, StateSections, SystemState, Track,
};

#[derive(Clone, Debug, Default)]
pub(crate) struct CoreState {
    pub(crate) library: LibrarySnapshot,
    pub(crate) queue: QueueState,
    pub(crate) current_track: Option<Arc<Track>>,
    pub(crate) playback: PlaybackState,
    /// Audio source ownership, independent of metadata and transport state.
    pub(crate) source_generation: u64,
    pub(crate) processing_audition: Option<ProcessingAudition>,
    pub(crate) system: SystemState,
    pub(crate) configuration: ConfigurationState,
    pub(crate) scan_owner: Option<MediaTaskOwner>,
    pub(crate) revisions: StateRevisions,
}

impl CoreState {
    pub(crate) fn current_track(&self) -> Option<&Track> {
        self.current_track.as_deref()
    }
    pub(crate) fn query_revisions(&self) -> QueryRevisions {
        self.library.query_revisions(self.queue.revision, None)
    }

    #[cfg(test)]
    pub(crate) fn playlist_query_revisions(&self, playlist_id: i64) -> QueryRevisions {
        self.library
            .query_revisions(self.queue.revision, Some(playlist_id))
    }

    pub(crate) fn query_revisions_for(&self, query: &Query) -> QueryRevisions {
        self.library.query_revisions_for(query, self.queue.revision)
    }

    /// Queue state is constant-size; Arc identity avoids traversing history/config.
    /// Replacing an Arc conservatively invalidates its section even if values match.
    pub(super) fn changed_sections(&self, previous: &Self) -> StateSections {
        let current_track_changed = match (&self.current_track, &previous.current_track) {
            (Some(current), Some(previous)) => !Arc::ptr_eq(current, previous),
            (None, None) => false,
            _ => true,
        };
        let system = &self.system;
        let old = &previous.system;
        StateSections {
            playback: self.playback != previous.playback
                || self.source_generation != previous.source_generation
                || current_track_changed
                || self.processing_audition != previous.processing_audition,
            queue: self.queue != previous.queue,
            library: self.library.revision != previous.library.revision
                || self.library.structure_revision != previous.library.structure_revision
                || self.library.playlist_revision != previous.library.playlist_revision
                || self.library.album_revision != previous.library.album_revision
                || self.library.bitrate_revision != previous.library.bitrate_revision
                || self.library.duplicate_groups_revision
                    != previous.library.duplicate_groups_revision
                || self.library.duplicate_members_revision
                    != previous.library.duplicate_members_revision
                || !Arc::ptr_eq(
                    &self.library.playlist_entry_revisions,
                    &previous.library.playlist_entry_revisions,
                )
                || self.library.track_total != previous.library.track_total
                || self.library.playlist_total != previous.library.playlist_total
                || !Arc::ptr_eq(&self.library.history, &previous.library.history),
            system: system.scanning != old.scanning
                || system.scan_message != old.scan_message
                || system.scan_progress != old.scan_progress
                || system.scan_task_id != old.scan_task_id
                || self.scan_owner != previous.scan_owner
                || system.relocation_task != old.relocation_task
                || system.processing != old.processing
                || system.last_error != old.last_error
                || !Arc::ptr_eq(&system.devices, &old.devices)
                || system.selected_device != old.selected_device
                || system.mpris_status != old.mpris_status
                || system.ffmpeg_status != old.ffmpeg_status
                || system.database_optimization != old.database_optimization
                || system.shutting_down != old.shutting_down,
            configuration: !Arc::ptr_eq(&self.configuration.config, &previous.configuration.config)
                || self.configuration.config_path != previous.configuration.config_path,
        }
    }
}
/// Selected audio source admitted by Core, independent of the latest task and editor.
/// Transport status remains separate: a selected source can be paused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessingAudition {
    pub id: u64,
    pub output: Option<GraphPort>,
    /// Cached graph output is Offline; a prepared live runner is Realtime or Hybrid.
    /// Standalone denoise/separation audition has no graph mode.
    pub mode: Option<GraphExecutionMode>,
}

/// Owner of the shared scan/import/admission task and its retained outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaTaskOwner {
    Library,
    Playlist,
    Queue,
}
