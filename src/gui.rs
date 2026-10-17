mod album_artwork;
mod album_browser;
mod artwork;
mod browser;
mod components;
mod duplicates;
mod fonts;
mod layout;
mod library;
mod list_interaction;
mod loading;
mod metadata;
mod paged;
mod panels;
mod playlist_editor;
mod playlist_selection;
mod queries;
mod queue;
mod scrolling;
pub(crate) mod session;
mod settings;
mod shell;
mod shortcuts;
mod theme;
mod visualization;
use crate::{
    config::{AudioVisualizerOptions, AudioVisualizerStyle},
    core::CoreHandle,
    i18n::Localizer,
    model::{Command, PlaybackStatus, Query, Track},
    projection::GuiSnapshot,
};
use anyhow::Result;
use ashpd::desktop::file_chooser::SelectedFiles;
use browser::BrowserState;
use components::input::{Input, InputEvent};
use components::search::{ListSearch, SearchEvent};
use components::{ButtonTooltip, caption, drag_preview, list_row};
use futures::{StreamExt, channel::mpsc};
use gpui::{prelude::*, *};
use layout::interaction::{
    Cancellation, DockGeometry, DockHover, DockRequest, DragCommit, Dragging, LayoutInteraction,
    Measured,
};
use layout::save_owner::LayoutSaveHandle;
use layout::{Axis, Edge, LayoutSet, Node};
use library::LibraryNode;
use list_interaction::{ContentDrag, ContentMenuRequest, TrackSource};
use metadata::session::MetadataSession;
use queries::ViewRequestKind;
pub(crate) use session::UiSession;
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

pub(crate) fn init(cx: &mut App) -> Result<()> {
    #[cfg(feature = "embed-nerd-font")]
    fonts::register_symbols(cx)
        .map_err(|error| error.context("Could not load embedded icon font"))?;
    Input::init(cx);
    Ok(())
}

const UI_INSET: f32 = 8.0;
const SPLIT_GUTTER: f32 = 4.0;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Field {
    PlaylistName,
    LayoutName,
    Title,
    Artist,
    Album,
}

struct PanelSpec {
    kind: &'static str,
    title_id: &'static str,
    render: fn(&mut GuiApp, &layout::Panel, &mut Window, &mut Context<GuiApp>) -> AnyElement,
}
const PANELS: &[PanelSpec] = &[
    PanelSpec {
        kind: "transport",
        title_id: "shell-panel-playback",
        render: |app, panel, window, cx| app.transport_panel(panel.id, window, cx),
    },
    PanelSpec {
        kind: "library",
        title_id: "shell-panel-library",
        render: |app, panel, window, cx| {
            app.library_panel(panel.id, panel.show_tab_bar, window, cx)
        },
    },
    PanelSpec {
        kind: "albums",
        title_id: "shell-panel-albums",
        render: |app, panel, window, cx| app.album_browser_panel(panel.id, window, cx),
    },
    PanelSpec {
        kind: "queue",
        title_id: "shell-panel-queue",
        render: |app, panel, window, cx| app.queue_panel(panel.id, panel.show_tab_bar, window, cx),
    },
    PanelSpec {
        kind: "playlists",
        title_id: "shell-panel-playlists",
        render: |app, panel, window, cx| app.playlists_panel(panel.id, window, cx),
    },
    PanelSpec {
        kind: "history",
        title_id: "shell-panel-history",
        render: |app, panel, window, cx| app.history_panel(panel.id, window, cx),
    },
    PanelSpec {
        kind: "metadata",
        title_id: "shell-panel-metadata",
        render: |app, panel, window, cx| app.metadata_panel(panel.id, window, cx),
    },
    PanelSpec {
        kind: "album_artwork",
        title_id: "shell-panel-album-artwork",
        render: |app, panel, _, cx| {
            app.artwork_panels
                .entry(panel.id)
                .or_insert_with(|| {
                    cx.new(|cx| {
                        album_artwork::AlbumArtworkPanel::new(
                            panel.id,
                            app.album_artwork.clone(),
                            cx,
                        )
                    })
                })
                .clone()
                .into_any_element()
        },
    },
    PanelSpec {
        kind: "spectrum",
        title_id: "shell-panel-spectrum",
        render: |app, panel, window, cx| app.spectrum_panel(panel.id, window, cx),
    },
    PanelSpec {
        kind: "spectrogram",
        title_id: "shell-panel-spectrogram",
        render: |app, panel, window, cx| app.spectrogram_panel(panel.id, window, cx),
    },
    PanelSpec {
        kind: "waveform",
        title_id: "shell-panel-waveform",
        render: |app, panel, window, cx| app.waveform_panel(panel.id, window, cx),
    },
    PanelSpec {
        kind: "audio_visualizer",
        title_id: "shell-panel-audio-visualizer",
        render: GuiApp::audio_visualizer_panel,
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
    panel_id: u64,
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
enum HistorySort {
    Recent,
    Title,
    Plays,
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
}
#[derive(Default)]
struct FullWaveformRequest {
    generation: u64,
    target: Option<FullWaveformTarget>,
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

fn retain_analysis_staging(
    frame: &mut crate::analysis::AnalysisFrame,
    demand: crate::analysis::DisplayDemand,
) {
    if !demand.enabled {
        *frame = crate::analysis::AnalysisFrame::default();
        return;
    }
    // Release optional allocations immediately, without waiting for the worker's
    // next publication or discarding data still used by another visible panel.
    let visualizer = &mut frame.visualizer;
    if !demand.visualizer.stereo_density {
        visualizer.demand.stereo_density = false;
        visualizer.density = crate::analysis::DensitySnapshot::default();
        visualizer.correlation = None;
        visualizer.balance = None;
        visualizer.side_ratio = None;
    }
    if !demand.visualizer.spatial_spectrum {
        visualizer.demand.spatial_spectrum = false;
        visualizer.spatial = Vec::new();
    }
    if !demand.visualizer.band_energy {
        visualizer.demand.band_energy = false;
        visualizer.band_rms = [0.0; 3];
    }
}

fn stage_analysis_frame(
    staging: &mut crate::analysis::AnalysisFrame,
    sequence: &mut u64,
    needs_refresh: bool,
    frame: &crate::analysis::AnalysisFrame,
    frame_is_current: bool,
) -> bool {
    if !frame_is_current
        || (frame.sequence == *sequence && !(needs_refresh && staging.settings.is_some()))
    {
        return false;
    }
    staging.frequencies_hz.clone_from(&frame.frequencies_hz);
    staging.spectrum_db.clone_from(&frame.spectrum_db);
    staging.sequence = frame.sequence;
    staging.display_token = frame.display_token;
    staging.sample_rate = frame.sample_rate;
    staging.sample_time = frame.sample_time;
    staging.generation = frame.generation;
    staging.settings = frame.settings;
    staging.rms_left = frame.rms_left;
    staging.rms_right = frame.rms_right;
    staging.visualizer.clone_from(&frame.visualizer);
    *sequence = frame.sequence;
    true
}

enum LayoutApply {
    Visualizer(u64),
    Artwork(u64),
}

pub(super) struct GuiApp {
    pub(super) i18n: Arc<Localizer>,
    localized_text: HashMap<String, SharedString>,
    handle: CoreHandle,
    state: GuiSnapshot,
    layouts: LayoutSet,
    session: Weak<RefCell<UiSession>>,
    layout_save: LayoutSaveHandle,
    layout_save_seen: u64,
    layout_save_error: Option<String>,
    layout_apply: Option<(u64, LayoutApply)>,
    inputs: HashMap<Field, Entity<Input>>,
    queue_search: Entity<ListSearch>,
    playlist_search: Entity<ListSearch>,
    playlist_entries_search: Entity<ListSearch>,
    history_search: Entity<ListSearch>,
    library_search: Entity<ListSearch>,
    // Last explicitly targeted panel; unsupported panels own (and suppress) row keys too.
    search_panel: Option<u64>,
    browser: BrowserState,
    loading_text: loading::LoadingText<loading::LoadingOwner>,
    metadata: MetadataSession,
    metadata_projection: crate::projection::QueryProjection,
    metadata_load: metadata::MetadataLoadState,
    metadata_needs_refresh: bool,
    manual_intents: metadata::ManualIntents,
    duplicate_review: duplicates::DuplicateReview,
    metadata_decoder_hint: crate::model::DecoderHint,
    layout_focus: FocusHandle,
    keyboard_scope: components::FocusScope,
    keyboard_surface: Option<shell::KeyboardSurface>,
    keyboard_restore: Option<FocusHandle>,
    scroll_views: scrolling::ScrollViews,
    keyboard_help: Option<crate::keyboard::Context>,
    help_scroll: std::rc::Rc<ScrollHandle>,

    interaction: LayoutInteraction,
    error: Option<String>,
    artwork_manager: Arc<crate::artwork::ArtworkManager>,
    artwork_images: artwork::ArtworkImages,
    artwork_loader: artwork::ArtworkLoader,
    artwork_view: artwork::ArtworkView,
    default_album: Entity<artwork::Artwork>,
    album_artwork: Entity<album_artwork::AlbumArtwork>,
    artwork_panels: HashMap<u64, Entity<album_artwork::AlbumArtworkPanel>>,
    album_browsers: HashMap<u64, Entity<album_browser::AlbumBrowser>>,
    playlist_editor: Option<playlist_editor::PlaylistEditor>,
    playlist_action: Option<panels::PlaylistAction>,
    settings: Entity<settings::Settings>,
    visuals: visualization::Visuals,
    audio_visualizer: visualization::AudioVisualizer,
    catalog_open: bool,
    settings_open: bool,
    about_open: bool,
    app_menu_open: bool,
    layouts_open: bool,
    layout_action: Option<layout::layout_manager::Action>,
    panel_menu: Option<PanelMenu>,
    content_menu: Option<list_interaction::ContentMenu>,
    content_generation: u64,
    content_resolution: bool,
    waveform: Entity<visualization::Waveform>,
    full_waveform_request: FullWaveformRequest,
    analysis_worker_enabled: bool,
    analysis_sequence: u64,
    analysis_staging: crate::analysis::AnalysisFrame,
    analysis_needs_refresh: bool,
    window_visible: bool,
    frame_pending: bool,
    frame_timer: Option<Task<()>>,
    last_analysis_request: Option<Instant>,
    ui_font: Font,
    history_column_widths: Option<panels::HistoryColumnWidths>,
    _subscriptions: Vec<Subscription>,
}
impl GuiApp {
    fn panel_demand(&self, kind: &str) -> bool {
        self.window_visible
            && !self.state.system.shutting_down
            && self
                .layouts
                .current()
                .active_panels()
                .iter()
                .any(|panel| panel.kind == kind)
    }
    pub(super) fn tr(&self, id: &str) -> SharedString {
        self.localized_text
            .get(id)
            .expect("cached localized GUI text")
            .clone()
    }

    fn input_focused(&self, window: &Window, cx: &App) -> bool {
        self.inputs
            .values()
            .any(|input| input.read(cx).is_focused(window))
            || [
                &self.queue_search,
                &self.playlist_search,
                &self.playlist_entries_search,
                &self.history_search,
                &self.library_search,
            ]
            .iter()
            .any(|search| search.read(cx).input_focused(window, cx))
            || self
                .album_browsers
                .values()
                .any(|albums| albums.read(cx).input_focused(window, cx))
    }

    fn focus_layout(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.layout_focus.focus(window, cx);
    }

    fn route_search_key(
        &mut self,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let searches = [
            &self.library_search,
            &self.queue_search,
            &self.playlist_search,
            &self.playlist_entries_search,
            &self.history_search,
        ];
        if let Some(search) = searches
            .iter()
            .find(|search| search.read(cx).input_focused(window, cx))
        {
            let search = (*search).clone();
            return search.update(cx, |search, cx| {
                if focus {
                    search.focus(window, cx);
                    true
                } else {
                    search.escape(window, cx)
                }
            });
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
        let panels = self.layouts.current().active_panels();
        if self.input_focused(window, cx) {
            return false;
        }
        let panel = self
            .search_panel
            .and_then(|id| panels.iter().find(|panel| panel.id == id).copied())
            .or_else(|| {
                let kind = match self.browser.focus() {
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
        let search = match panel.kind.as_str() {
            "library" => self.library_search.clone(),
            "queue" => self.queue_search.clone(),
            "history" => self.history_search.clone(),
            "playlists"
                if self.browser.focus() == Some(BrowserFocus::List(ListFocus::PlaylistTracks)) =>
            {
                self.playlist_entries_search.clone()
            }
            "playlists" => self.playlist_search.clone(),
            "albums" => {
                let Some(albums) = self.album_browsers.get(&panel.id).cloned() else {
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
            _ => return false,
        };
        search.update(cx, |search, cx| {
            if focus {
                search.focus(window, cx);
                true
            } else {
                search.escape(window, cx)
            }
        })
    }

    fn dismiss_input_composition(&self, window: &Window, cx: &mut Context<Self>) -> bool {
        self.inputs
            .values()
            .find(|input| input.read(cx).is_focused(window))
            .is_some_and(|input| input.update(cx, |input, cx| input.dismiss_composition(cx)))
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
            self.search_panel = Some(panel_id);
            self.catalog_open = false;
            if focus_search {
                self.library_search
                    .update(cx, |search, cx| search.focus(window, cx));
            }
            self.persist_layout(cx);
        } else if self.layouts.current().locked {
            self.error = Some(self.i18n.text("shell-library-locked").to_owned());
            cx.notify();
        } else {
            let panel_id = self
                .layouts
                .current_mut()
                .add("library", self.interaction.target_group());
            self.layouts.current_mut().activate(panel_id);
            self.layout_changed(cx);
            self.search_panel = Some(panel_id);
            self.browser.focus_list(ListFocus::Library);
            self.catalog_open = false;
            if focus_search {
                self.library_search
                    .update(cx, |search, cx| search.focus(window, cx));
            }
            self.persist_layout(cx);
        }
    }

    fn reveal_queue_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.cancel_interaction(window, cx);
        let panel = self
            .layouts
            .current()
            .panels()
            .into_iter()
            .find(|panel| panel.kind == "queue")
            .map(|panel| panel.id);
        let panel_id = if let Some(panel) = panel {
            self.layouts.current_mut().activate(panel);
            panel
        } else if self.layouts.current().locked {
            self.error = Some(self.i18n.text("gl-locate-queue-locked").to_owned());
            cx.notify();
            return false;
        } else {
            let panel = self
                .layouts
                .current_mut()
                .add("queue", self.interaction.target_group());
            self.layout_changed(cx);
            panel
        };
        self.search_panel = Some(panel_id);
        self.catalog_open = false;
        self.browser.focus_list(ListFocus::Queue);
        self.focus_layout(window, cx);
        self.persist_layout(cx);
        true
    }

    fn active_list_focus(&self) -> Option<ListFocus> {
        let panel = self.keyboard_panel()?;
        panel_list_focus(&panel.kind, self.browser.focus())
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
            ListFocus::Library => self.navigate_library_tree(down, cx),
            ListFocus::Queue => self.navigate_queue(down, cx),
            ListFocus::PlaylistTracks => self.navigate_playlist(down, false, false, cx),
        }
    }

    fn play_focused_selection(&mut self, focus: ListFocus, cx: &mut Context<Self>) {
        match focus {
            ListFocus::Library => {
                if self.library_range_pending() {
                    return;
                }
                self.activate_library_tree(cx);
            }
            ListFocus::Queue => {
                let queue_id = self.browser.queue.active().map(|row| row.id);
                if let Some(queue_id) = queue_id {
                    self.send(Command::PlayQueue { queue_id }, cx);
                }
            }
            ListFocus::PlaylistTracks => {
                if self.browser.playlists.track_selection.ready()
                    && let Some(row) = self.browser.playlists.track_selection.active
                {
                    self.send(
                        Command::Play {
                            track_id: row.track_id,
                        },
                        cx,
                    );
                }
            }
        }
    }

    fn remove_focused_queue(&mut self, focus: ListFocus, cx: &mut Context<Self>) {
        if focus == ListFocus::PlaylistTracks {
            self.remove_playlist_selection(cx);
            return;
        }
        if focus != ListFocus::Queue {
            return;
        }
        let queue_ids = self.selected_queue_ids_in_order();
        if queue_ids.is_empty() {
            return;
        }
        self.browser.queue.clear();

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
        let state = handle.gui_snapshot();
        theme::set_theme(state.system.config.ui_theme, cx);
        let localized_text = i18n
            .fixed_texts()
            .map(|(id, text)| (id.to_owned(), SharedString::from(text.to_owned())))
            .collect();
        let (mut layouts, load_error) = layout_save.snapshot();
        layouts.reset_edit_mode();
        let error = load_error
            .map(|error| i18n.format("shell-load-layouts-error", &[("error", error.into())]));
        if layouts
            .current_mut()
            .initialize_audio_visualizers(AudioVisualizerOptions::from(
                state.system.config.as_ref(),
            ))
            | layouts.current_mut().initialize_album_artworks(
                crate::config::AlbumArtworkOptions::from(state.system.config.as_ref()),
            )
        {
            layout_save.submit(&layouts);
        }
        let ui_font = fonts::resolve(&state.system.config.ui_font, cx);
        let mut inputs = HashMap::new();
        for (field, placeholder) in [
            (Field::PlaylistName, "shell-playlist-name"),
            (Field::LayoutName, "shell-layout-name"),
            (Field::Title, "shell-title"),
            (Field::Artist, "shell-artist"),
            (Field::Album, "shell-album"),
        ] {
            inputs.insert(
                field,
                cx.new(|cx| Input::new("", i18n.text(placeholder).to_owned(), cx)),
            );
        }
        let mut subscriptions = Vec::new();
        let queue_search = cx.new(|cx| ListSearch::new(Arc::clone(&i18n), cx));
        let playlist_search = cx.new(|cx| ListSearch::new(Arc::clone(&i18n), cx));
        let playlist_entries_search = cx.new(|cx| ListSearch::new(Arc::clone(&i18n), cx));
        let history_search = cx.new(|cx| ListSearch::new(Arc::clone(&i18n), cx));
        let library_search = cx.new(|cx| ListSearch::new(Arc::clone(&i18n), cx));
        subscriptions.push(cx.subscribe(&queue_search, |this, _, event, cx| {
            let SearchEvent::Changed(query) = event;
            this.apply_queue_search(query.clone(), cx);
        }));
        subscriptions.push(cx.subscribe(&playlist_search, |this, _, event, cx| {
            let SearchEvent::Changed(query) = event;
            this.apply_playlist_search(query.clone(), cx);
        }));
        subscriptions.push(
            cx.subscribe(&playlist_entries_search, |this, _, event, cx| {
                let SearchEvent::Changed(query) = event;
                this.apply_playlist_entries_search(query.clone(), cx);
            }),
        );
        subscriptions.push(cx.subscribe(&history_search, |this, _, event, cx| {
            let SearchEvent::Changed(query) = event;
            this.begin_manual_intent();
            this.browser.history.set_query(query.clone());
            if !this.metadata_protected(cx) {
                this.clear_metadata_selection(cx);
            }
            this.scroll_views.scroll_list_to("history", 0);
            cx.notify();
        }));
        subscriptions.push(cx.subscribe(&library_search, |this, _, event, cx| {
            let SearchEvent::Changed(query) = event;
            this.apply_library_search(query.clone(), cx);
        }));
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
                    this.settings_open = false;
                    this.focus_layout(window, cx);
                    cx.notify();
                }
                settings::SettingsEvent::ReviewDuplicates => {
                    this.open_duplicate_review(window, cx);
                }
                settings::SettingsEvent::ApplyVisualizer {
                    panel_id,
                    options,
                    session,
                } => {
                    if this
                        .layouts
                        .current_mut()
                        .set_audio_visualizer_options(*panel_id, *options)
                    {
                        let revision = this.queue_layout_save();
                        this.layout_apply = Some((revision, LayoutApply::Visualizer(*session)));
                    } else {
                        settings.update(cx, |settings, cx| {
                            settings.complete_visualizer_apply(
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
                        .layouts
                        .current_mut()
                        .set_album_artwork_options(*panel_id, *options)
                    {
                        let revision = this.queue_layout_save();
                        this.layout_apply = Some((revision, LayoutApply::Artwork(*session)));
                    } else {
                        settings.update(cx, |settings, cx| {
                            settings.complete_artwork_apply(
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
        subscriptions.push(cx.subscribe_in(
            &inputs[&Field::PlaylistName],
            window,
            |this, _, event, window, cx| match event {
                InputEvent::Submit => this.submit_playlist_name(window, cx),
                InputEvent::Changed { .. } => {
                    if let Some(editor) = this.playlist_editor.as_mut() {
                        editor.error = None;
                    }
                    cx.notify();
                }
            },
        ));
        for field in [Field::Title, Field::Artist, Field::Album] {
            subscriptions.push(cx.subscribe(&inputs[&field], move |_, _, event, cx| {
                if matches!(event, InputEvent::Changed { .. }) {
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
                    this.cancel_window_reads();
                }
                this.refresh(cx);
                this.sync_analysis(cx);
                if !this.window_visible {
                    // Hidden windows may never deliver another paint/frame.
                    // Flush both current and already-scheduled retired owners.
                    this.album_artwork.update(cx, |artwork, cx| {
                        artwork.release_all_images(window, cx);
                    });
                    for albums in this.album_browsers.values() {
                        albums.update(cx, |albums, cx| albums.release_all_images(window, cx));
                    }
                    this.artwork_images.release_all_retired(window, cx);
                }
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
                    .update_in(cx, |this, window, cx| {
                        this.refresh(cx);
                        this.request_frame(window, cx);
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
        let artwork_images = artwork::ArtworkImages::default();
        let mut app = Self {
            i18n: Arc::clone(&i18n),
            localized_text,
            settings,
            handle,
            state,
            layouts,
            session,
            layout_save,
            layout_save_seen: 0,
            layout_save_error: error.clone(),
            layout_apply: None,
            inputs,
            queue_search,
            playlist_search,
            playlist_entries_search,
            history_search,
            library_search,
            search_panel: None,
            browser: BrowserState::default(),
            loading_text: loading::LoadingText::default(),
            metadata: MetadataSession::default(),
            metadata_projection: Default::default(),
            metadata_load: metadata::MetadataLoadState::default(),
            metadata_needs_refresh: false,
            manual_intents: metadata::ManualIntents::default(),
            duplicate_review: duplicates::DuplicateReview::default(),
            metadata_decoder_hint: Default::default(),
            layout_focus: cx.focus_handle(),
            keyboard_scope: components::FocusScope::new(cx),
            keyboard_surface: None,
            keyboard_restore: None,
            scroll_views: scrolling::ScrollViews::default(),
            keyboard_help: None,
            help_scroll: std::rc::Rc::new(ScrollHandle::new()),
            interaction: LayoutInteraction::default(),
            error,
            playlist_editor: None,
            playlist_action: None,
            album_artwork: cx.new(|cx| {
                let default_album = cx.new(|_| album_default);
                album_artwork::AlbumArtwork::new(
                    Arc::clone(&artwork_manager),
                    default_album,
                    artwork_images.clone(),
                )
            }),
            artwork_panels: HashMap::new(),
            default_album,
            album_browsers: HashMap::new(),
            artwork_manager,
            artwork_loader: artwork::ArtworkLoader::default(),
            artwork_view: artwork::ArtworkView::new(artwork_images.clone()),
            artwork_images,
            visuals: visualization::Visuals::new(),
            audio_visualizer: visualization::AudioVisualizer::new(),
            catalog_open: false,
            settings_open: false,
            about_open: false,
            app_menu_open: false,
            layouts_open: false,
            layout_action: None,
            panel_menu: None,
            content_menu: None,
            content_generation: 0,
            content_resolution: false,
            waveform,
            full_waveform_request: FullWaveformRequest::default(),
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
            history_column_widths: None,
        };
        app.interaction
            .retain_layout(app.layouts.current().root.as_ref());
        app.sync_metadata_default(cx);
        app.browser
            .history
            .sync(&app.state.library.history, app.history_playback());
        app.sync_layout_save(cx);
        app.sync_analysis(cx);
        // Initialization may have completed before the wakeup callback was
        // installed. Reconcile once so the first frame never keeps the empty snapshot.
        app.refresh(cx);
        app.refresh_queue(cx);
        app.layout_focus.focus(window, cx);
        window
            .observe_release(&cx.entity(), cx, |this: &mut Self, window, cx| {
                this.release_render_images(window, cx);
            })
            .detach();
        app
    }
    fn release_render_images(&self, window: &mut Window, cx: &mut App) {
        self.audio_visualizer.release_all_images(window);
        self.visuals.release_all_images(window);
        self.waveform
            .read(cx)
            .image_releaser()
            .release_all_images(window);
        self.artwork_view.release_all_images(window, cx);
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
        self.begin_playlist_name(
            playlist_editor::PlaylistNameAction::SaveQueue,
            format!("Queue_{}", chrono::Local::now().format("%Y%m%d_%H%M%S")),
            window,
            cx,
        );
    }

    fn choose_playlist_import(&mut self, cx: &mut Context<Self>) {
        let i18n = Arc::clone(&self.i18n);
        cx.spawn(async move |this, cx| {
            let result = SelectedFiles::open_file()
                .title(i18n.text("shell-import-playlist"))
                .accept_label(i18n.text("shell-import"))
                .modal(true)
                .send()
                .await
                .and_then(|request| request.response());
            let _ = this.update(cx, |this, cx| {
                match chooser_result(result) {
                    Ok(Some(files)) => {
                        if let Some(path) = chooser_path(&files) {
                            this.send(Command::ImportPlaylist { path, name: None }, cx);
                        }
                    }
                    Ok(None) => return,
                    Err(error) => {
                        this.error = Some(this.i18n.format(
                            "shell-open-playlist-error",
                            &[("error", error.to_string().into())],
                        ))
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn choose_playlist_export(&mut self, playlist_id: i64, cx: &mut Context<Self>) {
        let i18n = Arc::clone(&self.i18n);
        cx.spawn(async move |this, cx| {
            let result = SelectedFiles::save_file()
                .title(i18n.text("shell-export-playlist"))
                .accept_label(i18n.text("shell-export"))
                .modal(true)
                .send()
                .await
                .and_then(|request| request.response());
            let _ = this.update(cx, |this, cx| {
                match chooser_result(result) {
                    Ok(Some(files)) => {
                        if let Some(path) = chooser_path(&files) {
                            this.send(Command::ExportPlaylist { playlist_id, path }, cx);
                        }
                    }
                    Ok(None) => return,
                    Err(error) => {
                        this.error = Some(this.i18n.format(
                            "shell-open-playlist-destination-error",
                            &[("error", error.to_string().into())],
                        ))
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn choose_library_root(&mut self, cx: &mut Context<Self>) {
        let i18n = Arc::clone(&self.i18n);
        cx.spawn(async move |this, cx| {
            let result = SelectedFiles::open_file()
                .title(i18n.text("shell-add-library-folder"))
                .accept_label(i18n.text("shell-add"))
                .directory(true)
                .modal(true)
                .send()
                .await
                .and_then(|request| request.response());
            let _ = this.update(cx, |this, cx| {
                match chooser_result(result) {
                    Ok(Some(files)) => {
                        if let Some(path) = chooser_path(&files)
                            && let Err(error) = this.handle.update_gui_config(
                                crate::core::GuiConfigChange::AddLibraryRoot(path),
                            )
                        {
                            this.error = Some(format!("{error:#}"));
                        }
                    }
                    Ok(None) => return,
                    Err(error) => {
                        this.error = Some(this.i18n.format(
                            "shell-open-library-error",
                            &[("error", error.to_string().into())],
                        ))
                    }
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
        let theme = crate::gui::theme::palette(cx);
        let error = match kind {
            ViewRequestKind::Summaries => self.browser.playlists.summaries().rows.error(index),
            ViewRequestKind::Entries => self.browser.playlists.entries().rows.error(index),
            ViewRequestKind::Queue => self.browser.queue.rows.error(index),
            _ => None,
        };
        list_row(theme, ("loading-row", index), false)
            .child(
                caption(
                    theme,
                    if error.is_some() {
                        self.tr("shell-loading-failed")
                    } else {
                        self.tr("shell-loading")
                    },
                )
                .when(
                    !self
                        .loading_text
                        .show(&loading::LoadingOwner::View(kind), error.is_some()),
                    |text| text.invisible(),
                ),
            )
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

    pub(super) fn select_history_track(
        &mut self,
        id: i64,
        shift: bool,
        multi: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(commit) = self.browser.history.prepare_selection(id, shift, multi) else {
            return;
        };
        let intent = self.begin_manual_intent();
        self.ready_manual_action(
            intent,
            metadata::ManualAction::History {
                commit,
                structure_revision: self.state.library.structure_revision,
            },
            cx,
        );
    }

    fn commit_history_selection(&mut self, commit: browser::HistoryCommit, cx: &mut Context<Self>) {
        let id = commit.track_id;
        if !self.browser.history.commit_selection(commit) {
            return;
        }
        self.request_track(id, cx);
        self.sync_waveform(cx);
        cx.notify();
    }
    fn apply_interaction_cancellation(
        &mut self,
        cancellation: Cancellation,
        cx: &mut Context<Self>,
    ) {
        if cancellation.split.is_some() {
            cancellation.restore_split(self.layouts.current_mut());
        }
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
        self.content_menu = None;
        self.content_generation = self.content_generation.wrapping_add(1);
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
        self.content_menu = None;
        self.content_generation = self.content_generation.wrapping_add(1);
        self.browser.clear_focus();
    }

    fn history_playback(&self) -> Option<(i64, i64)> {
        if self.state.playback.status == PlaybackStatus::Stopped {
            return None;
        }
        Some((
            self.state.current_track()?.id,
            self.state.playback.last_heard_at?,
        ))
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let state = (self.handle.revision() != self.state.system.revision)
            .then(|| self.handle.gui_snapshot());
        let changed = state
            .as_ref()
            .is_some_and(|state| state.revisions != self.state.revisions);
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
            let playlist_refresh = self
                .browser
                .playlists
                .refresh_changes(&self.state.library, &state.library);
            let roots_changed =
                self.state.system.config.library_roots != state.system.config.library_roots;
            let page_size_changed =
                self.state.system.config.page_size != state.system.config.page_size;
            let waveform_seek_disabled = self.state.system.config.waveform_seek_enabled
                && !state.system.config.waveform_seek_enabled;
            let still_mode_changed =
                self.state.system.config.still_mode != state.system.config.still_mode;
            if self.state.system.config.ui_font != state.system.config.ui_font {
                self.ui_font = fonts::resolve(&state.system.config.ui_font, cx);
            }
            if self.state.system.config.ui_theme != state.system.config.ui_theme {
                theme::set_theme(state.system.config.ui_theme, cx);
            }
            self.state = state;
            self.sync_duplicate_review(cx);
            if self
                .browser
                .library
                .sync_projection(self.state.query_revisions(None))
            {
                self.browser.requests.cancel_directory_demand();
            }
            self.browser.library.sync_root_counts(
                &self.state.system.config.library_roots,
                self.state.library.structure_revision,
            );
            self.sync_queue(cx);
            self.sync_metadata_query();
            self.browser
                .history
                .sync(&self.state.library.history, self.history_playback());
            self.sync_metadata_default(cx);
            self.sync_full_waveform_request();
            self.browser.playlists.reconcile_selection();
            if self.browser.playlists.selected().is_some_and(|id| {
                !self
                    .state
                    .library
                    .playlist_entry_revisions
                    .contains_key(&id)
            }) {
                self.browser.remove_playlist_selection();
            }
            self.sync_entry_projection(cx);
            if playlist_refresh.summaries && self.panel_demand("playlists") {
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
            }
            if page_size_changed {
                self.refresh_page_size();
            }
            if playlist_refresh.summaries {
                self.browser.playlists.invalidate_summaries();
                self.browser
                    .requests
                    .cancel(queries::ViewRequestKind::Summaries);
            }
            if playlist_refresh.entries {
                if let Some(id) = self.browser.playlists.entries().playlist_id {
                    self.browser.playlists.prepare_entries(id);
                }
                self.browser
                    .requests
                    .cancel(queries::ViewRequestKind::Entries);
            }
        }
        self.sync_album_artwork(cx);
        self.sync_loading_text(cx);
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
            if !stage_analysis_frame(
                &mut self.analysis_staging,
                &mut self.analysis_sequence,
                self.analysis_needs_refresh,
                &frame,
                self.handle.analysis_frame_is_current(&frame),
            ) {
                return;
            }
        }
        self.analysis_needs_refresh = false;
        self.visuals.update(&self.analysis_staging);
        self.audio_visualizer.update(&self.analysis_staging);
        cx.notify();
    }
    fn album_browser_panel(
        &mut self,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let page_size = self.view_page_size();
        let albums = self.album_browsers.entry(id).or_insert_with(|| {
            let albums = cx.new(|cx| {
                album_browser::AlbumBrowser::new(
                    Arc::clone(&self.i18n),
                    self.handle.clone(),
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
        let panels = self.layouts.current().panels();
        let active = self.layouts.current().active_panels();
        let visible = self.window_visible && !self.state.system.shutting_down;
        let revisions = self.state.query_revisions(None);
        let page_size = self.view_page_size();
        let current_track = self.state.current_track().map(|track| track.id);
        let playback_status = self.state.playback.status;
        self.album_browsers.retain(|id, albums| {
            let exists = panels
                .iter()
                .any(|panel| panel.id == *id && panel.kind == "albums");
            let shown = visible && active.iter().any(|panel| panel.id == *id);
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
        self.artwork_panels
            .retain(|id, _| self.layouts.current().album_artwork_options(*id).is_some());
        self.album_artwork.update(cx, |artwork, cx| {
            artwork.sync(
                self.state.current_track(),
                self.state.playback.status,
                &self.state.system.config,
                self.layouts.current().root.as_ref(),
                self.window_visible && !self.state.system.shutting_down,
                cx,
            );
        });
    }

    fn sync_analysis(&mut self, cx: &mut Context<Self>) {
        self.sync_album_artwork(cx);
        let enabled = !self.state.system.config.still_mode;
        self.visuals.set_enabled(enabled);
        let active_panels = if enabled {
            self.layouts.current().active_panels()
        } else {
            Vec::new()
        };
        let spectrogram_panel_ids = active_panels
            .iter()
            .filter(|panel| self.window_visible && panel.kind == "spectrogram")
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
        } else {
            // Reopening must wait for a current display-demand publication, not
            // refresh a retained pre-hide frame with the same sequence.
            self.analysis_needs_refresh = false;
        }
        self.analysis_worker_enabled = analysis_visible;
        let demand = crate::analysis::DisplayDemand {
            enabled: analysis_visible,
            visualizer: demand,
        };
        retain_analysis_staging(&mut self.analysis_staging, demand);
        self.handle.set_display_demand(demand);
        self.sync_waveform(cx);
    }

    fn sync_full_waveform_request(&mut self) {
        self.full_waveform_request.retain_target(
            self.state.current_track().or(self.metadata.track()),
            self.state.system.config.still_mode,
        );
    }

    fn sync_waveform(&mut self, cx: &mut Context<Self>) {
        self.sync_full_waveform_request();
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
        self.refresh(cx);
        if self.state.system.config.still_mode {
            return;
        }
        let Some(track) = self.state.current_track().or(self.metadata.track()) else {
            return;
        };
        let track_id = track.id;
        let generation = self.full_waveform_request.begin(track);
        self.waveform.update(cx, |waveform, cx| {
            waveform.cancel_full_request(cx);
        });
        let handle = self.handle.clone();
        let query = Query::Track { track_id };
        let stamp = query.stamp(self.state.query_revisions_for(&query));
        cx.spawn(async move |this, cx| {
            let response = cx
                .background_executor()
                .spawn(async move { handle.query(Query::Track { track_id }) })
                .await;
            let _ = this.update(cx, |this, cx| {
                // Core changes also arrive while hidden, without a rendered frame.
                this.refresh(cx);
                this.sync_full_waveform_request();
                if query.stamp(response.revisions) != stamp
                    || query.stamp(this.state.query_revisions_for(&query)) != stamp
                {
                    return;
                }
                if !this.full_waveform_request.accepts(
                    generation,
                    this.state.current_track().or(this.metadata.track()),
                ) {
                    return;
                }
                match response.result {
                    Ok(crate::response::ViewResponse::Track(Some(track)))
                        if this.full_waveform_request.accepts(generation, Some(&track)) =>
                    {
                        this.waveform.update(cx, |waveform, cx| {
                            waveform.load_full(Some(&track), &this.state.system.config, cx);
                        });
                    }
                    Err(error) => {
                        this.error = Some(error.to_string());
                        cx.notify();
                    }
                    _ => {}
                }
                this.full_waveform_request.invalidate();
            });
        })
        .detach();
    }
    fn spectrum_panel(&mut self, id: u64, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = crate::gui::theme::palette(cx);
        self.visuals.spectrum(theme, &self.i18n, id)
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
            &self.i18n,
            panel.id,
            self.state.playback.status == PlaybackStatus::Playing,
            options,
        )
    }
    fn spectrogram_panel(&mut self, id: u64, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = crate::gui::theme::palette(cx);
        self.visuals.spectrogram(theme, &self.i18n, id)
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
            waveform.view(&self.i18n, id, position, duration, &config, cx)
        })
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
            Dragging::Split(_) => self.drag_split(position, alt, cx),
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
        let theme = crate::gui::theme::palette(cx);
        let value = value.clamp(0., 1.);
        let is_dragging = match (self.interaction.dragging(), dragging) {
            (Some(Dragging::Seek(active)), Dragging::Seek(id))
            | (Some(Dragging::Volume(active)), Dragging::Volume(id)) => active == id,
            _ => false,
        };
        div()
            .id(id)
            .group("playback-slider")
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
                    .bg(rgb(theme.border))
                    .child(
                        div()
                            .h_full()
                            .w(relative(value))
                            .rounded_full()
                            .bg(rgb(theme.accent)),
                    ),
            )
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .mx(px(5.))
                    .opacity(0.)
                    .group_hover("playback-slider", |style| style.opacity(1.))
                    .when(is_dragging, |view| view.opacity(1.))
                    .child(
                        div()
                            .absolute()
                            .top(px(5.))
                            .left(relative(value))
                            .ml(px(-5.))
                            .size(px(10.))
                            .rounded_full()
                            .bg(rgb(theme.text)),
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
            .when(is_dragging, |view| {
                // Volume release has no command; repaint after the root clears the drag.
                view.on_mouse_up(MouseButton::Left, cx.listener(|_, _, _, cx| cx.notify()))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(|_, _, _, cx| cx.notify()))
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod state_tests {
    use super::{
        BrowserFocus, ListFocus, panel_list_focus, retain_analysis_staging, stage_analysis_frame,
    };
    use crate::analysis::{
        AnalysisFrame, AnalysisSettings, AnalysisWorker, DisplayDemand, TapFrame, VisualizerDemand,
    };
    use parking_lot::RwLock;
    use ringbuf::traits::Producer;
    use std::{
        sync::{Arc, atomic::Ordering},
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
        });
        worker.set_playback_active(true);
        let all = DisplayDemand {
            enabled: true,
            visualizer: VisualizerDemand {
                stereo_density: true,
                spatial_spectrum: true,
                band_energy: true,
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
                            epoch: worker.control.epoch.load(Ordering::Acquire),
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
            assert_eq!(staging.visualizer.spatial.capacity(), 0);
            assert_eq!(staging.settings, None);
            retain_analysis_staging(&mut staging, all);
            assert_eq!(staging.settings, None);
            assert_eq!(staging.visualizer.demand, VisualizerDemand::default());
            assert!(staging.visualizer.density.is_empty());
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
