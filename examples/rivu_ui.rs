//! Run with `cargo run --example rivu_ui` on a Wayland desktop.
//! A component consumer: all values, filtering and loading transitions are host-owned.
use gpui::{prelude::*, *};
use rivu_ui::{
    ChoiceAction, ChoiceMode, ChoicePopup, ChoiceState, DropdownItem, FocusScope, VirtualList,
    active_control, button, checkbox, column, compact_icon_button, disabled_control,
    dropdown_trigger, form_field, icon_button,
    input::{Input, InputEvent},
    input_aligned_control, list_row,
    loading::{LoadingRowLabels, LoadingText, paged_loading_row},
    quiet_icon_button, row, row_tooltip,
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
    enabled: bool,
    light: bool,
    large: bool,
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
    fn new(cx: &mut Context<Self>) -> Self {
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
        Self {
            scope: FocusScope::new(cx),
            input,
            search,
            _subscriptions: vec![input_subscription, search_subscription],
            edits: 0,
            clicks: 0,
            checked: false,
            enabled: true,
            light: false,
            large: false,
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
        .on_key_down(cx.listener(|this, event, window, cx| {
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
        let content = column().p_4().gap_3().size_full()
            .child(div().text_lg().child("Rivu UI — component contracts"))
            .child(row().flex_wrap()
                .child(button("theme", "Switch theme", cx, |this,_,cx| { this.light = !this.light; theme::set_palette(if this.light { &LIGHT } else { &DARK },cx); }))
                .child(button("font", "Change text size", cx, |this,_,cx| { this.large = !this.large; cx.notify(); }))
                .child(button("quit", "Quit", cx, |_,_,cx| cx.quit())))
            .child(row().flex_wrap()
                .child(disabled_control(colors, button("action", "Action", cx, |this,_,cx| { this.clicks += 1; cx.notify(); }), !self.enabled))
                .child(active_control(colors, quiet_icon_button(colors, icon_button("quiet", "✓", "Quiet selected action", cx, |this,_,cx| { this.checked = !this.checked; cx.notify(); })),self.checked))
                .child(compact_icon_button(colors, icon_button("compact", "+", "Compact inline action", cx, |this,_,cx| { this.clicks += 1; cx.notify(); })))
                .child(input_aligned_control(button("aligned", "Input-aligned", cx, |this,_,cx| { this.clicks += 1; cx.notify(); }), cx))
                .child(format!("Activations: {}",self.clicks)))
            .child(checkbox("enabled", "Enable action and editor", self.enabled, true, cx, |this,next,cx| {
                this.enabled = next; this.input.update(cx,|input,cx| input.set_enabled(next,cx));
            }))
            .child(checkbox("checked", "A host-owned setting", self.checked, self.enabled, cx, |this,next,_| { this.checked = next; }))
            .child(row().items_start().child(form_field(colors,"Retained text",self.input.clone()).flex_1()).child(form_field(colors,"Choice",dropdown).flex_1()))
            .child(format!("User edits: {} · Draft: {}",self.edits,self.input.read(cx).text()))
            .child(row().flex_wrap()
                .child(button("begin", "Begin loading", cx, |this,_,cx| this.set_rows(Rows::Loading,cx)))
                .child(button("fail", "Fail loading", cx, |this,_,cx| this.set_rows(Rows::Failed,cx)))
                .child(button("finish", "Finish loading", cx, |this,_,cx| this.set_rows(Rows::Ready,cx)))
                .child(self.search.clone()))
            .child(div().text_sm().child("Loading controls exercise presentation only; no network or database is used. Retry begins loading; Finish supplies the rows."))
            .child({
                let count = if self.rows != Rows::Ready { 1 } else { self.filtered.len() };
                VirtualList::new("example-list",count,&self.scroll,cx.processor(|this,range: std::ops::Range<usize>,_,cx| {
                    range.map(|index| {
                        let colors = theme::palette(cx);
                        if this.rows != Rows::Ready {
                            let error = (this.rows == Rows::Failed).then_some("Example operation failed: the complete diagnostic includes Unicode 路径 and remains available through Copy error.");
                            return paged_loading_row(("placeholder",index),error,this.loading.show(&(),error.is_some()),LoadingRowLabels {
                                message: if error.is_some() { "Loading failed".into() } else { "Loading…".into() },
                                retry: "Retry loading".into(), copy_error: "Copy full error".into(),
                            },cx,|this,_,cx| this.set_rows(Rows::Loading,cx)).into_any_element();
                        }
                        let title: SharedString = row_title(this.filtered[index]).into();
                        let detail: SharedString = "Secondary line · compact leading follows each line's font size".into();
                        list_row(colors,("row",index),false)
                            .child(row_tooltip(two_line_text(colors,title.clone(),detail.clone()).id(("text",index)),title,detail)).into_any_element()
                    }).collect::<Vec<_>>()
                })).flex_1().min_h_0().w_full().scrollbar("example-scroll")
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
                    .text_size(px(if self.large { 20. } else { 16. })),
            )
            .child(content);
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
            |_, cx| cx.new(Showcase::new),
        )
        .expect("open component example");
        cx.activate(true);
    });
}
