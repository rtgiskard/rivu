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
    pub(crate) status: PlaybackStatus,
    pub(crate) shuffle: bool,
    pub(crate) repeat: RepeatMode,
    pub(crate) navigation: rivu_presentation::navigation::Navigation,
    pub(crate) shutting_down: bool,
}

#[cfg(feature = "gui")]
impl TraySnapshot {
    pub(crate) fn from_core(state: &crate::core::CoreState) -> Self {
        Self {
            current_track: state.current_track.clone(),
            status: state.playback.status,
            shuffle: state.playback.shuffle,
            repeat: state.playback.repeat,
            navigation: rivu_presentation::navigation::Navigation::availability(
                &state.queue,
                &state.playback,
            ),
            shutting_down: state.system.shutting_down,
        }
    }

    pub(crate) fn current_track(&self) -> Option<&Track> {
        self.current_track.as_deref()
    }
}
