use super::*;
use anyhow::ensure;
use rand::RngExt;

const MAX_NAVIGATION_HISTORY: usize = 4096;

impl Core {
    pub(in crate::core) fn advance_queue_revision(&mut self) {
        self.state.queue.revision = self.state.queue.revision.wrapping_add(1);
    }

    pub(in crate::core) fn refresh_queue_tracks(&mut self) -> Result<()> {
        self.queue_projection_stale = true;
        self.refresh_queue_projection()?;
        // Track decoding is part of recovery too: a successful scalar read alone
        // must not reopen controls after a committed metadata mutation.
        self.queue_projection_stale = true;
        let track = self
            .state
            .queue
            .current_id
            .map(|id| self.store.queue_track_id(id))
            .transpose()?
            .map(|id| self.store.track(id))
            .transpose()?
            .flatten();
        self.replace_current_track(track)?;
        self.queue_projection_stale = false;
        Ok(())
    }

    pub(in crate::core) fn set_shuffle(&mut self, enabled: bool) {
        if self.state.playback.shuffle == enabled {
            return;
        }
        self.state.playback.shuffle = enabled;
        self.played.clear();
        self.played_cursor = 0;
        if let Some(id) = self.state.queue.current_id {
            self.record_queue_history(id);
        }
    }

    // Runtime selection remains authoritative until its write succeeds. Queue
    // mutations commit independently and are never replayed by this recovery gate.
    pub(in crate::core) fn save_queue(&mut self) -> Result<()> {
        self.queue_current_pending = true;
        self.refresh_queue_projection()
    }

    pub(in crate::core) fn random_queue_id(&self, rng: &mut impl rand::Rng) -> Result<Option<u64>> {
        if self.state.queue.total == 0 {
            return Ok(None);
        }
        self.store
            .queue_id_at(rng.random_range(0..self.state.queue.total))
    }

    pub(in crate::core) fn record_queue_history(&mut self, id: u64) {
        self.played.truncate(self.played_cursor);
        self.played.push(id);
        if self.played.len() > MAX_NAVIGATION_HISTORY {
            self.played
                .drain(..self.played.len() - MAX_NAVIGATION_HISTORY);
        }
        self.played_cursor = self.played.len();
    }

    pub(in crate::core) fn ensure_queue_control_ready(&mut self) -> Result<()> {
        if self.queue_projection_stale {
            self.reload_queue()?;
        } else {
            self.persist_queue_current()?;
        }
        Ok(())
    }

    pub(in crate::core) fn persist_queue_current(&mut self) -> Result<()> {
        ensure!(
            !self.database_unavailable,
            "Database outcome is uncertain; restart Rivu before saving"
        );
        if self.queue_current_pending {
            self.store.set_queue_current(self.state.queue.current_id)?;
            self.queue_current_pending = false;
        }
        Ok(())
    }

    pub(in crate::core) fn accept_queue_projection(
        &mut self,
        projection: Result<QueueState>,
    ) -> Result<()> {
        self.queue_projection_stale = true;
        let mut projection = projection?;
        // A delayed projection must never replace an unsaved runtime selection.
        if self.queue_current_pending {
            self.persist_queue_current()?;
            projection = self.store.queue_projection(self.state.queue.revision)?;
        }
        projection.revision = self.state.queue.revision;
        self.state.queue = projection;
        self.queue_projection_stale = false;
        Ok(())
    }

    pub(in crate::core) fn refresh_queue_projection(&mut self) -> Result<()> {
        self.queue_projection_stale = true;
        self.persist_queue_current()?;
        let projection = self.store.queue_projection(self.state.queue.revision);
        self.accept_queue_projection(projection)
    }

    pub(in crate::core) fn reload_queue(&mut self) -> Result<()> {
        self.queue_projection_stale = true;
        // A committed removal/replacement owns a missing selection. Otherwise
        // retry the runtime intent before accepting the database's old current.
        if self.queue_current_pending
            && let Some(id) = self.state.queue.current_id
            && self.store.queue_existing_ids(&[id])?.is_empty()
        {
            self.queue_current_pending = false;
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
        if self.store.remove_prepared_queue_entries(selection)? {
            self.advance_queue_revision();
        }
        self.queue_projection_stale = true;
        self.prune_queue_history()?;
        self.refresh_queue_projection()?;
        if self.state.queue.current_id.is_none() {
            self.state.current_track = None;
        }
        Ok(())
    }

    pub(in crate::core) fn move_queue_entries(
        &mut self,
        queue_ids: &[u64],
        index: usize,
    ) -> Result<()> {
        if queue_ids.is_empty() {
            return Ok(());
        }
        self.ensure_queue_control_ready()?;
        if self.store.move_queue_entries(queue_ids, index)? {
            self.advance_queue_revision();
            self.refresh_queue_projection()?;
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
            self.refresh_queue_projection()?;
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
            self.refresh_queue_projection()?;
        }
        Ok(())
    }

    pub(in crate::core) fn deduplicate_queue(&mut self) -> Result<()> {
        self.ensure_queue_control_ready()?;
        if self.store.deduplicate_queue()? {
            self.advance_queue_revision();
            self.reload_queue()?;
        }
        Ok(())
    }

    pub(in crate::core) fn prune_queue_history(&mut self) -> Result<()> {
        if self.played.is_empty() {
            self.played_cursor = 0;
            return Ok(());
        }
        let present = self.store.queue_existing_ids(&self.played)?;
        let mut index = 0;
        let mut cursor = 0;
        self.played.retain(|id| {
            let keep = present.contains(id);
            if keep && index < self.played_cursor {
                cursor += 1;
            }
            index += 1;
            keep
        });
        self.played_cursor = cursor;
        Ok(())
    }
}
