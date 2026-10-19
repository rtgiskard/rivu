use super::*;
use anyhow::ensure;
use rand::RngExt;

const MAX_NAVIGATION_HISTORY: usize = 4096;

impl Core {
    pub(in crate::core) fn refresh_queue_tracks(&mut self) -> Result<()> {
        self.state.queue = self.store.queue_projection(self.queue_ids.len())?;
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

    pub(in crate::core) fn save_queue(&mut self) -> Result<()> {
        self.store.set_queue_current(self.state.queue.current_id)?;
        self.state.queue = self.store.queue_projection(self.queue_ids.len())?;
        Ok(())
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

    pub(in crate::core) fn reload_queue(&mut self) -> Result<()> {
        self.queue_ids = self.store.queue_ids()?;
        self.state.queue = self.store.queue_projection(self.queue_ids.len())?;
        self.prune_queue_history();
        Ok(())
    }

    pub(in crate::core) fn append_queue_ids(&mut self) -> Result<()> {
        self.store.extend_queue_ids(&mut self.queue_ids)?;
        self.state.queue = self.store.queue_projection(self.queue_ids.len())?;
        Ok(())
    }

    pub(in crate::core) fn remove_queue_entries(&mut self, queue_ids: &[u64]) -> Result<()> {
        if queue_ids.is_empty() {
            return Ok(());
        }
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
        self.store.remove_queue_entries(queue_ids)?;
        self.queue_ids.retain(|id| !ids.contains(id));
        self.state.queue = self.store.queue_projection(self.queue_ids.len())?;
        self.prune_queue_history();
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
        self.state.queue = self.store.queue_projection(self.queue_ids.len())?;
        Ok(())
    }

    pub(in crate::core) fn enqueue(&mut self, ids: &[i64]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        self.store.enqueue_tracks(ids)?;
        self.append_queue_ids()
    }

    pub(in crate::core) fn randomize_queue(&mut self, rng: &mut impl rand::Rng) -> Result<()> {
        if self.queue_ids.len() < 2 {
            return Ok(());
        }
        let mut order = self.queue_ids.clone();
        order.shuffle(rng);
        if order == self.queue_ids {
            return Ok(());
        }
        self.store.reorder_queue(&order)?;
        self.queue_ids = order;
        self.state.queue = self.store.queue_projection(self.queue_ids.len())?;
        Ok(())
    }

    pub(in crate::core) fn deduplicate_queue(&mut self) -> Result<()> {
        if self.store.deduplicate_queue()? {
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
