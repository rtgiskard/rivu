use super::*;

impl Core {
    pub(in crate::core) fn create_playlist(&mut self, name: String) -> Result<()> {
        self.store.create_playlist(&name)?;
        self.state.library.playlists = Arc::new(self.store.playlists()?);
        Ok(())
    }

    pub(in crate::core) fn create_playlist_with_tracks(
        &mut self,
        name: String,
        track_ids: Vec<i64>,
    ) -> Result<()> {
        let playlist_id = self.store.create_playlist(&name)?;
        self.store.add_playlist(playlist_id, &track_ids)?;
        self.state.library.playlists = Arc::new(self.store.playlists()?);
        Ok(())
    }

    pub(in crate::core) fn rename_playlist(
        &mut self,
        playlist_id: i64,
        name: String,
    ) -> Result<()> {
        self.store.rename_playlist(playlist_id, &name)?;
        self.state.library.playlists = Arc::new(self.store.playlists()?);
        Ok(())
    }

    pub(in crate::core) fn delete_playlist(&mut self, playlist_id: i64) -> Result<()> {
        self.store.delete_playlist(playlist_id)?;
        self.state.library.playlists = Arc::new(self.store.playlists()?);
        Ok(())
    }

    pub(in crate::core) fn add_playlist(
        &mut self,
        playlist_id: i64,
        track_ids: Vec<i64>,
    ) -> Result<()> {
        self.store.add_playlist(playlist_id, &track_ids)?;
        self.state.library.playlists = Arc::new(self.store.playlists()?);
        Ok(())
    }

    pub(in crate::core) fn remove_playlist_entry(&mut self, entry_id: i64) -> Result<()> {
        self.store.remove_playlist_entry(entry_id)?;
        self.state.library.playlists = Arc::new(self.store.playlists()?);
        Ok(())
    }

    pub(in crate::core) fn move_playlist_entry(
        &mut self,
        entry_id: i64,
        index: usize,
    ) -> Result<()> {
        self.store.move_playlist_entry(entry_id, index)?;
        self.state.library.playlists = Arc::new(self.store.playlists()?);
        Ok(())
    }

    pub(in crate::core) fn export_playlist(&self, playlist_id: i64, path: &Path) -> Result<()> {
        let playlist = self
            .state
            .library
            .playlists
            .iter()
            .find(|playlist| playlist.id == playlist_id)
            .context("Playlist not found")?;
        library::export_m3u(path, playlist, &self.state.library.tracks)
    }
}
