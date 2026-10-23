//! Expandable search field with caller-supplied labels and debounce policy.

use super::{
    control::{IconButton, IconButtonDensity, IconButtonStyle, icon_button_height},
    input::{Input, InputEvent},
    layout::row,
};
use gpui::{prelude::*, *};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchEvent {
    Changed(Option<String>),
}

/// Labels are resolved by the host; the field does not load localization resources.
pub struct SearchLabels {
    pub name: SharedString,
    pub placeholder: SharedString,
    pub toggle: SharedString,
    pub clear: SharedString,
}

pub struct SearchField {
    labels: SearchLabels,
    debounce: Duration,
    input: Entity<Input>,
    focus_handle: FocusHandle,
    expanded: bool,
    expansion: u64,
    query: Option<String>,
    generation: u64,
    timer: Option<Task<()>>,
    counts: Option<(usize, usize)>,
    _subscription: Subscription,
    blur_subscription: Option<Subscription>,
}

impl EventEmitter<SearchEvent> for SearchField {}

impl SearchField {
    pub fn new(labels: SearchLabels, debounce: Duration, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            Input::new(labels.name.clone(), "", labels.placeholder.clone(), cx).embedded()
        });
        input.update(cx, |input, cx| input.set_trailing_text("—".into(), cx));
        let subscription = cx.subscribe(&input, |this, _, event, cx| {
            if matches!(event, InputEvent::Changed { user: true }) {
                this.edited(cx);
            }
        });
        Self {
            labels,
            debounce,
            input,
            focus_handle: cx.focus_handle(),
            expanded: false,
            expansion: 0,
            query: None,
            generation: 0,
            timer: None,
            counts: None,
            _subscription: subscription,
            blur_subscription: None,
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
            let delay = cx.background_executor().timer(self.debounce);
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
            self.clear_counts(cx);
            cx.emit(SearchEvent::Changed(query));
            cx.notify();
        }
    }

    pub fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.blur_subscription.is_none() {
            let input_focus = self.input.focus_handle(cx);
            let subscription = cx.on_blur(&input_focus, window, |this, _, cx| {
                if this.expanded && this.input.read(cx).text().is_empty() {
                    this.expanded = false;
                    cx.notify();
                }
            });
            self.blur_subscription = Some(subscription);
        }
        if !self.expanded {
            self.expansion = self.expansion.wrapping_add(1);
            self.expanded = true;
        }
        self.input
            .update(cx, |input, cx| input.focus_and_select_all(window, cx));
        cx.notify();
    }

    pub fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus(window, cx);
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.timer.take();
        self.generation = self.generation.wrapping_add(1);
        self.input.update(cx, |input, cx| input.set_text("", cx));
        self.apply(None, cx);
        cx.notify();
    }

    pub fn reset(&mut self, cx: &mut Context<Self>) {
        self.clear(cx);
        self.expanded = false;
        cx.notify();
    }

    pub fn escape(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
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

    pub fn input_focused(&self, window: &Window, cx: &App) -> bool {
        self.input.read(cx).is_focused(window)
    }

    pub fn set_counts(
        &mut self,
        matched: usize,
        total: usize,
        format: impl FnOnce(usize, usize) -> SharedString,
        cx: &mut Context<Self>,
    ) {
        if self.counts != Some((matched, total)) {
            self.counts = Some((matched, total));
            let text = format(matched, total);
            self.input
                .update(cx, |input, cx| input.set_trailing_text(text, cx));
            cx.notify();
        }
    }

    pub fn clear_counts(&mut self, cx: &mut Context<Self>) {
        if self.counts.take().is_some() {
            self.input
                .update(cx, |input, cx| input.set_trailing_text("—".into(), cx));
            cx.notify();
        }
    }
}

impl Render for SearchField {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::palette(cx);
        // Embedded actions lose their own border; the search supplies the outer border.
        let height = icon_button_height(window, cx).max(window.rem_size() * 2. + px(2.));
        let visible =
            self.expanded || !self.input.read(cx).text().is_empty() || self.query.is_some();
        let focused = self.input_focused(window, cx);
        if !visible {
            return row()
                .track_focus(&self.focus_handle)
                .h(height)
                .flex_shrink_0()
                .child(
                    IconButton::new("search-toggle", "󰍉", self.labels.toggle.clone()).build(
                        IconButtonStyle::Surface,
                        IconButtonDensity::Regular,
                        cx,
                        |this, window, cx| this.toggle(window, cx),
                    ),
                )
                .into_any_element();
        }
        let mut field = row()
            .gap_0()
            .h_full()
            .w_full()
            .min_w_0()
            .overflow_x_hidden()
            .rounded_md()
            .bg(rgb(theme.panel))
            .border_1()
            .border_color(if focused {
                rgb(theme.accent)
            } else {
                rgb(theme.border)
            })
            .child(
                IconButton::new("search-toggle", "󰍉", self.labels.toggle.clone())
                    .build(
                        IconButtonStyle::Surface,
                        IconButtonDensity::Regular,
                        cx,
                        |this, window, cx| this.toggle(window, cx),
                    )
                    .border_0()
                    .p_1()
                    .min_w_6()
                    .min_h_6()
                    .bg(gpui::transparent_black())
                    .rounded_none(),
            );
        field = field
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .px_1()
                    .child(self.input.clone()),
            )
            .when(!self.input.read(cx).text().is_empty(), |field| {
                field.child(
                    IconButton::new("search-clear", "", self.labels.clear.clone())
                        .build(
                            IconButtonStyle::Surface,
                            IconButtonDensity::Regular,
                            cx,
                            |this, window, cx| {
                                this.clear(cx);
                                this.input.update(cx, |input, cx| {
                                    input.focus_handle(cx).focus(window, cx)
                                });
                            },
                        )
                        .border_0()
                        .p_1()
                        .min_w_6()
                        .min_h_6()
                        .bg(gpui::transparent_black())
                        .rounded_none(),
                )
            });
        let control = row()
            .track_focus(&self.focus_handle)
            .min_w_0()
            .h(height)
            .min_h(height)
            .max_h(height)
            .w_56()
            .flex_basis(gpui::rems(2.))
            .flex_shrink(1.)
            .child(field);
        control
            .flex_grow(1.)
            .with_animation(
                ("search-expand", self.expansion),
                Animation::new(std::time::Duration::from_millis(150))
                    .with_easing(|t| 1. - (1. - t).powi(3)),
                |control, progress| control.max_w(gpui::rems(2. + 12. * progress)),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;
    use std::{cell::RefCell, rc::Rc};

    struct SearchColumn {
        search: Entity<SearchField>,
        remaining: Rc<RefCell<Option<Bounds<Pixels>>>>,
    }

    impl Render for SearchColumn {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let remaining = self.remaining.clone();
            div()
                .size_full()
                .flex()
                .flex_col()
                .child(self.search.clone())
                .child(
                    canvas(
                        move |bounds, _, _| *remaining.borrow_mut() = Some(bounds),
                        |_, _, _, _| {},
                    )
                    .flex_1()
                    .w_full(),
                )
        }
    }

    #[test]
    fn expanding_search_in_a_column_does_not_take_the_content_height() {
        let mut cx = TestAppContext::single();
        cx.update(crate::theme::init_test);
        let search = cx.update(|cx| {
            cx.new(|cx| {
                SearchField::new(
                    SearchLabels {
                        name: "Search".into(),
                        placeholder: "Search".into(),
                        toggle: "Search".into(),
                        clear: "Clear".into(),
                    },
                    Duration::ZERO,
                    cx,
                )
            })
        });
        let remaining = Rc::new(RefCell::new(None));
        let window = cx.add_window({
            let search = search.clone();
            let remaining = remaining.clone();
            move |_, _| SearchColumn { search, remaining }
        });
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let collapsed = remaining.borrow().unwrap();
        cx.update_window(window.into(), |_, window, cx| {
            search.update(cx, |search, cx| search.focus(window, cx));
            window.draw(cx).clear(cx);
        })
        .unwrap();
        assert_eq!(remaining.borrow().unwrap(), collapsed);
    }

    struct SearchWithOutsideFocus {
        search: Entity<SearchField>,
        outside: FocusHandle,
    }

    impl Render for SearchWithOutsideFocus {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(self.search.clone())
                .child(div().track_focus(&self.outside).size(gpui::px(32.)))
        }
    }

    #[test]
    fn empty_search_collapses_on_input_blur_without_cancelling_nonempty_queries() {
        let mut cx = TestAppContext::single();
        cx.update(crate::theme::init_test);
        let delay = Duration::from_millis(100);
        let search = cx.update(|cx| {
            cx.new(|cx| {
                SearchField::new(
                    SearchLabels {
                        name: "Search".into(),
                        placeholder: "Search".into(),
                        toggle: "Search".into(),
                        clear: "Clear".into(),
                    },
                    delay,
                    cx,
                )
            })
        });
        let outside = cx.update(|cx| cx.focus_handle());
        let window = cx.add_window({
            let search = search.clone();
            let outside = outside.clone();
            move |_, _| SearchWithOutsideFocus { search, outside }
        });
        let handle = window.into();
        cx.update_window(handle, |_, window, _| window.activate_window())
            .unwrap();
        cx.run_until_parked();
        let focus = |cx: &mut TestAppContext| {
            cx.update_window(handle, |_, window, cx| {
                search.update(cx, |search, cx| search.focus(window, cx));
                window.draw(cx).clear(cx);
            })
            .unwrap();
            cx.run_until_parked();
        };
        let blur = |cx: &mut TestAppContext| {
            cx.update_window(handle, |_, window, cx| {
                outside.focus(window, cx);
                window.draw(cx).clear(cx);
            })
            .unwrap();
            cx.run_until_parked();
        };
        focus(&mut cx);
        blur(&mut cx);
        assert!(!cx.read(|cx| search.read(cx).expanded));

        focus(&mut cx);
        cx.update_window(handle, |_, window, cx| {
            search.update(cx, |search, cx| {
                search.input.update(cx, |input, cx| {
                    input.replace_text_in_range(None, "needle", window, cx);
                });
            });
        })
        .unwrap();
        cx.run_until_parked();
        blur(&mut cx);
        assert!(cx.read(|cx| search.read(cx).expanded));
        cx.executor().advance_clock(delay);
        cx.run_until_parked();
        assert_eq!(
            cx.read(|cx| search.read(cx).query.clone()),
            Some("needle".into())
        );

        focus(&mut cx);
        cx.update(|cx| search.update(cx, |search, cx| search.clear(cx)));
        assert!(cx.read(|cx| search.read(cx).expanded));
        blur(&mut cx);
        assert!(!cx.read(|cx| search.read(cx).expanded));
        assert_eq!(cx.read(|cx| search.read(cx).query.clone()), None);
    }

    #[test]
    fn clearing_search_cancels_pending_query_and_new_edits_still_publish() {
        let mut cx = TestAppContext::single();
        cx.update(crate::theme::init_test);
        let delay = Duration::from_millis(100);
        let window = cx.add_window(move |_, cx| {
            SearchField::new(
                SearchLabels {
                    name: "Search".into(),
                    placeholder: "Search".into(),
                    toggle: "Search".into(),
                    clear: "Clear".into(),
                },
                delay,
                cx,
            )
        });
        let search = window.root(&mut cx).unwrap();
        let published = Rc::new(RefCell::new(Vec::new()));
        let _subscription = cx.update(|cx| {
            let published = published.clone();
            cx.subscribe(&search, move |_, event, _| {
                let SearchEvent::Changed(query) = event;
                published.borrow_mut().push(query.clone());
            })
        });
        let handle = window.into();
        let edit = |text: &str, cx: &mut TestAppContext| {
            cx.update_window(handle, |_, window, cx| {
                search.update(cx, |search, cx| {
                    search.input.update(cx, |input, cx| {
                        let end = input.text().encode_utf16().count();
                        input.replace_text_in_range(Some(0..end), text, window, cx);
                    });
                });
            })
            .unwrap();
            cx.run_until_parked();
        };
        edit("first", &mut cx);
        cx.executor().advance_clock(delay / 2);
        edit("replacement", &mut cx);
        cx.update(|cx| search.update(cx, |search, cx| search.clear(cx)));
        cx.executor().advance_clock(delay * 2);
        cx.run_until_parked();
        assert_eq!(*published.borrow(), Vec::<Option<String>>::new());

        edit("current", &mut cx);
        cx.executor().advance_clock(delay);
        cx.run_until_parked();
        assert_eq!(*published.borrow(), vec![Some("current".into())]);
        edit("uncommitted", &mut cx);
        cx.update(|cx| search.update(cx, |search, cx| search.reset(cx)));
        cx.executor().advance_clock(delay * 2);
        cx.run_until_parked();
        assert_eq!(*published.borrow(), vec![Some("current".into()), None]);
    }
}
