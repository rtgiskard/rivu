use super::*;

impl Core {
    pub(in crate::core) fn create_playlist(&mut self, name: String) -> Result<()> {
        let playlist_id = self.store.create_playlist(&name)?;
        self.playlist_created(playlist_id, false)
    }

    pub(in crate::core) fn create_playlist_with_tracks(
        &mut self,
        name: String,
        track_ids: Vec<i64>,
    ) -> Result<()> {
        let playlist_id = self.store.create_playlist_with_tracks(&name, &track_ids)?;
        self.playlist_created(playlist_id, !track_ids.is_empty())
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
        let had_members = self.store.delete_playlist(playlist_id)?;
        self.playlist_deleted(playlist_id, had_members)
    }

    pub(in crate::core) fn add_playlist(
        &mut self,
        playlist_id: i64,
        track_ids: Vec<i64>,
    ) -> Result<()> {
        if self.store.add_playlist(playlist_id, &track_ids)? {
            self.playlist_members_changed(playlist_id)?;
        }
        Ok(())
    }

    pub(in crate::core) fn remove_playlist_entry(&mut self, entry_id: i64) -> Result<()> {
        let playlist_id = self.store.remove_playlist_entry(entry_id)?;
        self.playlist_members_changed(playlist_id)
    }

    pub(in crate::core) fn move_playlist_entry(
        &mut self,
        entry_id: i64,
        index: usize,
    ) -> Result<()> {
        if let Some(playlist_id) = self.store.move_playlist_entry(entry_id, index)? {
            self.advance_playlist_entry_revision(playlist_id);
        }
        Ok(())
    }
    fn next_playlist_entry_revision(&mut self) -> u64 {
        self.state.playlist_content_sequence = self
            .state
            .playlist_content_sequence
            .checked_add(1)
            .expect("Playlist content revision exhausted");
        self.state.playlist_content_sequence
    }

    fn advance_playlist_entry_revision(&mut self, playlist_id: i64) {
        let next = self.next_playlist_entry_revision();
        let revision = Arc::make_mut(&mut self.state.library.playlist_entry_revisions)
            .get_mut(&playlist_id)
            .expect("Every extant playlist has a content revision");
        *revision = next;
    }

    pub(in crate::core) fn playlist_created(
        &mut self,
        playlist_id: i64,
        has_members: bool,
    ) -> Result<()> {
        let revision = self.next_playlist_entry_revision();
        Arc::make_mut(&mut self.state.library.playlist_entry_revisions)
            .insert(playlist_id, revision);
        if has_members {
            self.state.library.duplicate_members_revision = self
                .state
                .library
                .duplicate_members_revision
                .wrapping_add(1);
        }
        self.reload_playlists()
    }

    pub(in crate::core) fn playlist_deleted(
        &mut self,
        playlist_id: i64,
        had_members: bool,
    ) -> Result<()> {
        Arc::make_mut(&mut self.state.library.playlist_entry_revisions).remove(&playlist_id);
        if had_members {
            self.state.library.duplicate_members_revision = self
                .state
                .library
                .duplicate_members_revision
                .wrapping_add(1);
        }
        self.reload_playlists()
    }

    pub(in crate::core) fn invalidate_playlist_members(&mut self, ids: &[i64]) {
        if ids.is_empty() {
            return;
        }
        for &playlist_id in ids {
            if self
                .state
                .library
                .playlist_entry_revisions
                .contains_key(&playlist_id)
            {
                self.advance_playlist_entry_revision(playlist_id);
            }
        }
        self.state.library.playlist_revision = self.state.library.playlist_revision.wrapping_add(1);
        self.state.library.duplicate_members_revision = self
            .state
            .library
            .duplicate_members_revision
            .wrapping_add(1);
    }

    pub(in crate::core) fn playlist_members_changed(&mut self, playlist_id: i64) -> Result<()> {
        self.invalidate_playlist_members(&[playlist_id]);
        self.refresh_playlist_count()
    }

    pub(in crate::core) fn reload_playlists(&mut self) -> Result<()> {
        self.state.library.playlist_revision = self.state.library.playlist_revision.wrapping_add(1);
        self.refresh_playlist_count()
    }

    fn refresh_playlist_count(&mut self) -> Result<()> {
        self.state.library.playlist_total = self
            .store.playlist_summary_page(None, 0, 0)
            .context("Playlist change committed, but refreshing playlists failed; previous projection retained")?
            .total;
        Ok(())
    }

    fn check_playlist_entry_revision(&self, playlist_id: i64, expected: u64) -> Result<()> {
        if self
            .state
            .library
            .playlist_entry_revisions
            .get(&playlist_id)
            != Some(&expected)
        {
            bail!("Playlist entry revision changed");
        }
        Ok(())
    }

    pub(in crate::core) fn remove_playlist_entries(
        &mut self,
        playlist_id: i64,
        entry_ids: &[i64],
        expected_revision: u64,
    ) -> Result<()> {
        self.check_playlist_entry_revision(playlist_id, expected_revision)?;
        if self.store.remove_playlist_entries(playlist_id, entry_ids)? {
            self.playlist_members_changed(playlist_id)?;
        }
        Ok(())
    }

    pub(in crate::core) fn move_playlist_entries(
        &mut self,
        playlist_id: i64,
        entry_ids: &[i64],
        before_entry_id: Option<i64>,
        expected_revision: u64,
    ) -> Result<()> {
        self.check_playlist_entry_revision(playlist_id, expected_revision)?;
        if self
            .store
            .move_playlist_entries(playlist_id, entry_ids, before_entry_id)?
        {
            self.advance_playlist_entry_revision(playlist_id);
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
