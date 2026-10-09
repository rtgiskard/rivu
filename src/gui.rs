mod artwork;
mod components;
mod fonts;
mod input;
mod layout;
mod library;
mod paged;
mod panels;
mod radial_spectrum;
mod scrolling;
mod settings;
mod visuals;
mod waveform;
use crate::{
    core::AppHandle,
    model::{Command, PlaybackStatus, Query, playback_key_command},
    projection::GuiSnapshot,
    tray::TrayController,
};
use crate::{
    model::{LibraryRow, LibrarySort, PlaylistEntryRow, PlaylistSummary},
    response::ViewResponse,
};
use anyhow::Result;
use ashpd::desktop::file_chooser::SelectedFiles;
pub(super) use components::{
    ButtonTooltip, DropdownItem, DropdownState, POPOVER_MAX_HEIGHT, SelectionModel, StatusKind,
    TRACK_HEIGHT, active_control, artwork_preview, button, caption, context_menu_container,
    drag_preview, dropdown_container, dropdown_row, dropdown_trigger, empty_state, form_field,
    icon_button, list_row, list_viewport, menu_item, metadata_item, panel_header, panel_surface,
    panel_toolbar, row_text, section_group, sortable_header, status_message, toggle_button,
    track_row, tree_row, two_column,
};
use futures::{FutureExt, StreamExt, channel::mpsc};
use gpui::{prelude::*, *};
use input::{Input, InputEvent};
use layout::{Axis, Edge, Layout, Node};
use library::LibraryNode;
use paged::PagedRows;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::Duration,
};
use url::Url;

const BG: u32 = 0x1a1b26;
const PANEL: u32 = 0x16161e;
const BORDER: u32 = 0x3b4261;
const TEXT: u32 = 0xc0caf5;
const MUTED: u32 = 0x565f89;
const ACCENT: u32 = 0x7aa2f7;
const HIGHLIGHT: u32 = 0x292e42;
const ERROR: u32 = 0xf7768e;
const ERROR_BG: u32 = 0x3b2330;
const UI_INSET: f32 = 10.0;
const SPLIT_GUTTER: f32 = 4.0;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Field {
    Search,
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
        title: "Recent tracks",
        render: GuiApp::history_panel,
    },
    PanelSpec {
        kind: "spectrum",
        title: "Spectrum",
        render: GuiApp::spectrum_panel,
    },
    PanelSpec {
        kind: "audio_visualizer",
        title: "Audio Visualizer",
        render: GuiApp::audio_visualizer_panel,
    },
    PanelSpec {
        kind: "waveform",
        title: "Waveform",
        render: GuiApp::waveform_panel,
    },
    PanelSpec {
        kind: "spectrogram",
        title: "Spectrogram",
        render: GuiApp::spectrogram_panel,
    },
];
fn panel_spec(kind: &str) -> Option<&'static PanelSpec> {
    PANELS.iter().find(|panel| panel.kind == kind)
}

struct GuiHost {
    handle: AppHandle,
    layout_path: PathBuf,
    window: Option<WindowHandle<GuiApp>>,
    tray: Option<TrayController>,
    error: Option<anyhow::Error>,
    quitting: bool,
}

impl GuiHost {
    fn show_window(&mut self, cx: &mut App) {
        if self.quitting {
            return;
        }
        cx.activate(true);
        if self.handle.is_shutting_down() {
            self.quit(cx);
            return;
        }
        if let Some(window) = self.window {
            if window
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
            {
                cx.activate(true);
                return;
            }
            // A close may have been processed before its observer ran.
            self.window_closed(window.window_id());
        }
        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        let handle = self.handle.clone();
        let layout_path = self.layout_path.clone();
        match cx.open_window(
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
        ) {
            Ok(window) => {
                self.window = Some(window);
                tracing::info!("gui_window_created");
                cx.activate(true);
            }
            Err(error) => {
                self.error = Some(error.context("Could not open Rivu window"));
                self.quit(cx);
            }
        }
    }

    fn handle_requests(&mut self, cx: &mut App) {
        if self.quitting {
            return;
        }
        if self.handle.is_shutting_down() {
            self.quit(cx);
        } else if self.handle.take_raise_request() {
            self.show_window(cx);
        }
    }

    fn window_closed(&mut self, id: WindowId) {
        if self.window.is_some_and(|window| window.window_id() == id) {
            self.window = None;
            self.clear_window_state();
        }
    }

    fn clear_window_state(&self) {
        self.handle.clear_wakeup();
        let _ = self.handle.send(Command::Analysis { enabled: false });
    }

    fn shutdown(&mut self) {
        if self.quitting {
            return;
        }
        self.quitting = true;
        self.handle.clear_gui_opener();
        if self.window.take().is_some() {
            self.clear_window_state();
        }
        self.tray.take();
    }

    fn quit(&mut self, cx: &mut App) {
        self.shutdown();
        cx.quit();
    }
}

impl Drop for GuiHost {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub fn run(handle: AppHandle, layout_path: PathBuf) -> Result<()> {
    let tray = if handle.config_snapshot().tray_enabled {
        TrayController::start(handle.clone())?
    } else {
        None
    };
    let host = Rc::new(RefCell::new(GuiHost {
        handle: handle.clone(),
        layout_path,
        window: None,
        tray,
        error: None,
        quitting: false,
    }));
    let app_host = host.clone();
    gpui_platform::application()
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx: &mut App| {
            Input::init(cx);
            let closed_host = app_host.clone();
            cx.on_window_closed(move |_, id| closed_host.borrow_mut().window_closed(id))
                .detach();
            let quit_host = app_host.clone();
            cx.on_app_quit(move |_| {
                quit_host.borrow_mut().shutdown();
                futures::future::ready(())
            })
            .detach();

            // Only Raise and core shutdown wake the host. Playback updates belong
            // to the window's separate receiver, which is removed on close.
            let (wake_sender, mut wake_receiver) = mpsc::channel::<()>(1);
            let wake_sender = parking_lot::Mutex::new(wake_sender);
            app_host.borrow().handle.set_gui_opener(move || {
                let _ = wake_sender.lock().try_send(());
            });
            let request_host = app_host.clone();
            cx.spawn(async move |cx| {
                while wake_receiver.next().await.is_some() {
                    cx.update(|cx| request_host.borrow_mut().handle_requests(cx));
                }
            })
            .detach();
            app_host.borrow_mut().show_window(cx);
        });
    let error = {
        let mut host = host.borrow_mut();
        host.shutdown();
        host.error.take()
    };
    if let Some(error) = error {
        return Err(error);
    }
    // Explicit platform quit also terminates the core. Do this after the GUI
    // loop, outside GPUI's short quit-observer deadline and without blocking UI.
    if !handle.is_shutting_down() {
        let response = handle.request_ack(Command::Shutdown);
        if !response.ok {
            anyhow::bail!(
                "{}",
                response
                    .error
                    .as_deref()
                    .unwrap_or("Could not shut down Rivu")
            );
        }
    }
    Ok(())
}

fn row() -> Div {
    div().flex().items_center().gap_2().min_w_0()
}
fn column() -> Div {
    div().flex().flex_col().min_w_0().min_h_0().gap_2()
}

fn chooser_path(files: &SelectedFiles) -> Option<PathBuf> {
    files
        .uris()
        .first()
        .and_then(|uri| Url::parse(uri.as_str()).ok())
        .and_then(|uri| uri.to_file_path().ok())
}

fn copyable_message(
    id: impl Into<ElementId>,
    message: String,
    cx: &mut Context<GuiApp>,
) -> Stateful<Div> {
    row()
        .id(id)
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .cursor_pointer()
        .child(div().flex_1().min_w_0().truncate().child(message.clone()))
        .child(div().flex_shrink_0().child("⧉"))
        .tooltip({
            let message: SharedString = message.clone().into();
            move |_, cx| {
                cx.new(|_| ButtonTooltip {
                    text: message.clone(),
                })
                .into()
            }
        })
        .on_click(cx.listener(move |_, _, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(message.clone()));
            cx.stop_propagation();
        }))
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
        drag_preview(self.title.clone())
    }
}

#[derive(Clone, Copy)]
pub(super) struct QueueDrag {
    pub(super) queue_id: u64,
}

impl Render for QueueDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        drag_preview("Move queue entry")
    }
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
enum Measured {
    Node(u64),
    Seek(u64),
    Volume(u64),
    Device,
    SettingsFont,
    SettingsStyle,
    SettingsFft,
    SettingsLogLevel,
    SettingsWindow,
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

#[derive(Clone, Copy)]
enum PanelMenuPage {
    Main,
    ConfirmClearQueue,
    Playlists,
}

#[derive(Clone, Copy)]
struct PanelMenu {
    panel_id: u64,
    tab_bar_visible: bool,
    position: Point<Pixels>,
    page: PanelMenuPage,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HistorySort {
    Recent,
    Title,
    Plays,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ListFocus {
    Library,
    Queue,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum ViewRequestKind {
    Library,
    Ranking,
    PlaylistSummaries,
    PlaylistEntries,
    Track,
    Stats,
}

#[derive(Clone, Copy)]
pub(super) enum DragDestination {
    Queue,
    Playlist(i64),
}
#[derive(Default)]
struct LibraryViewBuffer {
    query: Option<String>,
    favorite: Option<bool>,
    missing: Option<bool>,
    sort: LibrarySort,
    offset: usize,
    total: usize,
    rows: PagedRows<LibraryRow>,
    scroll: gpui::UniformListScrollHandle,
}

#[derive(Default)]
struct PlaylistSummaryBuffer {
    offset: usize,
    total: usize,
    rows: PagedRows<PlaylistSummary>,
    scroll: gpui::UniformListScrollHandle,
}

#[derive(Default)]
struct PlaylistEntriesBuffer {
    playlist_id: Option<i64>,
    offset: usize,
    total: usize,
    rows: PagedRows<PlaylistEntryRow>,
    scroll: gpui::UniformListScrollHandle,
}

pub(super) struct GuiApp {
    handle: AppHandle,
    state: GuiSnapshot,
    layout: Layout,
    layout_path: PathBuf,
    inputs: HashMap<Field, Entity<Input>>,
    selected: HashSet<i64>,
    selected_queue: SelectionModel<u64>,
    list_focus: Option<ListFocus>,
    layout_focus: FocusHandle,
    scroll_views: scrolling::ScrollViews,

    selected_playlist: Option<i64>,
    selected_entry: Option<i64>,
    metadata_track: Option<i64>,
    full_track: Option<crate::model::Track>,
    error: Option<String>,
    artwork_manager: Arc<crate::artwork::ArtworkManager>,
    queue_save_pending: bool,
    queue_save_feedback: Option<Result<String, String>>,
    library_buffer: LibraryViewBuffer,
    directory_tree: library::DirectoryTree,
    playlist_buffer: PlaylistSummaryBuffer,
    playlist_entries: PlaylistEntriesBuffer,
    selected_playlist_name: String,
    library_navigation: Option<usize>,
    library_tree_active: bool,
    library_selected_nodes: HashSet<LibraryNode>,
    library_selection: library::LibrarySelection,
    favorites_only: bool,
    missing_only: bool,
    force_scan: bool,
    view_generation: u64,
    view_revision: u64,
    request_generations: HashMap<ViewRequestKind, u64>,
    ranking_rows: Vec<LibraryRow>,
    ranking_total: usize,
    ranking_offset: usize,
    history_sort: HistorySort,
    history_sort_desc: bool,
    library_stats: Option<crate::model::LibraryStats>,
    settings: settings::Settings,
    visuals: visuals::Visuals,
    radial_spectrum: radial_spectrum::RadialSpectrum,
    default_album: Entity<artwork::Artwork>,
    catalog_open: bool,
    settings_open: bool,
    about_open: bool,
    about_focus: FocusHandle,
    app_menu_open: bool,
    panel_menu: Option<PanelMenu>,
    playlist_delete_confirm: Option<i64>,
    settings_focus: FocusHandle,
    waveform: Entity<waveform::Waveform>,
    target_group: Option<u64>,
    dragging: Option<Dragging>,
    seek_preview: Option<f64>,
    waveform_preview_active: bool,
    seek_queue_id: Option<u64>,
    analysis_worker_enabled: bool,
    measured: Rc<RefCell<HashMap<Measured, Bounds<Pixels>>>>,
    analysis_sequence: u64,
    window_visible: bool,
    ui_font: Font,
    _subscriptions: Vec<Subscription>,
}
impl GuiApp {
    fn view_page_size(&self) -> usize {
        self.state.system.config.page_size as usize
    }

    fn input_focused(&self, window: &Window, cx: &App) -> bool {
        self.inputs
            .values()
            .any(|input| input.read(cx).is_focused(window))
    }

    fn focus_layout(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.layout_focus.focus(window, cx);
    }

    fn activate_library(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        focus_search: bool,
    ) {
        if let Some(panel) = self
            .layout
            .panels()
            .into_iter()
            .find(|panel| panel.kind == "library")
        {
            self.layout.activate(panel.id);
            self.list_focus = Some(ListFocus::Library);
            self.catalog_open = false;
            if focus_search {
                self.input(Field::Search).update(cx, |input, cx| {
                    input.focus_handle(cx).focus(window, cx);
                });
            }
            self.persist_layout(cx);
        } else if self.layout.locked {
            self.error = Some(
                "Library panel is not in this locked layout; unlock it to add Library.".into(),
            );
            cx.notify();
        } else {
            let panel_id = self.layout.add("library", self.target_group);
            self.layout.activate(panel_id);
            self.list_focus = Some(ListFocus::Library);
            self.catalog_open = false;
            if focus_search {
                self.input(Field::Search).update(cx, |input, cx| {
                    input.focus_handle(cx).focus(window, cx);
                });
            }
            self.persist_layout(cx);
        }
    }

    fn active_list_focus(&self) -> Option<ListFocus> {
        self.list_focus.or_else(|| {
            self.layout
                .active_panels()
                .into_iter()
                .find_map(|panel| match panel.kind.as_str() {
                    "library" => Some(ListFocus::Library),
                    "queue" => Some(ListFocus::Queue),
                    _ => None,
                })
        })
    }

    fn directory_row_selected(&self, row: &crate::model::DirectoryRow) -> bool {
        match row {
            crate::model::DirectoryRow::Directory { path } => {
                self.library_selected_nodes.iter().any(|node| {
                    matches!(node, library::LibraryNode::Directory(selected) if selected.as_ref() == path.as_path())
                })
            }
            crate::model::DirectoryRow::Track(row) => self.selected.contains(&row.id),
        }
    }

    fn navigate_list(&mut self, focus: ListFocus, down: bool, cx: &mut Context<Self>) {
        match focus {
            ListFocus::Library => {
                self.invalidate_library_selection();
                if self.library_tree_active {
                    self.navigate_library_tree(down, cx);
                    return;
                }
                let total = self.library_buffer.total;
                if total == 0 {
                    return;
                }
                let current = self.library_navigation.or_else(|| {
                    self.library_buffer
                        .rows
                        .position(|row| self.selected.contains(&row.id))
                });
                let index = match current {
                    Some(index) if down => (index + 1).min(total - 1),
                    Some(index) => index.saturating_sub(1),
                    None if down => self.library_buffer.rows.visible().start.min(total - 1),
                    None => total - 1,
                };
                self.library_buffer
                    .scroll
                    .scroll_to_item(index, ScrollStrategy::Nearest);
                if let Some(row) = self.library_buffer.rows.get(index) {
                    self.library_navigation = None;
                    self.select_track(row.id, false, cx);
                } else {
                    self.library_navigation = Some(index);
                    self.ensure_library_range(index..index + 1, cx);
                }
            }
            ListFocus::Queue => {
                if self.state.queue.entries.is_empty() {
                    return;
                }
                let current = self
                    .selected_queue
                    .anchor()
                    .copied()
                    .filter(|id| self.selected_queue.contains(id))
                    .and_then(|id| {
                        self.state
                            .queue
                            .entries
                            .iter()
                            .position(|entry| entry.id == id)
                    })
                    .or_else(|| {
                        self.state
                            .queue
                            .entries
                            .iter()
                            .position(|entry| self.selected_queue.contains(&entry.id))
                    });
                let index = match current {
                    Some(index) if down => (index + 1).min(self.state.queue.entries.len() - 1),
                    Some(index) => index.saturating_sub(1),
                    None if down => 0,
                    None => self.state.queue.entries.len() - 1,
                };
                if let Some(entry) = self.state.queue.entries.get(index) {
                    self.select_queue_entry(entry.id, false, false, cx);
                }
            }
        }
    }

    fn play_focused_selection(&mut self, focus: ListFocus, cx: &mut Context<Self>) {
        match focus {
            ListFocus::Library => {
                if self.library_range_pending() {
                    return;
                }
                if self.library_tree_active {
                    self.activate_library_tree(cx);
                    return;
                }
                let track_id = self
                    .library_buffer
                    .rows
                    .iter()
                    .find(|row| self.selected.contains(&row.id))
                    .map(|row| row.id);
                if let Some(track_id) = track_id {
                    self.send(Command::Play { track_id }, cx);
                }
            }
            ListFocus::Queue => {
                let queue_id = self
                    .selected_queue
                    .anchor()
                    .copied()
                    .filter(|id| self.selected_queue.contains(id))
                    .or_else(|| {
                        self.state
                            .queue
                            .entries
                            .iter()
                            .find(|entry| self.selected_queue.contains(&entry.id))
                            .map(|entry| entry.id)
                    });
                if let Some(queue_id) = queue_id {
                    self.send(Command::PlayQueue { queue_id }, cx);
                }
            }
        }
    }

    fn remove_focused_queue(&mut self, focus: ListFocus, cx: &mut Context<Self>) {
        if focus != ListFocus::Queue {
            return;
        }
        let queue_ids = self
            .state
            .queue
            .entries
            .iter()
            .filter(|entry| self.selected_queue.contains(&entry.id))
            .map(|entry| entry.id)
            .collect::<Vec<_>>();
        if queue_ids.is_empty() {
            return;
        }
        self.selected_queue.clear();
        self.selected_queue.clear_anchor();
        self.send(Command::RemoveQueueEntries { queue_ids }, cx);
    }
    fn new(
        handle: AppHandle,
        layout_path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let state = handle.gui_snapshot();
        let (layout, error) = if layout_path.exists() {
            match Layout::load(&layout_path) {
                Ok(layout) => (layout, None),
                Err(error) => (
                    Layout::default(),
                    Some(format!("Could not load layout: {error:#}")),
                ),
            }
        } else {
            (Layout::default(), None)
        };
        let mut layout = layout;
        let settings_panels: Vec<_> = layout
            .panels()
            .into_iter()
            .filter(|panel| panel.kind == "settings")
            .map(|panel| panel.id)
            .collect();
        for id in settings_panels {
            layout.remove(id);
        }
        let ui_font = fonts::resolve(&state.system.config.ui_font, cx);
        let mut inputs = HashMap::new();
        for (field, placeholder) in [
            (Field::Search, "Search title, artist or album"),
            (Field::PlaylistName, "Playlist name"),
            (Field::Title, "Title"),
            (Field::Artist, "Artist"),
            (Field::Album, "Album"),
        ] {
            inputs.insert(field, cx.new(|cx| Input::new("", placeholder, cx)));
        }
        let mut subscriptions = Vec::new();
        let waveform = cx.new(|_| waveform::Waveform::new(Arc::clone(&handle.waveform)));
        subscriptions.push(cx.observe(&waveform, |_, _, cx| cx.notify()));
        subscriptions.push(cx.subscribe(&waveform, |this, _, event, cx| match event {
            waveform::WaveformEvent::Preview(seconds) => {
                if !this.state.system.config.waveform_seek_enabled {
                    return;
                }
                this.waveform_preview_active = true;
                this.seek_preview = Some(*seconds);
                cx.notify();
            }
            waveform::WaveformEvent::Seek(seconds) => {
                this.waveform_preview_active = false;
                this.seek_preview = None;
                if this.state.system.config.waveform_seek_enabled
                    && this.state.current_track().is_some()
                {
                    this.send(Command::Seek { seconds: *seconds }, cx);
                }
            }
        }));
        subscriptions.push(cx.subscribe(&inputs[&Field::Search], |this, _, event, cx| {
            if matches!(event, InputEvent::Changed) {
                this.refresh_filter(cx);
                cx.notify();
            }
        }));
        for field in [Field::Title, Field::Artist, Field::Album] {
            subscriptions.push(cx.subscribe(&inputs[&field], |_, _, event, cx| {
                if matches!(event, InputEvent::Changed) {
                    cx.notify();
                }
            }));
        }
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
                    (this.window_visible && this.analysis_worker_enabled && this.state.playback.status == PlaybackStatus::Playing)
                        .then(|| Duration::from_secs_f64(1. / this.state.system.config.analysis_fps as f64))
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
        let artwork_manager = Arc::new(crate::artwork::ArtworkManager::new(
            layout_path.parent().unwrap_or_else(|| Path::new(".")),
        ));
        let mut app = Self {
            settings: settings::Settings::new(&state.system.config),
            handle,
            state,
            layout,
            layout_path,
            inputs,
            selected: HashSet::new(),
            selected_queue: SelectionModel::default(),
            selected_playlist: None,
            selected_entry: None,
            list_focus: None,
            layout_focus: cx.focus_handle(),
            scroll_views: scrolling::ScrollViews::default(),
            metadata_track: None,
            full_track: None,
            error,
            queue_save_pending: false,
            queue_save_feedback: None,
            artwork_manager,
            library_buffer: LibraryViewBuffer {
                sort: LibrarySort::Album,
                ..Default::default()
            },
            directory_tree: library::DirectoryTree::default(),
            playlist_buffer: PlaylistSummaryBuffer::default(),
            playlist_entries: PlaylistEntriesBuffer::default(),
            selected_playlist_name: String::new(),
            library_navigation: None,
            library_tree_active: true,
            library_selected_nodes: HashSet::new(),
            library_selection: library::LibrarySelection::default(),
            favorites_only: false,
            missing_only: false,
            force_scan: false,
            view_generation: 0,
            view_revision: 0,
            request_generations: HashMap::new(),
            ranking_rows: Vec::new(),
            ranking_total: 0,
            ranking_offset: 0,
            history_sort: HistorySort::Recent,
            history_sort_desc: true,
            library_stats: None,
            visuals: visuals::Visuals::new(),
            radial_spectrum: radial_spectrum::RadialSpectrum::new(),
            default_album: cx.new(|_| artwork::Artwork::new()),
            catalog_open: false,
            settings_open: false,
            about_open: false,
            about_focus: cx.focus_handle(),
            app_menu_open: false,
            panel_menu: None,
            playlist_delete_confirm: None,
            settings_focus: cx.focus_handle(),
            waveform,
            target_group: None,
            dragging: None,
            analysis_worker_enabled: false,
            measured: Rc::new(RefCell::new(HashMap::new())),
            analysis_sequence: 0,
            window_visible: true,
            seek_preview: None,
            waveform_preview_active: false,
            seek_queue_id: None,
            _subscriptions: subscriptions,
            ui_font,
        };
        app.sync_metadata_default(cx);
        app.request_library_page(0, cx);
        app.request_directory_page(library::TreeBranch::default(), 0, cx);
        app.request_playlist_summaries(0, cx);
        app.request_ranking(0, cx);
        app.sync_analysis(cx);
        // Initialization may have completed before the wakeup callback was
        // installed. Reconcile once so the first frame never keeps the empty snapshot.
        app.refresh(cx);
        app.layout_focus.focus(window, cx);
        app
    }
    fn send(&mut self, command: Command, cx: &mut Context<Self>) {
        if let Err(error) = self.handle.send(command) {
            self.error = Some(format!("{error:#}"));
        }
        cx.notify();
    }
    fn save_queue_as_playlist(&mut self, cx: &mut Context<Self>) {
        if self.queue_save_pending || self.state.queue.entries.is_empty() {
            return;
        }
        let track_ids = self
            .state
            .queue
            .entries
            .iter()
            .map(|entry| entry.track_id)
            .collect();
        let name = format!("Queue {}", chrono::Local::now().format("%Y-%m-%d %H-%M-%S"));
        self.queue_save_pending = true;
        self.queue_save_feedback = None;
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            let ack = handle.request_ack(Command::CreatePlaylistWithTracks { name, track_ids });
            let _ = this.update(cx, |this, cx| {
                this.queue_save_pending = false;
                this.queue_save_feedback = Some(if ack.ok {
                    Ok("Saved queue as playlist".to_owned())
                } else {
                    Err(ack
                        .error
                        .unwrap_or_else(|| "Could not save queue as playlist".to_owned()))
                });
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn choose_playlist_import(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let result = SelectedFiles::open_file()
                .title("Import M3U playlist")
                .accept_label("Import")
                .modal(true)
                .send()
                .await
                .and_then(|request| request.response());
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(files) => {
                        if let Some(path) = chooser_path(&files) {
                            this.send(Command::ImportPlaylist { path, name: None }, cx);
                        }
                    }
                    Err(error) => this.error = Some(format!("Opening playlist: {error}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn choose_playlist_export(&mut self, playlist_id: i64, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let result = SelectedFiles::save_file()
                .title("Export M3U playlist")
                .accept_label("Export")
                .modal(true)
                .send()
                .await
                .and_then(|request| request.response());
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(files) => {
                        if let Some(path) = chooser_path(&files) {
                            this.send(Command::ExportPlaylist { playlist_id, path }, cx);
                        }
                    }
                    Err(error) => {
                        this.error = Some(format!("Opening playlist destination: {error}"))
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn choose_library_root(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let result = SelectedFiles::open_file()
                .title("Add library folder")
                .accept_label("Add")
                .directory(true)
                .modal(true)
                .send()
                .await
                .and_then(|request| request.response());
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(files) => {
                        if let Some(path) = chooser_path(&files) {
                            let mut config = this.handle.config_snapshot().as_ref().clone();
                            if !config.library_roots.iter().any(|root| root == &path) {
                                config.library_roots.push(path);
                                this.send(Command::Configure { config }, cx);
                            }
                        }
                    }
                    Err(error) => this.error = Some(format!("Opening library folder: {error}")),
                }
                cx.notify();
            });
        })
        .detach();
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
    fn next_view_generation(&mut self) -> u64 {
        self.view_generation = self.view_generation.wrapping_add(1);
        self.view_generation
    }

    fn request_view(&mut self, query: Query, generation: u64, cx: &mut Context<Self>) {
        let kind = match &query {
            Query::LibraryPage {
                sort: LibrarySort::MostPlayed,
                ..
            } => ViewRequestKind::Ranking,
            Query::LibraryPage { .. } => ViewRequestKind::Library,
            Query::PlaylistSummaries { .. } => ViewRequestKind::PlaylistSummaries,
            Query::PlaylistEntries { .. } => ViewRequestKind::PlaylistEntries,
            Query::Track { .. } => ViewRequestKind::Track,
            Query::LibraryStats => ViewRequestKind::Stats,
            _ => return,
        };
        self.request_generations.insert(kind, generation);
        let delay = if matches!(&query, Query::LibraryPage { query: Some(_), .. }) {
            crate::model::SEARCH_DEBOUNCE
        } else {
            Duration::from_millis(80)
        };
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            // Search waits for a quiet interval; generations reject superseded queries.
            cx.background_executor().timer(delay).await;
            if !this
                .update(cx, |this, _| {
                    this.request_generations.get(&kind) == Some(&generation)
                })
                .unwrap_or(false)
            {
                return;
            }
            let response = cx
                .background_executor()
                .spawn(async move { handle.query(query) })
                .await;
            let revisions = response.revisions;
            let result = response.result;
            let _ = this.update(cx, |this, cx| {
                if this.request_generations.get(&kind) != Some(&generation) {
                    return;
                }
                let result = match result {
                    Ok(result) => result,
                    Err(error) => {
                        let message = error.to_string();
                        match kind {
                            ViewRequestKind::Library => {
                                this.library_buffer.rows.fail(message.clone())
                            }
                            ViewRequestKind::PlaylistSummaries => {
                                this.playlist_buffer.rows.fail(message.clone())
                            }
                            ViewRequestKind::PlaylistEntries => {
                                this.playlist_entries.rows.fail(message.clone())
                            }
                            _ => {}
                        }
                        this.error = Some(message);
                        cx.notify();
                        return;
                    }
                };
                if matches!(
                    kind,
                    ViewRequestKind::Library
                        | ViewRequestKind::Ranking
                        | ViewRequestKind::Track
                        | ViewRequestKind::Stats
                ) && revisions.library != this.state.library.revision
                {
                    match kind {
                        ViewRequestKind::Library => {
                            let navigation = this.library_navigation;
                            this.request_library_page(this.library_buffer.offset, cx);
                            this.library_navigation = navigation;
                        }
                        ViewRequestKind::Track => {
                            if let Some(id) = this.metadata_track {
                                this.request_track(id, cx);
                            }
                        }
                        ViewRequestKind::Ranking | ViewRequestKind::Stats => {
                            this.request_ranking(this.ranking_offset, cx)
                        }
                        _ => {}
                    }
                    return;
                }
                if matches!(
                    kind,
                    ViewRequestKind::PlaylistSummaries | ViewRequestKind::PlaylistEntries
                ) && revisions.playlist != this.state.library.playlist_revision
                {
                    match kind {
                        ViewRequestKind::PlaylistSummaries => {
                            this.request_playlist_summaries(this.playlist_buffer.offset, cx)
                        }
                        ViewRequestKind::PlaylistEntries => {
                            if let Some(id) = this.playlist_entries.playlist_id {
                                this.request_playlist_entries(id, this.playlist_entries.offset, cx);
                            }
                        }
                        _ => {}
                    }
                    return;
                }
                match result {
                    ViewResponse::LibraryPage(page) if kind == ViewRequestKind::Ranking => {
                        if this.ranking_offset > 0 && this.ranking_offset >= page.total {
                            let last = page.total.saturating_sub(1) / this.view_page_size()
                                * this.view_page_size();
                            this.request_ranking(last, cx);
                            return;
                        }
                        this.ranking_total = page.total;
                        this.ranking_rows = page.rows;
                    }
                    ViewResponse::LibraryPage(page) if kind == ViewRequestKind::Library => {
                        this.library_buffer.total = page.total;
                        this.library_buffer.rows.accept(page);
                        if let Some(index) = this
                            .library_buffer
                            .rows
                            .position(|row| this.selected.contains(&row.id))
                        {
                            this.library_buffer.rows.pin(Some(index));
                        }
                        if let Some(index) = this.library_navigation {
                            if let Some(id) = this.library_buffer.rows.get(index).map(|row| row.id)
                            {
                                this.library_navigation = None;
                                this.select_track(id, false, cx);
                            } else if index >= this.library_buffer.total {
                                this.library_navigation = None;
                            } else {
                                this.ensure_library_range(index..index + 1, cx);
                            }
                        }
                    }
                    ViewResponse::PlaylistSummaries(page)
                        if kind == ViewRequestKind::PlaylistSummaries =>
                    {
                        this.playlist_buffer.total = page.total;
                        this.playlist_buffer.rows.accept(page);
                        if let Some(index) = this
                            .playlist_buffer
                            .rows
                            .position(|row| Some(row.id) == this.selected_playlist)
                        {
                            this.playlist_buffer.rows.pin(Some(index));
                        }
                        if let Some(summary) = this
                            .playlist_buffer
                            .rows
                            .iter()
                            .find(|row| Some(row.id) == this.selected_playlist)
                        {
                            this.selected_playlist_name = summary.name.clone();
                        }
                    }
                    ViewResponse::PlaylistEntries(page)
                        if kind == ViewRequestKind::PlaylistEntries =>
                    {
                        this.playlist_entries.total = page.total;
                        this.playlist_entries.rows.accept(page);
                        if let Some(index) = this
                            .playlist_entries
                            .rows
                            .position(|row| Some(row.id) == this.selected_entry)
                        {
                            this.playlist_entries.rows.pin(Some(index));
                        }
                    }
                    ViewResponse::Track(track) if kind == ViewRequestKind::Track => {
                        let draft_dirty = this.full_track.as_ref().is_some_and(|current| {
                            this.value(Field::Title, cx) != current.title
                                || this.value(Field::Artist, cx) != current.artist
                                || this.value(Field::Album, cx) != current.album
                        });
                        if !draft_dirty {
                            if let Some(track) = track.as_ref() {
                                this.set_value(Field::Title, track.title.clone(), cx);
                                this.set_value(Field::Artist, track.artist.clone(), cx);
                                this.set_value(Field::Album, track.album.clone(), cx);
                            }
                        }
                        this.full_track = track;
                        this.sync_waveform(cx);
                    }
                    ViewResponse::LibraryStats(stats) if kind == ViewRequestKind::Stats => {
                        this.library_stats = Some(stats)
                    }
                    _ => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn request_library_page(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.invalidate_library_selection();
        let query = self.value(Field::Search, cx).trim().to_lowercase();
        self.library_buffer.query = (!query.is_empty()).then_some(query);
        self.library_buffer.favorite = self.favorites_only.then_some(true);
        self.library_buffer.missing = self.missing_only.then_some(true);
        self.library_buffer.rows.invalidate();
        self.load_library_page(offset, cx);
    }

    fn load_library_page(&mut self, offset: usize, cx: &mut Context<Self>) {
        let page_size = self.view_page_size();
        let offset = offset / page_size * page_size;
        self.library_buffer.offset = offset;
        self.library_buffer.rows.begin(offset, page_size);
        let generation = self.next_view_generation();
        self.request_view(
            Query::LibraryPage {
                query: self.library_buffer.query.clone(),
                favorite: self.library_buffer.favorite,
                missing: self.library_buffer.missing,
                sort: self.library_buffer.sort,
                offset,
                limit: page_size,
            },
            generation,
            cx,
        );
    }

    fn request_directory_page(
        &mut self,
        branch: library::TreeBranch,
        offset: usize,
        cx: &mut Context<Self>,
    ) {
        let page_size = self.view_page_size();
        self.directory_tree.configure_page_size(page_size);
        let offset = offset / page_size * page_size;
        let generation = self.next_view_generation();
        self.directory_tree
            .begin(branch.clone(), offset, generation);
        if branch.is_forest() {
            let roots = &self.state.system.config.library_roots;
            let page = crate::model::Page::<crate::model::DirectoryRow> {
                total: roots.len(),
                rows: roots
                    .iter()
                    .skip(offset)
                    .take(page_size)
                    .map(|path| crate::model::DirectoryRow::Directory { path: path.clone() })
                    .collect(),
            };
            self.directory_tree.accept(&branch, generation, page);
            cx.notify();
            return;
        }
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            let request_path = branch.path().to_path_buf();
            let response = cx
                .background_executor()
                .spawn(async move {
                    handle.query(Query::DirectoryPage {
                        path: request_path,
                        offset,
                        limit: page_size,
                    })
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !this.directory_tree.request_pending(&branch, generation) {
                    return;
                }
                let result = match response.result {
                    Ok(result) => result,
                    Err(error) => {
                        this.directory_tree.fail(&branch, generation);
                        this.error = Some(error.to_string());
                        cx.notify();
                        return;
                    }
                };
                if response.revisions.structure != this.state.library.structure_revision {
                    this.request_directory_page(branch, offset, cx);
                    return;
                }
                if let ViewResponse::DirectoryPage(page) = result {
                    let projection = this.directory_tree.projection_revision();
                    this.directory_tree.accept(&branch, generation, page);
                    if this.directory_tree.projection_revision() != projection {
                        this.invalidate_library_selection();
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn request_playlist_summaries(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.playlist_buffer.rows.invalidate();
        self.load_playlist_summaries(offset, cx);
    }

    fn load_playlist_summaries(&mut self, offset: usize, cx: &mut Context<Self>) {
        let page_size = self.view_page_size();
        let offset = offset / page_size * page_size;
        self.playlist_buffer.offset = offset;
        self.playlist_buffer.rows.begin(offset, page_size);
        let generation = self.next_view_generation();
        self.request_view(
            Query::PlaylistSummaries {
                offset,
                limit: page_size,
            },
            generation,
            cx,
        );
    }

    fn request_playlist_entries(
        &mut self,
        playlist_id: i64,
        offset: usize,
        cx: &mut Context<Self>,
    ) {
        if self.playlist_entries.playlist_id != Some(playlist_id) {
            self.selected_entry = None;
            self.clear_metadata_selection(cx);
            self.playlist_entries.rows.clear();
            self.playlist_entries.total = 0;
            self.playlist_entries
                .scroll
                .scroll_to_item(0, ScrollStrategy::Top);
        } else {
            self.playlist_entries.rows.invalidate();
        }
        self.playlist_entries.playlist_id = Some(playlist_id);
        self.load_playlist_entries(playlist_id, offset, cx);
    }

    fn load_playlist_entries(&mut self, playlist_id: i64, offset: usize, cx: &mut Context<Self>) {
        let page_size = self.view_page_size();
        let offset = offset / page_size * page_size;
        self.playlist_entries.offset = offset;
        self.playlist_entries.rows.begin(offset, page_size);
        let generation = self.next_view_generation();
        self.request_view(
            Query::PlaylistEntries {
                playlist_id,
                offset,
                limit: page_size,
            },
            generation,
            cx,
        );
    }

    fn ensure_library_range(&mut self, range: std::ops::Range<usize>, cx: &mut Context<Self>) {
        if let Some(offset) = self
            .library_buffer
            .rows
            .demand(range, self.view_page_size())
        {
            self.load_library_page(offset, cx);
        }
    }
    fn ensure_playlist_range(&mut self, range: std::ops::Range<usize>, cx: &mut Context<Self>) {
        if let Some(offset) = self
            .playlist_buffer
            .rows
            .demand(range, self.view_page_size())
        {
            self.load_playlist_summaries(offset, cx);
        }
    }
    fn ensure_entry_range(&mut self, range: std::ops::Range<usize>, cx: &mut Context<Self>) {
        if let Some(id) = self.playlist_entries.playlist_id {
            if let Some(offset) = self
                .playlist_entries
                .rows
                .demand(range, self.view_page_size())
            {
                self.load_playlist_entries(id, offset, cx);
            }
        }
    }

    fn paged_loading_row(
        &self,
        kind: ViewRequestKind,
        index: usize,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let error = match kind {
            ViewRequestKind::Library => self.library_buffer.rows.error(index),
            ViewRequestKind::PlaylistSummaries => self.playlist_buffer.rows.error(index),
            ViewRequestKind::PlaylistEntries => self.playlist_entries.rows.error(index),
            _ => None,
        };
        list_row(("loading-row", index), false)
            .child(caption(if error.is_some() {
                "Loading failed · click to retry"
            } else {
                "Loading…"
            }))
            .when_some(error.map(str::to_owned), |row, error| {
                row.tooltip(move |_, cx| {
                    cx.new(|_| ButtonTooltip {
                        text: error.clone().into(),
                    })
                    .into()
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    match kind {
                        ViewRequestKind::Library => {
                            this.library_buffer.rows.retry();
                            this.ensure_library_range(this.library_buffer.rows.visible(), cx);
                        }
                        ViewRequestKind::PlaylistSummaries => {
                            this.playlist_buffer.rows.retry();
                            this.ensure_playlist_range(this.playlist_buffer.rows.visible(), cx);
                        }
                        ViewRequestKind::PlaylistEntries => {
                            this.playlist_entries.rows.retry();
                            this.ensure_entry_range(this.playlist_entries.rows.visible(), cx);
                        }
                        _ => {}
                    }
                    cx.notify();
                }))
            })
    }

    fn request_track(&mut self, id: i64, cx: &mut Context<Self>) {
        let generation = self.next_view_generation();
        if self.metadata_track != Some(id) {
            self.full_track = None;
        }
        self.metadata_track = Some(id);
        self.request_view(Query::Track { track_id: id }, generation, cx);
    }
    pub(super) fn clear_metadata_selection(&mut self, cx: &mut Context<Self>) {
        self.metadata_track = None;
        self.full_track = None;
        self.request_generations.remove(&ViewRequestKind::Track);
        self.sync_metadata_default(cx);
        self.sync_waveform(cx);
    }

    fn toggle_library_view(&mut self, cx: &mut Context<Self>) {
        self.invalidate_library_selection();
        self.library_tree_active = !self.library_tree_active;
        if self.library_tree_active {
            if self
                .directory_tree
                .generation(&library::TreeBranch::default())
                .is_none()
            {
                self.request_directory_page(library::TreeBranch::default(), 0, cx);
            }
        }
        cx.notify();
    }

    fn refresh_filter(&mut self, cx: &mut Context<Self>) {
        // Filters apply to SQL track queries, not locally reconstructed folders.
        self.library_tree_active = false;
        self.library_navigation = None;
        self.library_buffer.rows.clear();
        self.library_buffer.total = 0;
        self.library_buffer
            .scroll
            .scroll_to_item(0, ScrollStrategy::Top);
        self.request_library_page(0, cx);
    }
    fn request_ranking(&mut self, offset: usize, cx: &mut Context<Self>) {
        let page_size = self.view_page_size();
        let offset = offset / page_size * page_size;
        self.ranking_offset = offset;
        self.ranking_rows.clear();
        let generation = self.next_view_generation();
        self.request_view(
            Query::LibraryPage {
                query: None,
                favorite: None,
                missing: None,
                sort: LibrarySort::MostPlayed,
                offset,
                limit: page_size,
            },
            generation,
            cx,
        );
        let generation = self.next_view_generation();
        self.request_view(Query::LibraryStats, generation, cx);
    }

    fn update_metadata_track(&mut self, id: i64, cx: &mut Context<Self>) {
        self.request_track(id, cx);
    }

    fn sync_metadata_default(&mut self, cx: &mut Context<Self>) {
        if self.metadata_track.is_some() {
            return;
        }
        self.request_generations.remove(&ViewRequestKind::Track);
        let current = self.state.current_track();
        if self.full_track.as_ref() == current {
            return;
        }
        self.full_track = current.cloned();
        let (title, artist, album) = self
            .full_track
            .as_ref()
            .map(|track| {
                (
                    track.title.clone(),
                    track.artist.clone(),
                    track.album.clone(),
                )
            })
            .unwrap_or_default();
        self.set_value(Field::Title, title, cx);
        self.set_value(Field::Artist, artist, cx);
        self.set_value(Field::Album, album, cx);
    }

    fn select_track(&mut self, id: i64, multi: bool, cx: &mut Context<Self>) {
        self.invalidate_library_selection();
        self.library_navigation = None;
        self.directory_tree.cancel_navigation();
        if multi {
            if !self.selected.insert(id) {
                self.selected.remove(&id);
            }
        } else {
            self.selected.clear();
            self.library_selected_nodes.clear();
            self.selected.insert(id);
        }
        self.directory_tree
            .remember_selection(&self.library_selected_nodes, &self.selected);
        if !self.library_tree_active
            && let Some(index) = self.library_buffer.rows.position(|row| row.id == id)
        {
            self.library_buffer.rows.pin(Some(index));
            self.library_buffer
                .scroll
                .scroll_to_item(index, gpui::ScrollStrategy::Nearest);
        }
        if let Some(selected) = self
            .selected
            .contains(&id)
            .then_some(id)
            .or_else(|| self.selected.iter().copied().min())
        {
            self.update_metadata_track(selected, cx);
        } else {
            self.metadata_track = None;
            self.full_track = None;
            self.sync_metadata_default(cx);
        }
        self.sync_waveform(cx);
        cx.notify();
    }
    fn select_queue_entry(&mut self, id: u64, shift: bool, multi: bool, cx: &mut Context<Self>) {
        self.invalidate_library_selection();
        let Some(index) = self
            .state
            .queue
            .entries
            .iter()
            .position(|entry| entry.id == id)
        else {
            return;
        };
        let track_id = self.state.queue.entries[index].track_id;
        if shift {
            let anchor = self
                .selected_queue
                .anchor()
                .copied()
                .filter(|anchor| {
                    self.state
                        .queue
                        .entries
                        .iter()
                        .any(|entry| entry.id == *anchor)
                })
                .unwrap_or(id);
            let Some(anchor_index) = self
                .state
                .queue
                .entries
                .iter()
                .position(|entry| entry.id == anchor)
            else {
                return;
            };
            let (start, end) = if anchor_index <= index {
                (anchor_index, index)
            } else {
                (index, anchor_index)
            };
            if !multi {
                self.selected_queue.clear();
            }
            self.selected_queue.extend(
                self.state.queue.entries[start..=end]
                    .iter()
                    .map(|entry| entry.id),
            );
            self.selected_queue.set_anchor(anchor);
        } else if multi {
            if !self.selected_queue.insert(id) {
                self.selected_queue.remove(&id);
            }
            self.selected_queue.set_anchor(id);
        } else {
            self.selected_queue.clear();
            self.selected_queue.insert(id);
            self.selected_queue.set_anchor(id);
        }
        self.update_metadata_track(track_id, cx);
        self.sync_waveform(cx);
        cx.notify();
    }
    fn reconcile_playlist_selection(&mut self) {
        let Some(playlist_id) = self.selected_playlist else {
            self.selected_entry = None;
            self.playlist_delete_confirm = None;
            return;
        };
        // Cache absence is not deletion. A selection outlives ordinary scrolling/eviction.
        if self.playlist_entries.playlist_id != Some(playlist_id) {
            self.selected_entry = None;
        }
    }

    fn refresh_page_size(&mut self, cx: &mut Context<Self>) {
        // Invalidate query tokens and payload, not identity, metadata drafts, or scroll position.
        self.request_generations
            .retain(|kind, _| *kind == ViewRequestKind::Track);
        self.request_library_page(self.library_buffer.rows.visible().start, cx);
        self.directory_tree
            .configure_page_size(self.view_page_size());
        self.directory_tree.invalidate();
        self.request_directory_page(library::TreeBranch::default(), 0, cx);
        self.request_playlist_summaries(self.playlist_buffer.rows.visible().start, cx);
        if let Some(id) = self.playlist_entries.playlist_id {
            self.request_playlist_entries(id, self.playlist_entries.rows.visible().start, cx);
        }
        self.request_ranking(0, cx);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let state = (self.handle.revision() != self.state.system.revision)
            .then(|| self.handle.gui_snapshot());
        let mut changed = state.is_some();
        if let Some(state) = state {
            let library_changed = self.state.library.revision != state.library.revision;
            let library_structure_changed =
                self.state.library.structure_revision != state.library.structure_revision;
            let playlist_changed =
                self.state.library.playlist_revision != state.library.playlist_revision;
            let queue_changed = !Arc::ptr_eq(&self.state.queue.entries, &state.queue.entries);
            let roots_changed =
                self.state.system.config.library_roots != state.system.config.library_roots;
            let page_size_changed =
                self.state.system.config.page_size != state.system.config.page_size;
            if library_changed || library_structure_changed || roots_changed || page_size_changed {
                self.invalidate_library_selection();
            }
            let waveform_seek_disabled = self.state.system.config.waveform_seek_enabled
                && !state.system.config.waveform_seek_enabled;
            if !matches!(self.dragging, Some(Dragging::Seek(_)))
                && !self.waveform_preview_active
                && self.seek_queue_id != state.queue.current_id
            {
                self.seek_preview = None;
                self.seek_queue_id = None;
            }
            if self.state.system.config.ui_font != state.system.config.ui_font {
                self.ui_font = fonts::resolve(&state.system.config.ui_font, cx);
            }
            self.state = state;
            self.sync_metadata_default(cx);
            self.reconcile_playlist_selection();
            if waveform_seek_disabled {
                self.waveform_preview_active = false;
                self.seek_preview = None;
                self.waveform
                    .update(cx, |waveform, _| waveform.set_seek_enabled(false));
            }
            if roots_changed {
                self.directory_tree.clear();
                self.library_selected_nodes.clear();
                if !page_size_changed {
                    self.request_directory_page(library::TreeBranch::default(), 0, cx);
                }
            }
            if page_size_changed {
                self.refresh_page_size(cx);
            }
            if queue_changed {
                let valid = self
                    .state
                    .queue
                    .entries
                    .iter()
                    .map(|entry| entry.id)
                    .collect::<HashSet<_>>();
                self.selected_queue.retain(|id| valid.contains(id));
                self.selected_queue.clear_anchor();
            }
            if library_changed || library_structure_changed {
                self.view_revision = self.state.library.revision;
                if !page_size_changed {
                    self.request_library_page(self.library_buffer.rows.visible().start, cx);
                    self.request_ranking(self.ranking_offset, cx);
                    self.directory_tree.invalidate();
                    self.request_directory_page(library::TreeBranch::default(), 0, cx);
                }
                if let Some(id) = self.metadata_track {
                    self.request_track(id, cx);
                }
            }
            if !page_size_changed
                && (library_changed || library_structure_changed || playlist_changed)
            {
                self.request_playlist_summaries(self.playlist_buffer.rows.visible().start, cx);
                if let Some(id) = self.playlist_entries.playlist_id {
                    self.request_playlist_entries(
                        id,
                        self.playlist_entries.rows.visible().start,
                        cx,
                    );
                }
            }
        }
        if self.state.system.shutting_down {
            return;
        }
        if changed {
            self.sync_analysis(cx);
        }
        if self.analysis_worker_enabled {
            let frame = self.handle.analysis.read();
            if frame.sequence != self.analysis_sequence {
                self.analysis_sequence = frame.sequence;
                self.visuals.update(&frame);
                self.radial_spectrum.update(&frame);
                changed = true;
            }
        }
        if changed {
            cx.notify();
        }
    }
    fn sync_analysis(&mut self, cx: &mut Context<Self>) {
        let active_panels = self.layout.active_panels();
        let spectrogram_panel_ids = active_panels
            .iter()
            .filter(|panel| panel.kind == "spectrogram")
            .map(|panel| panel.id)
            .collect::<Vec<_>>();
        let spectrum_panel_ids = active_panels
            .iter()
            .filter(|panel| panel.kind == "spectrum")
            .map(|panel| panel.id)
            .collect::<Vec<_>>();
        self.visuals
            .retain_spectrogram_panels(&spectrogram_panel_ids);
        self.visuals.retain_spectrum_panels(&spectrum_panel_ids);
        let analysis_visible = self.window_visible
            && active_panels.iter().any(|panel| {
                matches!(
                    panel.kind.as_str(),
                    "spectrum" | "spectrogram" | "audio_visualizer"
                )
            });
        if analysis_visible {
            self.visuals.configure(&self.state.system.config);
            self.radial_spectrum.configure(&self.state.system.config);
        }
        if analysis_visible != self.analysis_worker_enabled {
            self.analysis_worker_enabled = analysis_visible;
            self.send(
                Command::Analysis {
                    enabled: analysis_visible,
                },
                cx,
            );
        }
        self.sync_waveform(cx);
    }

    fn sync_waveform(&mut self, cx: &mut Context<Self>) {
        let panels = self.layout.active_panels();
        let track = self.state.current_track().or(self.full_track.as_ref());
        self.waveform.update(cx, |waveform, cx| {
            waveform.retain_panels(&panels);
            waveform.sync(track, cx);
        });
    }
    fn load_full_waveform(&mut self, cx: &mut Context<Self>) {
        let track = self.state.current_track().or(self.full_track.as_ref());
        self.waveform.update(cx, |waveform, cx| {
            waveform.load_full(track, &self.state.system.config, cx);
        });
    }
    fn prune_measured(&mut self) {
        fn collect(node: &Node, keys: &mut HashSet<Measured>) {
            match node {
                Node::Split {
                    id, first, second, ..
                } => {
                    keys.insert(Measured::Node(*id));
                    collect(first, keys);
                    collect(second, keys);
                }
                Node::Tabs { id, panels, .. } => {
                    keys.insert(Measured::Node(*id));
                    for panel in panels {
                        keys.insert(Measured::Seek(panel.id));
                        keys.insert(Measured::Volume(panel.id));
                    }
                }
            }
        }
        let mut active = HashSet::from([Measured::Device]);
        if let Some(root) = &self.layout.root {
            collect(root, &mut active);
        }
        self.measured
            .borrow_mut()
            .retain(|key, _| active.contains(key));
    }
    fn persist_layout(&mut self, cx: &mut Context<Self>) {
        self.prune_measured();
        if let Err(error) = self.layout.save(&self.layout_path) {
            self.error = Some(format!("Saving layout: {error:#}"));
        }
        self.sync_analysis(cx);
        cx.notify();
    }
    fn spectrum_panel(&mut self, id: u64, _: &mut Window, _: &mut Context<Self>) -> AnyElement {
        self.visuals.spectrum(id)
    }
    fn audio_visualizer_panel(
        &mut self,
        id: u64,
        _: &mut Window,
        _: &mut Context<GuiApp>,
    ) -> AnyElement {
        self.radial_spectrum
            .view(id, self.state.playback.status == PlaybackStatus::Playing)
    }
    fn spectrogram_panel(&mut self, id: u64, _: &mut Window, _: &mut Context<Self>) -> AnyElement {
        self.visuals.spectrogram(id)
    }
    fn waveform_panel(&mut self, id: u64, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let current = self.state.current_track();
        let track = current.or(self.full_track.as_ref());
        let position = if current.is_some() {
            self.seek_preview.unwrap_or(self.state.playback.position)
        } else {
            0.0
        };
        let duration = if current.is_some() {
            self.state.playback.duration
        } else {
            None
        }
        .or_else(|| track.and_then(|track| track.duration));
        let config = self.state.system.config.clone();
        self.waveform.update(cx, |waveform, cx| {
            waveform.view(id, position, duration, &config, cx)
        })
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
        .top_0()
        .left_0()
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
        if self.layout.locked {
            return;
        }
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
                        view.w(px(SPLIT_GUTTER)).h_full().cursor_col_resize()
                    })
                    .when(axis == Axis::Vertical, |view| {
                        view.h(px(SPLIT_GUTTER)).w_full().cursor_row_resize()
                    })
                    .when(self.layout.locked, |view| view.cursor_default())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            if this.layout.locked {
                                return;
                            }
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
                let show_tab_bar = panels
                    .iter()
                    .find(|panel| panel.id == active)
                    .is_none_or(|panel| panel.show_tab_bar);
                let mut tabs = row()
                    .h(px(36.))
                    .flex_shrink_0()
                    .px(gpui::px(UI_INSET))
                    .gap_1()
                    .border_b_1()
                    .border_color(rgb(BORDER));
                for (index, panel) in panels.iter().enumerate().filter(|_| show_tab_bar) {
                    let panel_id = panel.id;
                    let title = panel_spec(&panel.kind)
                        .map_or(panel.kind.as_str(), |spec| spec.title)
                        .to_owned();
                    let tab_bar_visible = panel.show_tab_bar;
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
                            .when(!self.layout.locked, |view| view.cursor_move())
                            .bg(rgb(if active == panel_id { HIGHLIGHT } else { PANEL }))
                            .text_color(rgb(if active == panel_id { TEXT } else { MUTED }))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.layout.activate(panel_id);
                                this.target_group = Some(node_id);
                                this.persist_layout(cx);
                            }))
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                    cx.stop_propagation();
                                    this.panel_menu = Some(PanelMenu {
                                        panel_id,
                                        tab_bar_visible,
                                        position: event.position,
                                        page: PanelMenuPage::Main,
                                    });
                                    cx.notify();
                                }),
                            )
                            .when(!self.layout.locked, |view| {
                                view.on_drag(drag, |drag, _, _, cx| cx.new(|_| drag.clone()))
                            })
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
                            .when(!self.layout.locked, |view| {
                                view.child(
                                    div()
                                        .id(("close", panel_id))
                                        .cursor_pointer()
                                        .px_1()
                                        .text_color(rgb(MUTED))
                                        .hover(|style| style.text_color(rgb(TEXT)))
                                        .child("×")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            cx.stop_propagation();
                                            if this.layout.locked {
                                                return;
                                            }
                                            this.layout.remove(panel_id);
                                            this.persist_layout(cx);
                                        })),
                                )
                            }),
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
                                    .p(gpui::px(UI_INSET))
                                    .child(format!("Unknown panel: {}", panel.kind))
                                    .into_any_element()
                            })
                    })
                    .unwrap_or_else(|| div().into_any_element());
                let content_area = div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .p(gpui::px(UI_INSET))
                    .overflow_hidden()
                    .child(content);
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
                if show_tab_bar {
                    panel = panel.child(tabs);
                }
                panel = panel
                    .child(content_area)
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            this.panel_menu = Some(PanelMenu {
                                panel_id: active,
                                tab_bar_visible: show_tab_bar,
                                position: event.position,
                                page: PanelMenuPage::Main,
                            });
                            cx.notify();
                        }),
                    )
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
                    .when(!self.layout.locked && cx.has_active_drag(), |view| {
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
                if self.layout.locked {
                    return;
                }
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
                if let Some(duration) = self.state.playback.duration {
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
                        this.seek_queue_id = this.state.queue.current_id;
                    }
                    this.drag_position(event.position, cx);
                }),
            )
            .into_any_element()
    }
}
impl Render for GuiApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        cx.set_global(components::NerdSymbols(
            self.state.system.config.nerd_symbols,
        ));
        window.set_rem_size(px(16. * self.state.system.config.ui_scale));
        self.scroll_views.begin_frame();
        let root = self.layout.root.take();
        let layout_view = root
            .as_ref()
            .map(|root| self.render_node(root, window, cx))
            .unwrap_or_else(|| {
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(MUTED))
                    .child("Add a panel to build your layout")
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
            .text_size(px(14. * self.state.system.config.ui_scale))
            .font(self.ui_font.clone())
            .track_focus(&self.layout_focus)
            .capture_key_down(cx.listener(|_, event: &KeyDownEvent, window, cx| {
                let modifiers = event.keystroke.modifiers;
                if event.keystroke.key == "f4"
                    && modifiers.alt
                    && !modifiers.control
                    && !modifiers.platform
                    && !modifiers.shift
                    && !modifiers.function
                {
                    window.remove_window();
                    cx.stop_propagation();
                }
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let key = event.keystroke.key.as_str();
                if this.about_open {
                    if key == "escape" {
                        this.about_open = false;
                        this.focus_layout(window, cx);
                        cx.notify();
                    }
                    cx.stop_propagation();
                    return;
                }
                if this.settings_dropdown_is_open() {
                    if this.settings_font_input_focused(window, cx) {
                        return;
                    }
                    if key == "escape" {
                        this.close_settings_dropdown();
                        cx.notify();
                    } else {
                        this.settings_dropdown_key(key, window, cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                if this.settings_open {
                    let modifiers = event.keystroke.modifiers;
                    if matches!(key, "pageup" | "pagedown")
                        && !modifiers.control
                        && !modifiers.platform
                        && !modifiers.alt
                        && !modifiers.shift
                        && !modifiers.function
                        && this.settings_focus.is_focused(window)
                    {
                        if this
                            .scroll_views
                            .page(key == "pagedown", window.mouse_position(), true)
                        {
                            cx.notify();
                        }
                        cx.stop_propagation();
                        return;
                    }
                    if key == "escape" {
                        this.settings_open = false;
                        this.focus_layout(window, cx);
                        cx.notify();
                        cx.stop_propagation();
                    }
                    return;
                }
                if this.app_menu_open || this.panel_menu.is_some() || this.catalog_open {
                    if key == "escape" {
                        this.app_menu_open = false;
                        if this.panel_menu.is_some() {
                            this.panel_menu = None;
                        } else {
                            this.catalog_open = false;
                        }
                        cx.notify();
                    }
                    cx.stop_propagation();
                    return;
                }
                if this.input_focused(window, cx) {
                    if key == "escape" {
                        this.focus_layout(window, cx);
                        cx.stop_propagation();
                    }
                    return;
                }
                let modifiers = event.keystroke.modifiers;
                let unmodified = !modifiers.control
                    && !modifiers.platform
                    && !modifiers.alt
                    && !modifiers.shift
                    && !modifiers.function;
                if !unmodified {
                    return;
                }
                match key {
                    "escape" => {
                        this.focus_layout(window, cx);
                        this.invalidate_library_selection();
                        this.library_navigation = None;
                        this.directory_tree.cancel_navigation();
                        this.library_selected_nodes.clear();
                        this.selected.clear();
                        this.directory_tree
                            .remember_selection(&this.library_selected_nodes, &this.selected);
                        this.selected_queue.clear();
                        this.selected_queue.clear_anchor();
                        this.selected_playlist = None;
                        this.selected_entry = None;
                        this.metadata_track = None;
                        this.full_track = None;
                        this.sync_metadata_default(cx);
                        this.sync_waveform(cx);
                        cx.notify();
                    }
                    "q" => {
                        window.remove_window();
                    }
                    "/" => this.activate_library(window, cx, true),
                    "t" => this.activate_library(window, cx, false),
                    "up" | "down" => {
                        if let Some(focus) = this.active_list_focus() {
                            this.list_focus = Some(focus);
                            this.navigate_list(focus, key == "down", cx);
                        }
                    }
                    "pageup" | "pagedown" => {
                        if this
                            .scroll_views
                            .page(key == "pagedown", window.mouse_position(), false)
                        {
                            cx.notify();
                        }
                    }
                    "left" | "right" | "space" | " "
                        if this.library_tree_active
                            && this.active_list_focus() == Some(ListFocus::Library) =>
                    {
                        this.library_tree_key(key, cx);
                    }
                    "enter" => {
                        if let Some(focus) = this.active_list_focus() {
                            this.play_focused_selection(focus, cx);
                        }
                    }
                    "d" | "delete" => {
                        if let Some(focus) = this.active_list_focus() {
                            this.remove_focused_queue(focus, cx);
                        }
                    }
                    _ => {
                        if let Some(command) = playback_key_command(key, &this.state.playback) {
                            this.send(command, cx);
                        }
                    }
                }
                cx.stop_propagation();
            }))
            .capture_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, _, _| {
                let modal = this.settings_open
                    || this.about_open
                    || this.app_menu_open
                    || this.panel_menu.is_some()
                    || this.catalog_open;
                this.scroll_views.click(event.position, modal);
            }))
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
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .p(gpui::px(UI_INSET))
                    .child(layout_view),
            );

        let error = self
            .error
            .clone()
            .or_else(|| self.state.system.last_error.clone());
        let mut footer = row()
            .h(rems(3.0))
            .flex_shrink_0()
            .px(gpui::px(UI_INSET))
            .pt_1()
            .pb(gpui::px(UI_INSET))
            .text_xs()
            .text_color(rgb(MUTED));
        if let Some(error) = error {
            footer = footer.child(
                row()
                    .flex_1()
                    .min_w_0()
                    .px_2()
                    .bg(rgb(ERROR_BG))
                    .text_color(rgb(TEXT))
                    .child(copyable_message("copy-error", error, cx))
                    .child(
                        icon_button(
                            "dismiss-error",
                            "×",
                            "Dismiss message",
                            cx,
                            |this, _, cx| {
                                this.error = None;
                                this.send(Command::DismissError, cx);
                            },
                        )
                        .size(px(24.0))
                        .border_0()
                        .bg(rgb(ERROR_BG))
                        .text_color(rgb(MUTED)),
                    ),
            );
        } else if self.state.system.scanning {
            footer = footer.child(
                copyable_message(
                    "copy-scan-message",
                    self.state.system.scan_message.clone(),
                    cx,
                )
                .text_color(rgb(TEXT)),
            );
        } else {
            footer = footer.child(div().flex_1());
        }
        footer = footer
            .child(
                icon_button(
                    "layout",
                    if self.layout.locked { "󰌾" } else { "⊞" },
                    if self.layout.locked {
                        "Layout locked\nRight-click to unlock"
                    } else {
                        "Left-click: add panel\nRight-click: lock layout"
                    },
                    cx,
                    |this, _, cx| {
                        if this.layout.locked {
                            return;
                        }
                        this.catalog_open = !this.catalog_open;
                        this.panel_menu = None;
                        cx.notify();
                    },
                )
                .text_color(rgb(if self.layout.locked { ACCENT } else { MUTED }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        this.layout.locked = !this.layout.locked;
                        this.catalog_open = false;
                        this.panel_menu = None;
                        if matches!(this.dragging, Some(Dragging::Split(..))) {
                            this.dragging = None;
                        }
                        this.persist_layout(cx);
                    }),
                ),
            )
            .child(icon_button("app-menu", "☰", "Menu", cx, |this, _, cx| {
                this.app_menu_open = !this.app_menu_open;
                this.catalog_open = false;
                this.panel_menu = None;
                cx.notify();
            }));
        if self.app_menu_open {
            let about = menu_item("app-about", "About", cx, |this, window, cx| {
                this.app_menu_open = false;
                this.about_open = true;
                this.about_focus.focus(window, cx);
                cx.notify();
            });
            app = app.child(
                column()
                    .id("app-menu-popup")
                    .occlude()
                    .absolute()
                    .bottom(rems(3.0))
                    .right(gpui::px(UI_INSET))
                    .w(px(220.))
                    .p_1()
                    .gap_1()
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .rounded_md()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.app_menu_open = false;
                        cx.stop_propagation();
                        cx.notify();
                    }))
                    .child(menu_item(
                        "app-settings",
                        "Settings",
                        cx,
                        |this, window, cx| {
                            this.app_menu_open = false;
                            this.load_settings(cx);
                            this.settings_open = true;
                            this.settings_focus.focus(window, cx);
                            cx.notify();
                        },
                    ))
                    .child(about)
                    .child(menu_item(
                        "app-close-window",
                        "Close window",
                        cx,
                        |this, window, _| {
                            this.app_menu_open = false;
                            window.remove_window();
                        },
                    ))
                    .child(menu_item(
                        "app-quit",
                        "Quit application",
                        cx,
                        |this, _, cx| {
                            this.app_menu_open = false;
                            this.send(Command::Shutdown, cx);
                        },
                    )),
            );
        }

        app = app.child(footer);

        if self.catalog_open && !self.layout.locked {
            let mut choices = row().flex_wrap().gap_1();
            for spec in PANELS {
                let kind = spec.kind;
                choices = choices.child(button(kind, spec.title, cx, move |this, _, cx| {
                    if this.layout.locked {
                        return;
                    }
                    this.layout.add(kind, this.target_group);
                    this.catalog_open = false;
                    this.persist_layout(cx);
                }));
            }
            app = app.child(
                column()
                    .id("panel-catalog")
                    .occlude()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.catalog_open = false;
                        cx.stop_propagation();
                        cx.notify();
                    }))
                    .absolute()
                    .bottom(rems(3.0))
                    .right(gpui::px(UI_INSET))
                    .max_w(px(560.))
                    .p(gpui::px(UI_INSET))
                    .gap_1()
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .child(div().text_xs().text_color(rgb(MUTED)).child("Add panel"))
                    .child(choices),
            );
        }

        if let Some(state) = self.panel_menu {
            let panel_kind = self
                .layout
                .panels()
                .into_iter()
                .find(|panel| panel.id == state.panel_id)
                .map(|panel| panel.kind.as_str());
            let is_queue = panel_kind == Some("queue");
            let is_waveform = panel_kind == Some("waveform");
            let is_audio_visualizer = panel_kind == Some("audio_visualizer");
            let has_selected_queue = self
                .state
                .queue
                .entries
                .iter()
                .any(|entry| self.selected_queue.contains(&entry.id));
            let mut menu = context_menu_container("panel-context-menu").on_mouse_down_out(
                cx.listener(|this, _, _, cx| {
                    this.panel_menu = None;
                    cx.stop_propagation();
                    cx.notify();
                }),
            );
            match state.page {
                PanelMenuPage::Main => {
                    if is_waveform {
                        menu = menu.child(menu_item(
                            "load-full-waveform",
                            "Load full waveform now",
                            cx,
                            |this, _, cx| {
                                this.panel_menu = None;
                                this.load_full_waveform(cx);
                                cx.notify();
                            },
                        ));
                        menu = menu.child(menu_item(
                            "waveform-click-seek",
                            if self.state.system.config.waveform_seek_enabled {
                                "Click and drag to seek ✓"
                            } else {
                                "Click and drag to seek"
                            },
                            cx,
                            |this, _, cx| {
                                this.panel_menu = None;
                                let mut config = this.handle.config_snapshot().as_ref().clone();
                                config.waveform_seek_enabled = !config.waveform_seek_enabled;
                                this.send(Command::Configure { config }, cx);
                            },
                        ));
                    }
                    if is_audio_visualizer {
                        menu = menu.child(menu_item(
                            "audio-visualizer-noctalia-fancy",
                            "Noctalia Fancy ✓",
                            cx,
                            |this, _, cx| {
                                this.panel_menu = None;
                                cx.notify();
                            },
                        ));
                    }
                    if is_queue {
                        menu = menu
                            .child(menu_item(
                                "randomize-queue",
                                "Randomize queue",
                                cx,
                                |this, _, cx| {
                                    this.panel_menu = None;
                                    this.send(Command::RandomizeQueue, cx);
                                    cx.notify();
                                },
                            ))
                            .child(menu_item(
                                "deduplicate-queue",
                                "Remove duplicates",
                                cx,
                                |this, _, cx| {
                                    this.panel_menu = None;
                                    this.send(Command::DeduplicateQueue, cx);
                                    cx.notify();
                                },
                            ));
                        if has_selected_queue {
                            menu = menu.child(menu_item(
                                "queue-add-playlist",
                                "Add to playlist  ›",
                                cx,
                                move |this, _, cx| {
                                    this.panel_menu = Some(PanelMenu {
                                        page: PanelMenuPage::Playlists,
                                        ..state
                                    });
                                    cx.notify();
                                },
                            ));
                        }
                        menu = menu.child(menu_item(
                            "clear-queue",
                            "Clear queue",
                            cx,
                            move |this, _, cx| {
                                this.panel_menu = Some(PanelMenu {
                                    page: PanelMenuPage::ConfirmClearQueue,
                                    ..state
                                });
                                cx.notify();
                            },
                        ));
                    }
                    menu = menu
                        .when(is_queue || is_waveform || is_audio_visualizer, |view| {
                            view.child(div().h(px(1.)).mx_2().my_1().bg(rgb(BORDER)))
                        })
                        .when(self.layout.locked, |view| {
                            view.child(
                                div()
                                    .px(gpui::px(UI_INSET))
                                    .py_1()
                                    .text_sm()
                                    .text_color(rgb(MUTED))
                                    .child("Layout is locked"),
                            )
                        })
                        .when(!self.layout.locked, |view| {
                            view.child(menu_item(
                                "toggle-tab-bar",
                                if state.tab_bar_visible {
                                    "Hide tab bar"
                                } else {
                                    "Show tab bar"
                                },
                                cx,
                                move |this, _, cx| {
                                    if this.layout.locked {
                                        return;
                                    }
                                    this.layout.set_tab_bar_visible(
                                        state.panel_id,
                                        !state.tab_bar_visible,
                                    );
                                    this.panel_menu = None;
                                    this.persist_layout(cx);
                                },
                            ))
                        });
                }
                PanelMenuPage::ConfirmClearQueue => {
                    menu = menu
                        .child(div().px(gpui::px(UI_INSET)).py_2().text_sm().child(format!(
                            "Clear all {} queued entries?",
                            self.state.queue.entries.len()
                        )))
                        .child(
                            panels::caption("This also stops playback.")
                                .px(gpui::px(UI_INSET))
                                .pb_2(),
                        )
                        .child(
                            row()
                                .child(
                                    menu_item("cancel-clear-queue", "Cancel", cx, |this, _, cx| {
                                        this.panel_menu = None;
                                        cx.notify();
                                    })
                                    .flex_1(),
                                )
                                .child(
                                    menu_item(
                                        "confirm-clear-queue",
                                        "Clear queue",
                                        cx,
                                        |this, _, cx| {
                                            this.panel_menu = None;
                                            this.selected_queue.clear();
                                            this.selected_queue.clear_anchor();
                                            this.send(Command::ClearQueue, cx);
                                            cx.notify();
                                        },
                                    )
                                    .flex_1()
                                    .text_color(rgb(ERROR)),
                                ),
                        );
                }
                PanelMenuPage::Playlists => {
                    menu = menu.child(
                        menu_item(
                            "queue-create-playlist",
                            "Save selected queue as new playlist",
                            cx,
                            |this, _, cx| {
                                let name = this.value(Field::PlaylistName, cx).trim().to_owned();
                                if name.is_empty() {
                                    this.panel_error(
                                        "Enter a playlist name in Playlists first.",
                                        cx,
                                    );
                                    return;
                                }
                                let mut track_ids = Vec::new();
                                for entry in this.state.queue.entries.iter() {
                                    if this.selected_queue.contains(&entry.id)
                                        && !track_ids.contains(&entry.track_id)
                                    {
                                        track_ids.push(entry.track_id);
                                    }
                                }
                                if track_ids.is_empty() {
                                    return;
                                }
                                this.panel_menu = None;
                                this.send(
                                    Command::CreatePlaylistWithTracks { name, track_ids },
                                    cx,
                                );
                            },
                        )
                        .w_full(),
                    );
                    if has_selected_queue {
                        if self.playlist_buffer.rows.is_empty() {
                            menu = menu.child(panels::caption(
                                "No playlists yet. Create one in Playlists.",
                            ));
                        } else {
                            let height = (self.playlist_buffer.rows.len() as f32
                                * panels::TRACK_HEIGHT)
                                .min(240.)
                                .min(f32::from(window.viewport_size().height) * 0.5);
                            menu =
                                menu.child(
                                    uniform_list(
                                        "queue-playlist-targets",
                                        self.playlist_buffer.rows.len(),
                                        cx.processor(
                                            move |this, range: std::ops::Range<usize>, _, cx| {
                                                range
                                                    .map(|index| {
                                                        let Some(playlist) =
                                                            this.playlist_buffer.rows.get(index)
                                                        else {
                                                            return this.paged_loading_row(
                                                            ViewRequestKind::PlaylistSummaries,
                                                            index,
                                                            cx,
                                                        ).h(px(panels::TRACK_HEIGHT));
                                                        };
                                                        let playlist_id = playlist.id;
                                                        menu_item(
                                                            ("queue-playlist", playlist_id as u64),
                                                            playlist.name.clone(),
                                                            cx,
                                                            move |this, _, cx| {
                                                                let track_ids = this
                                                                    .state
                                                                    .queue
                                                                    .entries
                                                                    .iter()
                                                                    .filter(|entry| {
                                                                        this.selected_queue
                                                                            .contains(&entry.id)
                                                                    })
                                                                    .map(|entry| entry.track_id)
                                                                    .collect();
                                                                this.panel_menu = None;
                                                                this.send(
                                                                    Command::AddPlaylist {
                                                                        playlist_id,
                                                                        track_ids,
                                                                    },
                                                                    cx,
                                                                );
                                                                cx.notify();
                                                            },
                                                        )
                                                        .h(px(panels::TRACK_HEIGHT))
                                                        .w_full()
                                                        .overflow_hidden()
                                                    })
                                                    .collect()
                                            },
                                        ),
                                    )
                                    .with_decoration(paged::viewport_demand(cx.processor(
                                        |this, range, _, cx| {
                                            this.ensure_playlist_range(range, cx);
                                        },
                                    )))
                                    .h(px(height))
                                    .w_full(),
                                );
                        }
                    } else {
                        menu = menu.child(panels::caption(
                            "The selected queue entry is no longer available.",
                        ));
                    }
                    menu = menu.child(menu_item(
                        "queue-playlist-back",
                        "Back",
                        cx,
                        move |this, _, cx| {
                            this.panel_menu = Some(PanelMenu {
                                page: PanelMenuPage::Main,
                                ..state
                            });
                            cx.notify();
                        },
                    ));
                }
            }
            app = app.child(
                anchored()
                    .position(state.position)
                    .snap_to_window()
                    .child(menu),
            );
        }

        if self.settings_open {
            let settings = self.settings_panel(u64::MAX, window, cx);
            app = app.child(
                div()
                    .id("settings-modal")
                    .track_focus(&self.settings_focus)
                    .occlude()
                    .absolute()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgb(0x000000).alpha(0.58))
                    .child(
                        column()
                            .max_w(px(620.))
                            .w(relative(0.9))
                            .h((window.viewport_size().height - px(32.))
                                .max(px(0.))
                                .min(px(640.)))
                            .gap_0()
                            .overflow_hidden()
                            .bg(rgb(PANEL))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .rounded_md()
                            .child(
                                row()
                                    .h(rems(2.5))
                                    .flex_shrink_0()
                                    .px(gpui::px(UI_INSET))
                                    .child(div().flex_1().text_lg().child("Settings")),
                            )
                            .child(settings),
                    ),
            );
        }
        if self.about_open {
            static LOGO: std::sync::LazyLock<Arc<Image>> = std::sync::LazyLock::new(|| {
                Arc::new(Image::from_bytes(
                    ImageFormat::Png,
                    crate::artwork::DEFAULT_IMAGE.to_vec(),
                ))
            });
            let built = chrono::DateTime::<chrono::Utc>::from_timestamp(
                env!("RIVU_BUILD_TIMESTAMP")
                    .parse()
                    .expect("build script emits Unix seconds"),
                0,
            )
            .expect("build script validates timestamp range")
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S %:z")
            .to_string();
            app = app.child(
                div()
                    .id("about-modal")
                    .track_focus(&self.about_focus)
                    .occlude()
                    .absolute()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgb(0x000000).alpha(0.58))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _, _, cx| {
                            cx.stop_propagation();
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|_, _, _, cx| {
                            cx.stop_propagation();
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Middle,
                        cx.listener(|_, _, _, cx| {
                            cx.stop_propagation();
                        }),
                    )
                    .on_scroll_wheel(cx.listener(|_, _, _, cx| {
                        cx.stop_propagation();
                    }))
                    .child(
                        row()
                            .id("about-card")
                            .relative()
                            .max_w(relative(0.9))
                            .items_center()
                            .gap_6()
                            .p_6()
                            .bg(rgb(PANEL))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .rounded_md()
                            .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                                this.about_open = false;
                                this.focus_layout(window, cx);
                                cx.stop_propagation();
                                cx.notify();
                            }))
                            .child(
                                img(Arc::clone(&LOGO))
                                    .w(rems(9.5))
                                    .h(rems(9.5))
                                    .flex_shrink_0(),
                            )
                            .child(
                                column()
                                    .items_start()
                                    .gap_3()
                                    .child(div().text_2xl().child("Rivu"))
                                    .child(caption("A local-first music player"))
                                    .child(caption(env!("RIVU_GIT_VERSION")))
                                    .child(caption(format!("Built {built}")))
                                    .child(caption("GPL-3.0-or-later")),
                            )
                            .child(
                                icon_button(
                                    "about-close",
                                    "×",
                                    "Close",
                                    cx,
                                    |this, window, cx| {
                                        this.about_open = false;
                                        this.focus_layout(window, cx);
                                        cx.notify();
                                    },
                                )
                                .absolute()
                                .top(px(8.))
                                .right(px(8.)),
                            ),
                    ),
            );
        }
        self.scroll_views.finish_frame();
        app
    }
}
