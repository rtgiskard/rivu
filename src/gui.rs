mod artwork;
mod browser;
mod components;
mod fonts;
mod host;
mod input;
mod layout;
mod library;
mod metadata;
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
    model::{Command, PlaybackStatus},
    projection::GuiSnapshot,
};
use anyhow::Result;
use ashpd::desktop::file_chooser::SelectedFiles;
use browser::BrowserState;
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
use layout::interaction::{
    Cancellation, DockGeometry, DockHover, DockRequest, DragCommit, Dragging, LayoutInteraction,
    Measured, PanelDocking,
};
use layout::{Axis, Edge, LayoutSet, Node};
use library::LibraryNode;
use metadata::MetadataSession;
use queries::ViewRequestKind;
use shell::{PanelMenu, PanelMenuPage};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::PathBuf,
    rc::Weak,
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
    browser: BrowserState,
    metadata: MetadataSession,
    layout_focus: FocusHandle,
    scroll_views: scrolling::ScrollViews,

    interaction: LayoutInteraction,
    error: Option<String>,
    artwork_manager: Arc<crate::artwork::ArtworkManager>,
    artwork_loader: artwork::ArtworkLoader,
    queue_save_pending: bool,
    queue_save_feedback: Option<Result<String, String>>,
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
    waveform: Entity<visualization::Waveform>,
    analysis_worker_enabled: bool,
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
        self.cancel_interaction(window, cx);
        if let Some(panel_id) = self
            .layouts
            .current()
            .panels()
            .into_iter()
            .find(|panel| panel.kind == "library")
            .map(|panel| panel.id)
        {
            self.layouts.current_mut().activate(panel_id);
            self.browser.focus_list(ListFocus::Library);
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
            let panel_id = self
                .layouts
                .current_mut()
                .add("library", self.interaction.target_group());
            self.layouts.current_mut().activate(panel_id);
            self.layout_changed(cx);
            self.browser.focus_list(ListFocus::Library);
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
        self.browser.focus().or_else(|| {
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
                self.browser.library.selected_nodes().iter().any(|node| {
                    matches!(node, library::LibraryNode::Directory(selected) if selected.as_ref() == path.as_path())
                })
            }
            crate::model::DirectoryRow::Track(row) => self.browser.library.selected().contains(&row.id),
        }
    }

    fn navigate_list(&mut self, focus: ListFocus, down: bool, cx: &mut Context<Self>) {
        match focus {
            ListFocus::Library => {
                self.browser.library.invalidate_selection();
                if self.browser.library.tree_active() {
                    self.navigate_library_tree(down, cx);
                    return;
                }
                let total = self.browser.library.buffer().total;
                if total == 0 {
                    return;
                }
                let current = self.browser.library.navigation().or_else(|| {
                    self.browser
                        .library
                        .buffer()
                        .rows
                        .position(|row| self.browser.library.selected().contains(&row.id))
                });
                let index = match current {
                    Some(index) if down => (index + 1).min(total - 1),
                    Some(index) => index.saturating_sub(1),
                    None if down => self
                        .browser
                        .library
                        .buffer()
                        .rows
                        .visible()
                        .start
                        .min(total - 1),
                    None => total - 1,
                };
                self.browser
                    .library
                    .buffer()
                    .scroll
                    .scroll_to_item(index, ScrollStrategy::Nearest);
                if let Some(id) = self
                    .browser
                    .library
                    .buffer()
                    .rows
                    .get(index)
                    .map(|row| row.id)
                {
                    self.browser.library.cancel_navigation();
                    self.select_track(id, false, cx);
                } else {
                    self.browser.library.defer_navigation(index);
                    self.ensure_library_range(index..index + 1, cx);
                }
            }
            ListFocus::Queue => {
                if self.state.queue.entries.is_empty() {
                    return;
                }
                let current = self
                    .browser
                    .queue
                    .anchor()
                    .copied()
                    .filter(|id| self.browser.queue.contains(id))
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
                            .position(|entry| self.browser.queue.contains(&entry.id))
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
                if self.browser.library.tree_active() {
                    self.activate_library_tree(cx);
                    return;
                }
                let track_id = self
                    .browser
                    .library
                    .buffer()
                    .rows
                    .iter()
                    .find(|row| self.browser.library.selected().contains(&row.id))
                    .map(|row| row.id);
                if let Some(track_id) = track_id {
                    self.send(Command::Play { track_id }, cx);
                }
            }
            ListFocus::Queue => {
                let queue_id = self
                    .browser
                    .queue
                    .anchor()
                    .copied()
                    .filter(|id| self.browser.queue.contains(id))
                    .or_else(|| {
                        self.state
                            .queue
                            .entries
                            .iter()
                            .find(|entry| self.browser.queue.contains(&entry.id))
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
            .filter(|entry| self.browser.queue.contains(&entry.id))
            .map(|entry| entry.id)
            .collect::<Vec<_>>();
        if queue_ids.is_empty() {
            return;
        }
        self.browser.queue.clear();

        self.send(Command::RemoveQueueEntries { queue_ids }, cx);
    }
    fn new(
        handle: AppHandle,
        layout_path: PathBuf,
        artwork_manager: Arc<crate::artwork::ArtworkManager>,
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
        subscriptions.push(
            cx.subscribe(&waveform, |this, waveform, event, cx| match event {
                visualization::WaveformEvent::Preview {
                    generation,
                    seconds,
                } => {
                    if !waveform.read(cx).accepts_seek_event(*generation) {
                        return;
                    }
                    if this.state.system.config.still_mode
                        || !this.state.system.config.waveform_seek_enabled
                    {
                        return;
                    }
                    if this.interaction.waveform_preview(
                        *seconds,
                        this.state.queue.current_id,
                        *generation,
                    ) {
                        cx.notify();
                    }
                }
                visualization::WaveformEvent::Seek {
                    generation,
                    seconds,
                } => {
                    if !waveform.read(cx).accepts_seek_event(*generation) {
                        return;
                    }
                    if let Some(seek) = this.interaction.finish_waveform(*seconds, *generation)
                        && !this.state.system.config.still_mode
                        && this.state.system.config.waveform_seek_enabled
                    {
                        this.send(
                            Command::SeekQueue {
                                queue_id: seek.queue_id,
                                seconds: seek.seconds,
                            },
                            cx,
                        );
                    }
                }
                visualization::WaveformEvent::Cancelled { generation } => {
                    if this.interaction.cancel_waveform(*generation) {
                        cx.notify();
                    }
                }
            }),
        );
        subscriptions.push(cx.subscribe(&inputs[&Field::Search], |this, _, event, cx| {
            if matches!(event, InputEvent::Changed { .. }) {
                this.refresh_search(cx);
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
            subscriptions.push(cx.subscribe(&inputs[&field], move |this, _, event, cx| {
                if let InputEvent::Changed { user } = event {
                    if *user {
                        this.metadata.edited_before_load(field);
                    }
                    cx.notify();
                }
            }));
        }
        subscriptions.push(
            cx.observe_window_visibility(window, |this, visibility, window, cx| {
                this.window_visible = matches!(visibility, WindowVisibility::Visible);
                this.frame_timer.take();
                if !this.window_visible {
                    this.invalidate_window_interaction(window, cx);
                }
                this.refresh(cx);
                this.sync_analysis(cx);
                this.request_frame(window, cx);
                cx.notify();
            }),
        );
        subscriptions.push(cx.observe_window_activation(window, |this, window, cx| {
            if !window.is_window_active() {
                this.cancel_interaction(window, cx);
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
        let mut app = Self {
            settings,
            handle,
            state,
            layouts,
            host,
            layout_path,
            inputs,
            browser: BrowserState::default(),
            metadata: MetadataSession::default(),
            layout_focus: cx.focus_handle(),
            scroll_views: scrolling::ScrollViews::default(),
            interaction: LayoutInteraction::default(),
            error,
            queue_save_pending: false,
            queue_save_feedback: None,
            artwork_manager,
            artwork_loader: artwork::ArtworkLoader::default(),
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
            waveform,
            analysis_worker_enabled: false,
            analysis_sequence: 0,
            analysis_staging: crate::analysis::AnalysisFrame::default(),
            analysis_needs_refresh: false,
            window_visible: window.is_visible(),
            frame_pending: false,
            frame_timer: None,
            last_analysis_request: None,
            _subscriptions: subscriptions,
            ui_font,
        };
        app.interaction
            .retain_layout(app.layouts.current().root.as_ref());
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
            ViewRequestKind::Library => self.browser.library.buffer().rows.error(index),
            ViewRequestKind::PlaylistSummaries => {
                self.browser.playlists.summaries().rows.error(index)
            }
            ViewRequestKind::PlaylistEntries => self.browser.playlists.entries().rows.error(index),
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
                    this.retry_view_page(kind, cx);
                    cx.notify();
                }))
            })
    }

    fn select_track(&mut self, id: i64, multi: bool, cx: &mut Context<Self>) {
        self.browser.library.invalidate_selection();
        self.browser
            .library
            .select_node(&LibraryNode::Track(id), multi);
        self.browser.library.focus_track(id);
        if let Some(selected) = self
            .browser
            .library
            .selected()
            .contains(&id)
            .then_some(id)
            .or_else(|| self.browser.library.selected().iter().copied().min())
        {
            self.request_track(selected, cx);
        } else {
            self.clear_metadata_selection(cx);
        }
        self.sync_waveform(cx);
        cx.notify();
    }
    fn select_queue_entry(&mut self, id: u64, shift: bool, multi: bool, cx: &mut Context<Self>) {
        self.browser.library.invalidate_selection();
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
                .browser
                .queue
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
            self.browser.queue.select_range(
                self.state.queue.entries[start..=end]
                    .iter()
                    .map(|entry| entry.id),
                anchor,
                multi,
            );
        } else {
            self.browser.queue.select(id, multi);
        }
        self.request_track(track_id, cx);
        self.sync_waveform(cx);
        cx.notify();
    }
    fn apply_interaction_cancellation(
        &mut self,
        cancellation: Cancellation,
        cx: &mut Context<Self>,
    ) {
        cancellation.restore_split(self.layouts.current_mut());
        self.waveform
            .update(cx, |waveform, _| waveform.cancel_seek());
    }

    fn cancel_interaction(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cancellation = self.interaction.cancel();
        if cancellation.docking {
            cx.stop_active_drag(window);
        }
        self.apply_interaction_cancellation(cancellation, cx);
        cx.notify();
    }

    fn invalidate_window_interaction(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cancellation = self.interaction.invalidate_window();
        if cancellation.docking {
            cx.stop_active_drag(window);
        }
        self.apply_interaction_cancellation(cancellation, cx);
        self.panel_menu = None;
        cx.notify();
    }

    fn synchronize_interaction(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let geometry = DockGeometry::current(window, self.state.system.config.ui_scale);
        if let Some(cancellation) = self.interaction.synchronize_geometry(geometry) {
            self.apply_interaction_cancellation(cancellation, cx);
        }
    }

    fn layout_changed(&mut self, cx: &mut Context<Self>) {
        let cancellation = self
            .interaction
            .layout_changed(self.layouts.current().root.as_ref());
        self.apply_interaction_cancellation(cancellation, cx);
        self.panel_menu = None;
        self.browser.clear_focus();
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
                self.browser.library.invalidate_projection();
            }
            let waveform_seek_disabled = self.state.system.config.waveform_seek_enabled
                && !state.system.config.waveform_seek_enabled;
            let still_mode_changed =
                self.state.system.config.still_mode != state.system.config.still_mode;
            if self.state.system.config.ui_font != state.system.config.ui_font {
                self.ui_font = fonts::resolve(&state.system.config.ui_font, cx);
            }
            self.state = state;
            self.sync_metadata_default(cx);
            self.browser.playlists.reconcile_selection();
            if playlist_changed || page_size_changed {
                self.request_playlist_selection(cx);
            }
            if still_mode_changed {
                let cancellation = self.interaction.cancel();
                self.apply_interaction_cancellation(cancellation, cx);
                // Resuming must wait for a new publication, not redraw pre-mode audio.
                self.analysis_staging = crate::analysis::AnalysisFrame::default();
                self.analysis_sequence = self.handle.analysis.read().sequence;
                self.analysis_needs_refresh = false;
            }
            if waveform_seek_disabled {
                let cancellation = self.interaction.cancel();
                self.apply_interaction_cancellation(cancellation, cx);
                self.waveform
                    .update(cx, |waveform, _| waveform.set_seek_enabled(false));
            }
            if roots_changed {
                self.browser.library.roots_changed();
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
                self.browser.queue.reconcile(|id| valid.contains(id));
            }
            if library_changed || library_structure_changed {
                if !page_size_changed {
                    self.request_library_page(
                        self.browser.library.buffer().rows.visible().start,
                        cx,
                    );
                    self.request_ranking(self.browser.history.offset(), cx);
                    self.browser.library.refresh_tree(self.view_page_size());
                    self.request_directory_page(library::TreeBranch::default(), 0, cx);
                }
                if let Some(id) = self.metadata.selected() {
                    self.request_track(id, cx);
                }
            }
            if !page_size_changed
                && (library_changed || library_structure_changed || playlist_changed)
            {
                self.request_playlist_summaries(
                    self.browser.playlists.summaries().rows.visible().start,
                    cx,
                );
                if let Some(id) = self.browser.playlists.entries().playlist_id {
                    self.request_playlist_entries(
                        id,
                        self.browser.playlists.entries().rows.visible().start,
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
                let track = this.state.current_track().or(this.metadata.track());
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
        let track = self.state.current_track().or(self.metadata.track());
        self.waveform.update(cx, |waveform, cx| {
            waveform.set_enabled(enabled);
            if !enabled {
                return;
            }
            waveform.retain_panels(&panels, cx);
            waveform.sync(track, cx);
        });
    }
    fn load_full_waveform(&mut self, cx: &mut Context<Self>) {
        if self.state.system.config.still_mode {
            return;
        }
        let track = self.state.current_track().or(self.metadata.track());
        self.waveform.update(cx, |waveform, cx| {
            waveform.load_full(track, &self.state.system.config, cx);
        });
    }
    fn persist_layout(&mut self, cx: &mut Context<Self>) {
        self.interaction.clear_docking_preview();
        self.layouts
            .current_mut()
            .initialize_audio_visualizers(AudioVisualizerOptions::from(
                self.state.system.config.as_ref(),
            ));
        self.interaction
            .retain_layout(self.layouts.current().root.as_ref());
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
        let track = current.or(self.metadata.track());
        let position = if current.is_some() {
            self.interaction
                .seek_preview()
                .unwrap_or(self.state.playback.position)
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
        let measured = self.interaction.measurement_handle(key);
        let ui_scale = self.state.system.config.ui_scale;
        canvas(
            move |bounds, window, _| {
                measured.record(bounds, DockGeometry::current(window, ui_scale));
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
        (f64::from(SPLIT_GUTTER) * self.interaction.scale_factor()).round()
            / self.interaction.scale_factor()
    }
    fn layout_edit_sizes(
        &self,
        panel: u64,
        docking: Option<(u64, Edge, u64)>,
    ) -> HashMap<u64, (f64, f64)> {
        let sizes = self
            .interaction
            .measurements()
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
        // Structural callers invalidate through layout_changed after adjusting sizes.
    }
    fn remove_layout_panel(&mut self, panel: u64, cx: &mut Context<Self>) {
        if self.layouts.current().locked {
            return;
        }
        let cancellation = self.interaction.cancel();
        self.apply_interaction_cancellation(cancellation, cx);
        let sizes = self.layout_edit_sizes(panel, None);
        if self.layouts.current_mut().remove(panel) {
            self.reconcile_layout_sizes(&sizes);
            self.layout_changed(cx);
            self.persist_layout(cx);
        }
    }
    fn begin_split_drag(
        &mut self,
        id: u64,
        index: usize,
        pointer: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.synchronize_interaction(window, cx);
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
        let measured = self.interaction.measurements();
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
        let original_fraction = boundary(index + 1);
        drop(measured);
        let cancellation = self.interaction.begin_split(drag, original_fraction, root);
        self.apply_interaction_cancellation(cancellation, cx);
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
                                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                    this.begin_split_drag(id, index, event.position, window, cx);
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
                                this.interaction.set_target_group(Some(node_id));
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
                                        this.start_panel_docking(drag.panel_id, window, cx);
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
        let Some(dragging) = self.interaction.dragging() else {
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
                let Some(bounds) = self.interaction.measurements().get(&key).copied() else {
                    return;
                };
                bounds
            }
        };
        match dragging {
            Dragging::Split(_) => {
                if self.layouts.current().locked {
                    return;
                }
                let config = &self.state.system.config;
                let Some((id, index, fraction)) = self.interaction.update_split(
                    position,
                    config.layout_snap_enabled ^ alt,
                    f64::from(config.ui_scale),
                    config.layout_snap_grid_px,
                ) else {
                    return;
                };
                if self.layouts.current_mut().set_boundary(id, index, fraction) {
                    cx.notify();
                }
            }
            Dragging::Seek(_) => {
                let fraction = ((position.x - bounds.origin.x) / bounds.size.width).clamp(0., 1.);
                if self.interaction.update_slider_seek(f64::from(fraction)) {
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
        match self.interaction.finish_drag() {
            DragCommit::None => {}
            DragCommit::Seek(seek) => self.send(
                Command::SeekQueue {
                    queue_id: seek.queue_id,
                    seconds: seek.seconds,
                },
                cx,
            ),
            DragCommit::PersistSplit => self.persist_layout(cx),
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
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.synchronize_interaction(window, cx);
                    let cancellation = this.interaction.begin_slider(
                        dragging,
                        this.state.queue.current_id,
                        this.state.playback.duration,
                    );
                    if cancellation.docking {
                        cx.stop_active_drag(window);
                    }
                    this.apply_interaction_cancellation(cancellation, cx);
                    this.drag_position(event.position, event.modifiers.alt, cx);
                }),
            )
            .into_any_element()
    }
}
