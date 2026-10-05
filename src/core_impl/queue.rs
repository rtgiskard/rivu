use super::*;

impl Core {
    pub(in crate::core) fn remove_queue_entries(&mut self, queue_ids: &[u64]) -> Result<()> {
        if queue_ids.is_empty() {
            return Ok(());
        }
        let ids = queue_ids.iter().copied().collect::<HashSet<_>>();
        let found = self
            .state
            .queue
            .entries
            .iter()
            .filter(|entry| ids.contains(&entry.id))
            .count();
        if found != ids.len() {
            let missing = ids
                .iter()
                .find(|id| {
                    !self
                        .state
                        .queue
                        .entries
                        .iter()
                        .any(|entry| entry.id == **id)
                })
                .copied()
                .expect("bulk queue validation mismatch");
            bail!("Queue entry not found: {missing}");
        }

        if self
            .state
            .queue
            .current_id
            .is_some_and(|id| ids.contains(&id))
        {
            self.stop()?;
            self.state.queue.current_id = None;
        }
        Arc::make_mut(&mut self.state.queue.entries).retain(|entry| !ids.contains(&entry.id));
        self.prune_queue_history();
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
        let ids = queue_ids.iter().copied().collect::<HashSet<_>>();
        // Validate before make_mut so an invalid request cannot even detach
        // the shared queue snapshot, let alone mutate playback state.
        let found = self
            .state
            .queue
            .entries
            .iter()
            .filter(|entry| ids.contains(&entry.id))
            .count();
        if found != ids.len() {
            let missing = ids
                .iter()
                .find(|id| {
                    !self
                        .state
                        .queue
                        .entries
                        .iter()
                        .any(|entry| entry.id == **id)
                })
                .copied()
                .expect("bulk queue validation mismatch");
            bail!("Queue entry not found: {missing}");
        }

        let queue = Arc::make_mut(&mut self.state.queue.entries);
        let mut selected = Vec::with_capacity(ids.len());
        selected.extend(queue.extract_if(.., |entry| ids.contains(&entry.id)));
        let insertion = index.min(queue.len());
        queue.splice(insertion..insertion, selected);
        Ok(())
    }

    pub(in crate::core) fn enqueue(&mut self, ids: &[i64]) -> Result<()> {
        for id in ids {
            self.track(*id)?;
        }
        let queue = Arc::make_mut(&mut self.state.queue.entries);
        for id in ids {
            queue.push(QueueEntry {
                id: self.next_queue_id,
                track_id: *id,
            });
            self.next_queue_id += 1;
        }
        self.shuffle_bag.clear();
        if !ids.is_empty() {
            self.queue_dirty = true;
        }
        Ok(())
    }
    pub(in crate::core) fn randomize_queue(&mut self, rng: &mut impl rand::Rng) {
        if self.state.queue.entries.len() > 1 {
            Arc::make_mut(&mut self.state.queue.entries).shuffle(rng);
        }
    }
    pub(in crate::core) fn deduplicate_queue(&mut self) {
        if self.state.queue.entries.len() < 2 {
            return;
        }
        let current = self.state.queue.current_id.and_then(|id| {
            self.state
                .queue
                .entries
                .iter()
                .find(|entry| entry.id == id)
                .map(|entry| (entry.id, entry.track_id))
        });
        let mut seen = std::collections::HashSet::with_capacity(self.state.queue.entries.len());
        Arc::make_mut(&mut self.state.queue.entries).retain_mut(|entry| {
            if !seen.insert(entry.track_id) {
                return false;
            }
            // Keep the current entry's identity at this track's first position,
            // even when playback had selected a later duplicate.
            if let Some((id, track_id)) = current
                && entry.track_id == track_id
            {
                entry.id = id;
            }
            true
        });
        self.prune_queue_history();
    }
    pub(in crate::core) fn prune_queue_history(&mut self) {
        let queue = &self.state.queue.entries;
        self.shuffle_bag
            .retain(|id| queue.iter().any(|entry| entry.id == *id));
        let mut index = 0;
        let mut cursor = 0;
        self.played.retain(|id| {
            let keep = queue.iter().any(|entry| entry.id == *id);
            if keep && index < self.played_cursor {
                cursor += 1;
            }
            index += 1;
            keep
        });
        self.played_cursor = cursor;
    }
}
