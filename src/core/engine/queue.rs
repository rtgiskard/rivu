use super::*;
use anyhow::ensure;
use rand::RngExt;

const MAX_NAVIGATION_HISTORY: usize = 4096;

impl Core {
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
        let mut selected = self
            .queue_ids
            .iter()
            .enumerate()
            .filter(|(_, id)| ids.contains(id));
        let (first, &first_id) = selected.next().context("Queue entry not found")?;
        let mut last = first;
        let mut found = 1;
        if ids.len() > 1 {
            for (position, _) in selected {
                last = position;
                found += 1;
                if found == ids.len() {
                    break;
                }
            }
        }
        ensure!(found == ids.len(), "Queue entry not found");
        let insertion = index.min(self.queue_ids.len() - ids.len());
        if ids.len() == 1 {
            if first == insertion {
                return Ok(());
            }
            self.store.move_queue_entry(first_id, insertion)?;
            if insertion < first {
                self.queue_ids[insertion..=first].rotate_right(1);
            } else {
                self.queue_ids[first..=insertion].rotate_left(1);
            }
        } else {
            let start = first.min(insertion);
            let end = (last + 1).max(insertion + ids.len());
            let span = &self.queue_ids[start..end];
            let mut order = Vec::with_capacity(span.len());
            let mut remaining = span.iter().copied().filter(|id| !ids.contains(id));
            order.extend(remaining.by_ref().take(insertion - start));
            order.extend(span.iter().copied().filter(|id| ids.contains(id)));
            order.extend(remaining);
            if order == span {
                return Ok(());
            }
            self.store.reorder_queue_range(start, &order)?;
            self.queue_ids[start..end].copy_from_slice(&order);
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
