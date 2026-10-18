use super::*;

impl Core {
    pub(in crate::core) fn keep_track_independent(&mut self, track_id: i64) -> Result<()> {
        if self.store.keep_independent(track_id)? {
            self.state.library.duplicate_members_revision = self
                .state
                .library
                .duplicate_members_revision
                .wrapping_add(1);
        }
        Ok(())
    }

    pub(in crate::core) fn edit_track(
        &mut self,
        track_id: i64,
        title: Option<String>,
        artist: Option<String>,
        album: Option<String>,
        decoder_hint: Option<DecoderHint>,
    ) -> Result<()> {
        if let Some(current) = self.state.current_track.as_ref()
            && current.id == track_id
            && self
                .playback
                .as_ref()
                .is_some_and(|playback| playback.track_id == track_id)
            && (title.as_ref().is_some_and(|title| *title != current.title)
                || artist
                    .as_ref()
                    .is_some_and(|artist| *artist != current.artist)
                || album.as_ref().is_some_and(|album| *album != current.album))
        {
            // Core is the sole ordinary producer. Reserve this admission before
            // committing a live edit; the worker can only free capacity meanwhile.
            self.engine.ensure_command_capacity(1)?;
        }
        let impact = self.store.edit_track(
            track_id,
            title.as_deref(),
            artist.as_deref(),
            album.as_deref(),
            decoder_hint,
        )?;
        if impact.member_facts {
            self.state.library.duplicate_members_revision = self
                .state
                .library
                .duplicate_members_revision
                .wrapping_add(1);
        }
        if impact.library {
            self.state
                .library
                .change_metadata(Some(&[track_id]), impact.playlists.as_deref());
            (|| -> Result<()> {
                self.refresh_affected_queue_tracks(&[track_id])?;
                if title.is_some()
                    && self
                        .state
                        .library
                        .history
                        .iter()
                        .any(|entry| entry.track_id == track_id)
                {
                    let track = self.store.track(track_id)?.context("Track not found")?;
                    self.refresh_history_track(&track);
                }
                Ok(())
            })()
            .context(
                "Track edit committed, but refreshing views failed; some projections may be stale",
            )?;
        }
        Ok(())
    }

    pub(in crate::core) fn remove_tracks(&mut self, mut track_ids: Vec<i64>) -> Result<()> {
        track_ids.sort_unstable();
        track_ids.dedup();
        if track_ids.is_empty() {
            return Ok(());
        }
        let removal = self.store.prepare_track_removal(&track_ids)?;
        if self
            .state
            .current_track()
            .is_some_and(|track| track_ids.contains(&track.id))
        {
            self.stop()?;
        }
        // Keep the selection on failure; only a committed cascade clears it.
        let impact = self.store.remove_prepared_tracks(removal)?;
        self.invalidate_playlist_members(&impact.playlists);
        let catalog = self.apply_scan_impacts(crate::store::ScanImpacts {
            changed: true,
            library: true,
            structure: true,
            duplicate_groups: impact.duplicate_groups,
            duplicate_members: false,
            ..crate::store::ScanImpacts::default()
        });
        let queue = self
            .reload_queue()
            .and_then(|()| self.refresh_queue_tracks());
        catalog.and(queue).context(
            "Track deletion committed, but refreshing views failed; some projections may be stale",
        )?;
        Ok(())
    }

    pub(in crate::core) fn set_favorite(
        &mut self,
        track_ids: &[i64],
        favorite: bool,
    ) -> Result<()> {
        let impact = self.store.set_favorite(track_ids, favorite)?;
        if impact.changed {
            self.state
                .library
                .change_metadata(impact.tracks.as_deref(), impact.playlists.as_deref());
            for entry in Arc::make_mut(&mut self.state.library.history) {
                if track_ids.contains(&entry.track_id) {
                    entry.favorite = favorite;
                }
            }
            self.refresh_affected_queue_tracks(track_ids)
                .context("Favorite edit committed, but refreshing views failed; some projections may be stale")?;
        }
        Ok(())
    }

    pub(in crate::core) fn toggle_favorite(&mut self, track_ids: &[i64]) -> Result<()> {
        let impact = self.store.toggle_favorite(track_ids)?;
        if impact.changed {
            self.state
                .library
                .change_metadata(impact.tracks.as_deref(), impact.playlists.as_deref());
            for entry in Arc::make_mut(&mut self.state.library.history) {
                if track_ids.contains(&entry.track_id) {
                    entry.favorite = !entry.favorite;
                }
            }
            self.refresh_affected_queue_tracks(track_ids)
                .context("Favorite edit committed, but refreshing views failed; some projections may be stale")?;
        }
        Ok(())
    }

    pub(in crate::core) fn remove_missing_tracks(&mut self) -> Result<()> {
        if self
            .state
            .current_track()
            .is_some_and(|track| track.missing)
        {
            self.stop()?;
        }
        // A stopped decoder does not imply the database deletion succeeded.
        let impact = self.store.remove_missing_tracks()?;
        if impact.removed != 0 {
            self.invalidate_playlist_members(&impact.playlists);
            let catalog = self.apply_scan_impacts(crate::store::ScanImpacts {
                changed: true,
                library: true,
                structure: true,
                duplicate_groups: impact.duplicate_groups,
                duplicate_members: false,
                ..crate::store::ScanImpacts::default()
            });
            let queue = self
                .reload_queue()
                .and_then(|()| self.refresh_queue_tracks());
            catalog.and(queue).context("Track deletion committed, but refreshing views failed; some projections may be stale")?;
        }
        Ok(())
    }

    pub(in crate::core) fn update_track_stats(
        &mut self,
        track_id: i64,
        stats: crate::store::TrackStats,
    ) -> Result<()> {
        self.state
            .library
            .change_metadata(Some(&[track_id]), Some(&[]));
        if let Some(index) =
            self.state.library.history.iter().position(|entry| {
                entry.track_id == track_id && entry.play_count != stats.play_count
            })
        {
            Arc::make_mut(&mut self.state.library.history)[index].play_count = stats.play_count;
        }
        if let Some(current) = self.state.current_track.as_mut()
            && current.id == track_id
            && (current.play_count != stats.play_count || current.last_played != stats.last_played)
        {
            let current = Arc::make_mut(current);
            current.play_count = stats.play_count;
            current.last_played = stats.last_played;
        }
        self.state.queue = self.store.queue_projection(self.queue_ids.len())
            .context("Playback statistics committed, but refreshing queue failed; previous queue projection retained")?;
        Ok(())
    }

    pub(in crate::core) fn refresh_history_track(&mut self, track: &Track) {
        let Some(index) = self
            .state
            .library
            .history
            .iter()
            .position(|entry| entry.track_id == track.id)
        else {
            return;
        };
        let entry = &self.state.library.history[index];
        if entry.title == track.title && entry.play_count == track.play_count {
            return;
        }
        let entry = &mut Arc::make_mut(&mut self.state.library.history)[index];
        entry.title.clone_from(&track.title);
        entry.play_count = track.play_count;
    }

    /// Announce committed producer facts before any fallible cached projection read.
    pub(in crate::core) fn apply_scan_impacts(
        &mut self,
        impacts: crate::store::ScanImpacts,
    ) -> Result<()> {
        if impacts.library {
            self.state.library.revision = self.state.library.revision.wrapping_add(1);
        }
        if impacts.structure {
            self.state.library.structure_revision =
                self.state.library.structure_revision.wrapping_add(1);
        }
        if impacts.duplicate_groups {
            self.state.library.duplicate_groups_revision =
                self.state.library.duplicate_groups_revision.wrapping_add(1);
        }
        if impacts.duplicate_members {
            self.state.library.duplicate_members_revision = self
                .state
                .library
                .duplicate_members_revision
                .wrapping_add(1);
        }
        let library = if impacts.structure {
            self.store
                .track_count()
                .map(|total| self.state.library.track_total = total)
                .context("Refreshing library count failed; previous count retained")
        } else {
            Ok(())
        };
        let history = if impacts.library {
            self.store
                .history(MAX_QUERY_ROWS)
                .map(|history| self.state.library.history = Arc::new(history))
                .context("Refreshing history failed; previous history retained")
        } else {
            Ok(())
        };
        // A failed projection read cannot hide another committed domain.
        let queue = if impacts.library {
            self.refresh_queue_tracks()
        } else {
            Ok(())
        };
        library.and(history).and(queue)
    }
}
