mod artwork;
mod components;
mod fonts;
mod host;
mod input;
mod layout;
mod library;
mod paged;
mod panels;
mod queries;
mod scrolling;
mod settings;
mod shell;
mod visualization;
use crate::{
    config::{AudioVisualizerOptions, AudioVisualizerStyle},
    core::AppHandle,
    model::{Command, LibraryRow, LibrarySort, PlaybackStatus},
    projection::GuiSnapshot,
};
use anyhow::Result;
use ashpd::desktop::file_chooser::SelectedFiles;
pub(super) use components::{
    ButtonTooltip, DropdownItem, DropdownState, POPOVER_MAX_HEIGHT, SelectionModel, StatusKind,
    TRACK_HEIGHT, active_control, artwork_preview, button, caption, context_menu_container,
    drag_preview, dropdown_container, dropdown_row, dropdown_trigger, form_field, icon_button,
    list_row, list_viewport, menu_item, metadata_item, panel_header, panel_surface, panel_toolbar,
    row_text, section_group, sortable_header, status_message, toggle_button, track_row, tree_row,
    two_column,
};
use futures::{StreamExt, channel::mpsc};
use gpui::{prelude::*, *};
use host::GuiHost;
pub use host::run;
use input::{Input, InputEvent};
use layout::{Axis, Edge, LayoutSet, Node};
use library::LibraryNode;
use queries::{
    LibraryViewBuffer, PlaylistEntriesBuffer, PlaylistSummaryBuffer, ViewRequestKind, ViewRequests,
};
use shell::{PanelMenu, PanelMenuPage};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    rc::{Rc, Weak},
    sync::Arc,
    time::{Duration, Instant},
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
    LayoutName,
    Title,
    Artist,
    Album,
}

struct PanelSpec {
    kind: &'static str,
    title: &'static str,
    render: fn(&mut GuiApp, &layout::Panel, &mut Window, &mut Context<GuiApp>) -> AnyElement,
}
const PANELS: &[PanelSpec] = &[
    PanelSpec {
        kind: "transport",
        title: "Playback",
        render: |app, panel, window, cx| app.transport_panel(panel.id, window, cx),
    },
    PanelSpec {
        kind: "library",
        title: "Library",
        render: |app, panel, window, cx| app.library_panel(panel.id, window, cx),
    },
    PanelSpec {
        kind: "queue",
        title: "Queue",
        render: |app, panel, window, cx| app.queue_panel(panel.id, window, cx),
    },
    PanelSpec {
        kind: "playlists",
        title: "Playlists",
        render: |app, panel, window, cx| app.playlists_panel(panel.id, window, cx),
    },
    PanelSpec {
        kind: "metadata",
        title: "Track details",
        render: |app, panel, window, cx| app.metadata_panel(panel.id, window, cx),
    },
    PanelSpec {
        kind: "history",
        title: "Recent tracks",
        render: |app, panel, window, cx| app.history_panel(panel.id, window, cx),
    },
    PanelSpec {
        kind: "spectrum",
        title: "Spectrum",
        render: |app, panel, window, cx| app.spectrum_panel(panel.id, window, cx),
    },
    PanelSpec {
        kind: "spectrogram",
        title: "Spectrogram",
        render: |app, panel, window, cx| app.spectrogram_panel(panel.id, window, cx),
    },
    PanelSpec {
        kind: "waveform",
        title: "Waveform",
        render: |app, panel, window, cx| app.waveform_panel(panel.id, window, cx),
    },
    PanelSpec {
        kind: "audio_visualizer",
        title: "Audio Visualizer",
        render: GuiApp::audio_visualizer_panel,
    },
];
fn panel_spec(kind: &str) -> Option<&'static PanelSpec> {
    PANELS.iter().find(|panel| panel.kind == kind)
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

fn copyable_message<T: 'static>(
    id: impl Into<ElementId>,
    message: String,
    cx: &mut Context<T>,
) -> Stateful<Div> {
    row()
        .id(id)
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .cursor_pointer()
        .child(
            div()
                .flex_initial()
                .min_w_0()
                .truncate()
                .child(message.clone()),
        )
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

#[derive(Clone, Copy, PartialEq, Eq)]
struct DockRequest {
    target: layout::docking::Target,
    index: Option<usize>,
}

struct DockHover {
    request: DockRequest,
    owner: Bounds<Pixels>,
    preview: Option<Bounds<Pixels>>,
    prepared: Option<layout::Layout>,
    unchanged: bool,
}

#[derive(Clone, Copy, PartialEq)]
struct DockGeometry {
    viewport: Size<Pixels>,
    ui_scale: f32,
    scale_factor: f32,
}

impl DockGeometry {
    fn current(window: &Window, ui_scale: f32) -> Self {
        Self {
            viewport: window.viewport_size(),
            ui_scale,
            scale_factor: window.scale_factor(),
        }
    }
}

struct PanelDocking {
    source: u64,
    guide: Option<layout::docking::Guide>,
    hover: Option<DockHover>,
    geometry: DockGeometry,
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
enum Measured {
    Node(u64),
    Tab(u64),
    Seek(u64),
    Volume(u64),
}
#[derive(Clone, Copy)]
enum Dragging {
    Split(layout::layout_geometry::SplitDrag),
    Seek(u64),
    Volume(u64),
}

fn dock_edge(position: Point<Pixels>, bounds: Bounds<Pixels>) -> Option<Edge> {
    let x = (position.x - bounds.origin.x) / bounds.size.width;
    let y = (position.y - bounds.origin.y) / bounds.size.height;
    let horizontal = if x < 0.22 {
        Some(Edge::Left)
    } else if x > 0.78 {
        Some(Edge::Right)
    } else {
        None
    };
    let vertical = if y < 0.22 {
        Some(Edge::Top)
    } else if y > 0.78 {
        Some(Edge::Bottom)
    } else {
        None
    };
    match (horizontal, vertical) {
        (Some(edge), None) | (None, Some(edge)) => Some(edge),
        (None, None) => Some(Edge::Center),
        _ => None,
    }
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

#[derive(Clone, Copy)]
pub(super) enum DragDestination {
    Queue,
    Playlist(i64),
}
pub(super) struct GuiApp {
    handle: AppHandle,
    state: GuiSnapshot,
    layouts: LayoutSet,
    host: Weak<RefCell<GuiHost>>,
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
    view_requests: ViewRequests,
    ranking_rows: Vec<LibraryRow>,
    ranking_total: usize,
    ranking_offset: usize,
    history_sort: HistorySort,
    history_sort_desc: bool,
    library_stats: Option<crate::model::LibraryStats>,
    settings: Entity<settings::Settings>,
    visuals: visualization::Visuals,
    audio_visualizer: visualization::AudioVisualizer,
    default_album: Entity<artwork::Artwork>,
    catalog_open: bool,
    settings_open: bool,
    about_open: bool,
    about_focus: FocusHandle,
    app_menu_open: bool,
    layouts_open: bool,
    layout_action: Option<layout::layout_manager::Action>,
    panel_menu: Option<PanelMenu>,
    playlist_delete_confirm: Option<i64>,
    waveform: Entity<visualization::Waveform>,
    target_group: Option<u64>,
    dragging: Option<Dragging>,
    panel_docking: Option<PanelDocking>,
    split_references: Vec<f64>,
    layout_scale_factor: f64,
    seek_preview: Option<f64>,
    waveform_preview_active: bool,
    seek_queue_id: Option<u64>,
    analysis_worker_enabled: bool,
    measured: Rc<RefCell<HashMap<Measured, Bounds<Pixels>>>>,
    measured_geometry: Rc<Cell<Option<DockGeometry>>>,
    analysis_sequence: u64,
    analysis_staging: crate::analysis::AnalysisFrame,
    analysis_needs_refresh: bool,
    window_visible: bool,
    frame_pending: bool,
    frame_timer: Option<Task<()>>,
    last_analysis_request: Option<Instant>,
    ui_font: Font,
    _subscriptions: Vec<Subscription>,
}
impl GuiApp {
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
        if let Some(panel_id) = self
            .layouts
            .current()
            .panels()
            .into_iter()
            .find(|panel| panel.kind == "library")
            .map(|panel| panel.id)
        {
            self.layouts.current_mut().activate(panel_id);
            self.list_focus = Some(ListFocus::Library);
            self.catalog_open = false;
            if focus_search {
                self.input(Field::Search).update(cx, |input, cx| {
                    input.focus_handle(cx).focus(window, cx);
                });
            }
            self.persist_layout(cx);
        } else if self.layouts.current().locked {
            self.error = Some(
                "Library panel is not in this locked layout; unlock it to add Library.".into(),
            );
            cx.notify();
        } else {
            let panel_id = self.layouts.current_mut().add("library", self.target_group);
            self.layouts.current_mut().activate(panel_id);
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
            self.layouts
                .current()
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
        host: Weak<RefCell<GuiHost>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let state = handle.gui_snapshot();
        let (mut layouts, mut error) = if layout_path.exists() {
            match LayoutSet::load(&layout_path) {
                Ok(layouts) => (layouts, None),
                Err(error) => (
                    LayoutSet::default(),
                    Some(format!("Could not load layouts: {error:#}")),
                ),
            }
        } else {
            (LayoutSet::default(), None)
        };
        if layouts
            .current_mut()
            .initialize_audio_visualizers(AudioVisualizerOptions::from(
                state.system.config.as_ref(),
            ))
        {
            if let Err(save_error) = layouts.save(&layout_path) {
                error = Some(format!("Saving layouts: {save_error:#}"));
            }
        }
        let ui_font = fonts::resolve(&state.system.config.ui_font, cx);
        let mut inputs = HashMap::new();
        for (field, placeholder) in [
            (Field::Search, "Search title, artist or album"),
            (Field::PlaylistName, "Playlist name"),
            (Field::LayoutName, "Layout name"),
            (Field::Title, "Title"),
            (Field::Artist, "Artist"),
            (Field::Album, "Album"),
        ] {
            inputs.insert(field, cx.new(|cx| Input::new("", placeholder, cx)));
        }
        let mut subscriptions = Vec::new();
        let settings = cx.new(|cx| {
            settings::Settings::new(
                handle.clone(),
                Arc::clone(&state.system.devices),
                state.system.ffmpeg_status.clone(),
                state.system.mpris_status.clone(),
                state.system.config.ui_scale,
                cx,
            )
        });
        subscriptions.push(cx.subscribe_in(
            &settings,
            window,
            |this, settings, event, window, cx| match event {
                settings::SettingsEvent::Closed => {
                    this.settings_open = false;
                    this.focus_layout(window, cx);
                    cx.notify();
                }
                settings::SettingsEvent::ApplyVisualizer {
                    panel_id,
                    options,
                    session,
                } => {
                    let previous = this.layouts.current().audio_visualizer_options(*panel_id);
                    let result = if this
                        .layouts
                        .current_mut()
                        .set_audio_visualizer_options(*panel_id, *options)
                    {
                        match this.layouts.save(&this.layout_path) {
                            Ok(()) => Ok(()),
                            Err(error) => {
                                if let Some(previous) = previous {
                                    this.layouts
                                        .current_mut()
                                        .set_audio_visualizer_options(*panel_id, previous);
                                }
                                Err(format!("Saving layout: {error:#}"))
                            }
                        }
                    } else {
                        Err("This visualizer instance no longer exists".into())
                    };
                    settings.update(cx, |settings, cx| {
                        settings.complete_visualizer_apply(*session, result, cx)
                    });
                    this.sync_analysis(cx);
                    cx.notify();
                }
            },
        ));
        let waveform = cx.new(|_| visualization::Waveform::new(Arc::clone(&handle.waveform)));
        subscriptions.push(cx.observe(&waveform, |_, _, cx| cx.notify()));
        subscriptions.push(cx.subscribe(&waveform, |this, _, event, cx| match event {
            visualization::WaveformEvent::Preview(seconds) => {
                if this.state.system.config.still_mode
                    || !this.state.system.config.waveform_seek_enabled
                {
                    return;
                }
                this.waveform_preview_active = true;
                this.seek_preview = Some(*seconds);
                cx.notify();
            }
            visualization::WaveformEvent::Seek(seconds) => {
                this.waveform_preview_active = false;
                this.seek_preview = None;
                if !this.state.system.config.still_mode
                    && this.state.system.config.waveform_seek_enabled
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
        subscriptions.push(cx.subscribe_in(
            &inputs[&Field::LayoutName],
            window,
            |this, _, event, window, cx| {
                if matches!(event, InputEvent::Submit)
                    && this.layouts_open
                    && this.layout_action.is_some()
                {
                    this.finish_layout_action(window, cx);
                }
            },
        ));
        for field in [Field::Title, Field::Artist, Field::Album] {
            subscriptions.push(cx.subscribe(&inputs[&field], |_, _, event, cx| {
                if matches!(event, InputEvent::Changed) {
                    cx.notify();
                }
            }));
        }
        subscriptions.push(
            cx.observe_window_visibility(window, |this, visibility, window, cx| {
                this.window_visible = matches!(visibility, WindowVisibility::Visible);
                this.frame_timer.take();
                if !this.window_visible {
                    this.cancel_panel_docking(window, cx);
                }
                this.refresh(cx);
                this.sync_analysis(cx);
                this.request_frame(window, cx);
                cx.notify();
            }),
        );
        subscriptions.push(cx.observe_window_activation(window, |this, window, cx| {
            if !window.is_window_active() {
                this.cancel_panel_docking(window, cx);
            }
        }));
        let (wake_sender, mut wake_receiver) = mpsc::channel::<()>(1);
        let wake_sender = parking_lot::Mutex::new(wake_sender);
        handle.set_wakeup(move || {
            let _ = wake_sender.lock().try_send(());
        });
        cx.spawn_in(window, async move |this, cx| {
            while wake_receiver.next().await.is_some() {
                if this
                    .update_in(cx, |this, window, cx| this.request_frame(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let artwork_manager = Arc::new(crate::artwork::ArtworkManager::new(
            layout_path.parent().unwrap_or_else(|| Path::new(".")),
        ));
        let mut app = Self {
            settings,
            handle,
            state,
            layouts,
            host,
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
            view_requests: ViewRequests::default(),
            ranking_rows: Vec::new(),
            ranking_total: 0,
            ranking_offset: 0,
            history_sort: HistorySort::Recent,
            history_sort_desc: true,
            library_stats: None,
            visuals: visualization::Visuals::new(),
            audio_visualizer: visualization::AudioVisualizer::new(),
            default_album: cx.new(|_| artwork::Artwork::new()),
            catalog_open: false,
            settings_open: false,
            about_open: false,
            about_focus: cx.focus_handle(),
            app_menu_open: false,
            layouts_open: false,
            layout_action: None,
            panel_menu: None,
            playlist_delete_confirm: None,
            waveform,
            target_group: None,
            dragging: None,
            panel_docking: None,
            split_references: Vec::new(),
            layout_scale_factor: 1.,
            analysis_worker_enabled: false,
            measured: Rc::new(RefCell::new(HashMap::new())),
            measured_geometry: Rc::new(Cell::new(None)),
            analysis_sequence: 0,
            analysis_staging: crate::analysis::AnalysisFrame::default(),
            analysis_needs_refresh: false,
            window_visible: window.is_visible(),
            frame_pending: false,
            frame_timer: None,
            last_analysis_request: None,
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
        window
            .observe_release(&cx.entity(), cx, |this: &mut Self, window, cx| {
                this.release_render_images(window, cx);
            })
            .detach();
        app
    }
    fn release_render_images(&self, window: &mut Window, cx: &App) {
        self.audio_visualizer.release_all_images(window);
        self.visuals.release_all_images(window);
        self.waveform
            .read(cx)
            .image_releaser()
            .release_all_images(window);
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
                            if let Err(error) = this.handle.update_gui_config(
                                crate::core::GuiConfigChange::AddLibraryRoot(path),
                            ) {
                                this.error = Some(format!("{error:#}"));
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

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let state = (self.handle.revision() != self.state.system.revision)
            .then(|| self.handle.gui_snapshot());
        let changed = state.is_some();
        let mut config_changed = false;
        if let Some(state) = state {
            config_changed = !Arc::ptr_eq(&self.state.system.config, &state.system.config);
            if !Arc::ptr_eq(&self.state.system.devices, &state.system.devices)
                || self.state.system.ffmpeg_status != state.system.ffmpeg_status
                || self.state.system.mpris_status != state.system.mpris_status
                || self.state.system.config.ui_scale != state.system.config.ui_scale
            {
                self.settings.update(cx, |settings, cx| {
                    settings.sync_runtime(
                        &state.system.devices,
                        &state.system.ffmpeg_status,
                        &state.system.mpris_status,
                        state.system.config.ui_scale,
                        cx,
                    );
                });
            }
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
            let still_mode_changed =
                self.state.system.config.still_mode != state.system.config.still_mode;
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
            if still_mode_changed {
                self.waveform_preview_active = false;
                self.seek_preview = None;
                // Resuming must wait for a new publication, not redraw pre-mode audio.
                self.analysis_staging = crate::analysis::AnalysisFrame::default();
                self.analysis_sequence = self.handle.analysis.read().sequence;
                self.analysis_needs_refresh = false;
            }
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
        if config_changed {
            self.sync_analysis(cx);
        }
        if changed {
            cx.notify();
        }
    }

    fn analysis_frame_interval(&self) -> Option<Duration> {
        (self.window_visible
            && self.analysis_worker_enabled
            && self.state.playback.status == PlaybackStatus::Playing
            && !self.state.system.shutting_down)
            .then(|| Duration::from_secs_f64(1. / self.state.system.config.analysis_fps as f64))
    }

    fn request_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.window_visible || self.frame_pending || self.frame_timer.is_some() {
            return;
        }
        // Native callbacks, core changes and local redraws share one consumer.
        // An existing deadline is also pending work, not another clock to race.
        self.frame_pending = true;
        let view = cx.weak_entity();
        window.on_next_frame(move |window, cx| {
            let _ = view.update(cx, |this, cx| {
                this.frame_pending = false;
                if !this.window_visible {
                    return;
                }
                this.refresh(cx);
                this.consume_analysis(cx);
                let track = this.state.current_track().or(this.full_track.as_ref());
                this.waveform
                    .update(cx, |waveform, cx| waveform.sync(track, cx));
                let Some(interval) = this.analysis_frame_interval() else {
                    this.last_analysis_request = None;
                    return;
                };
                let now = Instant::now();
                if this
                    .last_analysis_request
                    .is_none_or(|last| now - last >= interval)
                {
                    this.last_analysis_request = Some(now);
                    this.handle.request_analysis_frame();
                }
                let delay = interval.saturating_sub(this.last_analysis_request.unwrap().elapsed());
                let timer = cx.background_executor().timer(delay);
                this.frame_timer = Some(cx.spawn_in(window, async move |this, cx| {
                    timer.await;
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.frame_timer = None;
                        this.request_frame(window, cx);
                    });
                }));
            });
        });
        // A real draw keeps Wayland's outstanding native callback authoritative.
        // A clean local callback must not become a free-running analysis clock.
        cx.notify();
    }

    fn consume_analysis(&mut self, cx: &mut Context<Self>) {
        if !self.analysis_worker_enabled {
            return;
        }
        {
            let frame = self.handle.analysis.read();
            if frame.sequence == self.analysis_sequence
                && !(self.analysis_needs_refresh && self.analysis_staging.settings.is_some())
            {
                return;
            }
            self.analysis_staging
                .frequencies_hz
                .clone_from(&frame.frequencies_hz);
            self.analysis_staging
                .spectrum_db
                .clone_from(&frame.spectrum_db);
            self.analysis_staging.sequence = frame.sequence;
            self.analysis_staging.sample_rate = frame.sample_rate;
            self.analysis_staging.sample_time = frame.sample_time;
            self.analysis_staging.generation = frame.generation;
            self.analysis_staging.settings = frame.settings;
            self.analysis_staging.rms_left = frame.rms_left;
            self.analysis_staging.rms_right = frame.rms_right;
            self.analysis_staging
                .visualizer
                .clone_from(&frame.visualizer);
            self.analysis_sequence = frame.sequence;
        }
        self.analysis_needs_refresh = false;
        self.visuals.update(&self.analysis_staging);
        self.audio_visualizer.update(&self.analysis_staging);
        cx.notify();
    }
    fn sync_analysis(&mut self, cx: &mut Context<Self>) {
        let enabled = !self.state.system.config.still_mode;
        self.visuals.set_enabled(enabled);
        let active_panels = if enabled {
            self.layouts.current().active_panels()
        } else {
            Vec::new()
        };
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
        let spectrum_restored = self
            .visuals
            .set_spectrum_visible(self.window_visible && !spectrum_panel_ids.is_empty());
        let audio_visualizer_changed = self.audio_visualizer.sync(
            &self.state.system.config,
            &active_panels,
            self.window_visible,
        );
        let mut demand = crate::analysis::VisualizerDemand::default();
        if self.window_visible {
            for panel in &active_panels {
                if panel.kind != "audio_visualizer" {
                    continue;
                }
                let options = panel.audio_visualizer.unwrap_or_else(|| {
                    AudioVisualizerOptions::from(self.state.system.config.as_ref())
                });
                match options.style {
                    AudioVisualizerStyle::StereoDensity => demand.stereo_density = true,
                    AudioVisualizerStyle::SpatialSpectrum => demand.spatial_spectrum = true,
                    AudioVisualizerStyle::FractalBeams | AudioVisualizerStyle::FractalFlow => {
                        demand.band_energy = true;
                    }
                    _ => {}
                }
            }
        }
        self.handle.set_visualizer_demand(demand);
        let analysis_visible = self.window_visible
            && active_panels.iter().any(|panel| {
                matches!(
                    panel.kind.as_str(),
                    "spectrum" | "spectrogram" | "audio_visualizer"
                )
            });
        if analysis_visible {
            let spectrum_changed = self.visuals.configure(&self.state.system.config);
            self.analysis_needs_refresh |=
                spectrum_restored || spectrum_changed || audio_visualizer_changed;
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
        let enabled = !self.state.system.config.still_mode;
        let panels = if enabled {
            self.layouts.current().active_panels()
        } else {
            Vec::new()
        };
        let track = self.state.current_track().or(self.full_track.as_ref());
        self.waveform.update(cx, |waveform, cx| {
            waveform.set_enabled(enabled);
            if !enabled {
                return;
            }
            waveform.retain_panels(&panels);
            waveform.sync(track, cx);
        });
    }
    fn load_full_waveform(&mut self, cx: &mut Context<Self>) {
        if self.state.system.config.still_mode {
            return;
        }
        let track = self.state.current_track().or(self.full_track.as_ref());
        self.waveform.update(cx, |waveform, cx| {
            waveform.load_full(track, &self.state.system.config, cx);
        });
    }
    fn prune_measured(&mut self) {
        fn collect(node: &Node, keys: &mut HashSet<Measured>) {
            match node {
                Node::Split { id, children, .. } => {
                    keys.insert(Measured::Node(*id));
                    for child in children {
                        collect(child, keys);
                    }
                }
                Node::Tabs { id, panels, active } => {
                    keys.insert(Measured::Node(*id));
                    let show_tab_bar = panels
                        .iter()
                        .find(|panel| panel.id == *active)
                        .is_some_and(|panel| panel.show_tab_bar);
                    for panel in panels {
                        if show_tab_bar {
                            keys.insert(Measured::Tab(panel.id));
                        }
                        keys.insert(Measured::Seek(panel.id));
                        keys.insert(Measured::Volume(panel.id));
                    }
                }
            }
        }
        let mut active = HashSet::new();
        if let Some(root) = &self.layouts.current().root {
            collect(root, &mut active);
        }
        self.measured
            .borrow_mut()
            .retain(|key, _| active.contains(key));
    }
    fn persist_layout(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = &mut self.panel_docking {
            session.guide = None;
            session.hover = None;
        }
        self.layouts
            .current_mut()
            .initialize_audio_visualizers(AudioVisualizerOptions::from(
                self.state.system.config.as_ref(),
            ));
        self.prune_measured();
        if let Err(error) = self.layouts.save(&self.layout_path) {
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
        panel: &layout::Panel,
        _: &mut Window,
        _: &mut Context<GuiApp>,
    ) -> AnyElement {
        let options = panel
            .audio_visualizer
            .unwrap_or_else(|| AudioVisualizerOptions::from(self.state.system.config.as_ref()));
        self.audio_visualizer.view(
            panel.id,
            self.state.playback.status == PlaybackStatus::Playing,
            options,
        )
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
        let measured_geometry = self.measured_geometry.clone();
        let ui_scale = self.state.system.config.ui_scale;
        canvas(
            move |bounds, window, _| {
                let mut measured = measured.borrow_mut();
                if matches!(key, Measured::Node(_) | Measured::Tab(_)) {
                    let geometry = DockGeometry::current(window, ui_scale);
                    if measured_geometry.get() != Some(geometry) {
                        measured
                            .retain(|key, _| !matches!(key, Measured::Node(_) | Measured::Tab(_)));
                        measured_geometry.set(Some(geometry));
                    }
                }
                measured.insert(key, bounds);
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .into_any_element()
    }
    fn layout_gutter(&self) -> f64 {
        (f64::from(SPLIT_GUTTER) * self.layout_scale_factor).round() / self.layout_scale_factor
    }
    fn layout_edit_sizes(
        &self,
        panel: u64,
        docking: Option<(u64, Edge, u64)>,
    ) -> HashMap<u64, (f64, f64)> {
        let sizes = self
            .measured
            .borrow()
            .iter()
            .filter_map(|(key, bounds)| {
                let Measured::Node(id) = key else { return None };
                Some((
                    *id,
                    (
                        f64::from(f32::from(bounds.size.width)),
                        f64::from(f32::from(bounds.size.height)),
                    ),
                ))
            })
            .collect();
        self.layouts
            .current()
            .root
            .as_ref()
            .map(|root| {
                layout::layout_geometry::edit_sizes(
                    root,
                    sizes,
                    panel,
                    docking,
                    self.layout_gutter(),
                )
            })
            .unwrap_or_default()
    }
    fn reconcile_layout_sizes(&mut self, sizes: &HashMap<u64, (f64, f64)>) {
        let gutter = self.layout_gutter();
        self.layouts
            .current_mut()
            .preserve_region_sizes(sizes, gutter);
        self.dragging = None;
        self.split_references.clear();
        // A subsequent edit must not consume rectangles from the previous tree.
        self.measured
            .borrow_mut()
            .retain(|key, _| !matches!(key, Measured::Node(_)));
    }
    fn remove_layout_panel(&mut self, panel: u64, cx: &mut Context<Self>) {
        if self.layouts.current().locked {
            return;
        }
        let sizes = self.layout_edit_sizes(panel, None);
        if self.layouts.current_mut().remove(panel) {
            self.reconcile_layout_sizes(&sizes);
            self.persist_layout(cx);
        }
    }
    fn begin_split_drag(&mut self, id: u64, index: usize, pointer: Point<Pixels>) {
        if self.layouts.current().locked {
            return;
        }
        let Some(root) = self.layouts.current().root.as_ref() else {
            return;
        };
        let Some(Node::Split {
            axis,
            children,
            boundaries,
            ..
        }) = layout::layout_geometry::find(root, id)
        else {
            return;
        };
        let measured = self.measured.borrow();
        let Some(bounds) = measured.get(&Measured::Node(id)).copied() else {
            return;
        };
        let Some(first) = children
            .get(index)
            .and_then(|node| measured.get(&Measured::Node(layout::layout_geometry::node_id(node))))
        else {
            return;
        };
        let Some(second) = children
            .get(index + 1)
            .and_then(|node| measured.get(&Measured::Node(layout::layout_geometry::node_id(node))))
        else {
            return;
        };
        let boundary = |index: usize| {
            if index == 0 {
                0.
            } else if index == children.len() {
                1.
            } else {
                boundaries
                    .get(index - 1)
                    .copied()
                    .unwrap_or(index as f64 / children.len() as f64)
            }
        };
        let length = if *axis == Axis::Horizontal {
            bounds.size.width
        } else {
            bounds.size.height
        };
        let center = if *axis == Axis::Horizontal {
            (first.origin.x + first.size.width + second.origin.x) * 0.5
        } else {
            (first.origin.y + first.size.height + second.origin.y) * 0.5
        };
        let mut drag = layout::layout_geometry::SplitDrag {
            id,
            index,
            axis: *axis,
            bounds,
            content_length: f64::from(f32::from(length))
                - self.layout_gutter() * children.len().saturating_sub(1) as f64,
            gutter: self.layout_gutter(),
            before: boundary(index),
            after: boundary(index + 2),
            pointer_offset: 0.,
            pointer,
        };
        drag.pointer_offset = drag.pointer_coordinate(pointer) - f64::from(f32::from(center));
        self.split_references.clear();
        layout::layout_geometry::reference_centers(
            root,
            *axis,
            (id, index),
            &|id| measured.get(&Measured::Node(id)).copied(),
            &mut self.split_references,
        );
        self.dragging = Some(Dragging::Split(drag));
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
                children,
                boundaries,
            } => {
                let id = *id;
                let axis = *axis;
                let boundaries = boundaries.clone();
                let mut panes = Vec::with_capacity(children.len());
                for (index, child) in children.iter().enumerate() {
                    let content = self.render_node(child, window, cx);
                    let gutter = (index + 1 < children.len()).then(|| {
                        div()
                            .id(("split", index))
                            .absolute()
                            .bg(rgb(BG))
                            .hover(|style| style.bg(rgb(ACCENT)))
                            .when(axis == Axis::Horizontal, |view| view.cursor_col_resize())
                            .when(axis == Axis::Vertical, |view| view.cursor_row_resize())
                            .when(self.layouts.current().locked, |view| view.cursor_default())
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                    this.begin_split_drag(id, index, event.position);
                                    cx.stop_propagation();
                                }),
                            )
                    });
                    panes.push((content, gutter));
                }
                let measurement = self.measurement(Measured::Node(id));
                container_query(move |size, window, _| {
                    let length = if axis == Axis::Horizontal {
                        size.width
                    } else {
                        size.height
                    };
                    let mut geometry = layout::split_geometry(
                        &boundaries,
                        panes.len(),
                        f64::from(f32::from(length)),
                        f64::from(SPLIT_GUTTER),
                        f64::from(window.scale_factor()),
                    )
                    .peekable();
                    let mut split = div()
                        .id(("node", id))
                        .relative()
                        .size_full()
                        .min_w_0()
                        .min_h_0();
                    for (content, gutter) in panes {
                        let Some((start, length)) = geometry.next() else {
                            break;
                        };
                        let pane = div()
                            .absolute()
                            .min_w_0()
                            .min_h_0()
                            .overflow_hidden()
                            .when(axis == Axis::Horizontal, |view| {
                                view.left(px(start as f32))
                                    .top_0()
                                    .w(px(length as f32))
                                    .h_full()
                            })
                            .when(axis == Axis::Vertical, |view| {
                                view.top(px(start as f32))
                                    .left_0()
                                    .h(px(length as f32))
                                    .w_full()
                            })
                            .child(content);
                        split = split.child(pane);
                        if let (Some(gutter), Some(&(next, _))) = (gutter, geometry.peek()) {
                            let end = start + length;
                            let gutter = gutter
                                .when(axis == Axis::Horizontal, |view| {
                                    view.left(px(end as f32))
                                        .top_0()
                                        .w(px((next - end) as f32))
                                        .h_full()
                                })
                                .when(axis == Axis::Vertical, |view| {
                                    view.top(px(end as f32))
                                        .left_0()
                                        .h(px((next - end) as f32))
                                        .w_full()
                                });
                            split = split.child(gutter);
                        }
                    }
                    split.child(measurement)
                })
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
                for panel in panels.iter().filter(|_| show_tab_bar) {
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
                            .relative()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .text_xs()
                            .when(!self.layouts.current().locked, |view| view.cursor_move())
                            .bg(rgb(if active == panel_id { HIGHLIGHT } else { PANEL }))
                            .text_color(rgb(if active == panel_id { TEXT } else { MUTED }))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.layouts.current_mut().activate(panel_id);
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
                            .when(!self.layouts.current().locked, |view| {
                                let entity = cx.entity().downgrade();
                                view.on_drag(drag, move |drag, _, window, cx| {
                                    let _ = entity.update(cx, |this, cx| {
                                        this.start_panel_docking(drag.panel_id, window);
                                        if !this.panel_docking_blocked() {
                                            this.update_panel_docking(
                                                window.mouse_position(),
                                                window,
                                                cx,
                                            );
                                        }
                                    });
                                    cx.new(|_| drag.clone())
                                })
                            })
                            .child(title)
                            .child(self.measurement(Measured::Tab(panel_id)))
                            .when(!self.layouts.current().locked, |view| {
                                view.child(components::quiet_icon_button(icon_button(
                                    ("close", panel_id),
                                    "",
                                    "Remove panel",
                                    cx,
                                    move |this, _, cx| {
                                        if this.layouts.current().locked {
                                            return;
                                        }
                                        this.remove_layout_panel(panel_id, cx);
                                    },
                                )))
                            }),
                    );
                }
                let content = panels
                    .iter()
                    .find(|panel| panel.id == active)
                    .map(|panel| {
                        if self.state.system.config.still_mode
                            && matches!(
                                panel.kind.as_str(),
                                "spectrum" | "spectrogram" | "audio_visualizer" | "waveform"
                            )
                        {
                            return div()
                                .size_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_color(rgb(MUTED))
                                .child("Visualizations paused · Still Mode")
                                .into_any_element();
                        }
                        panel_spec(&panel.kind)
                            .map(|spec| (spec.render)(self, panel, window, cx))
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
                panel = panel.child(content_area).on_mouse_down(
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
                );
                // Structural edits account for the outer pane, including its border.
                div()
                    .relative()
                    .size_full()
                    .child(panel)
                    .child(self.measurement(Measured::Node(node_id)))
                    .into_any_element()
            }
        }
    }
    fn drag_position(&mut self, position: Point<Pixels>, alt: bool, cx: &mut Context<Self>) {
        let Some(dragging) = self.dragging else {
            return;
        };
        let bounds = match dragging {
            Dragging::Split(drag) => drag.bounds,
            Dragging::Seek(id) | Dragging::Volume(id) => {
                let key = if matches!(dragging, Dragging::Seek(_)) {
                    Measured::Seek(id)
                } else {
                    Measured::Volume(id)
                };
                let Some(bounds) = self.measured.borrow().get(&key).copied() else {
                    return;
                };
                bounds
            }
        };
        match dragging {
            Dragging::Split(mut drag) => {
                if self.layouts.current().locked {
                    return;
                }
                drag.pointer = position;
                self.dragging = Some(Dragging::Split(drag));
                let config = &self.state.system.config;
                let enabled = config.layout_snap_enabled ^ alt;
                let Some(fraction) = drag.fraction(
                    &self.split_references,
                    enabled,
                    f64::from(config.ui_scale),
                    config.layout_snap_grid_px,
                ) else {
                    return;
                };
                if self
                    .layouts
                    .current_mut()
                    .set_boundary(drag.id, drag.index, fraction)
                {
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
                    this.drag_position(event.position, event.modifiers.alt, cx);
                }),
            )
            .into_any_element()
    }
}
