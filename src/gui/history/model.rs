//! History projection and transactional selection; independent of panel instances.

use super::HistorySort;
use rivu_protocol::model::HistoryEntry;
use std::{collections::HashSet, sync::Arc};

pub(in crate::gui) struct HistoryCommit {
    pub(in crate::gui) track_id: i64,
    selected: HashSet<i64>,
    anchor: Option<i64>,
    generation: u64,
    selection_generation: u64,
}

pub(in crate::gui) struct HistoryBrowser {
    source: Arc<Vec<HistoryEntry>>,
    indices: Vec<usize>,
    title_keys: Vec<String>,
    query: Option<String>,
    positions: std::collections::HashMap<i64, usize>,
    selected: HashSet<i64>,
    anchor: Option<i64>,
    generation: u64,
    selection_generation: u64,
    playback: Option<(i64, i64)>,
    sort: HistorySort,
    descending: bool,
}

impl Default for HistoryBrowser {
    fn default() -> Self {
        Self {
            source: Arc::default(),
            indices: Vec::new(),
            title_keys: Vec::new(),
            query: None,
            positions: Default::default(),
            selected: HashSet::new(),
            anchor: None,
            generation: 0,
            selection_generation: 0,
            playback: None,
            sort: HistorySort::Recent,
            descending: true,
        }
    }
}

impl HistoryBrowser {
    /// Returns whether source data or visible order changed. Playback-only
    /// updates preserve pending row actions unless they actually reorder rows.
    pub(in crate::gui) fn sync(
        &mut self,
        source: &Arc<Vec<HistoryEntry>>,
        playback: Option<(i64, i64)>,
    ) -> bool {
        let source_changed = !Arc::ptr_eq(&self.source, source);
        let previous_playback = self.playback;
        self.playback = playback;
        if !source_changed {
            return previous_playback != playback
                && self.sort == HistorySort::Recent
                && self.reorder_playback(previous_playback);
        }
        self.source = Arc::clone(source);
        self.title_keys = source
            .iter()
            .map(|entry| entry.title.to_lowercase())
            .collect();
        self.rebuild();
        self.selected.retain(|id| self.positions.contains_key(id));
        self.anchor = self.anchor.filter(|id| self.positions.contains_key(id));
        true
    }

    pub(in crate::gui) fn set_query(&mut self, query: Option<String>) {
        let query = query.map(|query| query.to_lowercase());
        self.clear_selection();
        if self.query == query {
            return;
        }
        self.query = query;
        self.rebuild();
    }

    pub(in crate::gui) fn total(&self) -> usize {
        self.source.len()
    }

    fn recent_key(entry: &HistoryEntry, playback: Option<(i64, i64)>) -> (i64, bool) {
        let current = playback.filter(|&(id, _)| id == entry.track_id);
        (
            current.map_or(entry.played_at, |(_, time)| time),
            current.is_some(),
        )
    }

    fn compare_recent_indices(&self, a: usize, b: usize) -> std::cmp::Ordering {
        let ordering = Self::recent_key(&self.source[a], self.playback)
            .cmp(&Self::recent_key(&self.source[b], self.playback));
        let ordering = if self.descending {
            ordering.reverse()
        } else {
            ordering
        };
        // Rebuild's stable sort preserves source order for equal keys.
        ordering.then_with(|| a.cmp(&b))
    }

    fn reorder_playback(&mut self, previous: Option<(i64, i64)>) -> bool {
        let mut affected = [
            previous.and_then(|(id, _)| self.positions.get(&id).copied()),
            self.playback
                .and_then(|(id, _)| self.positions.get(&id).copied()),
        ];
        if affected[0] == affected[1] {
            affected[1] = None;
        }
        // Only the old and new current rows can have changed keys. Most ticks
        // keep them between the same neighbors, without touching the projection.
        let needs_reorder = affected.iter().flatten().any(|&position| {
            let index = self.indices[position];
            (position > 0
                && self
                    .compare_recent_indices(self.indices[position - 1], index)
                    .is_gt())
                || (position + 1 < self.indices.len()
                    && self
                        .compare_recent_indices(index, self.indices[position + 1])
                        .is_gt())
        });
        if !needs_reorder {
            return false;
        }

        // Remove both changed keys before searching the otherwise sorted rows.
        // Descending positions keep the second removal's original index valid.
        affected.sort_unstable_by(|a, b| b.cmp(a));
        let mut removed = [None, None];
        let mut start = self.indices.len();
        for (slot, position) in removed.iter_mut().zip(affected) {
            if let Some(position) = position {
                start = start.min(position);
                *slot = Some(self.indices.remove(position));
            }
        }
        for index in removed.into_iter().flatten() {
            let position = self
                .indices
                .partition_point(|&other| self.compare_recent_indices(other, index).is_lt());
            self.indices.insert(position, index);
            start = start.min(position);
        }
        // Reinserting the second row can shift the first inserted row again.
        // Refresh the affected suffix rather than retaining stale intermediate positions.
        for position in start..self.indices.len() {
            self.positions
                .insert(self.source[self.indices[position]].track_id, position);
        }
        self.generation = self.generation.wrapping_add(1);
        true
    }

    fn rebuild(&mut self) {
        self.indices.clear();
        self.indices.extend((0..self.source.len()).filter(|&index| {
            self.query
                .as_ref()
                .is_none_or(|query| self.title_keys[index].contains(query))
        }));
        self.indices.sort_by(|&a, &b| {
            let ordering = match self.sort {
                HistorySort::Recent => Self::recent_key(&self.source[a], self.playback)
                    .cmp(&Self::recent_key(&self.source[b], self.playback)),
                HistorySort::Title => self.title_keys[a].cmp(&self.title_keys[b]),
                HistorySort::Plays => self.source[a].play_count.cmp(&self.source[b].play_count),
            };
            if self.descending {
                ordering.reverse()
            } else {
                ordering
            }
        });
        self.positions.clear();
        self.positions.extend(
            self.indices
                .iter()
                .enumerate()
                .map(|(position, &index)| (self.source[index].track_id, position)),
        );
        self.generation = self.generation.wrapping_add(1);
    }

    pub(in crate::gui) fn contains(&self, id: i64) -> bool {
        self.selected.contains(&id)
    }

    pub(in crate::gui) fn generation(&self) -> u64 {
        self.generation
    }

    pub(in crate::gui) fn prepare_selection(
        &self,
        id: i64,
        shift: bool,
        additive: bool,
    ) -> Option<HistoryCommit> {
        let &position = self.positions.get(&id)?;
        let mut selected = if additive {
            self.selected.clone()
        } else {
            HashSet::new()
        };
        let anchor = self.anchor.filter(|id| self.positions.contains_key(id));
        if shift {
            let start = anchor.map_or(position, |id| self.positions[&id]);
            selected.extend(
                self.indices[start.min(position)..=start.max(position)]
                    .iter()
                    .map(|&index| self.source[index].track_id),
            );
        } else if !additive || !selected.remove(&id) {
            selected.insert(id);
        }
        Some(HistoryCommit {
            track_id: id,
            selected,
            anchor: if shift { anchor.or(Some(id)) } else { Some(id) },
            generation: self.generation,
            selection_generation: self.selection_generation,
        })
    }

    pub(in crate::gui) fn commit_valid(&self, commit: &HistoryCommit) -> bool {
        commit.generation == self.generation
            && commit.selection_generation == self.selection_generation
            && self.positions.contains_key(&commit.track_id)
    }

    pub(in crate::gui) fn commit_selection(&mut self, commit: HistoryCommit) -> bool {
        if !self.commit_valid(&commit) {
            return false;
        }
        self.selected = commit.selected;
        self.anchor = commit.anchor;
        self.selection_generation = self.selection_generation.wrapping_add(1);
        true
    }

    pub(in crate::gui) fn select_all(&mut self) {
        self.selected.clear();
        self.selected.extend(self.positions.keys().copied());
        self.anchor = None;
        self.selection_generation = self.selection_generation.wrapping_add(1);
    }

    pub(in crate::gui) fn clear_selection(&mut self) {
        self.selected.clear();
        self.anchor = None;
        self.selection_generation = self.selection_generation.wrapping_add(1);
    }

    pub(in crate::gui) fn ordered_selection(&self) -> Vec<i64> {
        self.indices
            .iter()
            .map(|&index| self.source[index].track_id)
            .filter(|id| self.selected.contains(id))
            .collect()
    }

    pub(in crate::gui) fn drag_ids(&self, id: i64, generation: u64) -> Vec<i64> {
        if generation != self.generation || !self.positions.contains_key(&id) {
            return Vec::new();
        }
        if self.contains(id) {
            self.ordered_selection()
        } else {
            vec![id]
        }
    }

    pub(in crate::gui) fn len(&self) -> usize {
        self.indices.len()
    }

    pub(in crate::gui) fn get(&self, index: usize) -> Option<&HistoryEntry> {
        self.indices.get(index).map(|&index| &self.source[index])
    }

    pub(in crate::gui) fn sort(&self) -> HistorySort {
        self.sort
    }
    pub(in crate::gui) fn descending(&self) -> bool {
        self.descending
    }
    pub(in crate::gui) fn set_sort(&mut self, sort: HistorySort) {
        if self.sort == sort {
            self.descending = !self.descending;
        } else {
            self.sort = sort;
            self.descending = matches!(sort, HistorySort::Recent | HistorySort::Plays);
        }
        self.rebuild();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history_selection_source(ids: &[i64]) -> Arc<Vec<HistoryEntry>> {
        Arc::new(
            ids.iter()
                .map(|&id| HistoryEntry {
                    track_id: id,
                    title: format!("track {}", 10 - id),
                    play_count: id as u64,
                    played_at: 100 - id,
                    favorite: false,
                })
                .collect(),
        )
    }

    fn select_history(history: &mut HistoryBrowser, id: i64, shift: bool, additive: bool) {
        let commit = history.prepare_selection(id, shift, additive).unwrap();
        assert!(history.commit_selection(commit));
    }

    #[test]
    fn history_search_rebuilds_cached_matches_and_keeps_identity_and_sort() {
        let mut history = HistoryBrowser::default();
        history.sync(&history_selection_source(&[1, 2, 3, 4]), None);
        history.select_all();
        let old_commit = history.prepare_selection(2, false, false).unwrap();
        history.set_query(Some("TRACK 8".into()));
        assert_eq!(history.total(), 4);
        assert_eq!(history.len(), 1);
        assert_eq!(history.get(0).unwrap().track_id, 2);
        assert!(history.ordered_selection().is_empty());
        assert!(!history.commit_selection(old_commit));
        history.select_all();
        assert_eq!(history.ordered_selection(), [2]);
        history.set_sort(HistorySort::Title);
        assert_eq!(history.ordered_selection(), [2]);
        let previous_match = history.prepare_selection(2, false, false).unwrap();
        let mut updated = history_selection_source(&[4, 3, 2, 1]);
        let updated_rows = Arc::make_mut(&mut updated);
        updated_rows[0].title = "Track 8 remastered".into();
        updated_rows[2].title = "Different title".into();
        history.sync(&updated, None);
        assert_eq!(history.len(), 1);
        assert_eq!(history.get(0).unwrap().track_id, 4);
        assert!(history.ordered_selection().is_empty());
        assert!(!history.commit_selection(previous_match));
        history.select_all();
        assert_eq!(history.ordered_selection(), [4]);
        history.sync(&history_selection_source(&[1, 3, 4]), None);
        assert_eq!(history.len(), 0);
        assert_eq!(history.total(), 3);
        assert!(history.ordered_selection().is_empty());
        history.set_query(None);
        assert_eq!(history.len(), 3);
        assert_eq!(history.get(0).unwrap().track_id, 4);
        assert!(history.ordered_selection().is_empty());
    }

    #[test]
    fn history_selection_ranges_follow_visible_order() {
        let mut history = HistoryBrowser::default();
        history.sync(&history_selection_source(&[1, 2, 3, 4]), None);
        select_history(&mut history, 2, false, false);
        select_history(&mut history, 4, true, false);
        assert_eq!(history.ordered_selection(), [2, 3, 4]);
        select_history(&mut history, 1, false, true);
        assert_eq!(history.ordered_selection(), [1, 2, 3, 4]);
        select_history(&mut history, 3, false, true);
        assert_eq!(history.ordered_selection(), [1, 2, 4]);
        select_history(&mut history, 4, true, true);
        assert_eq!(history.ordered_selection(), [1, 2, 3, 4]);
        history.set_sort(HistorySort::Title);
        assert_eq!(history.ordered_selection(), [4, 3, 2, 1]);
        select_history(&mut history, 2, false, false);
        select_history(&mut history, 4, true, false);
        assert_eq!(history.ordered_selection(), [4, 3, 2]);
    }

    #[test]
    fn history_prepared_selection_is_atomic_and_stale_projection_cannot_commit() {
        let mut history = HistoryBrowser::default();
        let source = history_selection_source(&[1, 2, 3]);
        history.sync(&source, None);
        select_history(&mut history, 1, false, false);
        let canceled = history.prepare_selection(3, true, false).unwrap();
        assert_eq!(history.ordered_selection(), [1]);
        drop(canceled);
        assert_eq!(history.ordered_selection(), [1]);
        let old_sort = history.prepare_selection(2, false, false).unwrap();
        history.set_sort(HistorySort::Title);
        assert!(!history.commit_valid(&old_sort));
        assert!(!history.commit_selection(old_sort));
        let old_source = history.prepare_selection(3, false, false).unwrap();
        history.sync(&history_selection_source(&[1, 2]), None);
        assert!(!history.commit_selection(old_source));
        assert_eq!(history.ordered_selection(), [1]);
        assert!(history.prepare_selection(3, false, false).is_none());
    }

    #[test]
    fn history_selection_changes_and_clear_reject_pending_commits() {
        let mut history = HistoryBrowser::default();
        history.sync(&history_selection_source(&[1, 2, 3]), None);
        select_history(&mut history, 1, false, false);
        let old_selection = history.prepare_selection(2, true, false).unwrap();
        select_history(&mut history, 3, false, true);
        assert!(!history.commit_selection(old_selection));
        assert_eq!(history.ordered_selection(), [1, 3]);
        let before_all = history.prepare_selection(2, false, false).unwrap();
        history.select_all();
        assert!(!history.commit_selection(before_all));
        let before_clear = history.prepare_selection(2, false, false).unwrap();
        history.clear_selection();
        assert!(history.ordered_selection().is_empty());
        assert!(history.anchor.is_none());
        assert!(!history.commit_selection(before_clear));
        assert!(history.ordered_selection().is_empty());
        assert!(history.anchor.is_none());
    }

    #[test]
    fn history_refresh_prunes_removed_ids_and_select_all_uses_current_projection() {
        let mut history = HistoryBrowser::default();
        history.sync(&history_selection_source(&[1, 2, 3, 4]), None);
        select_history(&mut history, 2, false, false);
        select_history(&mut history, 4, false, true);
        history.set_sort(HistorySort::Title);
        history.sync(&history_selection_source(&[4, 3, 1]), None);
        assert_eq!(history.ordered_selection(), [4]);
        history.select_all();
        assert_eq!(history.ordered_selection(), [4, 3, 1]);
        history.sync(&Arc::default(), None);
        assert!(history.ordered_selection().is_empty());
        assert!(history.anchor.is_none());
    }

    #[test]
    fn history_drag_freezes_current_group_without_including_unselected_rows() {
        let mut history = HistoryBrowser::default();
        let source = history_selection_source(&[1, 2, 3, 4]);
        history.sync(&source, None);
        let rendered_generation = history.generation();
        select_history(&mut history, 2, false, false);
        select_history(&mut history, 4, false, true);
        // Selection can change after rendering but before the actual drag begins.
        let frozen = history.drag_ids(4, rendered_generation);
        assert_eq!(frozen, [2, 4]);
        assert_eq!(history.drag_ids(3, rendered_generation), [3]);
        assert_eq!(history.ordered_selection(), [2, 4]);
        history.set_sort(HistorySort::Title);
        assert!(history.drag_ids(4, rendered_generation).is_empty());
        assert_eq!(history.drag_ids(4, history.generation()), [4, 2]);
        assert_eq!(frozen, [2, 4]);
        let sorted_generation = history.generation();
        history.sync(&history_selection_source(&[1, 2, 3]), None);
        assert!(history.drag_ids(4, sorted_generation).is_empty());
        assert!(history.drag_ids(4, history.generation()).is_empty());
    }

    #[test]
    fn history_playback_reorder_preserves_selection_but_invalidates_pending_actions() {
        let mut history = HistoryBrowser::default();
        let source = history_selection_source(&[1, 2, 3]);
        history.sync(&source, None);
        history.select_all();
        let generation = history.generation();
        let commit = history.prepare_selection(2, false, false).unwrap();
        assert!(history.sync(&source, Some((3, 200))));
        assert_eq!(history.generation(), generation.wrapping_add(1));
        assert_eq!(history.ordered_selection(), [3, 1, 2]);
        assert!(!history.commit_selection(commit));
        assert!(history.drag_ids(1, generation).is_empty());
        assert_eq!(history.drag_ids(1, history.generation()), [3, 1, 2]);

        let generation = history.generation();
        let commit = history.prepare_selection(2, false, false).unwrap();
        assert!(!history.sync(&source, Some((3, 200))));
        assert!(!history.sync(&source, Some((3, 201))));
        assert_eq!(history.generation(), generation);
        assert_eq!(history.ordered_selection(), [3, 1, 2]);
        assert_eq!(history.drag_ids(1, generation), [3, 1, 2]);
        assert!(history.commit_selection(commit));
        assert_eq!(history.ordered_selection(), [2]);
    }

    #[test]
    fn history_live_timestamp_changes_keep_non_recent_row_actions_valid() {
        for sort in [HistorySort::Title, HistorySort::Plays] {
            let mut history = HistoryBrowser::default();
            let source = history_selection_source(&[1, 2, 3]);
            history.sync(&source, Some((3, 200)));
            history.set_sort(sort);
            select_history(&mut history, 1, false, false);
            select_history(&mut history, 3, false, true);
            let generation = history.generation();
            let selected = history.ordered_selection();
            let commit = history.prepare_selection(2, false, false).unwrap();

            // Neither timestamp nor current-ID changes affect these projections.
            assert!(!history.sync(&source, Some((3, 201))));
            assert!(!history.sync(&source, Some((1, 202))));
            assert!(!history.sync(&source, None));
            assert_eq!(history.generation(), generation);
            assert_eq!(history.drag_ids(1, generation), selected);
            assert!(history.commit_selection(commit));
            assert_eq!(history.ordered_selection(), [2]);
            assert!(!history.sync(&source, None));
            assert_eq!(history.drag_ids(2, generation), [2]);
        }
    }

    #[test]
    fn history_same_order_current_changes_preserve_anchor_selection_and_drag() {
        let source = history_selection_source(&[1, 2, 3]);
        let mut history = HistoryBrowser::default();
        history.sync(&source, None);
        select_history(&mut history, 1, false, false);
        let generation = history.generation();
        let commit = history.prepare_selection(3, true, false).unwrap();
        for playback in [Some((1, 100)), Some((2, 98)), None, Some((99, 200))] {
            assert!(!history.sync(&source, playback));
            assert_eq!(history.generation(), generation);
            assert_eq!(history.anchor, Some(1));
            assert_eq!(history.drag_ids(1, generation), [1]);
            assert!(history.commit_valid(&commit));
        }
        assert!(history.commit_selection(commit));
        assert_eq!(history.ordered_selection(), [1, 2, 3]);
    }

    #[test]
    fn history_incremental_recent_order_matches_rebuild_for_ties_filters_and_directions() {
        let mut source = history_selection_source(&[4, 2, 1, 3]);
        for (entry, time) in Arc::make_mut(&mut source).iter_mut().zip([98, 98, 99, 97]) {
            entry.played_at = time;
            if entry.track_id != 1 {
                entry.title.push_str(" match");
            }
        }
        let mut playbacks = vec![None, Some((99, 200))];
        for id in [1, 2, 3, 4] {
            for time in [96, 97, 98, 99, 100] {
                playbacks.push(Some((id, time)));
            }
        }
        for descending in [true, false] {
            for query in [None, Some("match"), Some("track 9"), Some("missing")] {
                let projection = |playback| {
                    let mut history = HistoryBrowser::default();
                    history.sync(&source, playback);
                    history.set_query(query.map(str::to_owned));
                    if !descending {
                        history.set_sort(HistorySort::Recent);
                    }
                    history
                };
                for &previous in &playbacks {
                    for &next in &playbacks {
                        let mut history = projection(previous);
                        history.select_all();
                        let old_indices = history.indices.clone();
                        let generation = history.generation();
                        let mut rebuilt = projection(next);
                        rebuilt.select_all();
                        let changed = old_indices != rebuilt.indices;

                        assert_eq!(history.sync(&source, next), changed);
                        assert_eq!(history.indices, rebuilt.indices);
                        assert_eq!(history.positions, rebuilt.positions);
                        assert_eq!(history.ordered_selection(), rebuilt.ordered_selection());
                        assert_eq!(
                            history.generation(),
                            generation.wrapping_add(u64::from(changed))
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn history_source_replacement_refreshes_fields_and_prunes_selection() {
        let source = history_selection_source(&[1, 2, 3]);
        let mut history = HistoryBrowser::default();
        history.sync(&source, Some((3, 200)));
        select_history(&mut history, 1, false, false);
        select_history(&mut history, 3, false, true);
        let generation = history.generation();
        let commit = history.prepare_selection(2, false, false).unwrap();
        let mut replacement = history_selection_source(&[1, 2]);
        let entry = &mut Arc::make_mut(&mut replacement)[0];
        entry.title = "Updated title".into();
        entry.play_count = 42;
        entry.played_at = 300;
        entry.favorite = true;

        assert!(history.sync(&replacement, Some((3, 200))));
        assert_eq!(history.generation(), generation.wrapping_add(1));
        assert!(!history.commit_selection(commit));
        assert!(history.drag_ids(1, generation).is_empty());
        assert_eq!(history.ordered_selection(), [1]);
        assert_eq!(history.anchor, None);
        let entry = history.get(0).unwrap();
        assert_eq!(entry.track_id, 1);
        assert_eq!(entry.title, "Updated title");
        assert_eq!(entry.play_count, 42);
        assert_eq!(entry.played_at, 300);
        assert!(entry.favorite);
        history.set_query(Some("updated".into()));
        assert_eq!(history.len(), 1);
        assert_eq!(history.get(0).unwrap().track_id, 1);
    }

    #[test]
    fn history_sort_survives_reopening_and_tracks_source_replacement() {
        let source = Arc::new(vec![
            HistoryEntry {
                track_id: 1,
                title: "beta".into(),
                play_count: 2,
                played_at: 30,
                favorite: false,
            },
            HistoryEntry {
                track_id: 2,
                title: "ALPHA".into(),
                play_count: 4,
                played_at: 10,
                favorite: true,
            },
            HistoryEntry {
                track_id: 3,
                title: "alpha".into(),
                play_count: 1,
                played_at: 20,
                favorite: false,
            },
        ]);
        let mut history = HistoryBrowser::default();
        history.sync(&source, None);
        let ids = |history: &HistoryBrowser| {
            (0..history.len())
                .map(|index| history.get(index).unwrap().track_id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&history), [1, 3, 2]);
        history.set_sort(HistorySort::Title);
        assert_eq!(ids(&history), [2, 3, 1]);
        history.sync(&source, None);
        assert_eq!(ids(&history), [2, 3, 1]);
        history.set_sort(HistorySort::Title);
        assert_eq!(ids(&history), [1, 2, 3]);
        history.set_sort(HistorySort::Plays);
        assert_eq!(ids(&history), [2, 1, 3]);
        let replacement = Arc::new(vec![source[2].clone(), source[0].clone()]);
        history.sync(&replacement, None);
        assert_eq!(ids(&history), [1, 3]);
        history.sync(&Arc::default(), None);
        assert_eq!(history.len(), 0);
    }

    #[test]
    fn history_recency_uses_live_playback_time_without_replacing_history() {
        let source = Arc::new(vec![
            HistoryEntry {
                track_id: 1,
                title: "alpha".into(),
                play_count: 2,
                played_at: 30,
                favorite: false,
            },
            HistoryEntry {
                track_id: 2,
                title: "beta".into(),
                play_count: 1,
                played_at: 10,
                favorite: false,
            },
        ]);
        let mut history = HistoryBrowser::default();
        let ids = |history: &HistoryBrowser| {
            (0..history.len())
                .map(|index| history.get(index).unwrap().track_id)
                .collect::<Vec<_>>()
        };
        history.sync(&source, Some((2, 40)));
        assert_eq!(ids(&history), [2, 1]);
        // Equal-second timestamps still put the current track first in descending order.
        history.sync(&source, Some((2, 30)));
        assert_eq!(ids(&history), [2, 1]);
        history.set_sort(HistorySort::Recent);
        assert_eq!(ids(&history), [1, 2]);
        history.sync(&source, Some((2, 20)));
        assert_eq!(ids(&history), [2, 1]);
        history.set_sort(HistorySort::Recent);
        assert_eq!(ids(&history), [1, 2]);
        history.sync(&source, Some((1, 5)));
        assert_eq!(ids(&history), [2, 1]);
        history.sync(&source, None);
        assert_eq!(ids(&history), [1, 2]);
        history.set_sort(HistorySort::Title);
        history.sync(&source, Some((2, 40)));
        assert_eq!(ids(&history), [1, 2]);
        history.set_sort(HistorySort::Plays);
        assert_eq!(ids(&history), [1, 2]);
    }
}
