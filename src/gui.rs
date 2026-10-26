mod album_artwork;
mod album_browser;
mod appearance;
mod artwork;
mod audio_processing;
mod components;
mod coordination;
mod duplicates;
mod feature;
mod history;
mod layout;
mod library;
mod list_interaction;
mod metadata;
mod paged;
mod playback;
mod playlists;
mod presentation;
mod query;
mod queue;
mod scrolling;
pub(crate) mod session;
mod settings;
mod shell;
mod visualization;
mod workspace;
use crate::{
    config::AudioVisualizerOptions, core::CoreHandle, i18n::Localizer, projection::GuiSnapshot,
};
use anyhow::Result;
use ashpd::desktop::file_chooser::SelectedFiles;
use futures::{StreamExt, channel::mpsc};
use gpui::{prelude::*, *};
use layout::interaction::{
    Cancellation, DockGeometry, DockHover, DockRequest, DockSource, DragCommit, Dragging, Measured,
    PreparedDock,
};
use layout::panel_menu::{PanelMenu, PanelMenuPage};
use layout::save_owner::LayoutSaveHandle;
use layout::{Axis, Edge, Node};
use library::LibraryNode;
use list_interaction::{ContentDrag, ContentMenuRequest, TrackSource};
use rivu_protocol::model::{Command, Query, Track};
use rivu_ui::drag_preview;
use rivu_ui::input::{Input, InputEvent};
pub(crate) use session::UiSession;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::PathBuf,
    rc::{Rc, Weak},
    sync::Arc,
};
use url::Url;

pub(crate) fn init(cx: &mut App) -> Result<()> {
    #[cfg(feature = "embed-nerd-font")]
    appearance::fonts::register_symbols(cx)
        .map_err(|error| error.context("Could not load embedded icon font"))?;
    Input::init(cx);
    Ok(())
}

const SPLIT_GUTTER: f32 = 4.0;

struct PanelSpec {
    kind: &'static str,
    title_id: &'static str,
}
const PANELS: &[PanelSpec] = &[
    PanelSpec {
        kind: "transport",
        title_id: "shell-panel-playback",
    },
    PanelSpec {
        kind: "library",
        title_id: "shell-panel-library",
    },
    PanelSpec {
        kind: "albums",
        title_id: "shell-panel-albums",
    },
    PanelSpec {
        kind: "queue",
        title_id: "shell-panel-queue",
    },
    PanelSpec {
        kind: "playlists",
        title_id: "shell-panel-playlists",
    },
    PanelSpec {
        kind: "history",
        title_id: "shell-panel-history",
    },
    PanelSpec {
        kind: "metadata",
        title_id: "shell-panel-metadata",
    },
    PanelSpec {
        kind: "album_artwork",
        title_id: "shell-panel-album-artwork",
    },
    PanelSpec {
        kind: "spectrum",
        title_id: "shell-panel-spectrum",
    },
    PanelSpec {
        kind: "spectrogram",
        title_id: "shell-panel-spectrogram",
    },
    PanelSpec {
        kind: "waveform",
        title_id: "shell-panel-waveform",
    },
    PanelSpec {
        kind: "audio_visualizer",
        title_id: "shell-panel-audio-visualizer",
    },
    PanelSpec {
        kind: "audio_processing",
        title_id: "shell-panel-audio-processing",
    },
];
fn panel_spec(kind: &str) -> Option<&'static PanelSpec> {
    PANELS.iter().find(|panel| panel.kind == kind)
}

fn chooser_path(files: &SelectedFiles) -> Option<PathBuf> {
    files
        .uris()
        .first()
        .and_then(|uri| Url::parse(uri.as_str()).ok())
        .and_then(|uri| uri.to_file_path().ok())
}

fn chooser_result<T>(
    result: std::result::Result<T, ashpd::Error>,
) -> std::result::Result<Option<T>, ashpd::Error> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)) => Ok(None),
        Err(error) => Err(error),
    }
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
    source: DockSource,
    title: SharedString,
}
impl Render for PanelDrag {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        drag_preview(self.title.clone(), window, cx)
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
enum ListFocus {
    Library,
    Queue,
    PlaylistTracks,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BrowserFocus {
    List(ListFocus),
    Playlists,
    History,
}

fn panel_list_focus(kind: &str, preferred: Option<BrowserFocus>) -> Option<ListFocus> {
    match kind {
        "library" => Some(ListFocus::Library),
        "queue" => Some(ListFocus::Queue),
        "playlists" if preferred == Some(BrowserFocus::List(ListFocus::PlaylistTracks)) => {
            Some(ListFocus::PlaylistTracks)
        }
        _ => None,
    }
}

#[derive(Clone, Copy)]
pub(super) enum DragDestination {
    Queue,
    Playlist(i64),
    QueueAt {
        index: usize,
        revision: u64,
    },
    PlaylistAt {
        playlist_id: i64,
        index: usize,
        revision: u64,
    },
}

impl DragDestination {
    fn playlist_id(self) -> Option<i64> {
        match self {
            Self::Playlist(id)
            | Self::PlaylistAt {
                playlist_id: id, ..
            } => Some(id),
            Self::Queue | Self::QueueAt { .. } => None,
        }
    }

    fn current(self, state: &crate::projection::GuiSnapshot) -> bool {
        match self {
            Self::Queue => true,
            Self::QueueAt { revision, .. } => state.queue.revision == revision,
            Self::Playlist(id) => state.library.playlist_entry_revisions.contains_key(&id),
            Self::PlaylistAt {
                playlist_id,
                revision,
                ..
            } => state.library.playlist_entry_revisions.get(&playlist_id) == Some(&revision),
        }
    }
}
#[derive(Default)]
struct FullWaveformRequest {
    generation: u64,
    target: Option<FullWaveformTarget>,
    worker: Option<u64>,
}

struct FullWaveformTarget {
    track_id: i64,
    path: PathBuf,
    fingerprint: Option<String>,
    cue_range: Option<(u64, Option<u64>)>,
}

impl FullWaveformTarget {
    fn cue_range(track: &Track) -> Option<(u64, Option<u64>)> {
        track
            .cue
            .as_ref()
            .map(|cue| (cue.start_frame, cue.end_frame))
    }

    fn matches_source(&self, track: &Track) -> bool {
        self.path == track.path
            && self.cue_range == Self::cue_range(track)
            && self.fingerprint == track.fingerprint
    }

    fn matches(&self, track: &Track) -> bool {
        self.track_id == track.id && self.matches_source(track)
    }
}

impl FullWaveformRequest {
    fn begin(&mut self, track: &Track) -> u64 {
        self.invalidate();
        self.target = Some(FullWaveformTarget {
            track_id: track.id,
            path: track.path.clone(),
            fingerprint: track.fingerprint.clone(),
            cue_range: FullWaveformTarget::cue_range(track),
        });
        self.generation
    }

    fn reserve(&mut self) -> Option<(u64, i64)> {
        if self.worker.is_some() {
            return None;
        }
        let target = self.target.as_ref()?;
        self.worker = Some(self.generation);
        Some((self.generation, target.track_id))
    }

    fn retire(&mut self, generation: u64) {
        if self.worker == Some(generation) {
            self.worker = None;
        }
    }

    fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.target = None;
    }

    fn retain_target(&mut self, track: Option<&Track>, still_mode: bool) {
        if self
            .target
            .as_ref()
            .is_some_and(|target| still_mode || track.is_none_or(|track| !target.matches(track)))
        {
            self.invalidate();
        }
    }

    fn saved_hint_changed(&mut self, track: &Track) {
        if self
            .target
            .as_ref()
            .is_some_and(|target| target.matches_source(track))
        {
            self.invalidate();
        }
    }

    fn accepts(&self, generation: u64, track: Option<&Track>) -> bool {
        self.generation == generation
            && self
                .target
                .as_ref()
                .is_some_and(|target| track.is_some_and(|track| target.matches(track)))
    }
}

pub(super) struct GuiApp {
    pub(super) i18n: Arc<Localizer>,
    localized_text: Rc<HashMap<String, SharedString>>,
    handle: CoreHandle,
    snapshots: Rc<feature::SnapshotSource>,
    state: Rc<GuiSnapshot>,
    workspace: workspace::Workspace,
    session: Weak<RefCell<UiSession>>,
    layout_name: Entity<Input>,
    // Last explicitly targeted panel; unsupported panels suppress row keys too.
    search_panel: Option<u64>,
    focus: Option<BrowserFocus>,
    library: Entity<library::Library>,
    queue: Entity<queue::Queue>,
    playlists: Entity<playlists::Playlists>,
    history: Entity<history::History>,
    metadata: Entity<metadata::Metadata>,
    duplicates: Entity<duplicates::Duplicates>,
    playback: Entity<playback::Playback>,
    manual_intents: coordination::manual_actions::ManualIntents,
    shell: shell::WindowShell,

    error: Option<String>,
    artwork_manager: Arc<crate::artwork::ArtworkManager>,
    artwork_images: artwork::ArtworkImages,
    default_album: Entity<artwork::Artwork>,
    album_artwork: Entity<album_artwork::AlbumArtwork>,
    artwork_panels: HashMap<u64, Entity<album_artwork::AlbumArtworkPanel>>,
    album_browsers: HashMap<u64, Entity<album_browser::AlbumBrowser>>,
    panel_views: HashMap<u64, Entity<shell::panels::PanelView>>,
    queue_replace: Option<coordination::queue_replace::QueueReplacement>,
    settings: Entity<settings::Settings>,
    audio_processing: Entity<audio_processing::AudioProcessing>,
    presentation: Entity<presentation::Presentation>,
    layout_action: Option<layout::layout_manager::Action>,
    panel_menu: Option<PanelMenu>,
    content_menu: Option<list_interaction::ContentMenu>,
    content_resolution: list_interaction::CollectionResolution,
    waveform: Entity<visualization::Waveform>,
    full_waveform_request: FullWaveformRequest,
    window_visible: bool,
    ui_font: Font,
    _subscriptions: Vec<Subscription>,
}

impl GuiApp {
    fn panel_demand(&self, kind: &str) -> bool {
        self.workspace.panel_demand(
            kind,
            self.window_visible && !self.state.system.shutting_down,
        )
    }
    pub(super) fn tr(&self, id: &str) -> SharedString {
        self.localized_text
            .get(id)
            .expect("cached localized GUI text")
            .clone()
    }

    fn input_focused(&self, window: &Window, cx: &App) -> bool {
        self.layout_name.read(cx).is_focused(window)
            || self.library.read(cx).input_focused(window, cx)
            || self.queue.read(cx).input_focused(window, cx)
            || self.playlists.read(cx).input_focused(window, cx)
            || self.history.read(cx).input_focused(window, cx)
            || self.metadata.read(cx).input_focused(window, cx)
            || self.audio_processing.read(cx).input_focused(window, cx)
            || self
                .album_browsers
                .values()
                .any(|albums| albums.read(cx).input_focused(window, cx))
    }

    fn focus_layout(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.shell.layout_focus().focus(window, cx);
    }

    fn route_search_key(
        &mut self,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        for kind in ["library", "queue", "playlists", "history"] {
            let focused = match kind {
                "library" => self.library.read(cx).input_focused(window, cx),
                "queue" => self.queue.read(cx).input_focused(window, cx),
                "playlists" => self.playlists.read(cx).input_focused(window, cx),
                _ => self.history.read(cx).input_focused(window, cx),
            };
            if focused {
                return self.route_feature_search(kind, focus, window, cx);
            }
        }
        if let Some(albums) = self
            .album_browsers
            .values()
            .find(|albums| albums.read(cx).input_focused(window, cx))
            .cloned()
        {
            return albums.update(cx, |albums, cx| {
                if focus {
                    albums.focus_search(window, cx);
                    true
                } else {
                    albums.escape_search(window, cx)
                }
            });
        }
        let panels = self.workspace.layouts.current().active_panels();
        if self.input_focused(window, cx) {
            return false;
        }
        let panel = self
            .search_panel
            .and_then(|id| panels.iter().find(|panel| panel.id == id).copied())
            .or_else(|| {
                let kind = match self.focus {
                    Some(BrowserFocus::List(ListFocus::Library)) => "library",
                    Some(BrowserFocus::List(ListFocus::Queue)) => "queue",
                    Some(BrowserFocus::List(ListFocus::PlaylistTracks))
                    | Some(BrowserFocus::Playlists) => "playlists",
                    Some(BrowserFocus::History) => "history",
                    None => return None,
                };
                panels.iter().find(|panel| panel.kind == kind).copied()
            })
            .or_else(|| {
                panels
                    .iter()
                    .find(|panel| {
                        matches!(
                            panel.kind.as_str(),
                            "library" | "queue" | "playlists" | "history" | "albums"
                        )
                    })
                    .copied()
            });
        let Some(panel) = panel else {
            return false;
        };
        let id = panel.id;
        let kind = panel.kind.clone();
        if kind == "albums" {
            let Some(albums) = self.album_browsers.get(&id).cloned() else {
                return false;
            };
            return albums.update(cx, |albums, cx| {
                if focus {
                    albums.focus_search(window, cx);
                    true
                } else {
                    albums.escape_search(window, cx)
                }
            });
        }
        self.route_feature_search(&kind, focus, window, cx)
    }

    fn route_feature_search(
        &mut self,
        kind: &str,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match kind {
            "library" => self.library.update(cx, |owner, cx| {
                if focus {
                    owner.focus_search(window, cx);
                    true
                } else {
                    owner.escape_search(window, cx)
                }
            }),
            "queue" => self.queue.update(cx, |owner, cx| {
                if focus {
                    owner.focus_search(window, cx);
                    true
                } else {
                    owner.escape_search(window, cx)
                }
            }),
            "history" => self.history.update(cx, |owner, cx| {
                if focus {
                    owner.focus_search(window, cx);
                    true
                } else {
                    owner.escape_search(window, cx)
                }
            }),
            "playlists" => {
                let entries = self.focus == Some(BrowserFocus::List(ListFocus::PlaylistTracks));
                self.playlists.update(cx, |owner, cx| {
                    if focus {
                        owner.focus_search(entries, window, cx);
                        true
                    } else {
                        owner.escape_search(entries, window, cx)
                    }
                })
            }
            _ => false,
        }
    }

    fn dismiss_input_composition(&self, window: &Window, cx: &mut Context<Self>) -> bool {
        self.metadata
            .update(cx, |owner, cx| owner.dismiss_input_composition(window, cx))
            || (self.layout_name.read(cx).is_focused(window)
                && self
                    .layout_name
                    .update(cx, |input, cx| input.dismiss_composition(cx)))
    }

    fn activate_library(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        focus_search: bool,
    ) {
        self.cancel_interaction(window, cx);
        if let Some(panel_id) = self
            .workspace
            .layouts
            .current()
            .root
            .as_ref()
            .and_then(|root| root.find_panel(&|panel| panel.kind == "library"))
            .map(|panel| panel.id)
        {
            self.workspace.layouts.current_mut().activate(panel_id);
            self.set_browser_focus(Some(BrowserFocus::List(ListFocus::Library)), cx);
            self.search_panel = Some(panel_id);
            self.shell.dismiss(shell::Surface::PanelCatalog);
            if focus_search {
                self.library
                    .update(cx, |owner, cx| owner.focus_search(window, cx));
            }
            self.persist_layout(cx);
        } else if self.workspace.layouts.current().locked {
            self.error = Some(self.i18n.text("shell-library-locked").to_owned());
            cx.notify();
        } else {
            let panel_id = self
                .workspace
                .layouts
                .current_mut()
                .add("library", self.workspace.interaction.target_group());
            self.workspace.layouts.current_mut().activate(panel_id);
            self.layout_changed(cx);
            self.search_panel = Some(panel_id);
            self.set_browser_focus(Some(BrowserFocus::List(ListFocus::Library)), cx);
            self.shell.dismiss(shell::Surface::PanelCatalog);
            if focus_search {
                self.library
                    .update(cx, |owner, cx| owner.focus_search(window, cx));
            }
            self.persist_layout(cx);
        }
    }

    fn reveal_queue_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.cancel_interaction(window, cx);
        let panel = self
            .workspace
            .layouts
            .current()
            .root
            .as_ref()
            .and_then(|root| root.find_panel(&|panel| panel.kind == "queue"))
            .map(|panel| panel.id);
        let panel_id = if let Some(panel) = panel {
            self.workspace.layouts.current_mut().activate(panel);
            panel
        } else if self.workspace.layouts.current().locked {
            self.error = Some(self.i18n.text("gl-locate-queue-locked").to_owned());
            cx.notify();
            return false;
        } else {
            let panel = self
                .workspace
                .layouts
                .current_mut()
                .add("queue", self.workspace.interaction.target_group());
            self.layout_changed(cx);
            panel
        };
        self.search_panel = Some(panel_id);
        self.shell.dismiss(shell::Surface::PanelCatalog);
        self.set_browser_focus(Some(BrowserFocus::List(ListFocus::Queue)), cx);
        self.focus_layout(window, cx);
        self.persist_layout(cx);
        true
    }

    fn active_list_focus(&self) -> Option<ListFocus> {
        let panel = self.keyboard_panel()?;
        panel_list_focus(&panel.kind, self.focus)
    }

    fn navigate_list(&mut self, focus: ListFocus, down: bool, cx: &mut Context<Self>) {
        match focus {
            ListFocus::Library => self
                .library
                .update(cx, |owner, cx| owner.navigate_library_tree(down, cx)),
            ListFocus::Queue => self
                .queue
                .update(cx, |owner, cx| owner.navigate_queue(down, cx)),
            ListFocus::PlaylistTracks => self.playlists.update(cx, |owner, cx| {
                owner.navigate_playlist(down, false, false, cx)
            }),
        }
    }

    fn play_focused_selection(&mut self, focus: ListFocus, cx: &mut Context<Self>) {
        match focus {
            ListFocus::Library => self.library.update(cx, |owner, cx| {
                if !owner.library_range_pending() {
                    owner.activate_library_tree(cx);
                }
            }),
            ListFocus::Queue => {
                if let Some(row) = self.queue.read(cx).active() {
                    self.send(Command::PlayQueue { queue_id: row.id }, cx);
                }
            }
            ListFocus::PlaylistTracks => self
                .playlists
                .update(cx, |owner, cx| owner.play_selection(cx)),
        }
    }

    fn remove_focused_queue(&mut self, focus: ListFocus, cx: &mut Context<Self>) {
        if focus == ListFocus::PlaylistTracks {
            self.playlists
                .update(cx, |owner, cx| owner.remove_playlist_selection(cx));
            return;
        }
        if focus != ListFocus::Queue {
            return;
        }
        let queue_ids = self.queue.read(cx).selected_queue_ids_in_order();
        if queue_ids.is_empty() {
            return;
        }
        self.queue.update(cx, |owner, cx| owner.clear_selection(cx));

        self.send(Command::RemoveQueueEntries { queue_ids }, cx);
    }
    fn new(
        handle: CoreHandle,
        layout_save: LayoutSaveHandle,
        artwork_manager: Arc<crate::artwork::ArtworkManager>,
        session: Weak<RefCell<UiSession>>,
        i18n: Arc<Localizer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let snapshots = Rc::new(feature::SnapshotSource::new(handle.clone()));
        let state = snapshots.read();
        window
            .set_gpu_material_budget(state.system.config.gpu_material_budget_bytes())
            .expect("validated GPU material budget");
        appearance::theme::set_theme(state.system.config.ui_theme, cx);
        let localized_text = Rc::new(
            i18n.fixed_texts()
                .map(|(id, text)| (id.to_owned(), SharedString::from(text.to_owned())))
                .collect(),
        );
        let (mut workspace, load_error) = workspace::Workspace::new(
            layout_save.clone(),
            AudioVisualizerOptions::from(state.system.config.as_ref()),
            crate::config::AlbumArtworkOptions::from(state.system.config.as_ref()),
        );
        let error = load_error
            .map(|error| i18n.format("shell-load-layouts-error", &[("error", error.into())]));
        workspace.save_error = error.clone();
        let ui_font = appearance::fonts::resolve(&state.system.config.ui_font, cx);
        appearance::fonts::publish(&ui_font, state.system.config.ui_font_size, cx);
        let layout_name = cx.new(|cx| {
            Input::new(
                i18n.text("shell-layout-name").to_owned(),
                "",
                i18n.text("shell-layout-name").to_owned(),
                cx,
            )
        });
        let mut subscriptions = Vec::new();
        let settings = cx.new(|cx| {
            settings::Settings::new(
                Arc::clone(&i18n),
                handle.clone(),
                Arc::clone(&artwork_manager),
                Arc::clone(&state.system.devices),
                (
                    state.system.ffmpeg_status.clone(),
                    state.system.mpris_status.clone(),
                ),
                state.system.config.ui_scale,
                cx,
            )
        });
        subscriptions.push(cx.subscribe_in(
            &settings,
            window,
            |this, settings, event, window, cx| match event {
                settings::SettingsEvent::Closed => {
                    this.shell.dismiss(shell::Surface::Settings);
                    this.focus_layout(window, cx);
                    cx.notify();
                }
                settings::SettingsEvent::ReviewDuplicates => {
                    this.cancel_interaction(window, cx);
                    this.panel_menu = None;
                    this.shell.dismiss(shell::Surface::AppMenu);
                    this.shell.dismiss(shell::Surface::PanelCatalog);
                    this.duplicates.update(cx, |owner, cx| owner.open(cx));
                }
                settings::SettingsEvent::ApplyVisualizer {
                    panel_id,
                    options,
                    session,
                } => {
                    if this
                        .workspace
                        .layouts
                        .current_mut()
                        .set_audio_visualizer_options(*panel_id, *options)
                    {
                        this.workspace.submit_save();
                        settings.update(cx, |settings, cx| {
                            settings.complete_panel_apply(*session, Ok(()), cx)
                        });
                    } else {
                        settings.update(cx, |settings, cx| {
                            settings.complete_panel_apply(
                                *session,
                                Err(this.i18n.text("shell-visualizer-gone").to_owned()),
                                cx,
                            )
                        });
                    }
                    this.sync_analysis(cx);
                    cx.notify();
                }
                settings::SettingsEvent::ApplyArtwork {
                    panel_id,
                    options,
                    session,
                } => {
                    if this
                        .workspace
                        .layouts
                        .current_mut()
                        .set_album_artwork_options(*panel_id, *options)
                    {
                        this.workspace.submit_save();
                        settings.update(cx, |settings, cx| {
                            settings.complete_panel_apply(*session, Ok(()), cx)
                        });
                    } else {
                        settings.update(cx, |settings, cx| {
                            settings.complete_panel_apply(
                                *session,
                                Err(this.i18n.text("shell-artwork-gone").to_owned()),
                                cx,
                            )
                        });
                    }
                    this.sync_analysis(cx);
                    cx.notify();
                }
            },
        ));
        let waveform = cx.new(|_| visualization::Waveform::new(Arc::clone(&handle.waveform)));
        let presentation = cx.new(|cx| {
            presentation::Presentation::new(handle.clone(), Arc::clone(&i18n), waveform.clone(), cx)
        });
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
                    if this.workspace.interaction.waveform_preview(
                        *seconds,
                        this.state.queue.current_id,
                        *generation,
                    ) {
                        this.sync_waveform(cx);
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
                    if let Some(seek) = this
                        .workspace
                        .interaction
                        .finish_waveform(*seconds, *generation)
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
                    if this.workspace.interaction.cancel_waveform(*generation) {
                        this.sync_waveform(cx);
                        cx.notify();
                    }
                }
            }),
        );
        subscriptions.push(
            cx.subscribe_in(&layout_name, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::Submit)
                    && this.shell.is_open(shell::Surface::Layouts)
                    && this.layout_action.is_some()
                {
                    this.finish_layout_action(window, cx);
                }
            }),
        );
        subscriptions.push(
            cx.observe_window_visibility(window, |this, visibility, window, cx| {
                this.window_visible = matches!(visibility, WindowVisibility::Visible);
                this.reconcile_feature_demand(cx);
                if !this.window_visible {
                    this.invalidate_window_interaction(window, cx);
                    this.cancel_window_reads(cx);
                }
                this.refresh(cx);
                this.sync_analysis(cx);
                if !this.window_visible {
                    // Hidden windows may never deliver another paint/frame.
                    // Release display publications and artwork without a cleanup frame.
                    this.release_display_resources(window, cx);
                }
                this.presentation.update(cx, |presentation, cx| {
                    presentation.request_frame(window, cx)
                });
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
                    .update_in(cx, |this, window, cx| {
                        this.refresh(cx);
                        if !this.window_visible {
                            this.artwork_images.release_all_retired(window, cx);
                        }
                        this.presentation.update(cx, |presentation, cx| {
                            presentation.request_frame(window, cx)
                        });
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let (save_sender, mut save_receiver) = mpsc::channel::<()>(1);
        let save_sender = parking_lot::Mutex::new(save_sender);
        layout_save.set_wakeup(move || {
            let _ = save_sender.lock().try_send(());
        });
        cx.spawn(async move |this, cx| {
            while save_receiver.next().await.is_some() {
                if this
                    .update(cx, |this, cx| this.sync_layout_save(cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let default_album = cx.new(|cx| artwork::Artwork::new(&i18n, cx));
        let album_default = default_album.read(cx).circular();
        let artwork_images =
            artwork::ArtworkImages::new(state.system.config.artwork_decode_budget_mb);
        let feature_context = feature::FeatureContext {
            snapshots: Rc::clone(&snapshots),
            i18n: Arc::clone(&i18n),
            labels: Rc::clone(&localized_text),
            scroll: Rc::clone(&workspace.scroll_views),
        };
        let mut app = Self {
            i18n: Arc::clone(&i18n),
            localized_text,
            settings,
            handle,
            snapshots,
            state,
            workspace,
            session,
            layout_name,
            search_panel: None,
            focus: None,
            library: cx.new(|cx| library::Library::new(feature_context.clone(), cx)),
            queue: cx.new(|cx| queue::Queue::new(feature_context.clone(), cx)),
            playlists: cx.new(|cx| playlists::Playlists::new(feature_context.clone(), cx)),
            history: cx
                .new(|cx| history::History::new(feature_context.clone(), ui_font.clone(), cx)),
            metadata: cx.new(|cx| {
                metadata::Metadata::new(
                    feature_context.clone(),
                    ui_font.clone(),
                    Arc::clone(&artwork_manager),
                    artwork_images.clone(),
                    default_album.clone(),
                    cx,
                )
            }),
            duplicates: cx.new(|cx| duplicates::Duplicates::new(feature_context.clone(), cx)),
            audio_processing: cx
                .new(|cx| audio_processing::AudioProcessing::new(feature_context.clone(), cx)),
            playback: cx.new(|cx| playback::Playback::new(feature_context, cx)),
            manual_intents: coordination::manual_actions::ManualIntents::default(),
            shell: shell::WindowShell::new(cx),
            error,
            queue_replace: None,
            album_artwork: cx.new(|cx| {
                let default_album = cx.new(|_| album_default);
                album_artwork::AlbumArtwork::new(
                    Arc::clone(&artwork_manager),
                    default_album,
                    artwork_images.clone(),
                )
            }),
            artwork_panels: HashMap::new(),
            album_browsers: HashMap::new(),
            panel_views: HashMap::new(),
            artwork_manager,
            artwork_images,
            default_album,
            presentation,
            layout_action: None,
            panel_menu: None,
            content_menu: None,
            content_resolution: list_interaction::CollectionResolution::default(),
            waveform,
            full_waveform_request: FullWaveformRequest::default(),
            window_visible: window.is_visible(),
            _subscriptions: subscriptions,
            ui_font,
        };
        let subscriptions = app.subscribe_features(window, cx);
        app._subscriptions.extend(subscriptions);
        app.reconcile_features(cx);
        app.sync_layout_save(cx);
        app.sync_analysis(cx);
        // Initialization may have completed before the wakeup callback was
        // installed. Reconcile once so the first frame never keeps the empty snapshot.
        app.refresh(cx);
        app.queue.update(cx, |queue, cx| queue.refresh_queue(cx));
        app.shell.layout_focus().focus(window, cx);
        window
            .observe_release(&cx.entity(), cx, |this: &mut Self, window, cx| {
                this.content_resolution.cancel();
                this.release_display_resources(window, cx);
            })
            .detach();
        app
    }
    fn release_display_resources(&self, window: &mut Window, cx: &mut App) {
        self.presentation.update(cx, |presentation, cx| {
            presentation.release_display_resources(cx);
        });
        self.metadata.update(cx, |metadata, cx| {
            metadata.release_render_images(window, cx)
        });
        self.album_artwork.update(cx, |artwork, cx| {
            artwork.release_all_images(window, cx);
        });
        for albums in self.album_browsers.values() {
            albums.update(cx, |albums, cx| albums.release_all_images(window, cx));
        }
        self.artwork_images.release_all_retired(window, cx);
    }
    fn send(&mut self, command: Command, cx: &mut Context<Self>) {
        if let Err(error) = self.handle.send(command) {
            self.error = Some(format!("{error:#}"));
        }
        cx.notify();
    }
    fn save_queue_as_playlist(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.queue.total == 0 {
            return;
        }
        self.playlists.update(cx, |owner, cx| {
            owner.begin_playlist_name(
                playlists::PlaylistNameAction::SaveQueue,
                format!("Queue_{}", chrono::Local::now().format("%Y%m%d_%H%M%S")),
                window,
                cx,
            )
        });
    }

    fn apply_interaction_cancellation(
        &mut self,
        cancellation: Cancellation,
        cx: &mut Context<Self>,
    ) {
        self.workspace.restore_interaction(&cancellation);
        self.audio_processing
            .update(cx, |owner, cx| owner.invalidate_geometry(cx));
        self.waveform
            .update(cx, |waveform, _| waveform.cancel_seek());
        self.sync_waveform(cx);
    }

    fn cancel_interaction(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cancellation = self.workspace.interaction.cancel();
        if cancellation.docking {
            cx.stop_active_drag(window);
        }
        self.apply_interaction_cancellation(cancellation, cx);
        cx.notify();
    }

    fn invalidate_window_interaction(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cancellation = self.workspace.interaction.invalidate_window();
        if cancellation.docking {
            cx.stop_active_drag(window);
        }
        self.apply_interaction_cancellation(cancellation, cx);
        self.panel_menu = None;
        self.replace_content_menu(None, cx);
        self.content_resolution.cancel();
        cx.notify();
    }

    fn synchronize_interaction(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let geometry = DockGeometry::current(window, self.state.system.config.ui_scale);
        if let Some(cancellation) = self.workspace.interaction.synchronize_geometry(geometry) {
            self.apply_interaction_cancellation(cancellation, cx);
        }
    }

    fn layout_changed(&mut self, cx: &mut Context<Self>) {
        let cancellation = self.workspace.reconcile_structure();
        self.apply_layout_change(cancellation, cx);
    }

    fn apply_layout_change(&mut self, cancellation: Cancellation, cx: &mut Context<Self>) {
        self.panel_views
            .retain(|id, _| self.workspace.layouts.current().contains_panel(*id));
        self.apply_interaction_cancellation(cancellation, cx);
        self.panel_menu = None;
        self.replace_content_menu(None, cx);
        self.content_resolution.cancel();
        self.set_browser_focus(None, cx);
        for view in self.panel_views.values() {
            view.update(cx, |_, cx| cx.notify());
        }
        self.reconcile_feature_demand(cx);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let state = self.snapshots.read();
        if state.system.revision == self.state.system.revision {
            return;
        }
        if state.system.shutting_down && !self.state.system.shutting_down {
            self.content_resolution.cancel();
        }
        let changed = state.revisions.changed_since(self.state.revisions);
        let same_track = match (&state.current_track, &self.state.current_track) {
            (Some(next), Some(previous)) => Arc::ptr_eq(next, previous),
            (None, None) => true,
            _ => false,
        };
        let transport_only = changed.playback
            && !changed.library
            && !changed.queue
            && !changed.system
            && same_track
            && state.playback.status == self.state.playback.status;
        if !Arc::ptr_eq(&self.state.system.devices, &state.system.devices)
            || self.state.system.ffmpeg_status != state.system.ffmpeg_status
            || self.state.system.mpris_status != state.system.mpris_status
            || self.state.system.config.ui_scale != state.system.config.ui_scale
            || self.state.system.config.ui_font_size != state.system.config.ui_font_size
        {
            self.settings.update(cx, |settings, cx| {
                settings.sync_runtime(
                    &state.system.devices,
                    &state.system.ffmpeg_status,
                    &state.system.mpris_status,
                    state.system.config.ui_scale,
                    state.system.config.ui_font_size,
                    cx,
                );
            });
        }
        if self.state.library.revision != state.library.revision {
            self.settings
                .update(cx, |settings, cx| settings.sync_library_stats(cx));
        }
        let waveform_seek_disabled = self.state.system.config.waveform_seek_enabled
            && !state.system.config.waveform_seek_enabled;
        let still_mode_changed =
            self.state.system.config.still_mode != state.system.config.still_mode;
        let font_changed = self.state.system.config.ui_font != state.system.config.ui_font;
        let font_size_changed =
            self.state.system.config.ui_font_size != state.system.config.ui_font_size;
        let appearance_changed = font_changed
            || font_size_changed
            || self.state.system.config.ui_theme != state.system.config.ui_theme;
        if font_changed {
            self.ui_font = appearance::fonts::resolve(&state.system.config.ui_font, cx);
        }
        if font_changed || font_size_changed {
            appearance::fonts::publish(&self.ui_font, state.system.config.ui_font_size, cx);
        }
        if self.state.system.config.ui_theme != state.system.config.ui_theme {
            appearance::theme::set_theme(state.system.config.ui_theme, cx);
        }
        self.state = state;
        self.reconcile_features(cx);
        if appearance_changed {
            for view in self.panel_views.values() {
                view.update(cx, |_, cx| cx.notify());
            }
        }
        if self
            .artwork_images
            .set_decode_budget_mb(self.state.system.config.artwork_decode_budget_mb, cx)
        {
            self.album_artwork.update(cx, |_, cx| cx.notify());
            for albums in self.album_browsers.values() {
                albums.update(cx, |_, cx| cx.notify());
            }
            cx.notify();
        }
        if still_mode_changed || waveform_seek_disabled {
            let cancellation = self.workspace.interaction.cancel();
            self.apply_interaction_cancellation(cancellation, cx);
        }
        if waveform_seek_disabled {
            self.waveform
                .update(cx, |waveform, _| waveform.set_seek_enabled(false));
        }
        self.sync_analysis(cx);
        if !transport_only && !changed.is_empty() {
            cx.notify();
        }
    }

    fn album_browser_panel(
        &mut self,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let page_size = self.state.system.config.page_size as usize;
        let albums = self.album_browsers.entry(id).or_insert_with(|| {
            let albums = cx.new(|cx| {
                album_browser::AlbumBrowser::new(
                    id,
                    feature::FeatureContext {
                        snapshots: Rc::clone(&self.snapshots),
                        i18n: Arc::clone(&self.i18n),
                        labels: Rc::clone(&self.localized_text),
                        scroll: Rc::clone(&self.workspace.scroll_views),
                    },
                    Arc::clone(&self.artwork_manager),
                    self.default_album.clone(),
                    self.artwork_images.clone(),
                    cx,
                )
            });
            cx.subscribe(&albums, |this, _, request: &ContentMenuRequest, cx| {
                this.open_content_menu(request.clone(), cx);
            })
            .detach();
            cx.subscribe(
                &albums,
                |this, _, request: &album_browser::AlbumPlayRequest, cx| {
                    this.request_album_play(request.0.clone(), cx);
                },
            )
            .detach();
            window
                .observe_release(&albums, cx, |albums, _, cx| {
                    // A live panel removal still leaves the previous scene.
                    // The window root schedules retirement after its next paint.
                    albums.retire_all_images(cx);
                })
                .detach();
            albums
        });
        let visible = self.window_visible && !self.state.system.shutting_down;
        albums.update(cx, |albums, cx| {
            albums.sync(
                visible,
                self.state.query_revisions(None),
                page_size,
                self.state.current_track().map(|track| track.id),
                self.state.playback.status,
                cx,
            );
        });
        albums.clone().into_any_element()
    }

    fn sync_album_browser(&mut self, cx: &mut Context<Self>) {
        if self.album_browsers.is_empty() {
            return;
        }
        let layout = self.workspace.layouts.current();
        let visible = self.window_visible && !self.state.system.shutting_down;
        let revisions = self.state.query_revisions(None);
        let page_size = self.state.system.config.page_size as usize;
        let current_track = self.state.current_track().map(|track| track.id);
        let playback_status = self.state.playback.status;
        self.album_browsers.retain(|id, albums| {
            let exists = layout
                .root
                .as_ref()
                .and_then(|root| {
                    root.find_panel(&|panel| panel.id == *id && panel.kind == "albums")
                })
                .is_some();
            let shown = visible && layout.is_panel_active(*id);
            albums.update(cx, |albums, cx| {
                albums.sync(
                    shown,
                    revisions,
                    page_size,
                    current_track,
                    playback_status,
                    cx,
                );
            });
            exists
        });
    }

    fn sync_album_artwork(&mut self, cx: &mut Context<Self>) {
        self.sync_album_browser(cx);
        self.artwork_panels.retain(|id, _| {
            self.workspace
                .layouts
                .current()
                .album_artwork_options(*id)
                .is_some()
        });
        self.album_artwork.update(cx, |artwork, cx| {
            artwork.sync(
                self.state.current_track(),
                self.state.playback.status,
                &self.state.system.config,
                self.workspace.layouts.current().root.as_ref(),
                self.window_visible && !self.state.system.shutting_down,
                cx,
            );
        });
    }

    fn sync_analysis(&mut self, cx: &mut Context<Self>) {
        self.sync_album_artwork(cx);
        self.sync_waveform(cx);
    }

    fn sync_full_waveform_request(&mut self, cx: &App) {
        self.full_waveform_request.retain_target(
            self.state
                .current_track()
                .or(self.metadata.read(cx).track()),
            self.state.system.config.still_mode,
        );
    }

    fn sync_waveform(&mut self, cx: &mut Context<Self>) {
        self.sync_full_waveform_request(cx);
        self.sync_playback_interaction(cx);
        let metadata_track = self.metadata.read(cx).track_snapshot();
        let current = self.state.current_track();
        let track = current.or(metadata_track.as_deref());
        let position = if current.is_some() {
            self.workspace
                .interaction
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
        self.presentation.update(cx, |presentation, cx| {
            presentation.sync(
                presentation::PresentationState {
                    config: Arc::clone(&self.state.system.config),
                    layout: self.workspace.layouts.current(),
                    status: self.state.playback.status,
                    visible: self.window_visible,
                    shutting_down: self.state.system.shutting_down,
                    track,
                    position,
                    duration,
                },
                cx,
            );
        });
    }
    fn load_full_waveform(&mut self, cx: &mut Context<Self>) {
        self.refresh(cx);
        if self.state.system.config.still_mode {
            return;
        }
        let Some(track) = self
            .state
            .current_track()
            .or(self.metadata.read(cx).track())
            .cloned()
        else {
            return;
        };
        self.full_waveform_request.begin(&track);
        let snapshot = self
            .state
            .current_track()
            .map(|_| self.handle.waveform_snapshot());
        self.waveform.update(cx, |waveform, cx| {
            waveform.prepare_full(&track, &self.state.system.config, snapshot, cx);
        });
        self.dispatch_full_waveform_query(cx);
    }

    fn dispatch_full_waveform_query(&mut self, cx: &mut Context<Self>) {
        let Some((generation, track_id)) = self.full_waveform_request.reserve() else {
            return;
        };
        let handle = self.handle.clone();
        let query = Query::Track { track_id };
        let stamp = query.stamp(self.state.query_revisions_for(&query));
        cx.spawn(async move |this, cx| {
            let response = query::run(
                handle,
                cx,
                move |handle| async move { handle.query_for_gui(Query::Track { track_id }).await },
                |response, cx| {
                    let _ = this.update(cx, |this, cx| {
                        this.refresh(cx);
                        this.sync_full_waveform_request(cx);
                        if query.stamp(this.state.query_revisions_for(&query)) == stamp
                            && this.full_waveform_request.accepts(
                                generation,
                                this.state
                                    .current_track()
                                    .or(this.metadata.read(cx).track()),
                            )
                            && let Err(error) = response.result
                        {
                            this.error = Some(error.to_string());
                            this.waveform.update(cx, |waveform, cx| {
                                waveform.fail_full_query(error.to_string(), cx);
                            });
                            this.full_waveform_request.invalidate();
                            cx.notify();
                        }
                    });
                },
            )
            .await;
            let _ = this.update(cx, |this, cx| {
                // Core changes also arrive while hidden, without a rendered frame.
                this.refresh(cx);
                this.sync_full_waveform_request(cx);
                this.full_waveform_request.retire(generation);
                if this.full_waveform_request.generation != generation {
                    this.dispatch_full_waveform_query(cx);
                    return;
                }
                if query.stamp(response.revisions) != stamp
                    || query.stamp(this.state.query_revisions_for(&query)) != stamp
                {
                    this.waveform.update(cx, |waveform, cx| {
                        waveform.fail_full_query(
                            "Full waveform metadata changed before decoding".into(),
                            cx,
                        );
                    });
                    this.full_waveform_request.invalidate();
                    return;
                }
                if !this.full_waveform_request.accepts(
                    generation,
                    this.state
                        .current_track()
                        .or(this.metadata.read(cx).track()),
                ) {
                    return;
                }
                match response.result {
                    Ok(rivu_protocol::response::ViewResponse::Track(Some(track)))
                        if this.full_waveform_request.accepts(generation, Some(&track)) =>
                    {
                        this.waveform.update(cx, |waveform, cx| {
                            waveform.load_full(&track, cx);
                        });
                    }
                    Err(error) => {
                        this.error = Some(error.to_string());
                        this.waveform.update(cx, |waveform, cx| {
                            waveform.fail_full_query(error.to_string(), cx);
                        });
                        cx.notify();
                    }
                    _ => this.waveform.update(cx, |waveform, cx| {
                        waveform.fail_full_query(
                            "Full waveform source changed before decoding".into(),
                            cx,
                        );
                    }),
                }
                this.full_waveform_request.invalidate();
            });
        })
        .detach();
    }
    fn drag_position(&mut self, position: Point<Pixels>, alt: bool, cx: &mut Context<Self>) {
        let Some(dragging) = self.workspace.interaction.dragging() else {
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
                let Some(bounds) = self.workspace.interaction.measurements().get(&key).copied()
                else {
                    return;
                };
                bounds
            }
        };
        match dragging {
            Dragging::Split(_) => self.drag_split(position, alt, cx),
            Dragging::Seek(_) => {
                let Some(fraction) = rivu_ui::Slider::value_at(bounds, position) else {
                    return;
                };
                if self
                    .workspace
                    .interaction
                    .update_slider_seek(f64::from(fraction))
                {
                    self.sync_waveform(cx);
                    cx.notify();
                }
            }
            Dragging::Volume(_) => {
                let Some(value) = rivu_ui::Slider::value_at(bounds, position) else {
                    return;
                };
                self.playback
                    .update(cx, |owner, cx| owner.set_volume(value, cx));
            }
        }
    }
    fn finish_drag(&mut self, cx: &mut Context<Self>) {
        match self.workspace.interaction.finish_drag() {
            DragCommit::None => {}
            DragCommit::Seek(seek) => self
                .playback
                .update(cx, |owner, cx| owner.commit_seek(seek, cx)),
            DragCommit::PersistSplit => self.persist_layout(cx),
        }
        self.sync_playback_interaction(cx);
    }
}

#[cfg(test)]
mod state_tests {
    use super::presentation::{retain_analysis_staging, stage_analysis_frame};
    use super::{BrowserFocus, ListFocus, panel_list_focus};
    use parking_lot::RwLock;
    use ringbuf::traits::Producer;
    use rivu_audio::analysis::{
        AnalysisFrame, AnalysisSettings, AnalysisWorker, DisplayDemand, TapFrame, VisualizerDemand,
    };
    use std::{
        sync::Arc,
        thread,
        time::{Duration, Instant},
    };

    #[test]
    fn analysis_staging_releases_hidden_styles_and_waits_for_restored_publication() {
        let shared = Arc::new(RwLock::new(AnalysisFrame::default()));
        let worker = AnalysisWorker::new(shared.clone()).unwrap();
        worker.configure(AnalysisSettings {
            fps: 60,
            fft_size: 512,
            window: crate::config::AnalysisWindow::None,
            signal: crate::config::VisualizationSignal::After,
        });
        worker.set_playback_active(true);
        let all = DisplayDemand {
            enabled: true,
            visualizer: VisualizerDemand {
                stereo_density: true,
                spatial_spectrum: true,
                band_energy: true,
                pcm: true,
            },
        };
        worker.control.set_display_demand(all);
        let mut producer = worker.producer();
        let mut publish = |amplitude: f32| {
            let token = worker.control.display_token();
            let sequence = shared.read().sequence;
            for index in 0..512 {
                let sample = amplitude * (std::f32::consts::TAU * index as f32 / 32.0).sin();
                assert!(
                    producer
                        .try_push(TapFrame {
                            samples: [sample, -sample * 0.5],
                            epoch: worker.control.epoch(),
                            demand_token: token,
                            gap: false,
                        })
                        .is_ok()
                );
            }
            worker.control.request_frame();
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let frame = shared.read();
                if frame.sequence > sequence
                    && frame.display_token == token
                    && frame.settings.is_some()
                {
                    break frame.clone();
                }
                drop(frame);
                assert!(Instant::now() < deadline, "analysis publication timed out");
                thread::sleep(Duration::from_millis(1));
            }
        };
        let publication = publish(0.5);
        assert!(
            publication
                .visualizer
                .density
                .iter()
                .any(|value| *value > 0.0)
        );
        assert!(!publication.visualizer.spatial.is_empty());
        assert_eq!(publication.visualizer.pcm.len(), 512);
        assert!(
            publication
                .visualizer
                .band_rms
                .iter()
                .any(|value| *value > 0.0)
        );

        for survivor in [
            VisualizerDemand {
                stereo_density: true,
                ..Default::default()
            },
            VisualizerDemand {
                spatial_spectrum: true,
                ..Default::default()
            },
            VisualizerDemand {
                band_energy: true,
                ..Default::default()
            },
            VisualizerDemand {
                pcm: true,
                ..Default::default()
            },
        ] {
            let mut staging = AnalysisFrame::default();
            let mut sequence = 0;
            assert!(stage_analysis_frame(
                &mut staging,
                &mut sequence,
                false,
                &publication,
                publication.display_token == worker.control.display_token(),
            ));
            assert_eq!(
                staging.visualizer.density.as_ptr(),
                publication.visualizer.density.as_ptr(),
            );
            let frequencies = staging.frequencies_hz.as_ptr();
            let spectrum = staging.spectrum_db.as_ptr();
            let density = staging.visualizer.density.as_ptr();
            let spatial = staging.visualizer.spatial.as_ptr();
            let pcm = staging.visualizer.pcm.as_ptr();
            retain_analysis_staging(
                &mut staging,
                DisplayDemand {
                    enabled: true,
                    visualizer: survivor,
                },
            );
            assert_eq!(staging.frequencies_hz.as_ptr(), frequencies);
            assert_eq!(staging.spectrum_db.as_ptr(), spectrum);
            assert_eq!(staging.generation, publication.generation);
            assert_eq!(staging.visualizer.demand, survivor);
            if survivor.stereo_density {
                assert_eq!(staging.visualizer.density.as_ptr(), density);
                assert_eq!(
                    staging.visualizer.correlation,
                    publication.visualizer.correlation
                );
            } else {
                assert!(staging.visualizer.density.is_empty());
                assert_eq!(staging.visualizer.correlation, None);
                assert_eq!(staging.visualizer.balance, None);
                assert_eq!(staging.visualizer.side_ratio, None);
            }
            if survivor.spatial_spectrum {
                assert_eq!(staging.visualizer.spatial.as_ptr(), spatial);
            } else {
                assert_eq!(staging.visualizer.spatial.capacity(), 0);
            }
            if survivor.pcm {
                assert_eq!(staging.visualizer.pcm.as_ptr(), pcm);
            } else {
                assert!(staging.visualizer.pcm.is_empty());
            }
            assert_eq!(
                staging.visualizer.band_rms,
                if survivor.band_energy {
                    publication.visualizer.band_rms
                } else {
                    [0.0; 3]
                }
            );

            retain_analysis_staging(&mut staging, DisplayDemand::default());
            assert_eq!(staging.frequencies_hz.capacity(), 0);
            assert_eq!(staging.spectrum_db.capacity(), 0);
            assert!(staging.visualizer.density.is_empty());
            assert!(staging.visualizer.pcm.is_empty());
            assert_eq!(staging.visualizer.spatial.capacity(), 0);
            assert_eq!(staging.settings, None);
            retain_analysis_staging(&mut staging, all);
            assert_eq!(staging.settings, None);
            assert_eq!(staging.visualizer.demand, VisualizerDemand::default());
            assert!(staging.visualizer.density.is_empty());
            assert!(staging.visualizer.pcm.is_empty());
            assert_eq!(staging.visualizer.spatial.capacity(), 0);
        }

        worker.control.set_display_demand(DisplayDemand::default());
        worker.control.set_display_demand(all);
        assert_ne!(publication.display_token, worker.control.display_token());
        let mut staging = AnalysisFrame::default();
        let mut sequence = publication.sequence;
        // Even an explicit refresh cannot restore a retained pre-hide snapshot.
        assert!(!stage_analysis_frame(
            &mut staging,
            &mut sequence,
            true,
            &publication,
            publication.display_token == worker.control.display_token(),
        ));
        assert!(staging.visualizer.density.is_empty());
        assert!(staging.settings.is_none());
        let restored = publish(0.0);
        assert!(restored.generation > publication.generation);
        assert_eq!(restored.visualizer.demand, all.visualizer);
        assert!(
            restored
                .visualizer
                .density
                .iter()
                .all(|value| *value == 0.0)
        );
        assert_eq!(restored.visualizer.band_rms, [0.0; 3]);
        assert_eq!([restored.rms_left, restored.rms_right], [0.0; 2]);
        assert!(stage_analysis_frame(
            &mut staging,
            &mut sequence,
            false,
            &restored,
            restored.display_token == worker.control.display_token(),
        ));
        assert_eq!(
            staging.visualizer.density.as_ptr(),
            restored.visualizer.density.as_ptr(),
        );
        assert_eq!(staging.sample_time, restored.sample_time);
        assert_eq!(staging.generation, restored.generation);
        assert!(
            publication
                .visualizer
                .density
                .iter()
                .any(|value| *value > 0.0)
        );
    }

    #[test]
    fn row_keys_belong_to_the_target_panel_without_fallback() {
        let queue = Some(BrowserFocus::List(ListFocus::Queue));
        for kind in ["albums", "history", "metadata", "transport", "playlists"] {
            assert!(panel_list_focus(kind, queue).is_none());
        }
        assert!(panel_list_focus("queue", queue) == Some(ListFocus::Queue));
        assert!(panel_list_focus("library", queue) == Some(ListFocus::Library));
        assert!(
            panel_list_focus(
                "playlists",
                Some(BrowserFocus::List(ListFocus::PlaylistTracks))
            ) == Some(ListFocus::PlaylistTracks)
        );
    }
}
