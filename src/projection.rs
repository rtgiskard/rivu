use crate::{
    config::Config,
    core::CoreState,
    model::{
        DatabaseOptimization, HistoryEntry, PlaybackState, PlaybackStatus, Playlist, QueueState,
        Track,
    },
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ClientSnapshot {
    pub library: crate::model::LibrarySnapshot,
    pub queue: QueueState,
    pub playback: PlaybackState,
    pub system: crate::model::SystemState,
}

impl ClientSnapshot {
    pub(crate) fn from_core(state: &CoreState) -> Self {
        Self {
            library: state.library.clone(),
            queue: state.queue.clone(),
            playback: state.playback.clone(),
            system: state.system.clone(),
        }
    }

    pub fn current_track(&self) -> Option<&Track> {
        let queue_id = self.queue.current_id?;
        let track_id = self
            .queue
            .entries
            .iter()
            .find(|entry| entry.id == queue_id)?
            .track_id;
        let index = self
            .library
            .tracks
            .binary_search_by_key(&track_id, |track| track.id)
            .ok()?;
        self.library.tracks.get(index)
    }
}

#[derive(Clone, Debug, Default)]
pub struct GuiSnapshot {
    pub library: GuiLibrarySnapshot,
    pub queue: QueueState,
    pub playback: PlaybackState,
    pub system: GuiSystemSnapshot,
}

#[derive(Clone, Debug, Default)]
pub struct GuiLibrarySnapshot {
    pub revision: u64,
    pub structure_revision: u64,
    pub tracks: Arc<Vec<Track>>,
    pub playlists: Arc<Vec<Playlist>>,
    pub history: Arc<Vec<HistoryEntry>>,
}

#[derive(Clone, Debug, Default)]
pub struct GuiSystemSnapshot {
    pub scanning: bool,
    pub scan_message: String,
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
                revision: state.library.revision,
                structure_revision: state.library.structure_revision,
                tracks: Arc::clone(&state.library.tracks),
                playlists: Arc::clone(&state.library.playlists),
                history: Arc::clone(&state.library.history),
            },
            queue: state.queue.clone(),
            playback: state.playback.clone(),
            system: GuiSystemSnapshot {
                scanning: state.system.scanning,
                scan_message: state.system.scan_message.clone(),
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
        let queue_id = self.queue.current_id?;
        let track_id = self
            .queue
            .entries
            .iter()
            .find(|entry| entry.id == queue_id)?
            .track_id;
        let index = self
            .library
            .tracks
            .binary_search_by_key(&track_id, |track| track.id)
            .ok()?;
        self.library.tracks.get(index)
    }
}

#[derive(Clone, Debug, Default)]
pub struct TuiSnapshot {
    pub library: TuiLibrarySnapshot,
    pub queue: QueueState,
    pub playback: PlaybackState,
    pub system: TuiSystemSnapshot,
}

#[derive(Clone, Debug, Default)]
pub struct TuiLibrarySnapshot {
    pub revision: u64,
    pub tracks: Arc<Vec<Track>>,
    pub playlists: Arc<Vec<Playlist>>,
}

#[derive(Clone, Debug, Default)]
pub struct TuiSystemSnapshot {
    pub scan_message: String,
    pub last_error: Option<String>,
    pub revision: u64,
    pub shutting_down: bool,
    pub nerd_symbols: bool,
    pub library_roots: Arc<Vec<std::path::PathBuf>>,
}

impl TuiSnapshot {
    pub(crate) fn from_client(state: &ClientSnapshot) -> Self {
        Self {
            library: TuiLibrarySnapshot {
                revision: state.library.revision,
                tracks: Arc::clone(&state.library.tracks),
                playlists: Arc::clone(&state.library.playlists),
            },
            queue: state.queue.clone(),
            playback: state.playback.clone(),
            system: TuiSystemSnapshot {
                scan_message: state.system.scan_message.clone(),
                last_error: state.system.last_error.clone(),
                revision: state.system.revision,
                shutting_down: state.system.shutting_down,
                nerd_symbols: state.system.config.nerd_symbols,
                library_roots: Arc::new(state.system.config.library_roots.clone()),
            },
        }
    }
}

impl TuiSnapshot {
    pub fn current_track(&self) -> Option<&Track> {
        let queue_id = self.queue.current_id?;
        let track_id = self
            .queue
            .entries
            .iter()
            .find(|entry| entry.id == queue_id)?
            .track_id;
        let index = self
            .library
            .tracks
            .binary_search_by_key(&track_id, |track| track.id)
            .ok()?;
        self.library.tracks.get(index)
    }
}

#[derive(Clone, Debug)]
pub struct MprisSnapshot {
    pub tracks: Arc<Vec<Track>>,
    pub queue: QueueState,
    pub playback: PlaybackState,
    pub shutting_down: bool,
}

impl MprisSnapshot {
    pub(crate) fn from_core(state: &CoreState) -> Self {
        Self {
            tracks: Arc::clone(&state.library.tracks),
            queue: state.queue.clone(),
            playback: state.playback.clone(),
            shutting_down: state.system.shutting_down,
        }
    }

    pub fn current_track(&self) -> Option<&Track> {
        let queue_id = self.queue.current_id?;
        let track_id = self
            .queue
            .entries
            .iter()
            .find(|entry| entry.id == queue_id)?
            .track_id;
        let index = self
            .tracks
            .binary_search_by_key(&track_id, |track| track.id)
            .ok()?;
        self.tracks.get(index)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TraySnapshot {
    pub(crate) tracks: Arc<Vec<Track>>,
    pub(crate) current_track_id: Option<i64>,
    pub(crate) status: PlaybackStatus,
    pub(crate) shutting_down: bool,
}

impl TraySnapshot {
    pub(crate) fn from_core(state: &CoreState) -> Self {
        let current_track_id = state.queue.current_id.and_then(|queue_id| {
            state
                .queue
                .entries
                .iter()
                .find(|entry| entry.id == queue_id)
                .map(|entry| entry.track_id)
        });
        Self {
            tracks: Arc::clone(&state.library.tracks),
            current_track_id,
            status: state.playback.status,
            shutting_down: state.system.shutting_down,
        }
    }

    pub(crate) fn current_track(&self) -> Option<&Track> {
        let track_id = self.current_track_id?;
        self.tracks.iter().find(|track| track.id == track_id)
    }

    pub(crate) fn changed_from(&self, previous: &Self) -> bool {
        self.status != previous.status
            || self.current_track_id != previous.current_track_id
            || self.shutting_down != previous.shutting_down
            || !Arc::ptr_eq(&self.tracks, &previous.tracks)
    }
    pub(crate) fn menu_changed_from(&self, previous: &Self) -> bool {
        self.status != previous.status
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tui_projection_keeps_library_storage_and_selects_only_tui_fields() {
        let tracks = Arc::new(Vec::new());
        let mut client = ClientSnapshot::default();
        client.library.tracks = Arc::clone(&tracks);
        client.system.revision = 17;
        client.system.config = Arc::new(Config {
            nerd_symbols: true,
            ..Config::default()
        });

        let tui = TuiSnapshot::from_client(&client);

        assert!(Arc::ptr_eq(&tui.library.tracks, &tracks));
        assert_eq!(tui.system.revision, 17);
        assert!(tui.system.nerd_symbols);
    }

    #[test]
    fn gui_projection_keeps_library_revisions_and_shared_arcs() {
        let mut core = CoreState::default();
        core.library.revision = 3;
        core.library.structure_revision = 5;
        let tracks = Arc::clone(&core.library.tracks);

        let gui = GuiSnapshot::from_core(&core);

        assert_eq!(gui.library.revision, 3);
        assert_eq!(gui.library.structure_revision, 5);
        assert!(Arc::ptr_eq(&gui.library.tracks, &tracks));
    }

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

    #[test]
    fn tray_menu_changes_only_when_playback_label_changes() {
        let mut core = CoreState::default();
        let previous = TraySnapshot::from_core(&core);

        core.library.tracks = Arc::new(Vec::new());
        let metadata_changed = TraySnapshot::from_core(&core);
        assert!(metadata_changed.changed_from(&previous));
        assert!(!metadata_changed.menu_changed_from(&previous));

        core.playback.status = PlaybackStatus::Playing;
        let playing = TraySnapshot::from_core(&core);
        assert!(playing.menu_changed_from(&metadata_changed));
    }
}
