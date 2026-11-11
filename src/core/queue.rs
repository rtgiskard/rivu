use super::*;
use anyhow::ensure;
use rand::RngExt;

impl Core {
    pub(in crate::core) fn advance_queue_revision(&mut self) {
        self.state.queue.revision = self.state.queue.revision.wrapping_add(1);
    }

    pub(in crate::core) fn refresh_queue_tracks(&mut self) -> Result<()> {
        self.transport.projection_stale = true;
        self.refresh_queue_projection()?;
        self.refresh_current_track()
    }

    pub(in crate::core) fn set_shuffle(&mut self, enabled: bool) {
        if self.preferences.live().shuffle == enabled {
            return;
        }
        self.preferences.set_shuffle(enabled);
        self.transport.reset_history();
        if let Some(id) = self.state.queue.current_id {
            self.transport.record_history(id);
        }
    }

    // Runtime selection remains authoritative until its write succeeds. Queue
    // mutations commit independently and are never replayed by this recovery gate.
    #[cfg(test)]
    pub(in crate::core) fn save_queue(&mut self) -> Result<()> {
        self.transport.selection_pending = true;
        self.refresh_queue_projection()
    }

    pub(in crate::core) fn accept_loaded_occurrence(
        &mut self,
        queue_id: u64,
        position: usize,
        track: Arc<Track>,
        status: PlaybackStatus,
        history: QueueHistory,
    ) -> Result<()> {
        self.state.queue.current_id = Some(queue_id);
        self.state.queue.current_position = Some(position);
        self.state.playback.duration = track.duration;
        self.state.current_track = Some(track);
        self.state.playback.last_heard_at = None;
        self.state.playback.status = status;
        self.state.playback.stopped_source_generation = None;
        self.state.system.last_error = None;
        self.transport.select_occurrence(queue_id, history);
        // Load admission is already confirmed. A rejected durable selection
        // retains this occurrence and its listening source for later recovery.
        self.refresh_queue_projection()
    }

    pub(in crate::core) fn queue_replaced(&mut self, changed: bool) -> Result<()> {
        if changed {
            self.advance_queue_revision();
        }
        self.transport.replace_occurrences();
        self.state.queue.current_id = None;
        self.state.queue.current_position = None;
        self.state.current_track = None;
        self.state.playback.stopped_source_generation = None;
        let result = self.reload_queue();
        if changed {
            result.context("Queue modification committed, but refreshing views failed; changes are saved; do not repeat the modification")
        } else {
            result
        }
    }

    pub(in crate::core) fn clear_queue(&mut self) -> Result<()> {
        self.stop()?;
        let changed = self.store.clear_queue()?;
        self.queue_replaced(changed)
    }

    pub(in crate::core) fn play_album(&mut self, key: &AlbumKey) -> Result<()> {
        ensure!(self.store.album_has_tracks(key)?, "Album is empty");
        self.stop()?;
        self.store.queue_album(key, true)?;
        self.queue_replaced(true)?;
        let first = self.store.queue_id_at(0)?.context("Album is empty")?;
        self.start(first, QueueHistory::Record)
    }

    pub(in crate::core) fn play_playlist(&mut self, playlist_id: i64) -> Result<()> {
        let total = self
            .store
            .playlist_entries_page(playlist_id, None, 0, 0)?
            .total;
        ensure!(total > 0, "Playlist is empty");
        self.stop()?;
        self.store.replace_queue_from_playlist(playlist_id)?;
        self.queue_replaced(true)?;
        let first = self.store.queue_id_at(0)?.context("Playlist is empty")?;
        self.start(first, QueueHistory::Record)
    }

    pub(in crate::core) fn random_queue_id(&self, rng: &mut impl rand::Rng) -> Result<Option<u64>> {
        if self.state.queue.total == 0 {
            return Ok(None);
        }
        self.store
            .queue_id_at(rng.random_range(0..self.state.queue.total))
    }

    pub(in crate::core) fn ensure_queue_control_ready(&mut self) -> Result<()> {
        if self.transport.projection_stale {
            self.reload_queue()?;
        } else {
            self.persist_queue_current()?;
        }
        if self.projections.current_track {
            self.refresh_current_track()?;
        }
        Ok(())
    }

    pub(in crate::core) fn persist_queue_current(&mut self) -> Result<()> {
        ensure!(
            !self.database_unavailable,
            "Database outcome is uncertain; restart Rivu before saving"
        );
        if self.transport.selection_pending {
            self.store.set_queue_current(self.state.queue.current_id)?;
            self.transport.selection_pending = false;
        }
        Ok(())
    }

    pub(in crate::core) fn accept_queue_projection(
        &mut self,
        projection: Result<QueueState>,
    ) -> Result<()> {
        self.transport.projection_stale = true;
        let mut projection = projection?;
        if self.transport.selection_pending {
            self.persist_queue_current()?;
            projection = self.store.queue_projection(self.state.queue.revision)?;
        } else if projection.revision != self.state.queue.revision {
            projection = self.store.queue_projection(self.state.queue.revision)?;
        }
        // Projections carry committed queue structure, not a new selection intent.
        // Even a formerly pending selection may have been saved since this read
        // began. Preserve every surviving occurrence and resolve its current index.
        if let Some(id) = self.state.queue.current_id {
            let position = if projection.current_id == Some(id) {
                projection.current_position
            } else {
                self.store.queue_location(id)?.map(|(_, position)| position)
            };
            if let Some(position) = position {
                projection.current_id = Some(id);
                projection.current_position = Some(position);
            }
        }
        projection.revision = self.state.queue.revision;
        self.state.queue = projection;
        self.transport.projection_stale = false;
        Ok(())
    }

    pub(in crate::core) fn refresh_queue_projection(&mut self) -> Result<()> {
        self.transport.projection_stale = true;
        self.persist_queue_current()?;
        let projection = self.store.queue_projection(self.state.queue.revision);
        self.accept_queue_projection(projection)
    }

    pub(in crate::core) fn reload_queue(&mut self) -> Result<()> {
        self.transport.projection_stale = true;
        // A committed removal/replacement owns a missing selection. Otherwise
        // retry the runtime intent before accepting the database's old current.
        if self.transport.selection_pending
            && let Some(id) = self.state.queue.current_id
            && self.store.queue_existing_ids(&[id])?.is_empty()
        {
            self.transport.selection_pending = false;
        }
        self.prune_queue_history()?;
        self.refresh_queue_tracks()
    }

    pub(in crate::core) fn check_queue_insertion(
        &self,
        index: usize,
        expected_revision: u64,
    ) -> Result<()> {
        ensure!(
            self.state.queue.revision == expected_revision,
            "Queue revision changed"
        );
        ensure!(
            index <= self.state.queue.total,
            "Queue insertion index out of range"
        );
        Ok(())
    }

    pub(in crate::core) fn remove_queue_entries(&mut self, queue_ids: &[u64]) -> Result<()> {
        if queue_ids.is_empty() {
            return Ok(());
        }
        self.ensure_queue_control_ready()?;
        // Resolve and validate all identities once, before stopping playback.
        let selection = self.store.prepare_queue_removal(queue_ids)?;
        if self
            .state
            .queue
            .current_id
            .is_some_and(|id| queue_ids.contains(&id))
        {
            self.stop()?;
        }
        let changed = self.store.remove_prepared_queue_entries(selection)?;
        if changed {
            self.advance_queue_revision();
        }
        let result = self.reload_queue();
        if changed {
            result.context("Queue modification committed, but refreshing views failed; changes are saved; do not repeat the modification")
        } else {
            result
        }
    }

    pub(in crate::core) fn move_queue_entries(
        &mut self,
        queue_ids: &[u64],
        index: usize,
        expected_revision: u64,
    ) -> Result<()> {
        ensure!(
            self.state.queue.revision == expected_revision,
            "Queue revision changed"
        );
        if queue_ids.is_empty() {
            return Ok(());
        }
        self.ensure_queue_control_ready()?;
        if self.store.move_queue_entries(queue_ids, index)? {
            self.advance_queue_revision();
            self.refresh_queue_projection().context("Queue modification committed, but refreshing views failed; changes are saved; do not repeat the modification")?;
        }
        Ok(())
    }

    pub(in crate::core) fn enqueue(&mut self, ids: &[i64]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        self.ensure_queue_control_ready()?;
        if self.store.enqueue_tracks(ids)? {
            self.advance_queue_revision();
            self.refresh_queue_projection().context("Queue modification committed, but refreshing views failed; changes are saved; do not repeat the modification")?;
        }
        Ok(())
    }

    pub(in crate::core) fn randomize_queue(&mut self, rng: &mut impl rand::Rng) -> Result<()> {
        self.ensure_queue_control_ready()?;
        if self.state.queue.total < 2 {
            return Ok(());
        }
        // Fisher-Yates needs the identities only for this operation; no Core
        // owner retains a full queue projection after the transaction.
        let mut order = self.store.queue_ids()?;
        order.shuffle(rng);
        if self.store.reorder_queue(&order)? {
            self.advance_queue_revision();
            self.refresh_queue_projection().context("Queue modification committed, but refreshing views failed; changes are saved; do not repeat the modification")?;
        }
        Ok(())
    }

    pub(in crate::core) fn deduplicate_queue(&mut self) -> Result<()> {
        self.ensure_queue_control_ready()?;
        if self.store.deduplicate_queue()? {
            self.advance_queue_revision();
            self.reload_queue().context("Queue modification committed, but refreshing views failed; changes are saved; do not repeat the modification")?;
        }
        Ok(())
    }

    pub(in crate::core) fn prune_queue_history(&mut self) -> Result<()> {
        if self.transport.history.is_empty() {
            self.transport.history_cursor = 0;
            return Ok(());
        }
        let present = self.store.queue_existing_ids(&self.transport.history)?;
        let mut index = 0;
        let mut cursor = 0;
        self.transport.history.retain(|id| {
            let keep = present.contains(id);
            if keep && index < self.transport.history_cursor {
                cursor += 1;
            }
            index += 1;
            keep
        });
        self.transport.history_cursor = cursor;
        Ok(())
    }
}
