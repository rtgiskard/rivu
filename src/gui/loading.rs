use std::{
    collections::HashMap,
    hash::Hash,
    time::{Duration, Instant},
};

use gpui::{Context, Task};

const LOADING_TEXT_DELAY: Duration = Duration::from_millis(150);

struct PendingText {
    token: u64,
    ready_at: Instant,
    visible: bool,
}

/// Presentation only: request owners retain all demand, validity and worker gates.
/// A view has one cancellable wake for its earliest deadline, never one per row.
pub(super) struct LoadingText<K> {
    pending: HashMap<K, PendingText>,
    timer: Option<Task<()>>,
    wake_at: Option<Instant>,
}

impl<K> Default for LoadingText<K> {
    fn default() -> Self {
        Self {
            pending: HashMap::new(),
            timer: None,
            wake_at: None,
        }
    }
}

impl<K: Eq + Hash + 'static> LoadingText<K> {
    pub(super) fn begin(&mut self, owner: K, token: u64, now: Instant) {
        if self
            .pending
            .get(&owner)
            .is_some_and(|pending| pending.token == token)
        {
            return;
        }
        self.pending.insert(
            owner,
            PendingText {
                token,
                ready_at: now + LOADING_TEXT_DELAY,
                visible: false,
            },
        );
    }

    pub(super) fn finish(&mut self, owner: &K, token: u64) {
        if self
            .pending
            .get(owner)
            .is_some_and(|pending| pending.token == token)
        {
            self.pending.remove(owner);
        }
    }

    pub(super) fn retain(&mut self, mut current: impl FnMut(&K, u64) -> bool) {
        self.pending
            .retain(|owner, pending| current(owner, pending.token));
    }

    pub(super) fn clear(&mut self) {
        self.pending.clear();
        self.timer = None;
        self.wake_at = None;
    }

    pub(super) fn show(&self, owner: &K, failed: bool) -> bool {
        failed
            || self
                .pending
                .get(owner)
                .is_some_and(|pending| pending.visible)
    }

    fn advance(&mut self, now: Instant) -> Option<Instant> {
        let mut next = None;
        for pending in self.pending.values_mut() {
            if pending.ready_at <= now {
                pending.visible = true;
            } else {
                next = Some(next.map_or(pending.ready_at, |deadline: Instant| {
                    deadline.min(pending.ready_at)
                }));
            }
        }
        next
    }

    pub(super) fn schedule<T: 'static>(
        &mut self,
        cx: &mut Context<T>,
        locate: impl Fn(&mut T) -> &mut Self + Clone + 'static,
    ) {
        let next = self
            .pending
            .values()
            .filter(|pending| !pending.visible)
            .map(|pending| pending.ready_at)
            .min();
        if self.wake_at == next {
            return;
        }
        self.timer = None;
        self.wake_at = None;
        if next.is_none() {
            return;
        }
        let now = Instant::now();
        let Some(deadline) = self.advance(now) else {
            return;
        };
        self.wake_at = Some(deadline);
        let timer = cx
            .background_executor()
            .timer(deadline.saturating_duration_since(now));
        self.timer = Some(cx.spawn(async move |this, cx| {
            timer.await;
            let _ = this.update(cx, |this, cx| {
                let loading = locate(this);
                if loading.wake_at != Some(deadline) {
                    return;
                }
                loading.timer = None;
                loading.wake_at = None;
                loading.schedule(cx, locate.clone());
                cx.notify();
            });
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_completion_and_errors_do_not_wait_for_loading_prose() {
        let start = Instant::now();
        let mut text = LoadingText::default();
        text.begin("rows", 1, start);
        assert!(!text.show(&"rows", false));
        assert!(text.show(&"rows", true));
        text.finish(&"rows", 1);
        assert_eq!(text.advance(start + LOADING_TEXT_DELAY), None);
        assert!(!text.show(&"rows", false));
        assert!(text.show(&"rows", true));
    }

    #[test]
    fn supersession_restarts_delay_and_obsolete_completion_cannot_cancel_it() {
        let start = Instant::now();
        let mut text = LoadingText::default();
        text.begin("rows", 1, start);
        text.begin("rows", 2, start + Duration::from_millis(100));
        text.finish(&"rows", 1);
        assert_eq!(
            text.advance(start + LOADING_TEXT_DELAY),
            Some(start + Duration::from_millis(250))
        );
        assert!(!text.show(&"rows", false));
        // Re-dispatching the same logical request must not perpetually reset its clock.
        text.begin("rows", 2, start + Duration::from_millis(200));
        assert_eq!(text.advance(start + Duration::from_millis(250)), None);
        assert!(text.show(&"rows", false));
    }

    #[test]
    fn owners_share_only_the_earliest_wake_and_hiding_cancels_remaining_work() {
        let start = Instant::now();
        let mut text = LoadingText::default();
        text.begin("catalog", 1, start);
        text.begin("entries", 2, start + Duration::from_millis(50));
        assert_eq!(text.advance(start), Some(start + LOADING_TEXT_DELAY));
        assert_eq!(
            text.advance(start + LOADING_TEXT_DELAY),
            Some(start + Duration::from_millis(200))
        );
        assert!(text.show(&"catalog", false));
        assert!(!text.show(&"entries", false));
        text.retain(|_, _| false);
        assert_eq!(text.advance(start + Duration::from_secs(1)), None);
        assert!(!text.show(&"catalog", false));
        assert!(!text.show(&"entries", false));
    }
}
