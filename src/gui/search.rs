//! Shared interaction state for optional list-local search.

use super::{Input, InputEvent, Localizer, MUTED, caption, icon_button, row};
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
    query: Option<String>,
    generation: u64,
    timer: Option<Task<()>>,
    counts: Option<(usize, usize)>,
    _subscription: Subscription,
}

impl EventEmitter<SearchEvent> for ListSearch {}

impl ListSearch {
    pub(super) fn new(i18n: Arc<Localizer>, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| Input::new("", i18n.text("search-placeholder").to_owned(), cx));
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
        self.expanded = true;
        self.input
            .update(cx, |input, cx| input.focus_and_select_all(window, cx));
        cx.notify();
    }

    pub(super) fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.expanded && self.input.read(cx).text().is_empty() {
            self.expanded = false;
            self.focus_handle.focus(window, cx);
            cx.notify();
        } else {
            self.focus(window, cx);
        }
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let visible =
            self.expanded || !self.input.read(cx).text().is_empty() || self.query.is_some();
        let mut control = row()
            .track_focus(&self.focus_handle)
            .h(gpui::rems(2.25))
            .min_w_0()
            .flex_basis(gpui::rems(2.))
            .justify_end()
            .when(visible, |control| control.flex_grow(1.))
            .flex_shrink(1.)
            .child(icon_button(
                "search-toggle",
                "󰍉",
                self.i18n.text("search-toggle").to_owned(),
                cx,
                |this, window, cx| this.toggle(window, cx),
            ));
        if visible {
            control = control.child(
                gpui::div()
                    .w(gpui::rems(12.))
                    .flex_shrink(1.)
                    .min_w_0()
                    .child(self.input.clone()),
            );
            let count = match self.counts {
                Some((matched, total)) => self.i18n.format(
                    "search-count",
                    &[("matched", matched.into()), ("total", total.into())],
                ),
                None => "—".to_owned(),
            };
            control = control.child(
                caption(count)
                    .text_color(rgb(MUTED))
                    .flex_shrink_0()
                    .whitespace_nowrap(),
            );
        }
        control
    }
}
