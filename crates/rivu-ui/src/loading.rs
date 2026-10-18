use std::{
    collections::HashMap,
    hash::Hash,
    time::{Duration, Instant},
};

use gpui::{Context, Task};

struct PendingText {
    token: u64,
    ready_at: Instant,
    visible: bool,
}

/// Presentation only: request owners retain all demand, validity and worker gates.
/// A view has one cancellable wake for its earliest deadline, never one per row.
pub struct LoadingText<K> {
    delay: Duration,
    pending: HashMap<K, PendingText>,
    timer: Option<Task<()>>,
    wake_at: Option<Instant>,
}

impl<K> LoadingText<K> {
    pub fn new(delay: Duration) -> Self {
        Self {
            delay,
            pending: HashMap::new(),
            timer: None,
            wake_at: None,
        }
    }
}

impl<K: Eq + Hash + 'static> LoadingText<K> {
    pub fn begin(&mut self, owner: K, token: u64, now: Instant) {
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
                ready_at: now + self.delay,
                visible: false,
            },
        );
    }

    pub fn finish(&mut self, owner: &K, token: u64) {
        if self
            .pending
            .get(owner)
            .is_some_and(|pending| pending.token == token)
        {
            self.pending.remove(owner);
        }
    }

    pub fn retain(&mut self, mut current: impl FnMut(&K, u64) -> bool) {
        self.pending
            .retain(|owner, pending| current(owner, pending.token));
    }

    pub fn clear(&mut self) {
        self.pending.clear();
        self.timer = None;
        self.wake_at = None;
    }

    pub fn show(&self, owner: &K, failed: bool) -> bool {
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

    pub fn schedule<T: 'static>(
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

    const LOADING_TEXT_DELAY: Duration = Duration::from_millis(150);

    #[test]
    fn fast_completion_and_errors_do_not_wait_for_loading_prose() {
        let start = Instant::now();
        let mut text = LoadingText::new(LOADING_TEXT_DELAY);
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
        let mut text = LoadingText::new(LOADING_TEXT_DELAY);
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
        let mut text = LoadingText::new(LOADING_TEXT_DELAY);
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

use crate::{caption, list_row};
use gpui::{Div, ElementId, SharedString, Stateful, prelude::*};
/// Page placeholder presentation; request identity and retry gating remain with the owner.
pub fn paged_loading_row<T: 'static>(
    id: impl Into<ElementId>,
    error: Option<&str>,
    show: bool,
    label: SharedString,
    cx: &mut gpui::Context<T>,
    retry: impl Fn(&mut T, &mut gpui::Window, &mut gpui::Context<T>) + 'static,
) -> Stateful<Div> {
    let theme = crate::theme::palette(cx);
    list_row(theme, id, false)
        .child(caption(theme, label).when(!show, |text| text.invisible()))
        .when_some(
            error.map(|error| SharedString::from(error.to_owned())),
            |row, error| {
                row.tooltip(move |_, cx| {
                    cx.new(|_| crate::ButtonTooltip {
                        text: error.clone(),
                    })
                    .into()
                })
                .on_click(cx.listener(move |this, _, window, cx| retry(this, window, cx)))
            },
        )
}

#[cfg(test)]
mod row_tests {
    use super::*;
    use gpui::{Context, InputEvent, Render, TestAppContext, Window, div};

    struct LoadingRow {
        error: Option<&'static str>,
        show: bool,
        retries: usize,
    }

    impl Render for LoadingRow {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(
                paged_loading_row(
                    ("loading-row", 0usize),
                    self.error,
                    self.show,
                    if self.error.is_some() {
                        "Loading failed".into()
                    } else {
                        "Loading".into()
                    },
                    cx,
                    |this, _, cx| {
                        this.retries += 1;
                        cx.notify();
                    },
                )
                .w(gpui::px(240.))
                .h(gpui::rems(crate::LIST_ROW_REMS)),
            )
        }
    }

    #[gpui::test]
    fn placeholder_retry_listener_tracks_rendered_error(cx: &mut TestAppContext) {
        cx.update(crate::theme::init_test);
        let window = cx.add_window(|_, _| LoadingRow {
            error: None,
            show: false,
            retries: 0,
        });
        let row = window.root(cx).unwrap();
        let handle = window.into();
        for (error, show, retries) in [
            (None, false, 0),
            (None, true, 0),
            (Some("Page failed"), true, 1),
            (None, false, 1),
            (None, true, 1),
            (Some("Another failure"), true, 2),
        ] {
            row.update(cx, |row, cx| {
                row.error = error;
                row.show = show;
                cx.notify();
            });
            cx.run_until_parked();
            cx.update_window(handle, |_, window, cx| {
                window.draw(cx).clear(cx);
                let position = gpui::point(gpui::px(12.), gpui::px(12.));
                window.dispatch_event(
                    gpui::MouseDownEvent {
                        button: gpui::MouseButton::Left,
                        position,
                        ..Default::default()
                    }
                    .to_platform_input(),
                    cx,
                );
                window.dispatch_event(
                    gpui::MouseUpEvent {
                        button: gpui::MouseButton::Left,
                        position,
                        ..Default::default()
                    }
                    .to_platform_input(),
                    cx,
                );
            })
            .unwrap();
            assert_eq!(row.read_with(cx, |row, _| row.retries), retries);
        }
    }
}
