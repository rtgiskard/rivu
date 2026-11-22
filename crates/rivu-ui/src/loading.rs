use std::{
    collections::HashMap,
    hash::Hash,
    time::{Duration, Instant},
};

use gpui::Context;

use crate::deadline::DeadlineWake;

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
    wake: DeadlineWake,
}

impl<K> LoadingText<K> {
    pub fn new(delay: Duration) -> Self {
        Self {
            delay,
            pending: HashMap::new(),
            wake: DeadlineWake::default(),
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
        self.wake.cancel();
    }

    pub fn show(&self, owner: &K, failed: bool) -> bool {
        failed
            || self
                .pending
                .get(owner)
                .is_some_and(|pending| pending.visible)
    }

    fn advance(&mut self, now: Instant) -> (Option<Instant>, bool) {
        let mut next = None;
        let mut changed = false;
        for pending in self.pending.values_mut() {
            if pending.visible {
                continue;
            }
            if pending.ready_at <= now {
                pending.visible = true;
                changed = true;
            } else {
                next = Some(next.map_or(pending.ready_at, |deadline: Instant| {
                    deadline.min(pending.ready_at)
                }));
            }
        }
        (next, changed)
    }

    pub fn schedule<T: 'static>(
        &mut self,
        cx: &mut Context<T>,
        locate: impl Fn(&mut T) -> &mut Self + 'static,
    ) {
        let now = cx.background_executor().now();
        let next = self
            .pending
            .values()
            .filter(|pending| !pending.visible)
            .map(|pending| pending.ready_at)
            .min();
        if self.wake.is_armed_for(next) {
            return;
        }
        let next = if next.is_some_and(|at| at <= now) {
            self.advance(now).0
        } else {
            next
        };
        self.arm(next, cx, locate);
    }

    fn arm<T: 'static>(
        &mut self,
        next: Option<Instant>,
        cx: &mut Context<T>,
        locate: impl Fn(&mut T) -> &mut Self + 'static,
    ) {
        self.wake.arm(next, cx, move |this, now, cx| {
            let loading = locate(this);
            let (next, changed) = loading.advance(now);
            loading.arm(next, cx, locate);
            if changed {
                cx.notify();
            }
        });
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
        assert_eq!(text.advance(start + LOADING_TEXT_DELAY), (None, false));
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
            (Some(start + Duration::from_millis(250)), false)
        );
        assert!(!text.show(&"rows", false));
        // Re-dispatching the same logical request must not perpetually reset its clock.
        text.begin("rows", 2, start + Duration::from_millis(200));
        assert_eq!(
            text.advance(start + Duration::from_millis(250)),
            (None, true)
        );
        assert!(text.show(&"rows", false));
    }

    #[test]
    fn owners_share_only_the_earliest_wake_and_hiding_cancels_remaining_work() {
        let start = Instant::now();
        let mut text = LoadingText::new(LOADING_TEXT_DELAY);
        text.begin("catalog", 1, start);
        text.begin("entries", 2, start + Duration::from_millis(50));
        assert_eq!(
            text.advance(start),
            (Some(start + LOADING_TEXT_DELAY), false)
        );
        assert_eq!(
            text.advance(start + LOADING_TEXT_DELAY),
            (Some(start + Duration::from_millis(200)), true)
        );
        assert!(text.show(&"catalog", false));
        assert!(!text.show(&"entries", false));
        text.retain(|_, _| false);
        assert_eq!(text.advance(start + Duration::from_secs(1)), (None, false));
        assert!(!text.show(&"catalog", false));
        assert!(!text.show(&"entries", false));
    }
}

use crate::{
    caption,
    control::{IconButton, IconButtonDensity, IconButtonStyle},
    list_row,
};
use gpui::{Div, ElementId, SharedString, Stateful, prelude::*};

/// Host-localized labels for a page placeholder and its recovery actions.
pub struct LoadingRowLabels {
    pub message: SharedString,
    pub retry: SharedString,
    pub copy_error: SharedString,
}

/// Page placeholder presentation; request identity and retry gating remain with the owner.
pub fn paged_loading_row<T: 'static>(
    id: impl Into<ElementId>,
    error: Option<&str>,
    show: bool,
    labels: LoadingRowLabels,
    height: gpui::Pixels,
    cx: &mut gpui::Context<T>,
    retry: impl Fn(&mut T, &mut gpui::Window, &mut gpui::Context<T>) + 'static,
) -> Stateful<Div> {
    let theme = crate::theme::palette(cx);
    let surface = crate::theme::interaction_surface(cx);
    list_row(theme, id, false, height, surface)
        .child(
            caption(theme, labels.message)
                .flex_1()
                .min_w_0()
                .truncate()
                .when(!show, |text| text.invisible()),
        )
        .when_some(
            error.map(|error| SharedString::from(error.to_owned())),
            |row, error| {
                row.role(gpui::Role::Group)
                    .aria_label(error.clone())
                    .child(
                        IconButton::new("copy-error", "⧉", labels.copy_error)
                            .surface(surface)
                            .build_with(
                                IconButtonStyle::Quiet,
                                IconButtonDensity::Regular,
                                cx,
                                move |_, _, cx| {
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                        error.to_string(),
                                    ));
                                },
                                |control| control.min_h_0().h(height - gpui::px(2.)).py_0(),
                            ),
                    )
                    .child(
                        IconButton::new("retry", "󰑐", labels.retry)
                            .surface(surface)
                            .build_with(
                                IconButtonStyle::Quiet,
                                IconButtonDensity::Regular,
                                cx,
                                retry,
                                |control| control.min_h_0().h(height - gpui::px(2.)).py_0(),
                            ),
                    )
            },
        )
}

#[cfg(test)]
mod row_tests {
    use super::*;
    use gpui::{Context, Render, TestAppContext, Window, div};

    struct LoadingRow {
        error: Option<&'static str>,
        show: bool,
        retries: usize,
    }

    impl Render for LoadingRow {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(
                paged_loading_row(
                    ("loading-row", 0usize),
                    self.error,
                    self.show,
                    LoadingRowLabels {
                        message: if self.error.is_some() {
                            "Loading failed".into()
                        } else {
                            "Loading".into()
                        },
                        retry: "Retry loading".into(),
                        copy_error: "Copy full error".into(),
                    },
                    crate::menu_row_height(window, cx),
                    cx,
                    |this, _, cx| {
                        this.retries += 1;
                        cx.notify();
                    },
                )
                .w(gpui::px(240.)),
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
                let position = gpui::point(gpui::px(220.), gpui::px(12.));
                crate::test_events::click(window, cx, gpui::MouseButton::Left, position);
            })
            .unwrap();
            assert_eq!(row.read_with(cx, |row, _| row.retries), retries);
        }
    }

    #[gpui::test]
    fn keyboard_recovery_copies_full_error_without_retrying(cx: &mut TestAppContext) {
        cx.update(crate::theme::init_test);
        let error = "Page failed: a long diagnostic with Unicode 路径 and a second line\nDetails";
        let window = cx.add_window(|_, _| LoadingRow {
            error: Some(error),
            show: true,
            retries: 0,
        });
        let row = window.root(cx).unwrap();
        let handle = window.into();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.draw(cx).clear(cx);
            window.focus_next(cx);
        })
        .unwrap();
        for expected_retries in [0, 1] {
            cx.update_window(handle, |_, window, cx| {
                window.draw(cx).clear(cx);
                crate::test_events::press(window, cx, "enter");
            })
            .unwrap();
            cx.run_until_parked();
            assert_eq!(row.read_with(cx, |row, _| row.retries), expected_retries);
            cx.update_window(handle, |_, window, cx| window.focus_next(cx))
                .unwrap();
        }
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some(error)
        );
    }
}
