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
            self.state.library.playlist_revision =
                self.state.library.playlist_revision.wrapping_add(1);
        }
        Ok(())
    }

    pub(in crate::core) fn delete_playlist(&mut self, playlist_id: i64) -> Result<()> {
        let had_members = self.store.delete_playlist(playlist_id)?;
        self.playlist_deleted(playlist_id, had_members)
    }

    pub(in crate::core) fn move_playlist(
        &mut self,
        playlist_id: i64,
        before_playlist_id: Option<i64>,
        after: bool,
        expected_revision: u64,
    ) -> Result<()> {
        anyhow::ensure!(
            self.state.library.playlist_revision == expected_revision,
            "Playlist catalog revision changed"
        );
        if self
            .store
            .move_playlist(playlist_id, before_playlist_id, after)?
        {
            self.state.library.playlist_revision =
                self.state.library.playlist_revision.wrapping_add(1);
        }
        Ok(())
    }

    pub(in crate::core) fn add_playlist(
        &mut self,
        playlist_id: i64,
        track_ids: Vec<i64>,
    ) -> Result<()> {
        if self.store.add_playlist(playlist_id, &track_ids)? {
            self.invalidate_playlist_members(&[playlist_id])?;
        }
        Ok(())
    }

    pub(in crate::core) fn remove_playlist_entry(&mut self, entry_id: i64) -> Result<()> {
        let playlist_id = self.store.remove_playlist_entry(entry_id)?;
        self.invalidate_playlist_members(&[playlist_id])?;
        Ok(())
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
    pub(in crate::core) fn advance_playlist_entry_revision(&mut self, playlist_id: i64) {
        self.next_playlist_token = self
            .next_playlist_token
            .checked_add(1)
            .expect("playlist token sequence exhausted");
        self.set_playlist_entry_revision(playlist_id, Some(self.next_playlist_token));
    }

    pub(in crate::core) fn set_playlist_entry_revision(
        &mut self,
        playlist_id: i64,
        revision: Option<u64>,
    ) -> bool {
        if self
            .state
            .library
            .playlist_entry_revisions
            .get(&playlist_id)
            .copied()
            == revision
        {
            return false;
        }
        let revisions = Arc::make_mut(&mut self.state.library.playlist_entry_revisions);
        if let Some(revision) = revision {
            revisions.insert(playlist_id, revision);
        } else {
            revisions.remove(&playlist_id);
        }
        true
    }

    pub(in crate::core) fn playlist_created(
        &mut self,
        playlist_id: i64,
        has_members: bool,
    ) -> Result<()> {
        self.invalidate_playlist_creation(has_members);
        self.advance_playlist_entry_revision(playlist_id);
        self.refresh_playlist_count().context(
            "Playlist creation committed, but refreshing views failed; some projections may be stale",
        )
    }

    pub(in crate::core) fn invalidate_playlist_creation(&mut self, has_members: bool) {
        if has_members {
            self.state.library.duplicate_members_revision = self
                .state
                .library
                .duplicate_members_revision
                .wrapping_add(1);
        }
        self.state.library.playlist_revision = self.state.library.playlist_revision.wrapping_add(1);
    }

    pub(in crate::core) fn playlist_deleted(
        &mut self,
        playlist_id: i64,
        had_members: bool,
    ) -> Result<()> {
        self.set_playlist_entry_revision(playlist_id, None);
        if had_members {
            self.state.library.duplicate_members_revision = self
                .state
                .library
                .duplicate_members_revision
                .wrapping_add(1);
        }
        self.state.library.playlist_revision = self.state.library.playlist_revision.wrapping_add(1);
        self.refresh_playlist_count()
    }

    pub(in crate::core) fn invalidate_playlist_members(&mut self, ids: &[i64]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        self.state.library.playlist_revision = self.state.library.playlist_revision.wrapping_add(1);
        self.state.library.duplicate_members_revision = self
            .state
            .library
            .duplicate_members_revision
            .wrapping_add(1);
        for &playlist_id in ids {
            self.advance_playlist_entry_revision(playlist_id);
        }
        Ok(())
    }

    fn refresh_playlist_count(&mut self) -> Result<()> {
        self.state.library.playlist_total = self
            .store.playlist_summary_page(None, 0, 0)
            .context("Playlist change committed, but refreshing playlists failed; previous projection retained")?
            .total;
        Ok(())
    }

    pub(in crate::core) fn check_playlist_entry_revision(
        &self,
        playlist_id: i64,
        expected: u64,
    ) -> Result<()> {
        anyhow::ensure!(
            self.state
                .library
                .playlist_entry_revisions
                .get(&playlist_id)
                .copied()
                == Some(expected),
            "Playlist entry revision changed"
        );
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
            self.invalidate_playlist_members(&[playlist_id])?;
        }
        Ok(())
    }

    pub(in crate::core) fn move_playlist_entries(
        &mut self,
        playlist_id: i64,
        entry_ids: &[i64],
        before_entry_id: Option<i64>,
        after: bool,
        expected_revision: u64,
    ) -> Result<()> {
        self.check_playlist_entry_revision(playlist_id, expected_revision)?;
        if self
            .store
            .move_playlist_entries(playlist_id, entry_ids, before_entry_id, after)?
        {
            self.advance_playlist_entry_revision(playlist_id);
        }
        Ok(())
    }
}
