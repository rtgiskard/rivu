use crate::{
    config::Config,
    core::CoreState,
    model::{
        ClientSnapshot, DatabaseOptimization, HistoryEntry, PlaybackState, PlaybackStatus,
        QueueState, RepeatMode, Track,
    },
};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone, Debug, Default)]
pub struct GuiSnapshot {
    pub library: GuiLibrarySnapshot,
    pub queue: QueueState,
    pub current_track: Option<Arc<Track>>,
    pub playback: PlaybackState,
    pub system: GuiSystemSnapshot,
    pub revisions: crate::response::StateRevisions,
}

#[derive(Clone, Debug, Default)]
pub struct GuiLibrarySnapshot {
    pub track_total: usize,
    pub playlist_total: usize,
    pub revision: u64,
    pub structure_revision: u64,
    pub playlist_revision: u64,
    pub playlist_entry_revisions: Arc<BTreeMap<i64, u64>>,
    pub duplicate_groups_revision: u64,
    pub duplicate_members_revision: u64,
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
    pub relocation_task: Option<crate::model::RelocationTaskStatus>,
}

impl GuiSnapshot {
    pub(crate) fn from_core(state: &CoreState) -> Self {
        Self {
            revisions: state.revisions,
            library: GuiLibrarySnapshot {
                track_total: state.library.track_total,
                playlist_total: state.library.playlist_total,
                revision: state.library.revision,
                structure_revision: state.library.structure_revision,
                playlist_revision: state.library.playlist_revision,
                playlist_entry_revisions: Arc::clone(&state.library.playlist_entry_revisions),
                duplicate_groups_revision: state.library.duplicate_groups_revision,
                duplicate_members_revision: state.library.duplicate_members_revision,
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
                relocation_task: state.system.relocation_task.clone(),
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
    pub revision: u64,
    pub structure_revision: u64,
    pub playlist_revision: u64,
    pub playlist_entry_revisions: Arc<BTreeMap<i64, u64>>,
    pub duplicate_groups_revision: u64,
    pub duplicate_members_revision: u64,
}

#[derive(Clone, Debug)]
pub struct TuiSystemSnapshot {
    pub scan_message: String,
    pub last_error: Option<String>,
    pub revision: u64,
    pub shutting_down: bool,
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
            library_roots: Arc::default(),
            page_size: crate::config::DEFAULT_PAGE_SIZE,
        }
    }
}

impl TuiSnapshot {
    pub(crate) fn from_client(state: &ClientSnapshot, previous: Option<&Self>) -> Self {
        Self {
            library: TuiLibrarySnapshot {
                revision: state.library.revision,
                structure_revision: state.library.structure_revision,
                playlist_revision: state.library.playlist_revision,
                playlist_entry_revisions: Arc::clone(&state.library.playlist_entry_revisions),
                duplicate_groups_revision: state.library.duplicate_groups_revision,
                duplicate_members_revision: state.library.duplicate_members_revision,
            },
            queue: state.queue.clone(),
            current_track: state.current_track.clone(),
            playback: state.playback.clone(),
            system: TuiSystemSnapshot {
                scan_message: state.system.scan_message.clone(),
                last_error: state.system.last_error.clone(),
                revision: state.system.revision,
                shutting_down: state.system.shutting_down,
                library_roots: previous
                    .filter(|previous| {
                        previous.system.library_roots.as_ref() == &state.system.config.library_roots
                    })
                    .map(|previous| Arc::clone(&previous.system.library_roots))
                    .unwrap_or_else(|| Arc::new(state.system.config.library_roots.clone())),
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

/// Owns projection identity and freshness, never payload or business operations.
/// Page demand and the physical in-flight worker gate stay with the consumer.
#[derive(Debug, Default)]
pub(crate) struct QueryProjection {
    identity: Option<crate::model::Query>,
    stamp: Option<crate::model::QueryStamp>,
    generation: u64,
}

impl QueryProjection {
    pub(crate) fn sync(
        &mut self,
        query: &crate::model::Query,
        revisions: crate::response::QueryRevisions,
    ) -> bool {
        let stamp = query.stamp(revisions);
        let identity_changed = !self
            .identity
            .as_ref()
            .is_some_and(|current| current.identity() == query.identity());
        if !identity_changed && self.stamp == Some(stamp) {
            return false;
        }
        if identity_changed {
            self.identity = Some(query.clone());
        }
        self.stamp = Some(stamp);
        self.cancel();
        true
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn stamp(&self) -> Option<crate::model::QueryStamp> {
        self.stamp
    }

    /// Retiring submissions does not invalidate a still-current cached payload.
    pub(crate) fn cancel(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    pub(crate) fn accepts(
        &self,
        query: &crate::model::Query,
        generation: u64,
        revisions: crate::response::QueryRevisions,
    ) -> bool {
        self.generation == generation
            && self
                .identity
                .as_ref()
                .is_some_and(|current| current.identity() == query.identity())
            && self.stamp == Some(query.stamp(revisions))
    }
}

impl GuiLibrarySnapshot {
    pub(crate) fn query_revisions(
        &self,
        playlist_id: Option<i64>,
    ) -> crate::response::QueryRevisions {
        crate::response::QueryRevisions {
            library: self.revision,
            structure: self.structure_revision,
            playlist: self.playlist_revision,
            duplicate_groups: self.duplicate_groups_revision,
            duplicate_members: self.duplicate_members_revision,
            playlist_entries: playlist_id.and_then(|id| {
                self.playlist_entry_revisions.get(&id).map(|revision| {
                    crate::response::PlaylistEntryRevision {
                        playlist_id: id,
                        revision: *revision,
                    }
                })
            }),
            ..Default::default()
        }
    }
}

impl GuiSnapshot {
    pub(crate) fn query_revisions(
        &self,
        playlist_id: Option<i64>,
    ) -> crate::response::QueryRevisions {
        crate::response::QueryRevisions {
            library: self.library.revision,
            structure: self.library.structure_revision,
            playlist: self.library.playlist_revision,
            queue: self.queue.revision,
            duplicate_groups: self.library.duplicate_groups_revision,
            duplicate_members: self.library.duplicate_members_revision,
            playlist_entries: playlist_id.and_then(|id| {
                self.library
                    .playlist_entry_revisions
                    .get(&id)
                    .map(|revision| crate::response::PlaylistEntryRevision {
                        playlist_id: id,
                        revision: *revision,
                    })
            }),
        }
    }
}

impl TuiSnapshot {
    pub(crate) fn query_revisions(
        &self,
        playlist_id: Option<i64>,
    ) -> crate::response::QueryRevisions {
        crate::response::QueryRevisions {
            library: self.library.revision,
            structure: self.library.structure_revision,
            playlist: self.library.playlist_revision,
            queue: self.queue.revision,
            duplicate_groups: self.library.duplicate_groups_revision,
            duplicate_members: self.library.duplicate_members_revision,
            playlist_entries: playlist_id.and_then(|id| {
                self.library
                    .playlist_entry_revisions
                    .get(&id)
                    .map(|revision| crate::response::PlaylistEntryRevision {
                        playlist_id: id,
                        revision: *revision,
                    })
            }),
        }
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
        (self.queue.current_id.is_some() && self.queue.current_position.is_some()).then_some(track)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TraySnapshot {
    pub(crate) current_track: Option<Arc<Track>>,
    pub(crate) current_track_id: Option<i64>,
    pub(crate) status: PlaybackStatus,
    pub(crate) shuffle: bool,
    pub(crate) repeat: RepeatMode,
    pub(crate) shutting_down: bool,
}

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
