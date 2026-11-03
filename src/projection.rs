use crate::core::CoreState;
#[cfg(any(test, feature = "gui", feature = "test-support"))]
use rivu_protocol::display::{DisplayLibraryState, DisplayState, DisplaySystemState};
use rivu_protocol::model::{PlaybackState, QueueState, Track};
#[cfg(feature = "gui")]
use rivu_protocol::model::{PlaybackStatus, RepeatMode};
use std::sync::Arc;

#[cfg(any(test, feature = "gui", feature = "test-support"))]
pub(crate) fn display_state(state: &CoreState) -> DisplayState {
    DisplayState {
        revisions: state.revisions,
        library: DisplayLibraryState {
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
        system: DisplaySystemState {
            scanning: state.system.scanning,
            scan_message: state.system.scan_message.clone(),
            scan_progress: state.system.scan_progress.clone(),
            scan_task_id: state.system.scan_task_id,
            scan_owner: state.scan_owner,
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

#[derive(Clone, Debug)]
pub(crate) struct MprisSnapshot {
    pub current_track: Option<Arc<Track>>,
    pub queue: QueueState,
    pub playback: PlaybackState,
    pub shutting_down: bool,
}

impl MprisSnapshot {
    pub(crate) fn from_core(state: &CoreState) -> Self {
        Self {
            current_track: state.current_track.clone(),
            queue: state.queue.clone(),
            playback: state.playback.clone(),
            shutting_down: state.system.shutting_down,
        }
    }

    pub fn current_track(&self) -> Option<&Track> {
        let track = self.current_track.as_deref()?;
        (self.queue.current_id.is_some() && self.queue.current_position.is_some()).then_some(track)
    }
}

#[cfg(feature = "gui")]
#[derive(Clone, Debug)]
pub(crate) struct TraySnapshot {
    pub(crate) current_track: Option<Arc<Track>>,
    pub(crate) current_track_id: Option<i64>,
    pub(crate) status: PlaybackStatus,
    pub(crate) shuffle: bool,
    pub(crate) repeat: RepeatMode,
    pub(crate) shutting_down: bool,
}

#[cfg(feature = "gui")]
impl TraySnapshot {
    pub(crate) fn from_core(state: &CoreState) -> Self {
        Self {
            current_track: state.current_track.clone(),
            current_track_id: state.current_track.as_ref().map(|track| track.id),
            status: state.playback.status,
            shuffle: state.playback.shuffle,
            repeat: state.playback.repeat,
            shutting_down: state.system.shutting_down,
        }
    }

    pub(crate) fn current_track(&self) -> Option<&Track> {
        self.current_track.as_deref()
    }

    pub(crate) fn changed_from(&self, previous: &Self) -> bool {
        let track_changed = match (&self.current_track, &previous.current_track) {
            (None, None) => false,
            (Some(current), Some(previous)) => !Arc::ptr_eq(current, previous),
            _ => true,
        };
        self.status != previous.status
            || self.current_track_id != previous.current_track_id
            || self.shuffle != previous.shuffle
            || self.repeat != previous.repeat
            || self.shutting_down != previous.shutting_down
            || track_changed
    }
    pub(crate) fn menu_changed_from(&self, previous: &Self) -> bool {
        self.status != previous.status
            || self.shuffle != previous.shuffle
            || self.repeat != previous.repeat
    }
}

#[cfg(all(test, feature = "gui"))]
mod tests {
    use super::*;

    #[test]
    fn tray_projection_ignores_playback_position_but_tracks_status_changes() {
        let mut core = CoreState::default();
        let previous = TraySnapshot::from_core(&core);
        core.playback.position = 12.0;
        let position_only = TraySnapshot::from_core(&core);
        assert!(!position_only.changed_from(&previous));

        core.playback.status = PlaybackStatus::Playing;
        let playing = TraySnapshot::from_core(&core);
        assert!(playing.changed_from(&position_only));
    }
}
