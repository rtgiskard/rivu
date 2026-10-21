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
    pub metadata_revisions: Arc<crate::model::MetadataRevisions>,
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
    pub processing: Option<crate::model::ProcessingTaskStatus>,
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
                metadata_revisions: Arc::clone(&state.library.metadata_revisions),
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
                processing: state.system.processing.clone(),
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
    pub metadata_revisions: Arc<crate::model::MetadataRevisions>,
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
    pub ui_theme: crate::theme::Theme,
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
            ui_theme: crate::theme::Theme::default(),
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
                metadata_revisions: Arc::clone(&state.library.metadata_revisions),
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
                ui_theme: state.system.config.ui_theme,
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
    generation: crate::core::QueryEpoch,
}

impl QueryProjection {
    pub(crate) fn sync(
        &mut self,
        query: &crate::model::Query,
        revisions: crate::response::QueryRevisions,
    ) -> bool {
        let stamp = query.stamp(revisions);
        let identity_changed = !self.matches_identity(query);
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
        self.generation.generation()
    }

    pub(crate) fn stamp(&self) -> Option<crate::model::QueryStamp> {
        self.stamp
    }

    pub(crate) fn matches_identity(&self, query: &crate::model::Query) -> bool {
        self.identity
            .as_ref()
            .is_some_and(|current| current.identity() == query.identity())
    }

    /// Retiring submissions does not invalidate a still-current cached payload.
    pub(crate) fn cancel(&mut self) {
        self.generation.cancel();
    }

    pub(crate) fn ticket(&mut self) -> crate::core::QueryTicket {
        self.generation.ticket()
    }

    pub(crate) fn accepts(
        &self,
        query: &crate::model::Query,
        generation: u64,
        revisions: crate::response::QueryRevisions,
    ) -> bool {
        self.generation.generation() == generation
            && self.matches_identity(query)
            && self.stamp == Some(query.stamp(revisions))
    }
}

impl GuiLibrarySnapshot {
    pub(crate) fn query_revisions(
        &self,
        playlist_id: Option<i64>,
    ) -> crate::response::QueryRevisions {
        crate::response::QueryRevisions::from_library(
            self.revision,
            self.structure_revision,
            self.playlist_revision,
            0,
            self.duplicate_groups_revision,
            self.duplicate_members_revision,
        )
        .with_playlist_scope(
            playlist_id,
            &self.playlist_entry_revisions,
            &self.metadata_revisions,
        )
    }
}

impl GuiSnapshot {
    pub(crate) fn query_revisions(
        &self,
        playlist_id: Option<i64>,
    ) -> crate::response::QueryRevisions {
        let mut revisions = self.library.query_revisions(playlist_id);
        revisions.queue = self.queue.revision;
        revisions
    }

    pub(crate) fn query_revisions_for(
        &self,
        query: &crate::model::Query,
    ) -> crate::response::QueryRevisions {
        self.query_revisions(None).for_query(
            query,
            &self.library.playlist_entry_revisions,
            &self.library.metadata_revisions,
        )
    }
}

impl TuiSnapshot {
    pub(crate) fn query_revisions(
        &self,
        playlist_id: Option<i64>,
    ) -> crate::response::QueryRevisions {
        crate::response::QueryRevisions::from_library(
            self.library.revision,
            self.library.structure_revision,
            self.library.playlist_revision,
            self.queue.revision,
            self.library.duplicate_groups_revision,
            self.library.duplicate_members_revision,
        )
        .with_playlist_scope(
            playlist_id,
            &self.library.playlist_entry_revisions,
            &self.library.metadata_revisions,
        )
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
