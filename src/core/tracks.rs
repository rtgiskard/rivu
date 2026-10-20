use super::*;

impl Core {
    pub(in crate::core) fn track(&self, id: i64) -> Result<Track> {
        self.store.track(id)?.context("Track not found")
    }

    pub(in crate::core) fn refresh_affected_queue_tracks(
        &mut self,
        track_ids: &[i64],
    ) -> Result<()> {
        self.state.queue = self.store.queue_projection(self.queue_ids.len())?;
        if let Some(current) = self.state.current_track.as_ref()
            && track_ids.contains(&current.id)
        {
            let track = self.store.track(current.id)?;
            if track.as_ref() != Some(current.as_ref()) {
                self.replace_current_track(track)?;
            }
        }
        Ok(())
    }

    /// Both targeted edits and scan/CUE refreshes pass through this boundary.
    /// Queue admission precedes replacing the cache: a committed producer that
    /// cannot synchronize audio reports a refresh error and retains the old
    /// snapshot, so a later refresh still observes the outstanding difference.
    pub(in crate::core) fn replace_current_track(&mut self, track: Option<Track>) -> Result<()> {
        if let Some(track) = track.as_ref()
            && self
                .playback
                .as_ref()
                .is_some_and(|playback| playback.track_id == track.id)
            && self.state.current_track.as_ref().is_none_or(|current| {
                current.title != track.title
                    || current.artist != track.artist
                    || current.album != track.album
            })
        {
            self.audio(AudioCommand::Metadata {
                generation: self.generation,
                metadata: Self::playback_metadata(track),
            })?;
        }
        self.state.current_track = track.map(Arc::new);
        Ok(())
    }

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
            && (title.is_some() || artist.is_some() || album.is_some() || decoder_hint.is_some())
        {
            // A failed projection refresh can leave current_track behind the store.
            let stored = self.track(track_id)?;
            let next_title = title.as_deref().unwrap_or(&stored.title);
            let next_artist = artist.as_deref().unwrap_or(&stored.artist);
            let next_album = album.as_deref().unwrap_or(&stored.album);
            let library_changed = next_title != stored.title
                || next_artist != stored.artist
                || next_album != stored.album
                || decoder_hint.is_some_and(|hint| hint != stored.decoder_hint);
            let metadata_changed = next_title != current.title
                || next_artist != current.artist
                || next_album != current.album;
            if library_changed && metadata_changed {
                // Core is the sole producer; the worker can only free capacity.
                self.engine.ensure_command_capacity(1)?;
            }
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
        if self
            .retired_playback
            .as_ref()
            .is_some_and(|retired| track_ids.contains(&retired.stats.track_id))
        {
            self.flush_deferred_playback()?;
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
        if self.retired_playback.is_some() {
            self.flush_deferred_playback()?;
        }
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

    /// Publish committed facts without reading potentially large projections.
    pub(in crate::core) fn invalidate_scan_impacts(&mut self, impacts: crate::store::ScanImpacts) {
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
    }

    /// Announce committed producer facts before any fallible cached projection read.
    pub(in crate::core) fn apply_scan_impacts(
        &mut self,
        impacts: crate::store::ScanImpacts,
    ) -> Result<()> {
        self.invalidate_scan_impacts(impacts);
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
