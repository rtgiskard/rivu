//! Shared interaction state for optional list-local search.

use super::{ACCENT, BORDER, Input, InputEvent, Localizer, PANEL, icon_button, row};
use gpui::{prelude::*, *};
use std::sync::Arc;

pub(super) enum SearchEvent {
    Changed(Option<String>),
}

pub(super) struct ListSearch {
    i18n: Arc<Localizer>,
    input: Entity<Input>,
    focus_handle: FocusHandle,
    expanded: bool,
    expansion: u64,
    query: Option<String>,
    generation: u64,
    timer: Option<Task<()>>,
    counts: Option<(usize, usize)>,
    _subscription: Subscription,
}

impl EventEmitter<SearchEvent> for ListSearch {}

impl ListSearch {
    pub(super) fn new(i18n: Arc<Localizer>, cx: &mut Context<Self>) -> Self {
        let input =
            cx.new(|cx| Input::new("", i18n.text("search-placeholder").to_owned(), cx).embedded());
        let subscription = cx.subscribe(&input, |this, _, event, cx| {
            if matches!(event, InputEvent::Changed { user: true }) {
                this.edited(cx);
            }
        });
        Self {
            i18n,
            input,
            focus_handle: cx.focus_handle(),
            expanded: false,
            expansion: 0,
            query: None,
            generation: 0,
            timer: None,
            counts: None,
            _subscription: subscription,
        }
    }

    fn edited(&mut self, cx: &mut Context<Self>) {
        self.timer.take();
        self.generation = self.generation.wrapping_add(1);
        let text = self.input.read(cx).text().trim();
        let query = (!text.is_empty()).then(|| text.to_owned());
        if query.is_none() {
            self.apply(None, cx);
        } else {
            let generation = self.generation;
            let delay = cx
                .background_executor()
                .timer(crate::model::SEARCH_DEBOUNCE);
            self.timer = Some(cx.spawn(async move |this, cx| {
                delay.await;
                let _ = this.update(cx, |this, cx| {
                    if this.generation == generation {
                        this.timer = None;
                        this.apply(query, cx);
                    }
                });
            }));
        }
        cx.notify();
    }

    fn apply(&mut self, query: Option<String>, cx: &mut Context<Self>) {
        if self.query != query {
            self.query = query.clone();
            self.counts = None;
            cx.emit(SearchEvent::Changed(query));
            cx.notify();
        }
    }

    pub(super) fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.expanded {
            self.expansion = self.expansion.wrapping_add(1);
            self.expanded = true;
        }
        self.input
            .update(cx, |input, cx| input.focus_and_select_all(window, cx));
        cx.notify();
    }

    pub(super) fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus(window, cx);
    }

    pub(super) fn clear(&mut self, cx: &mut Context<Self>) {
        self.timer.take();
        self.generation = self.generation.wrapping_add(1);
        self.input.update(cx, |input, cx| input.set_text("", cx));
        self.apply(None, cx);
        cx.notify();
    }

    pub(super) fn reset(&mut self, cx: &mut Context<Self>) {
        self.clear(cx);
        self.expanded = false;
        cx.notify();
    }

    pub(super) fn escape(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.input_focused(window, cx)
            && self
                .input
                .update(cx, |input, cx| input.dismiss_composition(cx))
        {
            return true;
        }
        if !self.input.read(cx).text().is_empty() || self.query.is_some() {
            self.clear(cx);
            return true;
        }
        if self.expanded {
            self.expanded = false;
            if self.input_focused(window, cx) {
                self.focus_handle.focus(window, cx);
            }
            cx.notify();
            return true;
        }
        false
    }

    pub(super) fn input_focused(&self, window: &Window, cx: &App) -> bool {
        self.input.read(cx).is_focused(window)
    }

    pub(super) fn set_counts(&mut self, matched: usize, total: usize, cx: &mut Context<Self>) {
        if self.counts != Some((matched, total)) {
            self.counts = Some((matched, total));
            cx.notify();
        }
    }

    pub(super) fn clear_counts(&mut self, cx: &mut Context<Self>) {
        if self.counts.take().is_some() {
            cx.notify();
        }
    }
}

impl Render for ListSearch {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let visible =
            self.expanded || !self.input.read(cx).text().is_empty() || self.query.is_some();
        let focused = self.input_focused(window, cx);
        let mut field = row()
            .gap_0()
            .h(gpui::rems(2.25))
            .min_w_0()
            .max_w_full()
            .overflow_hidden()
            .rounded_md()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(if focused { rgb(ACCENT) } else { rgb(BORDER) })
            .child(
                icon_button(
                    "search-toggle",
                    "󰍉",
                    self.i18n.text("search-toggle").to_owned(),
                    cx,
                    |this, window, cx| this.toggle(window, cx),
                )
                .border_0()
                .bg(gpui::transparent_black())
                .rounded_none(),
            );
        if visible {
            let count = match self.counts {
                Some((matched, total)) => self.i18n.format(
                    "search-count",
                    &[("matched", matched.into()), ("total", total.into())],
                ),
                None => "—".to_owned(),
            };
            self.input.update(cx, |input, cx| {
                input.set_trailing_text(count.into(), cx);
            });
            field = field
                .child(div().flex_1().min_w_0().child(self.input.clone()))
                .when(!self.input.read(cx).text().is_empty(), |field| {
                    field.child(
                        icon_button(
                            "search-clear",
                            "",
                            self.i18n.text("search-clear").to_owned(),
                            cx,
                            |this, window, cx| {
                                this.clear(cx);
                                this.input.update(cx, |input, cx| {
                                    input.focus_handle(cx).focus(window, cx)
                                });
                            },
                        )
                        .border_0()
                        .bg(gpui::transparent_black())
                        .rounded_none(),
                    )
                });
        }
        let field = field.w(gpui::rems(if visible { 16. } else { 2. }));
        let control = row()
            .track_focus(&self.focus_handle)
            .min_w_0()
            .flex_basis(gpui::rems(2.))
            .flex_shrink(1.)
            .child(field);
        if visible {
            control
                .flex_grow(1.)
                .with_animation(
                    ("search-expand", self.expansion),
                    Animation::new(std::time::Duration::from_millis(150))
                        .with_easing(|t| 1. - (1. - t).powi(3)),
                    |control, progress| control.max_w(gpui::rems(2. + 14. * progress)),
                )
                .into_any_element()
        } else {
            control.max_w(gpui::rems(2.)).into_any_element()
        }
    }
}
