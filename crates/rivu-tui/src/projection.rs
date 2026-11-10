use rivu_format::config::{Config, DEFAULT_QUERY_BATCH_SIZE, Theme};
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

#[derive(Clone, Debug)]
pub struct TuiSystemSnapshot {
    pub scanning: bool,
    pub scan_progress: Option<rivu_protocol::model::ScanProgress>,
    pub scan_task_id: Option<u64>,
    pub scan_message: String,
    pub last_error: Option<String>,
    pub library_roots: Arc<Vec<std::path::PathBuf>>,
    pub query_batch_size: u32,
    pub ui_theme: Theme,
}

impl Default for TuiSystemSnapshot {
    fn default() -> Self {
        Self {
            scanning: false,
            scan_progress: None,
            scan_task_id: None,
            scan_message: String::new(),
            last_error: None,
            library_roots: Arc::default(),
            query_batch_size: DEFAULT_QUERY_BATCH_SIZE,
            ui_theme: Theme::default(),
        }
    }
}

impl TuiSnapshot {
    pub(crate) fn from_client(state: ClientSnapshot, previous: Option<&Self>) -> Self {
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
                library_roots: previous
                    .filter(|previous| {
                        Arc::ptr_eq(&previous.configuration, &state.configuration.config)
                            || previous.system.library_roots.as_ref()
                                == &state.configuration.config.library_roots
                    })
                    .map(|previous| Arc::clone(&previous.system.library_roots))
                    .unwrap_or_else(|| Arc::new(state.configuration.config.library_roots.clone())),
                query_batch_size: state.configuration.config.query_batch_size,
                ui_theme: state.configuration.config.ui_theme,
            },
            configuration: state.configuration.config,
        }
    }

    /// Publication sequencing and non-terminal configuration do not change the screen.
    pub(crate) fn same_presentation(&self, previous: &Self) -> bool {
        self.playback == previous.playback
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
            && self.system.library_roots == previous.system.library_roots
            && self.system.query_batch_size == previous.system.query_batch_size
            && self.system.ui_theme == previous.system.ui_theme
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
        let initial = TuiSnapshot::from_client(client.clone(), None);
        client.system.revision += 1;
        client.system.selected_device = Some("another device".into());
        Arc::make_mut(&mut client.configuration.config).volume = 0.25;
        client.library.duplicate_groups_revision += 1;
        client.library.duplicate_members_revision += 1;
        let ignored = TuiSnapshot::from_client(client.clone(), Some(&initial));
        assert!(ignored.same_presentation(&initial));

        client.system.scan_message = "Scanning library".into();
        let scan = TuiSnapshot::from_client(client.clone(), Some(&ignored));
        assert!(!scan.same_presentation(&ignored));
        client.system.last_error = Some("Scan failed".into());
        let error = TuiSnapshot::from_client(client.clone(), Some(&scan));
        assert!(!error.same_presentation(&scan));
        client.playback.position = 3.0;
        let playback = TuiSnapshot::from_client(client.clone(), Some(&error));
        assert!(!playback.same_presentation(&error));
        Arc::make_mut(&mut client.configuration.config).query_batch_size += 1;
        let page_size = TuiSnapshot::from_client(client, Some(&playback));
        assert!(!page_size.same_presentation(&playback));
    }

    #[test]
    fn projection_reuses_library_roots_until_the_configuration_changes_them() {
        let mut client = ClientSnapshot::default();
        Arc::make_mut(&mut client.configuration.config).library_roots = vec!["/music".into()];
        let initial = TuiSnapshot::from_client(client.clone(), None);
        let unchanged = TuiSnapshot::from_client(client.clone(), Some(&initial));
        assert!(Arc::ptr_eq(
            &initial.system.library_roots,
            &unchanged.system.library_roots
        ));

        Arc::make_mut(&mut client.configuration.config).query_batch_size = 17;
        let other_setting = TuiSnapshot::from_client(client.clone(), Some(&unchanged));
        assert!(Arc::ptr_eq(
            &unchanged.system.library_roots,
            &other_setting.system.library_roots
        ));

        Arc::make_mut(&mut client.configuration.config)
            .library_roots
            .push("/other".into());
        let changed_roots = TuiSnapshot::from_client(client, Some(&other_setting));
        assert_eq!(changed_roots.system.library_roots.len(), 2);
        assert!(!Arc::ptr_eq(
            &other_setting.system.library_roots,
            &changed_roots.system.library_roots
        ));
        assert_eq!(initial.system.library_roots.len(), 1);
    }
}
