use super::*;

impl Core {
    pub(in crate::core) fn create_playlist(&mut self, name: String) -> Result<()> {
        self.store.create_playlist(&name)?;
        self.reload(true)
    }

    pub(in crate::core) fn create_playlist_with_tracks(
        &mut self,
        name: String,
        track_ids: Vec<i64>,
    ) -> Result<()> {
        let playlist_id = self.store.create_playlist(&name)?;
        self.store.add_playlist(playlist_id, &track_ids)?;
        self.reload(true)
    }

    pub(in crate::core) fn rename_playlist(
        &mut self,
        playlist_id: i64,
        name: String,
    ) -> Result<()> {
        self.store.rename_playlist(playlist_id, &name)?;
        self.reload(true)
    }

    pub(in crate::core) fn delete_playlist(&mut self, playlist_id: i64) -> Result<()> {
        self.store.delete_playlist(playlist_id)?;
        self.reload(true)
    }

    pub(in crate::core) fn add_playlist(
        &mut self,
        playlist_id: i64,
        track_ids: Vec<i64>,
    ) -> Result<()> {
        self.store.add_playlist(playlist_id, &track_ids)?;
        self.reload(true)
    }

    pub(in crate::core) fn remove_playlist_entry(&mut self, entry_id: i64) -> Result<()> {
        self.store.remove_playlist_entry(entry_id)?;
        self.reload(true)
    }

    pub(in crate::core) fn move_playlist_entry(
        &mut self,
        entry_id: i64,
        index: usize,
    ) -> Result<()> {
        self.store.move_playlist_entry(entry_id, index)?;
        self.reload(true)
    }

    pub(in crate::core) fn export_playlist(&self, playlist_id: i64, path: &Path) -> Result<()> {
        let mut after = None;
        let mut rows: Vec<PlaylistEntryRow> = Vec::new();
        let mut index = 0;
        let mut done = false;
        let tracks = std::iter::from_fn(|| {
            loop {
                if index < rows.len() {
                    let row = &rows[index];
                    index += 1;
                    return Some(self.store.track(row.track_id).and_then(|track| {
                        track.context("Playlist contains a missing library track")
                    }));
                }
                if done {
                    return None;
                }
                let batch = match self.store.playlist_entry_batch(
                    playlist_id,
                    after,
                    crate::model::MAX_QUERY_ROWS,
                ) {
                    Ok(batch) => batch,
                    Err(error) => return Some(Err(error)),
                };
                after = batch.next;
                done = after.is_none();
                rows = batch.rows;
                index = 0;
            }
        });
        library::export_m3u(path, tracks)
    }
}
