use super::*;

impl Core {
    pub(in crate::core) fn create_playlist(&mut self, name: String) -> Result<()> {
        self.store.create_playlist(&name)?;
        self.reload_playlists()
    }

    pub(in crate::core) fn create_playlist_with_tracks(
        &mut self,
        name: String,
        track_ids: Vec<i64>,
    ) -> Result<()> {
        self.store.create_playlist_with_tracks(&name, &track_ids)?;
        self.reload_playlists()
    }

    pub(in crate::core) fn rename_playlist(
        &mut self,
        playlist_id: i64,
        name: String,
    ) -> Result<()> {
        if self.store.rename_playlist(playlist_id, &name)? {
            self.reload_playlists()?;
        }
        Ok(())
    }

    pub(in crate::core) fn delete_playlist(&mut self, playlist_id: i64) -> Result<()> {
        self.store.delete_playlist(playlist_id)?;
        self.reload_playlists()
    }

    pub(in crate::core) fn add_playlist(
        &mut self,
        playlist_id: i64,
        track_ids: Vec<i64>,
    ) -> Result<()> {
        if self.store.add_playlist(playlist_id, &track_ids)? {
            self.reload_playlists()?;
        }
        Ok(())
    }

    pub(in crate::core) fn remove_playlist_entry(&mut self, entry_id: i64) -> Result<()> {
        self.store.remove_playlist_entry(entry_id)?;
        self.reload_playlists()
    }

    pub(in crate::core) fn move_playlist_entry(
        &mut self,
        entry_id: i64,
        index: usize,
    ) -> Result<()> {
        if let Some(playlist_id) = self.store.move_playlist_entry(entry_id, index)? {
            let revision = Arc::make_mut(&mut self.state.library.playlist_entry_revisions)
                .entry(playlist_id)
                .or_default();
            *revision = revision.wrapping_add(1);
        }
        Ok(())
    }

    pub(in crate::core) fn export_playlist(&self, playlist_id: i64, path: &Path) -> Result<()> {
        let mut after = None;
        let mut rows = Vec::new().into_iter();
        let mut done = false;
        let tracks = std::iter::from_fn(|| {
            loop {
                if let Some(track) = rows.next() {
                    return Some(Ok(track));
                }
                if done {
                    return None;
                }
                let batch = match self.store.playlist_track_batch(
                    playlist_id,
                    after,
                    crate::model::MAX_QUERY_ROWS,
                ) {
                    Ok(batch) => batch,
                    Err(error) => return Some(Err(error)),
                };
                after = batch.next;
                done = after.is_none();
                rows = batch.rows.into_iter();
            }
        });
        library::export_m3u(path, tracks)
    }
}
