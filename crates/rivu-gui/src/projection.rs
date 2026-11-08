use rivu_format::config::Config;
use rivu_protocol::model::{
    ClientSnapshot, LibrarySnapshot, MediaTaskOwner, PlaybackState, ProcessingAudition, QueueState,
    Track,
};
use std::sync::Arc;
#[derive(Clone, Debug, Default)]
pub struct GuiSnapshot {
    pub library: LibrarySnapshot,
    pub queue: QueueState,
    pub current_track: Option<Arc<Track>>,
    pub playback: PlaybackState,
    pub(crate) source_generation: u64,
    pub system: GuiSystemSnapshot,
    pub revisions: rivu_protocol::response::StateRevisions,
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
    pub processing_selection: Option<ProcessingAudition>,
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
            library: state.library.clone(),
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
                processing_selection: state.processing_selection,
            },
        }
    }
    pub fn current_track(&self) -> Option<&Track> {
        self.current_track.as_deref()
    }
}

impl GuiSnapshot {
    pub(crate) fn query_revisions(
        &self,
        playlist_id: Option<i64>,
    ) -> rivu_protocol::response::QueryRevisions {
        self.library
            .query_revisions(self.queue.revision, playlist_id)
    }

    pub(crate) fn query_revisions_for(
        &self,
        query: &rivu_protocol::model::Query,
    ) -> rivu_protocol::response::QueryRevisions {
        self.library.query_revisions_for(query, self.queue.revision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;
    use rivu_protocol::model::{GraphExecutionMode, GraphPort};

    #[test]
    fn retained_selection_is_not_projected_as_actual_audio_during_bypass() {
        let selection = ProcessingAudition {
            id: 42,
            output: Some(GraphPort { node: 7, port: 1 }),
            mode: Some(GraphExecutionMode::Hybrid),
        };
        let mut state = ClientSnapshot {
            processing_selection: Some(selection),
            ..Default::default()
        };
        Arc::make_mut(&mut state.configuration.config).processing_bypassed = true;
        let snapshot = GuiSnapshot::from_client(&state, Default::default());
        assert_eq!(snapshot.system.processing_selection, Some(selection));
        assert_eq!(snapshot.system.processing_audition, None);
        assert!(snapshot.system.config.processing_bypassed);

        Arc::make_mut(&mut state.configuration.config).processing_bypassed = false;
        state.processing_audition = Some(selection);
        let snapshot = GuiSnapshot::from_client(&state, Default::default());
        assert_eq!(snapshot.system.processing_selection, Some(selection));
        assert_eq!(snapshot.system.processing_audition, Some(selection));
    }
}
