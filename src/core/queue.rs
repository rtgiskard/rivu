use super::*;
use anyhow::ensure;
use rand::RngExt;

pub(in crate::core) enum QueueRefresh {
    Projection,
    ProjectionAndHistory,
    Reload,
}

/// This context is attached only after a queue transaction has committed.
#[derive(Debug)]
pub(in crate::core) struct QueueSyncError;

impl std::fmt::Display for QueueSyncError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Queue modification committed, but refreshing views failed; changes are saved; do not repeat the modification")
    }
}

impl std::error::Error for QueueSyncError {}

impl Core {
    /// Displayed queued row values changed; occurrence positions are unchanged.
    pub(in crate::core) fn advance_queue_revision(&mut self) {
        self.state.queue.revision = self.state.queue.revision.wrapping_add(1);
    }

    /// Occurrence membership or order changed, which also changes queue content.
    pub(in crate::core) fn advance_queue_structure(&mut self) {
        self.state.queue.structure_revision = self.state.queue.structure_revision.wrapping_add(1);
        self.advance_queue_revision();
    }

    /// Committed queue facts stamped with the structure they were read at.
    pub(in crate::core) fn read_queue_projection(&self) -> Result<QueueState> {
        Ok(QueueState {
            structure_revision: self.state.queue.structure_revision,
            ..self.store.queue_projection()?
        })
    }

    pub(in crate::core) fn queue_mutation_committed(
        &mut self,
        refresh: QueueRefresh,
    ) -> Result<()> {
        self.advance_queue_structure();
        match refresh {
            QueueRefresh::Projection => self.refresh_queue_projection(),
            QueueRefresh::ProjectionAndHistory => {
                self.transport.projection_stale = true;
                self.prune_queue_history()
                    .and_then(|()| self.refresh_queue_projection())
            }
            QueueRefresh::Reload => self.reload_queue(),
        }
        .context(QueueSyncError)
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
        self.state.playback.audio_output = None;
        self.transport.output_prior_underruns = 0;
        self.transport.output_prior_missing_seconds = 0.0;
        // Admitting audio does not resolve a pending History projection failure.
        if !self.transport.history_pending {
            self.state.system.last_error = None;
        }
        self.transport.select_occurrence(queue_id, history);
        // Load admission is already confirmed. A rejected durable selection
        // retains this occurrence and its listening source for later recovery.
        self.refresh_queue_projection()
    }

    pub(in crate::core) fn queue_replaced(&mut self, changed: bool) -> Result<()> {
        self.transport.replace_occurrences();
        self.state.queue.current_id = None;
        self.state.queue.current_position = None;
        self.state.current_track = None;
        self.state.playback.stopped_source_generation = None;
        self.state.playback.audio_output = None;
        self.transport.output_prior_underruns = 0;
        self.transport.output_prior_missing_seconds = 0.0;
        if changed {
            self.queue_mutation_committed(QueueRefresh::Reload)
        } else {
            self.reload_queue()
        }
    }

    pub(in crate::core) fn clear_queue(&mut self) -> Result<()> {
        self.stop()?;
        let changed = self.store.clear_queue()?;
        self.queue_replaced(changed)
    }

    pub(in crate::core) fn play_album(&mut self, key: &AlbumKey) -> Result<()> {
        ensure!(self.store.album_has_tracks(key)?, "Album is empty");
        self.play_replaced_queue("Album is empty", |store| {
            store.replace_queue_with_album(key)
        })
    }

    pub(in crate::core) fn play_playlist(&mut self, playlist_id: i64) -> Result<()> {
        ensure!(
            self.store.playlist_has_entries(playlist_id)?,
            "Playlist is empty"
        );
        self.play_replaced_queue("Playlist is empty", |store| {
            store.replace_queue_from_playlist(playlist_id)
        })
    }

    fn play_replaced_queue(
        &mut self,
        empty: &'static str,
        replace: impl FnOnce(&crate::store::Store) -> Result<()>,
    ) -> Result<()> {
        self.stop()?;
        replace(&self.store)?;
        self.queue_replaced(true)?;
        let first = self.store.queue_id_at(0)?.context(empty)?;
        self.play_after_queue_replacement(first)
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
            projection = self.read_queue_projection()?;
        } else if projection.structure_revision != self.state.queue.structure_revision {
            projection = self.read_queue_projection()?;
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
        self.state.queue = QueueState {
            revision: self.state.queue.revision,
            ..projection
        };
        self.transport.projection_stale = false;
        Ok(())
    }

    pub(in crate::core) fn refresh_queue_projection(&mut self) -> Result<()> {
        self.transport.projection_stale = true;
        self.persist_queue_current()?;
        let projection = self.read_queue_projection();
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
        expected_structure_revision: u64,
    ) -> Result<()> {
        ensure!(
            self.state.queue.structure_revision == expected_structure_revision,
            "Queue structure revision changed"
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
            self.queue_mutation_committed(QueueRefresh::Reload)
        } else {
            self.reload_queue()
        }
    }

    pub(in crate::core) fn move_queue_entries(
        &mut self,
        queue_ids: &[u64],
        index: usize,
        expected_structure_revision: u64,
    ) -> Result<()> {
        ensure!(
            self.state.queue.structure_revision == expected_structure_revision,
            "Queue structure revision changed"
        );
        if queue_ids.is_empty() {
            return Ok(());
        }
        self.ensure_queue_control_ready()?;
        if self.store.move_queue_entries(queue_ids, index)? {
            self.queue_mutation_committed(QueueRefresh::Projection)?;
        }
        Ok(())
    }

    pub(in crate::core) fn enqueue(&mut self, ids: &[i64]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        self.ensure_queue_control_ready()?;
        if self.store.enqueue_tracks(ids)? {
            self.queue_mutation_committed(QueueRefresh::Projection)?;
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
            self.queue_mutation_committed(QueueRefresh::Projection)?;
        }
        Ok(())
    }

    pub(in crate::core) fn deduplicate_queue(&mut self) -> Result<()> {
        self.ensure_queue_control_ready()?;
        if self.store.deduplicate_queue()? {
            self.queue_mutation_committed(QueueRefresh::ProjectionAndHistory)?;
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
