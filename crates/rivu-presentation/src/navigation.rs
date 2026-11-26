use rivu_protocol::model::{PlaybackState, QueueState, RepeatMode};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Navigation {
    pub previous: bool,
    pub next: bool,
}

impl Navigation {
    pub fn availability(queue: &QueueState, playback: &PlaybackState) -> Self {
        if queue.total == 0 {
            return Self::default();
        }
        // Only Core knows the shuffle history. Keep unknown directions available.
        Self {
            previous: playback.shuffle || queue.current_position.is_none_or(|index| index > 0),
            next: playback.shuffle
                || playback.repeat == RepeatMode::All
                || queue
                    .current_position
                    .is_none_or(|index| index < queue.total - 1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_at_queue_boundaries() {
        for (total, current_position, shuffle, repeat, previous, next) in [
            (0, None, true, RepeatMode::All, false, false),
            (1, None, false, RepeatMode::Off, true, true),
            (1, Some(0), false, RepeatMode::One, false, false),
            (2, Some(0), false, RepeatMode::All, false, true),
            (2, Some(1), false, RepeatMode::Off, true, false),
            (2, Some(1), false, RepeatMode::All, true, true),
            (1, Some(0), true, RepeatMode::Off, true, true),
        ] {
            let queue = QueueState {
                total,
                current_position,
                ..Default::default()
            };
            let playback = PlaybackState {
                shuffle,
                repeat,
                ..Default::default()
            };
            assert_eq!(
                Navigation::availability(&queue, &playback),
                Navigation { previous, next }
            );
        }
    }
}
