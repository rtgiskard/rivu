use crate::core::CoreState;
use rivu_protocol::model::{PlaybackState, QueueState, Track};
#[cfg(feature = "gui")]
use rivu_protocol::model::{PlaybackStatus, RepeatMode};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct MprisSnapshot {
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
    pub(crate) fn from_core(state: &crate::core::CoreState) -> Self {
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
        let track = Arc::new(crate::test_support::artwork_track("track.wav".into()));
        let mut state = crate::core::CoreState {
            current_track: Some(Arc::clone(&track)),
            ..Default::default()
        };
        let previous = TraySnapshot::from_core(&state);
        state.playback.position = 12.0;
        let position_only = TraySnapshot::from_core(&state);
        assert!(!position_only.changed_from(&previous));
        state.playback.status = PlaybackStatus::Playing;
        let playing = TraySnapshot::from_core(&state);
        assert!(playing.changed_from(&position_only));
        let mut renamed = (*track).clone();
        renamed.title = "Updated title".into();
        state.current_track = Some(Arc::new(renamed));
        let metadata_changed = TraySnapshot::from_core(&state);
        assert!(metadata_changed.changed_from(&playing));
        assert_eq!(
            metadata_changed.current_track().unwrap().title,
            "Updated title"
        );
        state.current_track = None;
        assert!(TraySnapshot::from_core(&state).changed_from(&metadata_changed));
    }
}
