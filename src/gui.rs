mod artwork;
mod input;
mod layout;
mod panels;
mod settings;
mod visuals;

use crate::{
    core::AppHandle,
    model::{AppState, Command, PlaybackStatus},
};
use anyhow::Result;
use futures::{FutureExt, StreamExt, channel::mpsc};
use gpui::{prelude::*, *};
use input::{Input, InputEvent};
use layout::{Axis, Edge, Layout, Node};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::Duration,
};

const BG: u32 = 0x1a1b26;
const PANEL: u32 = 0x16161e;
const BORDER: u32 = 0x3b4261;
const TEXT: u32 = 0xc0caf5;
const MUTED: u32 = 0x565f89;
const ACCENT: u32 = 0x7aa2f7;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Field {
    Search,
    Path,
    PlaylistName,
    Title,
    Artist,
    Album,
}

struct PanelSpec {
    kind: &'static str,
    title: &'static str,
    render: fn(&mut GuiApp, u64, &mut Window, &mut Context<GuiApp>) -> AnyElement,
}
const PANELS: &[PanelSpec] = &[
    PanelSpec {
        kind: "transport",
        title: "Playback",
        render: GuiApp::transport_panel,
    },
    PanelSpec {
        kind: "library",
        title: "Library",
        render: GuiApp::library_panel,
    },
    PanelSpec {
        kind: "queue",
        title: "Queue",
        render: GuiApp::queue_panel,
    },
    PanelSpec {
        kind: "playlists",
        title: "Playlists",
        render: GuiApp::playlists_panel,
    },
    PanelSpec {
        kind: "metadata",
        title: "Track details",
        render: GuiApp::metadata_panel,
    },
    PanelSpec {
        kind: "history",
        title: "Listening history",
        render: GuiApp::history_panel,
    },
    PanelSpec {
        kind: "spectrum",
        title: "Spectrum",
        render: GuiApp::spectrum_panel,
    },
    PanelSpec {
        kind: "spectrogram",
        title: "Spectrogram",
        render: GuiApp::spectrogram_panel,
    },
    PanelSpec {
        kind: "settings",
        title: "Settings",
        render: GuiApp::settings_panel,
    },
];
fn panel_spec(kind: &str) -> Option<&'static PanelSpec> {
    PANELS.iter().find(|panel| panel.kind == kind)
}

pub fn run(handle: AppHandle, layout_path: PathBuf) -> Result<()> {
    let error = Rc::new(RefCell::new(None));
    let startup_error = error.clone();
    gpui_platform::application().run(move |cx: &mut App| {
        Input::init(cx);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        let result = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Rivu".into()),
                    ..Default::default()
                }),
                window_min_size: Some(size(px(760.), px(480.))),
                app_id: Some("rivu".into()),
                ..Default::default()
            },
            move |window, cx| cx.new(|cx| GuiApp::new(handle, layout_path, window, cx)),
        );
        if let Err(error) = result {
            *startup_error.borrow_mut() = Some(error);
            cx.quit();
        } else {
            cx.activate(true);
        }
    });
    match error.borrow_mut().take() {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn row() -> Div {
    div().flex().items_center().gap_2().min_w_0()
}
fn column() -> Div {
    div().flex().flex_col().min_w_0().min_h_0().gap_2()
}

struct ButtonTooltip {
    text: SharedString,
}

impl Render for ButtonTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .max_w(px(360.))
            .px_3()
            .py_2()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .text_sm()
            .text_color(rgb(TEXT))
            .child(self.text.clone())
    }
}

fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    cx: &mut Context<GuiApp>,
    action: impl Fn(&mut GuiApp, &mut Window, &mut Context<GuiApp>) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_md()
        .text_sm()
        .cursor_pointer()
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(BORDER))
        .hover(|style| style.bg(rgb(0x292e42)).border_color(rgb(ACCENT)))
        .child(label.into())
        .on_click(cx.listener(move |this, _, window, cx| action(this, window, cx)))
}

fn icon_button(
    id: impl Into<ElementId>,
    icon: impl Into<SharedString>,
    hint: impl Into<SharedString>,
    cx: &mut Context<GuiApp>,
    action: impl Fn(&mut GuiApp, &mut Window, &mut Context<GuiApp>) + 'static,
) -> Stateful<Div> {
    let hint = hint.into();
    button(id, icon, cx, action)
        .tooltip(move |_, cx| cx.new(|_| ButtonTooltip { text: hint.clone() }).into())
}
fn format_time(seconds: f64) -> String {
    let seconds = if seconds.is_finite() {
        seconds.max(0.) as u64
    } else {
        0
    };
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

#[derive(Clone)]
struct PanelDrag {
    panel_id: u64,
    title: SharedString,
}
impl Render for PanelDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_4()
            .py_2()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(ACCENT))
            .rounded_md()
            .text_color(rgb(TEXT))
            .child(self.title.clone())
    }
}

#[derive(Clone, Copy)]
pub(super) struct QueueDrag {
    pub(super) queue_id: u64,
}

impl Render for QueueDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_4()
            .py_2()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(ACCENT))
            .rounded_md()
            .text_color(rgb(TEXT))
            .child("Move queue entry")
    }
}
#[derive(Clone, Copy, Hash, PartialEq, Eq)]
enum Measured {
    Node(u64),
    Seek(u64),
    Volume(u64),
}
#[derive(Clone, Copy)]
enum Dragging {
    Split(u64, Axis),
    Seek(u64),
    Volume(u64),
}

fn dock_edge(position: Point<Pixels>, bounds: Bounds<Pixels>) -> Edge {
    let x = (position.x - bounds.origin.x) / bounds.size.width;
    let y = (position.y - bounds.origin.y) / bounds.size.height;
    if x < 0.22 {
        Edge::Left
    } else if x > 0.78 {
        Edge::Right
    } else if y < 0.22 {
        Edge::Top
    } else if y > 0.78 {
        Edge::Bottom
    } else {
        Edge::Center
    }
}

struct GuiApp {
    handle: AppHandle,
    state: AppState,
    layout: Layout,
    layout_path: PathBuf,
    inputs: HashMap<Field, Entity<Input>>,
    selected: HashSet<i64>,
    selected_queue: Option<u64>,
    selected_playlist: Option<i64>,
    selected_entry: Option<i64>,
    metadata_track: Option<i64>,
    error: Option<String>,
    filtered_rows: Vec<usize>,
    library_index: HashMap<i64, usize>,
    most_played: Vec<usize>,
    settings: settings::Settings,
    visuals: visuals::Visuals,
    default_album: artwork::Artwork,
    catalog_open: bool,
    device_popup_open: bool,
    hidden_tab_bars: HashSet<u64>,
    target_group: Option<u64>,
    dragging: Option<Dragging>,
    seek_preview: Option<f64>,
    seek_queue_id: Option<u64>,
    measured: Rc<RefCell<HashMap<Measured, Bounds<Pixels>>>>,
    analysis_enabled: bool,
    analysis_sequence: u64,
    window_visible: bool,
    ui_font: SharedString,
    _subscriptions: Vec<Subscription>,
}
impl GuiApp {
    fn new(
        handle: AppHandle,
        layout_path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let state = handle.snapshot();
        let (layout, error) = if layout_path.exists() {
            match Layout::load(&layout_path) {
                Ok(layout) => (layout, None),
                Err(error) => (
                    Layout::default(),
                    Some(format!("Could not load workspace: {error:#}")),
                ),
            }
        } else {
            (Layout::default(), None)
        };
        let fonts = cx.text_system().all_font_names();
        let ui_font = [
            "Noto Sans CJK SC",
            "Source Han Sans SC",
            "WenQuanYi Micro Hei",
            "LXGW Neo XiHei",
        ]
        .into_iter()
        .find(|name| fonts.iter().any(|font| font == name))
        .unwrap_or(".SystemUIFont")
        .into();
        let mut inputs = HashMap::new();
        for (field, placeholder) in [
            (Field::Search, "Search title, artist or album"),
            (Field::Path, "File, folder or playlist path"),
            (Field::PlaylistName, "Playlist name"),
            (Field::Title, "Title"),
            (Field::Artist, "Artist"),
            (Field::Album, "Album"),
        ] {
            inputs.insert(field, cx.new(|cx| Input::new("", placeholder, cx)));
        }
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe(&inputs[&Field::Search], |this, _, event, cx| {
            if matches!(event, InputEvent::Changed) {
                this.refresh_filter(cx);
                cx.notify();
            }
        }));
        subscriptions.push(
            cx.observe_window_visibility(window, |this, visibility, _, cx| {
                this.window_visible = matches!(visibility, WindowVisibility::Visible);
                this.sync_analysis(cx);
                cx.notify();
            }),
        );
        let (wake_sender, mut wake_receiver) = mpsc::channel::<()>(1);
        let wake_sender = parking_lot::Mutex::new(wake_sender);
        handle.set_wakeup(move || {
            let _ = wake_sender.lock().try_send(());
        });
        cx.spawn(async move |this, cx| {
            loop {
                let Ok(interval) = this.update(cx, |this, _| {
                    (this.window_visible && this.analysis_enabled && this.state.status == PlaybackStatus::Playing)
                        .then(|| Duration::from_secs_f64(1. / this.state.config.analysis_fps as f64))
                }) else { break; };
                if let Some(interval) = interval {
                    let timer = cx.background_executor().timer(interval).fuse();
                    let changed = wake_receiver.next().fuse();
                    futures::pin_mut!(timer, changed);
                    futures::select! { _ = timer => {}, message = changed => if message.is_none() { break; } }
                } else if wake_receiver.next().await.is_none() { break; }
                if this.update(cx, |this, cx| this.refresh(cx)).is_err() { break; }
            }
        }).detach();
        let mut app = Self {
            settings: settings::Settings::new(&state.config, cx),
            handle,
            state,
            layout,
            layout_path,
            inputs,
            selected: HashSet::new(),
            selected_queue: None,
            selected_playlist: None,
            selected_entry: None,
            metadata_track: None,
            error,
            filtered_rows: Vec::new(),
            library_index: HashMap::new(),
            most_played: Vec::new(),
            visuals: visuals::Visuals::new(),
            default_album: artwork::Artwork::new(),
            catalog_open: false,
            device_popup_open: false,
            hidden_tab_bars: HashSet::new(),
            target_group: None,
            dragging: None,
            measured: Rc::new(RefCell::new(HashMap::new())),
            analysis_enabled: false,
            analysis_sequence: 0,
            window_visible: true,
            seek_preview: None,
            seek_queue_id: None,
            _subscriptions: subscriptions,
            ui_font,
        };
        app.rebuild_library(cx);
        app.sync_analysis(cx);
        app
    }
    fn send(&mut self, command: Command, cx: &mut Context<Self>) {
        if let Err(error) = self.handle.send(command) {
            self.error = Some(format!("{error:#}"));
        }
        cx.notify();
    }
    fn input(&self, field: Field) -> Entity<Input> {
        self.inputs[&field].clone()
    }
    fn value(&self, field: Field, cx: &App) -> String {
        self.inputs[&field].read(cx).text().to_owned()
    }
    fn set_value(&mut self, field: Field, value: impl Into<SharedString>, cx: &mut Context<Self>) {
        let value = value.into();
        self.inputs[&field].update(cx, |input, cx| input.set_text(value, cx));
    }
    fn rebuild_library(&mut self, cx: &App) {
        self.library_index.clear();
        self.library_index.extend(
            self.state
                .library
                .iter()
                .enumerate()
                .map(|(index, track)| (track.id, index)),
        );
        self.selected
            .retain(|id| self.library_index.contains_key(id));
        self.most_played.clear();
        self.most_played.extend(
            self.state
                .library
                .iter()
                .enumerate()
                .filter_map(|(index, track)| (track.play_count > 0).then_some(index)),
        );
        self.most_played.sort_unstable_by_key(|&index| {
            (
                std::cmp::Reverse(self.state.library[index].play_count),
                self.state.library[index].id,
            )
        });
        self.refresh_filter(cx);
    }
    fn refresh_filter(&mut self, cx: &App) {
        let query = self.value(Field::Search, cx).to_lowercase();
        self.filtered_rows.clear();
        self.filtered_rows
            .extend(
                self.state
                    .library
                    .iter()
                    .enumerate()
                    .filter_map(|(index, track)| {
                        (query.is_empty()
                            || track.title.to_lowercase().contains(&query)
                            || track.artist.to_lowercase().contains(&query)
                            || track.album.to_lowercase().contains(&query))
                        .then_some(index)
                    }),
            );
    }
    fn select_track(&mut self, id: i64, multi: bool, cx: &mut Context<Self>) {
        if multi {
            if !self.selected.insert(id) {
                self.selected.remove(&id);
            }
        } else {
            self.selected.clear();
            self.selected.insert(id);
        }
        if let Some(&index) = self.library_index.get(&id) {
            let track = &self.state.library[index];
            let values = (
                track.title.clone(),
                track.artist.clone(),
                track.album.clone(),
            );
            self.metadata_track = Some(id);
            self.set_value(Field::Title, values.0, cx);
            self.set_value(Field::Artist, values.1, cx);
            self.set_value(Field::Album, values.2, cx);
        }
        cx.notify();
    }
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let state = self.handle.snapshot();
        let library_changed = !Arc::ptr_eq(&self.state.library, &state.library);
        if !matches!(self.dragging, Some(Dragging::Seek(_)))
            && self.seek_queue_id != state.current_queue_id
        {
            self.seek_preview = None;
            self.seek_queue_id = None;
        }
        self.state = state;
        if library_changed {
            self.rebuild_library(cx);
        }
        if self.state.shutting_down {
            cx.quit();
            return;
        }
        if self.handle.take_raise_request() {
            cx.activate(true);
        }
        self.sync_analysis(cx);
        if self.analysis_enabled {
            let frame = self.handle.analysis.read();
            if frame.sequence != self.analysis_sequence {
                self.analysis_sequence = frame.sequence;
                self.visuals.update(&frame);
            }
        }
        cx.notify();
    }
    fn sync_analysis(&mut self, cx: &mut Context<Self>) {
        let visible = self.window_visible
            && self
                .layout
                .active_panels()
                .iter()
                .any(|panel| matches!(panel.kind.as_str(), "spectrum" | "spectrogram"));
        if visible != self.analysis_enabled {
            self.analysis_enabled = visible;
            self.send(Command::Analysis { enabled: visible }, cx);
        }
    }
    fn persist_layout(&mut self, cx: &mut Context<Self>) {
        if let Err(error) = self.layout.save(&self.layout_path) {
            self.error = Some(format!("Saving workspace: {error:#}"));
        }
        self.sync_analysis(cx);
        cx.notify();
    }
    fn spectrum_panel(&mut self, id: u64, _: &mut Window, _: &mut Context<Self>) -> AnyElement {
        self.visuals.spectrum(id)
    }
    fn spectrogram_panel(&mut self, id: u64, _: &mut Window, _: &mut Context<Self>) -> AnyElement {
        self.visuals.spectrogram(id)
    }
    fn measurement(&self, key: Measured) -> AnyElement {
        let measured = self.measured.clone();
        canvas(
            move |bounds, _, _| {
                measured.borrow_mut().insert(key, bounds);
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full()
        .into_any_element()
    }
    fn dock_drop(
        &mut self,
        panel_id: u64,
        target: u64,
        edge: Edge,
        index: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        if self.layout.move_panel(panel_id, target, edge, index) {
            self.target_group = Some(target);
            self.persist_layout(cx);
        }
    }
    fn render_node(
        &mut self,
        node: &Node,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match node {
            Node::Split {
                id,
                axis,
                ratio,
                first,
                second,
            } => {
                let id = *id;
                let axis = *axis;
                let ratio = *ratio;
                let a = self.render_node(first, window, cx);
                let b = self.render_node(second, window, cx);
                let wrap = |child: AnyElement, fraction: f32| {
                    div()
                        .min_w_0()
                        .min_h_0()
                        .overflow_hidden()
                        .when(axis == Axis::Horizontal, |view| {
                            view.h_full().w(relative(fraction))
                        })
                        .when(axis == Axis::Vertical, |view| {
                            view.w_full().h(relative(fraction))
                        })
                        .child(child)
                };
                let gutter = div()
                    .id(("split", id))
                    .flex_shrink_0()
                    .bg(rgb(BG))
                    .hover(|style| style.bg(rgb(ACCENT)))
                    .when(axis == Axis::Horizontal, |view| {
                        view.w(px(6.)).h_full().cursor_col_resize()
                    })
                    .when(axis == Axis::Vertical, |view| {
                        view.h(px(6.)).w_full().cursor_row_resize()
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.dragging = Some(Dragging::Split(id, axis));
                            cx.stop_propagation();
                        }),
                    );
                div()
                    .id(("node", id))
                    .relative()
                    .size_full()
                    .flex()
                    .min_w_0()
                    .min_h_0()
                    .when(axis == Axis::Vertical, |view| view.flex_col())
                    .child(wrap(a, ratio))
                    .child(gutter)
                    .child(wrap(b, 1. - ratio))
                    .child(self.measurement(Measured::Node(id)))
                    .into_any_element()
            }
            Node::Tabs { id, panels, active } => {
                let node_id = *id;
                let active = *active;
                let tabs_hidden = self.hidden_tab_bars.contains(&node_id);
                let mut tabs = row()
                    .h(px(36.))
                    .flex_shrink_0()
                    .px_2()
                    .gap_1()
                    .border_b_1()
                    .border_color(rgb(BORDER));
                for (index, panel) in panels.iter().enumerate() {
                    let panel_id = panel.id;
                    let title = panel_spec(&panel.kind)
                        .map_or(panel.kind.as_str(), |spec| spec.title)
                        .to_owned();
                    let hide_tab_bar_on_right_click =
                        matches!(panel.kind.as_str(), "spectrum" | "spectrogram");
                    let drag = PanelDrag {
                        panel_id,
                        title: title.clone().into(),
                    };
                    tabs = tabs.child(
                        div()
                            .id(("tab", panel_id))
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .text_xs()
                            .cursor_move()
                            .bg(rgb(if active == panel_id { 0x29343b } else { PANEL }))
                            .text_color(rgb(if active == panel_id { TEXT } else { MUTED }))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.layout.activate(panel_id);
                                this.target_group = Some(node_id);
                                this.persist_layout(cx);
                            }))
                            .when(hide_tab_bar_on_right_click, |view| {
                                view.on_mouse_down(
                                    MouseButton::Right,
                                    cx.listener(move |this, _, window, cx| {
                                        window.prevent_default();
                                        cx.stop_propagation();
                                        this.hidden_tab_bars.insert(node_id);
                                        cx.notify();
                                    }),
                                )
                            })
                            .on_drag(drag, |drag, _, _, cx| cx.new(|_| drag.clone()))
                            .on_drop(cx.listener(move |this, drag: &PanelDrag, _, cx| {
                                this.dock_drop(
                                    drag.panel_id,
                                    node_id,
                                    Edge::Center,
                                    Some(index),
                                    cx,
                                )
                            }))
                            .child(title)
                            .child(
                                div()
                                    .id(("close", panel_id))
                                    .cursor_pointer()
                                    .px_1()
                                    .text_color(rgb(MUTED))
                                    .hover(|style| style.text_color(rgb(TEXT)))
                                    .child("×")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.layout.remove(panel_id);
                                        this.persist_layout(cx);
                                    })),
                            ),
                    );
                }
                let content = panels
                    .iter()
                    .find(|panel| panel.id == active)
                    .map(|panel| {
                        panel_spec(&panel.kind)
                            .map(|spec| (spec.render)(self, panel.id, window, cx))
                            .unwrap_or_else(|| {
                                div()
                                    .p_4()
                                    .child(format!("Unknown panel: {}", panel.kind))
                                    .into_any_element()
                            })
                    })
                    .unwrap_or_else(|| div().into_any_element());
                let mut content_area = div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .p_3()
                    .overflow_hidden()
                    .child(content);
                if tabs_hidden {
                    content_area = content_area.child(
                        icon_button(
                            ("show-tabs", node_id),
                            "▾",
                            "Show tab bar",
                            cx,
                            move |this, _, cx| {
                                this.hidden_tab_bars.remove(&node_id);
                                cx.notify();
                            },
                        )
                        .absolute()
                        .top_0()
                        .right_0(),
                    );
                }
                let mut panel = column()
                    .id(("group", node_id))
                    .relative()
                    .size_full()
                    .gap_0()
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .rounded_md()
                    .overflow_hidden();
                if !tabs_hidden {
                    panel = panel.child(tabs);
                }
                panel = panel
                    .child(content_area)
                    .on_drop(cx.listener(move |this, drag: &PanelDrag, window, cx| {
                        let position = window.mouse_position();
                        let bounds = this
                            .measured
                            .borrow()
                            .get(&Measured::Node(node_id))
                            .copied();
                        let edge = bounds
                            .map(|bounds| dock_edge(position, bounds))
                            .unwrap_or(Edge::Center);
                        this.dock_drop(drag.panel_id, node_id, edge, None, cx);
                    }))
                    .drag_over::<PanelDrag>(|style, _, _, _| style.border_color(rgb(ACCENT)))
                    .on_drag_move::<PanelDrag>(cx.listener(|_, _, _, cx| cx.notify()))
                    .when(cx.has_active_drag(), |view| {
                        view.child(
                            canvas(
                                |bounds, _, _| bounds,
                                |bounds, _, window, _| {
                                    let position = window.mouse_position();
                                    if !bounds.contains(&position) {
                                        return;
                                    }
                                    let mut preview = bounds;
                                    match dock_edge(position, bounds) {
                                        Edge::Left => preview.size.width *= 0.5,
                                        Edge::Right => {
                                            preview.origin.x += preview.size.width * 0.5;
                                            preview.size.width *= 0.5;
                                        }
                                        Edge::Top => preview.size.height *= 0.5,
                                        Edge::Bottom => {
                                            preview.origin.y += preview.size.height * 0.5;
                                            preview.size.height *= 0.5;
                                        }
                                        Edge::Center => {}
                                    }
                                    window.paint_quad(fill(preview, rgb(ACCENT).alpha(0.18)));
                                },
                            )
                            .absolute()
                            .size_full(),
                        )
                    })
                    .child(self.measurement(Measured::Node(node_id)));
                panel.into_any_element()
            }
        }
    }
    fn drag_position(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(dragging) = self.dragging else {
            return;
        };
        let key = match dragging {
            Dragging::Split(id, _) => Measured::Node(id),
            Dragging::Seek(id) => Measured::Seek(id),
            Dragging::Volume(id) => Measured::Volume(id),
        };
        let Some(bounds) = self.measured.borrow().get(&key).copied() else {
            return;
        };
        match dragging {
            Dragging::Split(id, axis) => {
                let (offset, length) = if axis == Axis::Horizontal {
                    (position.x - bounds.origin.x, bounds.size.width)
                } else {
                    (position.y - bounds.origin.y, bounds.size.height)
                };
                let minimum = if axis == Axis::Horizontal { 160. } else { 100. };
                let lower = (minimum / f32::from(length)).clamp(0.1, 0.45);
                let ratio = (offset / length).clamp(lower, 1. - lower);
                if self.layout.set_ratio(id, ratio) {
                    cx.notify();
                }
            }
            Dragging::Seek(_) => {
                if let Some(duration) = self.state.duration {
                    let fraction =
                        ((position.x - bounds.origin.x) / bounds.size.width).clamp(0., 1.);
                    self.seek_preview = Some(duration * fraction as f64);
                    cx.notify();
                }
            }
            Dragging::Volume(_) => {
                let value = ((position.x - bounds.origin.x) / bounds.size.width).clamp(0., 1.);
                self.send(Command::Volume { value }, cx);
            }
        }
    }
    fn finish_drag(&mut self, cx: &mut Context<Self>) {
        let dragging = self.dragging.take();
        if matches!(dragging, Some(Dragging::Seek(_))) {
            if let (Some(queue_id), Some(seconds)) =
                (self.seek_queue_id.take(), self.seek_preview.take())
            {
                self.send(Command::SeekQueue { queue_id, seconds }, cx);
            }
        } else {
            self.seek_preview = None;
            self.seek_queue_id = None;
        }
        if matches!(dragging, Some(Dragging::Split(..))) {
            self.persist_layout(cx);
        }
    }
    fn slider(
        &self,
        id: impl Into<ElementId>,
        value: f32,
        dragging: Dragging,
        key: Measured,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(id)
            .relative()
            .h(px(20.))
            .w_full()
            .min_w(px(60.))
            .cursor_pointer()
            .flex()
            .items_center()
            .child(
                div()
                    .w_full()
                    .h(px(4.))
                    .rounded_full()
                    .bg(rgb(BORDER))
                    .child(
                        div()
                            .h_full()
                            .w(relative(value.clamp(0., 1.)))
                            .rounded_full()
                            .bg(rgb(ACCENT)),
                    ),
            )
            .child(self.measurement(key))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.dragging = Some(dragging);
                    if matches!(dragging, Dragging::Seek(_)) {
                        this.seek_queue_id = this.state.current_queue_id;
                    }
                    this.drag_position(event.position, cx);
                }),
            )
            .into_any_element()
    }
}
impl Render for GuiApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        window.set_rem_size(px(16. * self.state.config.ui_scale));
        let root = self.layout.root.take();
        let workspace = root
            .as_ref()
            .map(|root| self.render_node(root, window, cx))
            .unwrap_or_else(|| {
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(MUTED))
                    .child("Add a panel to build your workspace")
                    .into_any_element()
            });
        self.layout.root = root;
        let mut app = column()
            .id("rivu")
            .relative()
            .size_full()
            .gap_0()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .text_size(px(14. * self.state.config.ui_scale))
            .font_family(self.ui_font.clone())
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                this.drag_position(event.position, cx)
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.finish_drag(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.finish_drag(cx)),
            )
            .child(div().flex_1().min_h_0().min_w_0().p_3().child(workspace));

        let error = self.error.clone().or_else(|| self.state.last_error.clone());
        let mut footer = row()
            .h(px(36.))
            .flex_shrink_0()
            .px_5()
            .py_1()
            .text_xs()
            .text_color(rgb(MUTED));
        if let Some(error) = error {
            footer = footer.child(
                row()
                    .flex_1()
                    .min_w_0()
                    .px_2()
                    .bg(rgb(0x422b2a))
                    .text_color(rgb(TEXT))
                    .child(div().flex_1().min_w_0().truncate().child(error))
                    .child(icon_button(
                        "dismiss-error",
                        "×",
                        "Dismiss message",
                        cx,
                        |this, _, cx| {
                            this.error = None;
                            this.send(Command::DismissError, cx);
                        },
                    )),
            );
        } else if self.state.scanning {
            footer = footer.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(rgb(TEXT))
                    .child(self.state.scan_message.clone()),
            );
        } else {
            footer = footer.child(div().flex_1());
        }
        footer = footer
            .child(icon_button(
                "catalog",
                "⊞",
                "Add panel",
                cx,
                |this, _, cx| {
                    this.catalog_open = !this.catalog_open;
                    cx.notify();
                },
            ))
            .child(icon_button(
                "settings",
                "⚙",
                "Settings",
                cx,
                |this, _, cx| {
                    this.load_settings(cx);
                    let id = this
                        .layout
                        .panels()
                        .into_iter()
                        .find(|panel| panel.kind == "settings")
                        .map(|panel| panel.id);
                    if let Some(id) = id {
                        this.layout.activate(id);
                    } else {
                        this.layout.add("settings", this.target_group);
                    }
                    this.persist_layout(cx);
                },
            ));
        app = app.child(footer);

        if self.catalog_open {
            let mut choices = row().flex_wrap().gap_1();
            for spec in PANELS {
                let kind = spec.kind;
                choices = choices.child(button(kind, spec.title, cx, move |this, _, cx| {
                    this.layout.add(kind, this.target_group);
                    this.catalog_open = false;
                    this.persist_layout(cx);
                }));
            }
            app = app.child(
                column()
                    .absolute()
                    .bottom(px(36.))
                    .right_0()
                    .max_w(px(560.))
                    .p_3()
                    .gap_1()
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .child(div().text_xs().text_color(rgb(MUTED)).child("Add panel"))
                    .child(choices),
            );
        }
        app
    }
}
impl Drop for GuiApp {
    fn drop(&mut self) {
        let _ = self.handle.send(Command::Analysis { enabled: false });
    }
}
