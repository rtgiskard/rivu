use rivu_format::config::Config;
use rivu_protocol::model::{ClientSnapshot, LibrarySnapshot, PlaybackState, QueueState, Track};
use std::sync::Arc;

#[derive(Clone, Debug, Default)]
pub struct TuiSnapshot {
    pub library: LibrarySnapshot,
    pub queue: QueueState,
    pub current_track: Option<Arc<Track>>,
    pub playback: PlaybackState,
    pub system: TuiSystemSnapshot,
    pub(crate) configuration: Arc<Config>,
}

#[derive(Clone, Debug, Default)]
pub struct TuiSystemSnapshot {
    pub scanning: bool,
    pub scan_progress: Option<rivu_protocol::model::ScanProgress>,
    pub scan_task_id: Option<u64>,
    pub scan_message: String,
    pub last_error: Option<String>,
}

impl TuiSnapshot {
    pub(crate) fn from_client(state: ClientSnapshot) -> Self {
        Self {
            library: state.library,
            queue: state.queue,
            current_track: state.current_track,
            playback: state.playback,
            system: TuiSystemSnapshot {
                scanning: state.system.scanning,
                scan_progress: state.system.scan_progress,
                scan_task_id: state.system.scan_task_id,
                scan_message: state.system.scan_message,
                last_error: state.system.last_error,
            },
            configuration: state.configuration.config,
        }
    }

    pub(crate) fn same_library_roots(&self, previous: &Self) -> bool {
        Arc::ptr_eq(&self.configuration, &previous.configuration)
            || self.configuration.library_roots == previous.configuration.library_roots
    }

    /// Position is compared against the last rendered progress, not the last snapshot.
    pub(crate) fn same_presentation(&self, previous: &Self) -> bool {
        let (a, b) = (&self.playback, &previous.playback);
        a.status == b.status
            && a.duration == b.duration
            && a.last_heard_at == b.last_heard_at
            && a.volume == b.volume
            && a.shuffle == b.shuffle
            && a.repeat == b.repeat
            && a.seek_revision == b.seek_revision
            && a.stopped_source_generation == b.stopped_source_generation
            && a.audio_output == b.audio_output
            && a.speed == b.speed
            && self.queue == previous.queue
            && self.current_track == previous.current_track
            && self.library.revision == previous.library.revision
            && self.library.structure_revision == previous.library.structure_revision
            && self.library.album_revision == previous.library.album_revision
            && self.library.playlist_revision == previous.library.playlist_revision
            && self.library.bitrate_revision == previous.library.bitrate_revision
            && self.library.playlist_entry_revisions == previous.library.playlist_entry_revisions
            && Arc::ptr_eq(
                &self.library.metadata_revisions,
                &previous.library.metadata_revisions,
            )
            && Arc::ptr_eq(&self.library.history, &previous.library.history)
            && self.system.scanning == previous.system.scanning
            && self.system.scan_progress == previous.system.scan_progress
            && self.system.scan_task_id == previous.system.scan_task_id
            && self.system.scan_message == previous.system.scan_message
            && self.system.last_error == previous.system.last_error
            && self.same_library_roots(previous)
            && self.configuration.query_batch_size == previous.configuration.query_batch_size
            && self.configuration.ui_theme == previous.configuration.ui_theme
            && self.configuration.confirm_queue_replace
                == previous.configuration.confirm_queue_replace
    }
}

impl TuiSnapshot {
    pub fn current_track(&self) -> Option<&Track> {
        self.current_track.as_deref()
    }
}

impl TuiSnapshot {
    pub(crate) fn query_revisions(
        &self,
        playlist_id: Option<i64>,
    ) -> rivu_protocol::response::QueryRevisions {
        self.library
            .query_revisions(self.queue.revision, playlist_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redraw_ignores_publication_sequence_and_unprojected_state() {
        let mut client = ClientSnapshot::default();
        let initial = TuiSnapshot::from_client(client.clone());
        client.system.revision += 1;
        client.system.selected_device = Some("another device".into());
        Arc::make_mut(&mut client.configuration.config).volume = 0.25;
        client.library.duplicate_groups_revision += 1;
        client.library.duplicate_members_revision += 1;
        let ignored = TuiSnapshot::from_client(client.clone());
        assert!(ignored.same_presentation(&initial));

        client.system.scan_message = "Scanning library".into();
        let scan = TuiSnapshot::from_client(client.clone());
        assert!(!scan.same_presentation(&ignored));
        client.system.last_error = Some("Scan failed".into());
        let error = TuiSnapshot::from_client(client.clone());
        assert!(!error.same_presentation(&scan));
        client.playback.position = 3.0;
        let playback = TuiSnapshot::from_client(client.clone());
        assert!(playback.same_presentation(&error));
        Arc::make_mut(&mut client.configuration.config).query_batch_size += 1;
        let page_size = TuiSnapshot::from_client(client);
        assert!(!page_size.same_presentation(&playback));
    }

    #[test]
    fn redraw_tracks_root_changes_but_ignores_unrelated_config_changes() {
        let mut client = ClientSnapshot::default();
        Arc::make_mut(&mut client.configuration.config).library_roots = vec!["/music".into()];
        let initial = TuiSnapshot::from_client(client.clone());

        Arc::make_mut(&mut client.configuration.config).volume = 0.25;
        let other_setting = TuiSnapshot::from_client(client.clone());
        assert!(other_setting.same_presentation(&initial));

        Arc::make_mut(&mut client.configuration.config)
            .library_roots
            .push("/other".into());
        assert!(!TuiSnapshot::from_client(client).same_presentation(&other_setting));
    }
}
