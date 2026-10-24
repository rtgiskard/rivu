//! Run with `cargo run -p rivu-ui --example showcase --features showcase` on Wayland.
//! A component consumer: all values, filtering and loading transitions are host-owned.
use gpui::{prelude::*, *};
use rivu_ui::{
    ChoiceAction, ChoiceMode, ChoicePopup, ChoiceState, DropdownItem, FocusScope, FormField,
    IconButton, IconButtonDensity, IconButtonGroup, IconButtonStyle, ProgressBar, Slider,
    SliderInput, SliderKey, VirtualList, button, checkbox, column, disabled_control,
    dropdown_trigger,
    input::{Input, InputEvent},
    input_aligned_control, list_row,
    loading::{LoadingRowLabels, LoadingText, paged_loading_row},
    row, row_tooltip,
    search::{SearchEvent, SearchField, SearchLabels},
    theme::{self, Palette},
    two_line_text,
};
use std::{
    cell::Cell,
    time::{Duration, Instant},
};

static DARK: Palette = Palette {
    bg: 0x161820,
    panel: 0x20232d,
    control: 0x2a2e3b,
    input: 0x2a2e3b,
    structure_border: 0x343949,
    border: 0x596174,
    text: 0xf0f2f7,
    secondary: 0xbec5d2,
    muted: 0x9ca7ba,
    disabled: 0x828a99,
    accent: 0x9ebcff,
    accent_hover: 0xb4cdff,
    on_accent: 0x141926,
    highlight: 0x343d52,
    selected: 0x3b4d70,
    selected_hover: 0x485e87,
    error: 0xffa8a8,
    error_bg: 0x512e36,
    warning: 0xf2d28b,
};
static LIGHT: Palette = Palette {
    bg: 0xf6f7fa,
    panel: 0xffffff,
    control: 0xe7ebf2,
    input: 0xe7ebf2,
    structure_border: 0xd5dbe6,
    border: 0x7c8698,
    text: 0x192231,
    secondary: 0x48566b,
    muted: 0x59677a,
    disabled: 0x758095,
    accent: 0x214ea7,
    accent_hover: 0x193d85,
    on_accent: 0xffffff,
    highlight: 0xe3eaf8,
    selected: 0xcbdaf5,
    selected_hover: 0xb6cbef,
    error: 0xa72132,
    error_bg: 0xffe5e9,
    warning: 0x795800,
};

fn row_title(index: usize) -> String {
    format!(
        "Example row {} — long text truncates without owning outer spacing",
        index + 1
    )
}

#[derive(Clone, Copy, PartialEq)]
enum Rows {
    Ready,
    Loading,
    Failed,
}

struct Showcase {
    scope: FocusScope,
    input: Entity<Input>,
    search: Entity<SearchField>,
    _subscriptions: Vec<Subscription>,
    edits: usize,
    clicks: usize,
    checked: bool,
    favorite: bool,
    enabled: bool,
    light: bool,
    font_size: u32,
    slider_value: f32,
    slider_preview: Option<f32>,
    slider_bounds: Cell<Bounds<Pixels>>,
    slider_enabled: bool,
    slider_commits: usize,
    slider_cancels: usize,
    slider_outcome: SharedString,
    filtered: Vec<usize>,
    choices: ChoiceState<usize>,
    choice: usize,
    anchor: Cell<Bounds<Pixels>>,
    scroll: UniformListScrollHandle,
    rows: Rows,
    loading: LoadingText<()>,
    generation: u64,
}

impl Showcase {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| Input::new("Example text", "A retained draft", "Type here", cx));
        let search = cx.new(|cx| {
            SearchField::new(
                SearchLabels {
                    name: "Filter example rows".into(),
                    placeholder: "Row title".into(),
                    toggle: "Search rows".into(),
                    clear: "Clear search".into(),
                },
                Duration::from_millis(150),
                cx,
            )
        });
        let input_subscription = cx.subscribe(&input, |this, _, event, cx| {
            if matches!(event, InputEvent::Changed { user: true }) {
                this.edits += 1;
                cx.notify();
            }
        });
        let search_subscription = cx.subscribe(&search, |this, _, event, cx| {
            let SearchEvent::Changed(query) = event;
            let query = query.as_deref().unwrap_or("").to_lowercase();
            this.filtered = (0..100)
                .filter(|index| row_title(*index).to_lowercase().contains(&query))
                .collect();
            this.scroll.scroll_to_item(0, ScrollStrategy::Top);
            cx.notify();
        });
        let activation_subscription = cx.observe_window_activation(window, |this, window, cx| {
            if !window.is_window_active() {
                this.cancel_slider("Preview cancelled because the window lost focus.", cx);
            }
        });
        Self {
            scope: FocusScope::new(cx),
            input,
            search,
            _subscriptions: vec![
                input_subscription,
                search_subscription,
                activation_subscription,
            ],
            edits: 0,
            clicks: 0,
            checked: false,
            favorite: false,
            enabled: true,
            light: false,
            font_size: 12,
            slider_value: 0.35,
            slider_preview: None,
            slider_bounds: Cell::new(Bounds::default()),
            slider_enabled: true,
            slider_commits: 0,
            slider_cancels: 0,
            slider_outcome: "Ready — the host actual value starts at 35%.".into(),
            filtered: (0..100).collect(),
            choices: ChoiceState::default(),
            choice: 0,
            anchor: Cell::new(Bounds::default()),
            scroll: UniformListScrollHandle::new(),
            rows: Rows::Ready,
            loading: LoadingText::new(Duration::from_millis(150)),
            generation: 0,
        }
    }
    fn set_font_size(&mut self, font_size: u32, cx: &mut Context<Self>) {
        // Match the application's supported point-size range without coupling this demo to it.
        self.font_size = font_size.clamp(8, 18);
        theme::set_typography(
            gpui::font("sans-serif"),
            "Symbols Nerd Font Mono",
            self.font_size,
            cx,
        );
        cx.notify();
    }
    fn cancel_slider(&mut self, reason: &'static str, cx: &mut Context<Self>) {
        if self.slider_preview.take().is_some() {
            self.slider_cancels += 1;
            self.slider_outcome = reason.into();
            cx.notify();
        }
    }

    fn slider_input(&mut self, input: SliderInput, _: &mut Window, cx: &mut Context<Self>) {
        if !self.slider_enabled {
            return;
        }
        match input {
            SliderInput::PointerDown { position, .. } => {
                self.slider_preview = Slider::value_at(self.slider_bounds.get(), position);
                if self.slider_preview.is_some() {
                    self.slider_outcome =
                        "Previewing — actual value is unchanged until release.".into();
                }
            }
            SliderInput::Key(key) => {
                // A key supersedes the pointer session: its eventual release must not commit.
                self.cancel_slider("Pointer preview cancelled by keyboard.", cx);
                self.slider_value = match key {
                    SliderKey::Decrease => (self.slider_value - 0.05).max(0.),
                    SliderKey::Increase => (self.slider_value + 0.05).min(1.),
                    SliderKey::Start => 0.,
                    SliderKey::End => 1.,
                };
                self.slider_commits += 1;
                self.slider_outcome =
                    "Keyboard committed immediately; any pointer preview was cancelled.".into();
            }
        }
        cx.notify();
    }

    fn move_slider(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if self.slider_preview.is_none() {
            return;
        }
        if !event.dragging() {
            self.cancel_slider("Preview cancelled: pointer button is no longer held.", cx);
        } else if let Some(value) = Slider::value_at(self.slider_bounds.get(), event.position) {
            self.slider_preview = Some(value);
            cx.notify();
        }
    }

    fn release_slider(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) {
        if event.button != MouseButton::Left || self.slider_preview.is_none() {
            return;
        }
        let value =
            Slider::value_at(self.slider_bounds.get(), event.position).or(self.slider_preview);
        self.slider_preview = None;
        if self.slider_enabled
            && let Some(value) = value
        {
            self.slider_value = value;
            self.slider_commits += 1;
            self.slider_outcome =
                "Pointer release committed the preview to the host actual value.".into();
        }
        cx.notify();
    }
    fn set_rows(&mut self, rows: Rows, cx: &mut Context<Self>) {
        self.loading.clear();
        self.rows = rows;
        if rows == Rows::Loading {
            self.generation = self.generation.wrapping_add(1);
            self.loading.begin((), self.generation, Instant::now());
            self.loading.schedule(cx, |this| &mut this.loading);
        }
        cx.notify();
    }
    fn choose(&mut self, index: usize, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(item) = self.choices.item(index) {
            self.choice = item.value;
        }
        self.choices.close();
        cx.notify();
    }
}

impl Render for Showcase {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = theme::palette(cx);
        let labels = [
            "First option",
            "Second option with a long descriptive label",
            "Third option",
        ];
        let dropdown = dropdown_trigger(
            colors,
            "example-choice",
            labels[self.choice],
            self.choices.is_open(),
            cx,
        )
        .role(Role::ComboBox)
        .aria_label("Example choice")
        .on_click(cx.listener(|this, _, _, cx| {
            if this.choices.is_open() {
                this.choices.close();
            } else {
                this.choices.open(
                    [
                        "First option",
                        "Second option with a long descriptive label",
                        "Third option",
                    ]
                    .into_iter()
                    .enumerate()
                    .map(|(i, label)| DropdownItem::new(i, label)),
                    this.choice,
                );
            }
            cx.notify();
        }))
        .capture_key_down(cx.listener(|this, event, window, cx| {
            match this
                .choices
                .key(event, ChoiceMode::Select, false, window)
                .apply(window, cx)
            {
                ChoiceAction::Commit(index) => this.choose(index, window, cx),
                ChoiceAction::Dismiss => {
                    this.choices.close();
                    cx.notify();
                }
                ChoiceAction::Navigated => cx.notify(),
                _ => {}
            }
        }))
        .child(
            canvas(
                cx.processor(|this, bounds, _, _| this.anchor.set(bounds)),
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        );
        let icon_groups = row().flex_wrap().children(
            [
                (
                    "Surface · Regular",
                    IconButtonStyle::Surface,
                    IconButtonDensity::Regular,
                ),
                (
                    "Surface · Compact",
                    IconButtonStyle::Surface,
                    IconButtonDensity::Compact,
                ),
                (
                    "Quiet · Regular",
                    IconButtonStyle::Quiet,
                    IconButtonDensity::Regular,
                ),
                (
                    "Quiet · Compact",
                    IconButtonStyle::Quiet,
                    IconButtonDensity::Compact,
                ),
                (
                    "Bare · Regular",
                    IconButtonStyle::Bare,
                    IconButtonDensity::Regular,
                ),
                (
                    "Bare · Compact",
                    IconButtonStyle::Bare,
                    IconButtonDensity::Compact,
                ),
            ]
            .into_iter()
            .enumerate()
            .map(|(index, (label, style, density))| {
                column().gap_1().child(label).child(
                    IconButtonGroup::new(style, density)
                        .wrap()
                        .button(
                            IconButton::new(("group-action", index), "+", "Action")
                                .enabled(self.enabled),
                            cx,
                            |this, _, cx| {
                                this.clicks += 1;
                                cx.notify();
                            },
                        )
                        .button(
                            IconButton::new(("group-favorite", index), "♥", "Favorite")
                                .toggled(self.favorite)
                                .enabled(self.enabled),
                            cx,
                            |this, _, cx| {
                                this.favorite = !this.favorite;
                                cx.notify();
                            },
                        )
                        .button(
                            IconButton::new(("group-switch", index), "✓", "Host-owned setting")
                                .switch(self.checked)
                                .enabled(self.enabled),
                            cx,
                            |this, _, cx| {
                                this.checked = !this.checked;
                                cx.notify();
                            },
                        )
                        .button(
                            IconButton::new(("group-disabled", index), "−", "Unavailable action")
                                .enabled(false),
                            cx,
                            |this, _, cx| {
                                this.clicks += 1;
                                cx.notify();
                            },
                        )
                        .button(
                            IconButton::new(
                                ("group-disabled-selected", index),
                                "♥",
                                "Unavailable favorite (on)",
                            )
                            .toggled(true)
                            .enabled(false),
                            cx,
                            |this, _, cx| {
                                this.clicks += 1;
                                cx.notify();
                            },
                        )
                        .into_div(),
                )
            }),
        );
        let slider = Slider::new(
            "example-slider",
            self.slider_preview.unwrap_or(self.slider_value),
            "Example host-owned value",
        )
        .disabled(!self.slider_enabled)
        .dragging(self.slider_preview.is_some())
        .render(cx, Self::slider_input)
        .child(
            canvas(
                cx.processor(|this, bounds, _, _| this.slider_bounds.set(bounds)),
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        );
        let slider_demo = column().gap_1()
            .child("ProgressBar + Slider — controlled by the example host")
            .child(row().items_start()
                .child(column().flex_1().child(format!("Actual: {:.0}%", self.slider_value * 100.))
                    .child(ProgressBar::new("example-progress", self.slider_value, "Host actual progress").render(cx)))
                .child(column().flex_1().child(format!("Preview: {}", self.slider_preview.map(|value| format!("{:.0}%", value * 100.)).unwrap_or_else(|| "none".into())))
                    .child(slider)))
            .child(row().flex_wrap()
                .child(checkbox("slider-enabled", "Enable slider", self.slider_enabled, true, cx, |this, next, cx| {
                    this.cancel_slider("Preview cancelled because the slider was disabled.", cx);
                    this.slider_enabled = next;
                    cx.notify();
                }))
                .child(button("slider-actual", "Host actual +10% (wrap)", cx, |this, _, cx| {
                    this.cancel_slider("Preview cancelled by a host actual-value update.", cx);
                    this.slider_value = if this.slider_value >= 1. { 0. } else { (this.slider_value + 0.1).min(1.) };
                    this.slider_outcome = "Host changed actual progress independently of slider input.".into();
                    cx.notify();
                }))
                .child(format!("Commits: {} · Cancels: {}", self.slider_commits, self.slider_cancels)))
            .child(div().text_sm().child(self.slider_outcome.clone()))
            .child(div().text_sm().child("Drag previews; release commits. Escape/window exit cancels. Arrow keys step 5%; Home/End commit endpoints. Disabled slider ignores input; host updates still work."));
        let content = column().p_4().gap_3().w_full().h_auto().flex_shrink_0()
            .child(div().text_lg().child("Rivu UI — component contracts"))
            .child(row().flex_wrap()
                .child(button("theme", "Switch theme", cx, |this,_,cx| { this.light = !this.light; theme::set_palette(if this.light { &LIGHT } else { &DARK },cx); }))
                .child(disabled_control(colors, button("font-smaller", "Smaller text (−1 pt)", cx, |this,_,cx| { this.set_font_size(this.font_size.saturating_sub(1), cx); }), self.font_size <= 8))
                .child(format!("Font: {} pt (8–18 pt)", self.font_size))
                .child(disabled_control(colors, button("font-larger", "Larger text (+1 pt)", cx, |this,_,cx| { this.set_font_size(this.font_size + 1, cx); }), self.font_size >= 18))
                .child(button("quit", "Quit", cx, |_,_,cx| cx.quit())))
            .child(row().flex_wrap()
                .child(disabled_control(colors, button("action", "Action", cx, |this,_,cx| { this.clicks += 1; cx.notify(); }), !self.enabled))
                .child(input_aligned_control(button("aligned", "Input-aligned", cx, |this,_,cx| { this.clicks += 1; cx.notify(); }), cx))
                .child(format!("Activations: {}",self.clicks)))
            .child(column().w_64().max_w_full().child(button("long-action", "保存更改 / Save changes with a deliberately long action label", cx, |this,_,cx| { this.clicks += 1; cx.notify(); })))
            .child(icon_groups)
            .child(checkbox("enabled", "Enable action and editor", self.enabled, true, cx, |this,next,cx| {
                this.enabled = next; this.input.update(cx,|input,cx| input.set_enabled(next,cx));
            }))
            .child(checkbox("checked", "A host-owned setting", self.checked, self.enabled, cx, |this,next,_| { this.checked = next; }))
            .child(row().items_start().child(FormField::new(colors, "Retained text", self.input.clone()).hint("The host retains this draft and decides when to apply it.").into_div().flex_1()).child(FormField::new(colors, "Choice", dropdown).into_div().flex_1()))
            .child(format!("User edits: {} · Draft: {}",self.edits,self.input.read(cx).text()))
            .child(slider_demo)
            .child(row().flex_wrap()
                .child(button("begin", "Begin loading", cx, |this,_,cx| this.set_rows(Rows::Loading,cx)))
                .child(button("fail", "Fail loading", cx, |this,_,cx| this.set_rows(Rows::Failed,cx)))
                .child(button("finish", "Finish loading", cx, |this,_,cx| this.set_rows(Rows::Ready,cx)))
                .child(self.search.clone()))
            .child(div().text_sm().child("Loading controls exercise presentation only; no network or database is used. Retry begins loading; Finish supplies the rows."))
            .child({
                let count = if self.rows != Rows::Ready { 1 } else { self.filtered.len() };
                VirtualList::new("example-list",count,&self.scroll,cx.processor(|this,range: std::ops::Range<usize>,window,cx| {
                    let row_height = rivu_ui::list_row_height(window, cx);
                    range.map(|index| {
                        let colors = theme::palette(cx);
                        if this.rows != Rows::Ready {
                            let error = (this.rows == Rows::Failed).then_some("Example operation failed: the complete diagnostic includes Unicode 路径 and remains available through Copy error.");
                            return paged_loading_row(("placeholder",index),error,this.loading.show(&(),error.is_some()),LoadingRowLabels {
                                message: if error.is_some() { "Loading failed".into() } else { "Loading…".into() },
                                retry: "Retry loading".into(), copy_error: "Copy full error".into(),
                            },cx,|this,_,cx| this.set_rows(Rows::Loading,cx)).h(row_height).into_any_element();
                        }
                        let title: SharedString = row_title(this.filtered[index]).into();
                        let detail: SharedString = "Secondary line · compact leading follows each line's font size".into();
                        list_row(colors,("row",index),false)
                            .h(row_height)
                            .child(row_tooltip(two_line_text(colors,title.clone(),detail.clone()).id(("text",index)),title,detail)).into_any_element()
                    }).collect::<Vec<_>>()
                })).h(rems(12.)).flex_shrink_0().w_full().scrollbar("example-scroll")
            });
        let mut root = self
            .scope
            .surface(
                div()
                    .id("showcase")
                    .size_full()
                    .bg(rgb(colors.bg))
                    .text_color(rgb(colors.text))
                    .font(theme::font(cx).clone())
                    .text_size(px(14.))
                    .font_size_scale(theme::font_size_scale(cx)),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" && this.slider_preview.is_some() {
                    this.cancel_slider("Preview cancelled by Escape; actual value retained.", cx);
                    cx.stop_propagation();
                }
            }))
            .child(
                div()
                    .id("showcase-page")
                    .size_full()
                    .overflow_y_scroll()
                    .child(content),
            )
            .child({
                let entity = cx.entity().downgrade();
                canvas(
                    |_, _, _| (),
                    move |_, _, window, _| {
                        let view = entity.clone();
                        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                            if phase == DispatchPhase::Capture {
                                let _ = view.update(cx, |this, cx| this.move_slider(event, cx));
                            }
                        });
                        let view = entity.clone();
                        window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                            if phase == DispatchPhase::Capture {
                                let _ = view.update(cx, |this, cx| this.release_slider(event, cx));
                            }
                        });
                        let view = entity.clone();
                        window.on_mouse_event(move |_: &MouseExitEvent, phase, _, cx| {
                            if phase == DispatchPhase::Capture {
                                let _ = view.update(cx, |this, cx| {
                                    this.cancel_slider(
                                        "Preview cancelled on window exit; actual value retained.",
                                        cx,
                                    )
                                });
                            }
                        });
                    },
                )
                .absolute()
                .size_full()
            });
        if self.choices.is_open() {
            root = root.child(
                ChoicePopup::new("example-options", "options-scroll", self.anchor.get()).render(
                    self,
                    |this| &this.choices,
                    Self::choose,
                    |this, _, _, cx| {
                        this.choices.close();
                        cx.notify();
                        true
                    },
                    window,
                    cx,
                ),
            );
        }
        root
    }
}

fn main() {
    gpui_platform::application().run(|cx| {
        theme::set_palette(&DARK, cx);
        theme::set_typography(gpui::font("sans-serif"), "Symbols Nerd Font Mono", 12, cx);
        Input::init(cx);
        let bounds = Bounds::centered(None, size(px(1000.), px(820.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Rivu UI components".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Showcase::new(window, cx)),
        )
        .expect("open component example");
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;
    use gpui::InputEvent as _;

    #[test]
    fn keyboard_confirmation_keeps_the_showcase_dropdown_closed() {
        for confirm in ["enter", "space"] {
            let mut cx = TestAppContext::single();
            cx.update(|cx| {
                theme::set_palette(&DARK, cx);
                theme::set_typography(gpui::font("sans-serif"), "sans-serif", 12, cx);
                Input::init(cx);
            });
            let window = cx.add_window(Showcase::new);
            let showcase = window.root(&mut cx).unwrap();
            cx.update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                showcase.read(cx).input.focus_handle(cx).focus(window, cx);
                window.draw(cx).clear(cx);
            })
            .unwrap();
            for key in ["tab", confirm, "down", confirm] {
                cx.update_window(window.into(), |_, window, cx| {
                    let keystroke = Keystroke::parse(key).unwrap();
                    window.dispatch_event(
                        KeyDownEvent {
                            keystroke: keystroke.clone(),
                            is_held: false,
                            prefer_character_input: false,
                        }
                        .to_platform_input(),
                        cx,
                    );
                    window.dispatch_event(KeyUpEvent { keystroke }.to_platform_input(), cx);
                    window.draw(cx).clear(cx);
                })
                .unwrap();
                cx.run_until_parked();
            }
            cx.update(|cx| {
                let showcase = showcase.read(cx);
                assert_eq!(showcase.choice, 1, "confirmation key: {confirm}");
                assert!(!showcase.choices.is_open(), "confirmation key: {confirm}");
            });
        }
    }
}
