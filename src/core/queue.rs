use super::*;
use anyhow::ensure;
use rand::RngExt;

const MAX_NAVIGATION_HISTORY: usize = 4096;

impl Core {
    pub(in crate::core) fn advance_queue_revision(&mut self) {
        self.state.queue.revision = self.state.queue.revision.wrapping_add(1);
    }
    pub(in crate::core) fn refresh_queue_tracks(&mut self) -> Result<()> {
        self.refresh_queue_projection()?;
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

    // Queue mutations persist their own transaction. Playback selection uses
    // this boundary too; shutdown retries final persistence.
    pub(in crate::core) fn save_queue(&mut self) -> Result<()> {
        ensure!(
            !self.database_unavailable,
            "Database outcome is uncertain; restart Rivu before saving"
        );
        self.queue_current_pending = true;
        if self.queue_ids_stale {
            self.reload_queue()
        } else {
            self.refresh_queue_projection()
        }
    }
    pub(in crate::core) fn random_queue_id(&self, rng: &mut impl rand::Rng) -> Option<u64> {
        if self.queue_ids.is_empty() {
            return None;
        }
        Some(self.queue_ids[rng.random_range(0..self.queue_ids.len())])
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
        if self.queue_ids_stale {
            self.reload_queue()?;
        } else {
            self.persist_queue_current()?;
            if self.queue_projection_stale {
                self.refresh_queue_tracks()?;
            }
        }
        Ok(())
    }

    fn persist_queue_current(&mut self) -> Result<()> {
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
        projection.revision = self.state.queue.revision;
        self.state.queue = projection;
        self.queue_projection_stale = false;
        Ok(())
    }

    fn refresh_queue_projection(&mut self) -> Result<()> {
        self.queue_projection_stale = true;
        self.persist_queue_current()?;
        let projection = self
            .store
            .queue_projection(self.queue_ids.len(), self.state.queue.revision);
        self.accept_queue_projection(projection)
    }

    pub(in crate::core) fn reload_queue(&mut self) -> Result<()> {
        self.queue_ids_stale = true;
        self.queue_ids = self.store.queue_ids()?;
        // A committed removal or replacement owns the disappearance of a
        // selection. Otherwise retry the runtime intent before reading DB current.
        if self.queue_current_pending
            && self
                .state
                .queue
                .current_id
                .is_some_and(|id| !self.queue_ids.contains(&id))
        {
            self.queue_current_pending = false;
        }
        self.prune_queue_history();
        self.refresh_queue_tracks()?;
        self.queue_ids_stale = false;
        Ok(())
    }

    pub(in crate::core) fn append_queue_ids(&mut self) -> Result<()> {
        if self.queue_ids_stale {
            return self.reload_queue();
        }
        // Extending can fail after adding a prefix, so only a full reload can
        // recover identities on failure. Projection failures need no ID reload.
        self.queue_ids_stale = true;
        self.store.extend_queue_ids(&mut self.queue_ids)?;
        self.queue_ids_stale = false;
        self.refresh_queue_projection()
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
            index <= self.queue_ids.len(),
            "Queue insertion index out of range"
        );
        Ok(())
    }

    pub(in crate::core) fn insert_queue_ids(
        &mut self,
        index: usize,
        inserted: Vec<u64>,
    ) -> Result<()> {
        if inserted.is_empty() {
            return Ok(());
        }
        self.advance_queue_revision();
        let projection = self.store.queue_projection(
            self.queue_ids.len() + inserted.len(),
            self.state.queue.revision,
        );
        self.accept_queue_insertion(index, inserted, projection)
    }

    pub(in crate::core) fn accept_queue_insertion(
        &mut self,
        index: usize,
        inserted: Vec<u64>,
        projection: Result<QueueState>,
    ) -> Result<()> {
        self.queue_ids.splice(index..index, inserted);
        self.accept_queue_projection(projection)
    }

    pub(in crate::core) fn remove_queue_entries(&mut self, queue_ids: &[u64]) -> Result<()> {
        if queue_ids.is_empty() {
            return Ok(());
        }
        self.ensure_queue_control_ready()?;
        let ids = queue_ids.iter().copied().collect::<HashSet<_>>();
        self.validate_queue_ids(&ids)?;
        if self
            .state
            .queue
            .current_id
            .is_some_and(|id| ids.contains(&id))
        {
            self.stop()?;
        }
        if self.store.remove_queue_entries(queue_ids)? {
            self.advance_queue_revision();
        }
        self.queue_ids.retain(|id| !ids.contains(id));
        self.prune_queue_history();
        self.refresh_queue_projection()?;
        if self.state.queue.current_id.is_none() {
            self.state.current_track = None;
        }
        Ok(())
    }

    fn validate_queue_ids(&self, ids: &HashSet<u64>) -> Result<()> {
        let found = self.queue_ids.iter().filter(|id| ids.contains(id)).count();
        ensure!(found == ids.len(), "Queue entry not found");
        Ok(())
    }

    pub(in crate::core) fn move_queue_entries(
        &mut self,
        queue_ids: &[u64],
        index: usize,
    ) -> Result<()> {
        let ids = queue_ids.iter().copied().collect::<HashSet<_>>();
        if ids.is_empty() {
            return Ok(());
        }
        self.ensure_queue_control_ready()?;
        let mut selected = Vec::with_capacity(ids.len());
        for (position, &id) in self.queue_ids.iter().enumerate() {
            if ids.contains(&id) {
                selected.push((
                    i64::try_from(id).context("Queue ID exceeds database range")?,
                    i64::try_from(position).context("Queue position exceeds database range")?,
                ));
                if selected.len() == ids.len() {
                    break;
                }
            }
        }
        ensure!(selected.len() == ids.len(), "Queue entry not found");
        let insertion = index.min(self.queue_ids.len() - selected.len());
        if !self.store.move_queue_entries(&selected, insertion)? {
            return Ok(());
        }
        self.advance_queue_revision();

        let first = selected[0].1 as usize;
        let last = selected[selected.len() - 1].1 as usize;
        let start = first.min(insertion);
        let end = (last + 1).max(insertion + selected.len());
        let span = &mut self.queue_ids[start..end];
        // Compact survivors stably, then open the selected block in place.
        // Only the K selected identities are buffered; work is linear in the span.
        let mut remaining = 0;
        for position in 0..span.len() {
            let id = span[position];
            if !ids.contains(&id) {
                span[remaining] = id;
                remaining += 1;
            }
        }
        let offset = insertion - start;
        span[offset..].rotate_right(selected.len());
        for (slot, &(id, _)) in span[offset..offset + selected.len()]
            .iter_mut()
            .zip(&selected)
        {
            *slot = id as u64;
        }
        self.refresh_queue_projection()?;
        Ok(())
    }

    pub(in crate::core) fn enqueue(&mut self, ids: &[i64]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        self.ensure_queue_control_ready()?;
        if self.store.enqueue_tracks(ids)? {
            self.advance_queue_revision();
            self.append_queue_ids()?;
        }
        Ok(())
    }

    pub(in crate::core) fn randomize_queue(&mut self, rng: &mut impl rand::Rng) -> Result<()> {
        self.ensure_queue_control_ready()?;
        if self.queue_ids.len() < 2 {
            return Ok(());
        }
        let mut order = self.queue_ids.clone();
        order.shuffle(rng);
        if order == self.queue_ids {
            return Ok(());
        }
        if self.store.reorder_queue(&order)? {
            self.advance_queue_revision();
        }
        self.queue_ids = order;
        self.refresh_queue_projection()?;
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

    pub(in crate::core) fn prune_queue_history(&mut self) {
        if self.played.is_empty() {
            self.played_cursor = 0;
            return;
        }
        // History is bounded independently of queue size. Avoid a second full queue index.
        let retained = self.played.iter().copied().collect::<HashSet<_>>();
        let present = self
            .queue_ids
            .iter()
            .copied()
            .filter(|id| retained.contains(id))
            .collect::<HashSet<_>>();
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
    }
}
