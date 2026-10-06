use crate::{
    config::Config,
    core::CoreState,
    model::{DatabaseOptimization, HistoryEntry, PlaybackState, PlaybackStatus, QueueState, Track},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ClientSnapshot {
    pub library: crate::model::LibrarySnapshot,
    pub queue: QueueState,
    pub current_track: Option<Arc<Track>>,
    pub playback: PlaybackState,
    pub system: crate::model::SystemState,
}

impl ClientSnapshot {
    pub(crate) fn from_core(state: &CoreState) -> Self {
        Self {
            library: state.library.clone(),
            queue: state.queue.clone(),
            current_track: state.current_track.clone(),
            playback: state.playback.clone(),
            system: state.system.clone(),
        }
    }

    pub fn current_track(&self) -> Option<&Track> {
        self.current_track.as_deref()
    }
}

#[derive(Clone, Debug, Default)]
pub struct GuiSnapshot {
    pub library: GuiLibrarySnapshot,
    pub queue: QueueState,
    pub current_track: Option<Arc<Track>>,
    pub playback: PlaybackState,
    pub system: GuiSystemSnapshot,
}

#[derive(Clone, Debug, Default)]
pub struct GuiLibrarySnapshot {
    pub track_total: usize,
    pub playlist_total: usize,
    pub revision: u64,
    pub structure_revision: u64,
    pub playlist_revision: u64,
    pub history: Arc<Vec<HistoryEntry>>,
}

#[derive(Clone, Debug, Default)]
pub struct GuiSystemSnapshot {
    pub scanning: bool,
    pub scan_message: String,
    pub scan_progress: Option<crate::model::ScanProgress>,
    pub last_error: Option<String>,
    pub devices: Arc<Vec<String>>,
    pub selected_device: Option<String>,
    pub revision: u64,
    pub config: Arc<Config>,
    pub mpris_status: String,
    pub ffmpeg_status: String,
    pub database_optimization: Option<DatabaseOptimization>,
    pub shutting_down: bool,
}

impl GuiSnapshot {
    pub(crate) fn from_core(state: &CoreState) -> Self {
        Self {
            library: GuiLibrarySnapshot {
                track_total: state.library.track_total,
                playlist_total: state.library.playlist_total,
                revision: state.library.revision,
                structure_revision: state.library.structure_revision,
                playlist_revision: state.library.playlist_revision,
                history: Arc::clone(&state.library.history),
            },
            queue: state.queue.clone(),
            current_track: state.current_track.clone(),
            playback: state.playback.clone(),
            system: GuiSystemSnapshot {
                scanning: state.system.scanning,
                scan_message: state.system.scan_message.clone(),
                scan_progress: state.system.scan_progress.clone(),
                last_error: state.system.last_error.clone(),
                devices: Arc::clone(&state.system.devices),
                selected_device: state.system.selected_device.clone(),
                revision: state.system.revision,
                config: Arc::clone(&state.system.config),
                mpris_status: state.system.mpris_status.clone(),
                ffmpeg_status: state.system.ffmpeg_status.clone(),
                database_optimization: state.system.database_optimization.clone(),
                shutting_down: state.system.shutting_down,
            },
        }
    }

    pub fn current_track(&self) -> Option<&Track> {
        self.current_track.as_deref()
    }
}

#[derive(Clone, Debug, Default)]
pub struct TuiSnapshot {
    pub library: TuiLibrarySnapshot,
    pub queue: QueueState,
    pub current_track: Option<Arc<Track>>,
    pub playback: PlaybackState,
    pub system: TuiSystemSnapshot,
}

#[derive(Clone, Debug, Default)]
pub struct TuiLibrarySnapshot {
    pub track_total: usize,
    pub playlist_total: usize,
    pub revision: u64,
    pub structure_revision: u64,
    pub playlist_revision: u64,
    pub history: Arc<Vec<HistoryEntry>>,
}

#[derive(Clone, Debug)]
pub struct TuiSystemSnapshot {
    pub scan_message: String,
    pub last_error: Option<String>,
    pub revision: u64,
    pub shutting_down: bool,
    pub nerd_symbols: bool,
    pub library_roots: Arc<Vec<std::path::PathBuf>>,
    pub page_size: u32,
}

impl Default for TuiSystemSnapshot {
    fn default() -> Self {
        Self {
            scan_message: String::new(),
            last_error: None,
            revision: 0,
            shutting_down: false,
            nerd_symbols: false,
            library_roots: Arc::default(),
            page_size: crate::config::DEFAULT_PAGE_SIZE,
        }
    }
}

impl TuiSnapshot {
    pub(crate) fn from_client(state: &ClientSnapshot) -> Self {
        Self {
            library: TuiLibrarySnapshot {
                track_total: state.library.track_total,
                playlist_total: state.library.playlist_total,
                revision: state.library.revision,
                structure_revision: state.library.structure_revision,
                playlist_revision: state.library.playlist_revision,
                history: Arc::clone(&state.library.history),
            },
            queue: state.queue.clone(),
            current_track: state.current_track.clone(),
            playback: state.playback.clone(),
            system: TuiSystemSnapshot {
                scan_message: state.system.scan_message.clone(),
                last_error: state.system.last_error.clone(),
                revision: state.system.revision,
                shutting_down: state.system.shutting_down,
                nerd_symbols: state.system.config.nerd_symbols,
                library_roots: Arc::new(state.system.config.library_roots.clone()),
                page_size: state.system.config.page_size,
            },
        }
    }
}

impl TuiSnapshot {
    pub fn current_track(&self) -> Option<&Track> {
        self.current_track.as_deref()
    }
}

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
        self.queue
            .entries
            .iter()
            .any(|entry| Some(entry.id) == self.queue.current_id && entry.track_id == track.id)
            .then_some(track)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TraySnapshot {
    pub(crate) current_track: Option<Arc<Track>>,
    pub(crate) current_track_id: Option<i64>,
    pub(crate) status: PlaybackStatus,
    pub(crate) shutting_down: bool,
}

impl TraySnapshot {
    pub(crate) fn from_core(state: &CoreState) -> Self {
        Self {
            current_track: state.current_track.clone(),
            current_track_id: state.current_track.as_ref().map(|track| track.id),
            status: state.playback.status,
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
            || self.shutting_down != previous.shutting_down
            || track_changed
    }
    pub(crate) fn menu_changed_from(&self, previous: &Self) -> bool {
        self.status != previous.status
    }
}

#[cfg(test)]
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
