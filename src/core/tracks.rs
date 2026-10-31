use super::*;
use std::collections::HashMap;

impl Core {
    pub(in crate::core) fn invalidate_collection_impacts(&mut self, queue: bool, albums: bool) {
        if queue {
            self.advance_queue_revision();
        }
        if albums {
            self.state.library.album_revision = self.state.library.album_revision.wrapping_add(1);
        }
    }
    pub(in crate::core) fn track(&self, id: i64) -> Result<Track> {
        self.store.track(id)?.context("Track not found")
    }

    pub(in crate::core) fn refresh_affected_queue_tracks(
        &mut self,
        track_ids: &[i64],
    ) -> Result<()> {
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
    pub(in crate::core) fn replace_current_track(
        &mut self,
        mut track: Option<Track>,
    ) -> Result<()> {
        if let Some(track) = track.as_mut()
            && let Some(playback) = self.playback.as_ref().filter(|p| p.track_id == track.id)
        {
            // A rescan can change the CUE FILE, range, and duration without loading
            // a new decoder. Publish the source actually owned by this session;
            // the next explicit start reads the new library facts.
            if track.path != playback.loaded_track.path
                || track.cue != playback.loaded_track.cue
                || track.fingerprint != playback.loaded_track.fingerprint
            {
                let source = self
                    .state
                    .current_track
                    .as_deref()
                    .filter(|current| current.id == track.id)
                    .unwrap_or(&playback.loaded_track);
                track.codec.clone_from(&source.codec);
                track.channels = source.channels;
                track.sample_rate = source.sample_rate;
                track.bits_per_sample = source.bits_per_sample;
                track.bitrate_bps = source.bitrate_bps;
                track.bitrate_exact = source.bitrate_exact;
                track.path.clone_from(&playback.loaded_track.path);
                track.cue.clone_from(&playback.loaded_track.cue);
                track
                    .fingerprint
                    .clone_from(&playback.loaded_track.fingerprint);
            }
            track.duration = self.state.playback.duration;
        }
        if track.as_ref() == self.state.current_track.as_deref() {
            return Ok(());
        }
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
            self.engine.send(AudioCommand::Metadata {
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
        self.invalidate_collection_impacts(impact.queue, impact.albums);
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
        self.finish_track_removal(
            &impact.playlists,
            impact.duplicate_groups,
            impact.queue,
            impact.albums,
        )
    }

    pub(in crate::core) fn remove_history(&mut self, mut track_ids: Vec<i64>) -> Result<()> {
        track_ids.sort_unstable();
        track_ids.dedup();
        if track_ids.is_empty() {
            return Ok(());
        }
        let (changed, albums) = self.store.remove_history(&track_ids)?;
        // Commit before suppressing producer writes: failed removals preserve history.
        // The flag belongs to the session, including delayed Started/stop snapshots.
        for playback in self.playback.iter_mut().chain(
            self.retired_playback
                .iter_mut()
                .map(|retired| &mut retired.stats),
        ) {
            if track_ids.binary_search(&playback.track_id).is_ok() {
                playback.history_removed = true;
            }
        }
        if changed {
            self.invalidate_collection_impacts(false, albums);
            self.state
                .library
                .change_metadata(Some(&track_ids), Some(&[]));
            if let Some(current) = self.state.current_track.as_mut()
                && track_ids.binary_search(&current.id).is_ok()
            {
                Arc::make_mut(current).last_played = None;
            }
            self.playback_history_pending = true;
        }
        if self.playback_history_pending {
            let history = self.store.history(MAX_QUERY_ROWS).context(
                "History removal committed, but refreshing history failed; previous history retained",
            )?;
            self.replace_history(history);
        }
        Ok(())
    }

    pub(in crate::core) fn set_favorite(
        &mut self,
        track_ids: &[i64],
        favorite: bool,
    ) -> Result<()> {
        let impact = self.store.set_favorite(track_ids, favorite)?;
        self.invalidate_collection_impacts(impact.queue, impact.albums);
        if impact.changed {
            self.state
                .library
                .change_metadata(impact.tracks.as_deref(), impact.playlists.as_deref());
            self.update_history_favorites(track_ids, Some(favorite));
            self.refresh_affected_queue_tracks(track_ids)
                .context("Favorite edit committed, but refreshing views failed; some projections may be stale")?;
        }
        Ok(())
    }

    pub(in crate::core) fn toggle_favorite(&mut self, track_ids: &[i64]) -> Result<()> {
        let impact = self.store.toggle_favorite(track_ids)?;
        self.invalidate_collection_impacts(impact.queue, impact.albums);
        if impact.changed {
            self.state
                .library
                .change_metadata(impact.tracks.as_deref(), impact.playlists.as_deref());
            self.update_history_favorites(track_ids, None);
            self.refresh_affected_queue_tracks(track_ids)
                .context("Favorite edit committed, but refreshing views failed; some projections may be stale")?;
        }
        Ok(())
    }

    fn update_history_favorites(&mut self, track_ids: &[i64], favorite: Option<bool>) {
        // History is bounded; index it rather than copying a potentially large
        // selection or repeatedly scanning that selection for every history row.
        let mut positions: HashMap<_, _> = self
            .state
            .library
            .history
            .iter()
            .enumerate()
            .filter(|(_, entry)| favorite != Some(entry.favorite))
            .map(|(index, entry)| (entry.track_id, index))
            .collect();
        if positions.is_empty() {
            return;
        }
        for track_id in track_ids {
            // Removing a hit also makes duplicate toggle IDs idempotent.
            if let Some(index) = positions.remove(track_id) {
                let entry = &mut Arc::make_mut(&mut self.state.library.history)[index];
                entry.favorite = favorite.unwrap_or(!entry.favorite);
                if positions.is_empty() {
                    break;
                }
            }
        }
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
            self.finish_track_removal(
                &impact.playlists,
                impact.duplicate_groups,
                impact.queue,
                impact.albums,
            )?;
        }
        Ok(())
    }

    fn finish_track_removal(
        &mut self,
        affected_playlists: &[i64],
        duplicate_groups_changed: bool,
        queue_changed: bool,
        albums_changed: bool,
    ) -> Result<()> {
        let impacts = crate::store::ScanImpacts {
            changed: true,
            library: true,
            structure: true,
            duplicate_groups: duplicate_groups_changed,
            queue: queue_changed,
            albums: albums_changed,
            ..crate::store::ScanImpacts::default()
        };
        self.invalidate_scan_impacts(impacts);
        let playlists = self.invalidate_playlist_members(affected_playlists);
        let catalog = self.refresh_scan_catalog(impacts);
        // Recover the committed scalar projection and selected track; a failed
        // refresh keeps dependent queue controls gated until recovery succeeds.
        let queue = if queue_changed {
            self.reload_queue()
        } else {
            Ok(())
        };
        playlists.and(catalog).and(queue).context(
            "Track deletion committed, but refreshing views failed; some projections may be stale",
        )
    }

    pub(in crate::core) fn update_track_stats(
        &mut self,
        track_id: i64,
        stats: crate::store::TrackStats,
    ) -> Result<()> {
        self.invalidate_collection_impacts(stats.queue, stats.albums);
        if stats.changed {
            self.state
                .library
                .change_metadata(Some(&[track_id]), Some(&[]));
        }
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
        let dependencies = impacts.dependencies(false);
        self.invalidate_collection_impacts(
            dependencies.queue_revision,
            dependencies.album_revision,
        );
        if dependencies.library_revision {
            self.state.library.revision = self.state.library.revision.wrapping_add(1);
        }
        if dependencies.structure_revision {
            self.state.library.structure_revision =
                self.state.library.structure_revision.wrapping_add(1);
        }
        if dependencies.duplicate_group_revision {
            self.state.library.duplicate_groups_revision =
                self.state.library.duplicate_groups_revision.wrapping_add(1);
        }
        if dependencies.duplicate_member_revision {
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
        let catalog = self.refresh_scan_catalog(impacts);
        // A failed projection read cannot hide another committed domain.
        let dependencies = impacts.dependencies(false);
        let queue = if dependencies.refresh_queue {
            self.refresh_queue_tracks()
        } else {
            Ok(())
        };
        catalog.and(queue)
    }

    fn refresh_scan_catalog(&mut self, impacts: crate::store::ScanImpacts) -> Result<()> {
        let dependencies = impacts.dependencies(false);
        let library = if dependencies.refresh_track_total {
            self.store
                .track_count()
                .map(|total| self.state.library.track_total = total)
                .context("Refreshing library count failed; previous count retained")
        } else {
            Ok(())
        };
        let history = if dependencies.refresh_history {
            self.store
                .history(MAX_QUERY_ROWS)
                .map(|history| self.replace_history(history))
                .context("Refreshing history failed; previous history retained")
        } else {
            Ok(())
        };
        library.and(history)
    }
}
