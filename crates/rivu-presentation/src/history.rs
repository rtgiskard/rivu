use rivu_protocol::model::{HistoryEntry, PlaybackStatus};
use std::cmp::Ordering;

/// Live activity participates in Recent while playing or paused, never stopped.
pub fn playback_activity(
    status: PlaybackStatus,
    track_id: Option<i64>,
    last_heard_at: Option<i64>,
) -> Option<(i64, i64)> {
    (status != PlaybackStatus::Stopped)
        .then(|| track_id.zip(last_heard_at))
        .flatten()
}

/// Activity time followed by a live-row tie break. Stored history is unchanged.
pub fn activity_key(entry: &HistoryEntry, playback: Option<(i64, i64)>) -> (i64, bool) {
    let current = playback.filter(|&(id, _)| id == entry.track_id);
    (
        current.map_or(entry.played_at, |(_, time)| time),
        current.is_some(),
    )
}

/// Equal activity keys retain source order in either sort direction.
pub fn compare_recent(
    source: &[HistoryEntry],
    playback: Option<(i64, i64)>,
    descending: bool,
    a: usize,
    b: usize,
) -> Ordering {
    let ordering = activity_key(&source[a], playback).cmp(&activity_key(&source[b], playback));
    let ordering = if descending {
        ordering.reverse()
    } else {
        ordering
    };
    ordering.then_with(|| a.cmp(&b))
}

/// Reinsert at most two changed rows into an otherwise sorted projection.
/// Returns the first changed position, for owners that maintain a position map.
pub fn reorder_changed_rows(
    indices: &mut Vec<usize>,
    mut affected: [Option<usize>; 2],
    compare: impl Fn(usize, usize) -> Ordering,
) -> Option<usize> {
    if affected[0] == affected[1] {
        affected[1] = None;
    }
    // Ordinary playback ticks need not touch the projection or its allocation.
    let needs_reorder = affected.iter().flatten().any(|&position| {
        (position > 0 && compare(indices[position - 1], indices[position]).is_gt())
            || (position + 1 < indices.len()
                && compare(indices[position], indices[position + 1]).is_gt())
    });
    if !needs_reorder {
        return None;
    }
    // Both changed keys must be removed before searching the sorted remainder.
    affected.sort_unstable_by(|a, b| b.cmp(a));
    let mut removed = [None, None];
    let mut start = indices.len();
    for (slot, position) in removed.iter_mut().zip(affected) {
        if let Some(position) = position {
            start = start.min(position);
            *slot = Some(indices.remove(position));
        }
    }
    for index in removed.into_iter().flatten() {
        let position = indices.partition_point(|&other| compare(other, index).is_lt());
        indices.insert(position, index);
        start = start.min(position);
    }
    Some(start)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_activity_overrides_history_and_breaks_ties_before_source_order() {
        use PlaybackStatus::{Paused, Playing, Stopped};
        let history = [90, 4, 60].map(|track_id| HistoryEntry {
            track_id,
            played_at: 20,
            title: String::new(),
            play_count: 1,
            favorite: false,
        });
        for (status, id, heard, time, order) in [
            (Playing, Some(60), Some(20), 20, [2, 0, 1]),
            (Paused, Some(60), Some(10), 10, [0, 1, 2]),
            (Paused, Some(60), Some(30), 30, [2, 0, 1]),
            (Stopped, Some(60), Some(30), 20, [0, 1, 2]),
            (Playing, None, Some(30), 20, [0, 1, 2]),
            (Playing, Some(60), None, 20, [0, 1, 2]),
        ] {
            let activity = playback_activity(status, id, heard);
            assert_eq!(activity_key(&history[2], activity).0, time);
            let mut indices = [0, 1, 2];
            indices.sort_by(|&a, &b| compare_recent(&history, activity, true, a, b));
            assert_eq!(indices, order);
        }
    }
}
