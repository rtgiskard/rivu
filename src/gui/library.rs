//! Window-local Library owner: inputs, publication, demand and resource lifetimes.
mod locate;
mod model;
mod queries;
mod requests;
pub(in crate::gui) mod selection;
pub(super) mod tree;
mod view;

use crate::{
    gui::{
        components::search::{SearchScope, new_search},
        coordination::manual_actions::ManualIntentId,
        feature::FeatureContext,
    },
    projection::GuiSnapshot,
};
use gpui::{App, Context, SharedString, Window, prelude::*};
use rivu_protocol::model::{Command, DirectoryRow};
use rivu_ui::search::SearchEvent;
use std::path::Path;
use std::{cell::RefCell, rc::Rc, sync::Arc, time::Instant};

use tree::TreeItem;
pub(super) use tree::{BranchStatus, DirectoryTree, TreeBranch};

pub(super) use locate::LibraryLocateState;
use model::LibraryBrowser;
use selection::{LibraryCommit, LibraryRowRequest};

pub(in crate::gui) enum LibraryEvent {
    Select(LibraryRowRequest),
    Navigate(usize),
    Ready(
        super::coordination::manual_actions::ManualIntentId,
        std::cell::RefCell<Option<LibraryCommit>>,
    ),
    Failed(super::coordination::manual_actions::ManualIntentId),
    SelectionChanged(Option<i64>),
    SearchChanged,
    Focus,
    Reveal,
    ContentMenu(std::cell::RefCell<Option<LibraryMenuRequest>>),
    Error(String),
}

pub(in crate::gui) struct LibraryMenuRequest {
    pub(in crate::gui) request: super::ContentMenuRequest,
    pub(in crate::gui) guard: super::list_interaction::ContentGuard,
    pub(in crate::gui) remove: Option<Command>,
    pub(in crate::gui) nodes: Option<Vec<LibraryNode>>,
    pub(in crate::gui) root: Option<std::path::PathBuf>,
}

pub(in crate::gui) struct Library {
    context: FeatureContext,
    state: std::rc::Rc<crate::projection::GuiSnapshot>,
    i18n: Arc<crate::i18n::Localizer>,
    library: LibraryBrowser,
    library_search: gpui::Entity<rivu_ui::search::SearchField>,
    requests: requests::LibraryRequests,
    loading_text: rivu_ui::loading::LoadingText<TreeBranch>,
    scan_error: Option<String>,
    visible: bool,
    available: bool,
    focused: bool,
    selection_intent: Option<super::coordination::manual_actions::ManualIntentId>,
    _search_subscription: gpui::Subscription,
}

impl gpui::EventEmitter<LibraryEvent> for Library {}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum LibraryNode {
    OtherTracks,
    Directory(Arc<Path>),
    Track(i64),
}

// Position ticks and unrelated feature publications do not change this surface.
fn display_changed(previous: &GuiSnapshot, next: &GuiSnapshot) -> bool {
    let previous_config = &previous.system.config;
    let next_config = &next.system.config;
    previous.library.revision != next.library.revision
        || previous.library.structure_revision != next.library.structure_revision
        || previous_config.library_roots != next_config.library_roots
        || previous_config.page_size != next_config.page_size
        || previous_config.library_columns != next_config.library_columns
        || previous_config.ui_theme != next_config.ui_theme
        || previous_config.ui_font != next_config.ui_font
        || previous_config.ui_scale != next_config.ui_scale
        || previous_config.ui_font_size != next_config.ui_font_size
        || previous.current_track().map(|track| track.id)
            != next.current_track().map(|track| track.id)
        || previous.playback.status != next.playback.status
        || previous.system.scanning != next.system.scanning
        || previous.system.scan_task_id != next.system.scan_task_id
        || previous.system.scan_owner != next.system.scan_owner
        || previous.system.scan_message != next.system.scan_message
        || previous.system.scan_progress != next.system.scan_progress
        || previous.system.shutting_down != next.system.shutting_down
}

impl Library {
    pub(in crate::gui) fn new(context: FeatureContext, cx: &mut Context<Self>) -> Self {
        let state = context.snapshot();
        let i18n = Arc::clone(&context.i18n);
        let library_search = cx.new(|cx| new_search(&i18n, SearchScope::Library, cx));
        let subscription = cx.subscribe(&library_search, |this, _, event, cx| {
            let SearchEvent::Changed(query) = event;
            this.apply_library_search(query.clone(), cx);
        });
        let mut library = LibraryBrowser::default();
        library.refresh_tree(state.system.config.page_size as usize);
        library.sync_projection(state.query_revisions(None));
        library.sync_root_counts(
            &state.system.config.library_roots,
            state.library.structure_revision,
        );
        Self {
            context,
            state,
            i18n,
            library,
            library_search,
            requests: Default::default(),
            loading_text: rivu_ui::loading::LoadingText::new(std::time::Duration::from_millis(150)),
            scan_error: None,
            visible: false,
            available: false,
            focused: false,
            selection_intent: None,
            _search_subscription: subscription,
        }
    }

    pub(in crate::gui) fn reconcile(&mut self, state: Rc<GuiSnapshot>, cx: &mut Context<Self>) {
        let display_changed = display_changed(&self.state, &state);
        let roots_changed =
            self.state.system.config.library_roots != state.system.config.library_roots;
        if roots_changed {
            self.library.roots_changed();
        }
        self.library
            .refresh_tree(state.system.config.page_size as usize);
        let projection_changed = self.library.sync_projection(state.query_revisions(None));
        if projection_changed {
            self.requests.cancel_directory_demand();
        }
        if self.library.range_selection.target.is_none()
            && let Some(intent) = self.selection_intent
        {
            self.fail_selection(intent, cx);
        }
        self.library.sync_root_counts(
            &state.system.config.library_roots,
            state.library.structure_revision,
        );
        self.state = state;
        if !self.demanded() {
            if let Some(intent) = self.library.navigation_intent {
                self.fail_selection(intent, cx);
            }
            self.cancel_reads();
        }
        self.sync_loading_text(cx);
        if display_changed || projection_changed {
            cx.notify();
        }
    }

    pub(super) fn refresh(&mut self, cx: &mut Context<Self>) {
        self.reconcile(self.context.snapshot(), cx);
    }

    pub(in crate::gui) fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if !self.demanded() {
            if let Some(intent) = self.library.navigation_intent {
                self.fail_selection(intent, cx);
            }
            self.cancel_reads();
        }
        self.sync_loading_text(cx);
        cx.notify();
    }

    pub(in crate::gui) fn set_available(&mut self, available: bool) {
        self.available = available;
    }

    pub(in crate::gui) fn set_focused(&mut self, focused: bool, cx: &mut Context<Self>) {
        if self.focused != focused {
            self.focused = focused;
            cx.notify();
        }
    }

    pub(super) fn demanded(&self) -> bool {
        self.visible && !self.state.system.shutting_down
    }

    pub(in crate::gui) fn cancel_reads(&mut self) {
        self.requests.cancel_directory_demand();
        if self.requests.reads_pending() || self.library.search_counts_pending() {
            self.library.cancel_pending_reads();
        }
    }

    pub(in crate::gui) fn cancel_selection(&mut self, cx: &mut Context<Self>) {
        let pending = self.selection_intent.take().is_some();
        self.library.cancel_manual_selection();
        if pending {
            cx.notify();
        }
    }

    pub(in crate::gui) fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.cancel_selection(cx);
        self.library.clear_selection();
        cx.notify();
    }

    pub(super) fn manual_intent_current(&self, intent: ManualIntentId) -> bool {
        self.selection_intent == Some(intent)
    }

    pub(super) fn ready_selection(
        &mut self,
        intent: ManualIntentId,
        commit: LibraryCommit,
        cx: &mut Context<Self>,
    ) {
        if self.manual_intent_current(intent) {
            cx.emit(LibraryEvent::Ready(intent, RefCell::new(Some(commit))));
        }
    }

    pub(super) fn fail_selection(&mut self, intent: ManualIntentId, cx: &mut Context<Self>) {
        if self.manual_intent_current(intent) {
            self.cancel_selection(cx);
            cx.emit(LibraryEvent::Failed(intent));
        }
    }

    pub(super) fn report_error(&self, error: String, cx: &mut Context<Self>) {
        cx.emit(LibraryEvent::Error(error));
    }

    pub(super) fn send(&mut self, command: Command, cx: &mut Context<Self>) {
        if matches!(
            command,
            Command::Scan { .. } | Command::CheckOtherTracks { .. }
        ) {
            self.scan_error = None;
            let handle = self.context.handle().clone();
            cx.spawn(async move |this, cx| {
                let ack = handle.request_ack(command).await;
                let _ = this.update(cx, |this, cx| {
                    if !ack.is_success() {
                        this.scan_error = Some(
                            ack.error
                                .unwrap_or_else(|| this.i18n.text("gl-scan-failed").to_owned()),
                        );
                    }
                    this.refresh(cx);
                    cx.notify();
                });
            })
            .detach();
            cx.notify();
            return;
        }
        if let Err(error) = self.context.handle().send(command) {
            self.report_error(format!("{error:#}"), cx);
        }
        cx.notify();
    }

    pub(super) fn tr(&self, id: &str) -> SharedString {
        self.context.tr(id)
    }

    pub(in crate::gui) fn input_focused(&self, window: &Window, cx: &App) -> bool {
        self.library_search.read(cx).input_focused(window, cx)
    }

    pub(in crate::gui) fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.library_search
            .update(cx, |search, cx| search.focus(window, cx));
    }

    pub(in crate::gui) fn escape_search(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.library_search
            .update(cx, |search, cx| search.escape(window, cx))
    }

    pub(super) fn directory_row_selected(&self, row: &DirectoryRow) -> bool {
        match row {
            DirectoryRow::OtherTracks => self.library.selected_nodes().contains(&LibraryNode::OtherTracks),
            DirectoryRow::Directory { path } => self.library.selected_nodes().iter().any(|node| {
                matches!(node, LibraryNode::Directory(selected) if selected.as_ref() == path.as_path())
            }),
            DirectoryRow::Track(row) => self.library.selected().contains(&row.id),
        }
    }

    pub(super) fn open_object_menu(
        &self,
        request: crate::gui::ContentMenuRequest,
        guard: crate::gui::list_interaction::ContentGuard,
        remove: Option<Command>,
        nodes: Option<Vec<LibraryNode>>,
        root: Option<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) {
        cx.emit(LibraryEvent::ContentMenu(RefCell::new(Some(
            LibraryMenuRequest {
                request,
                guard,
                remove,
                nodes,
                root,
            },
        ))));
    }

    pub(super) fn start_loading_text(
        &mut self,
        branch: TreeBranch,
        token: u64,
        cx: &mut Context<Self>,
    ) {
        self.loading_text.begin(branch, token, Instant::now());
        self.loading_text
            .schedule(cx, |this| &mut this.loading_text);
    }

    pub(super) fn finish_loading_text(
        &mut self,
        branch: &TreeBranch,
        token: u64,
        cx: &mut Context<Self>,
    ) {
        self.loading_text.finish(branch, token);
        self.loading_text
            .schedule(cx, |this| &mut this.loading_text);
    }

    pub(super) fn sync_loading_text(&mut self, cx: &mut Context<Self>) {
        let visible = self.demanded();
        let tree = self.library.tree();
        self.loading_text
            .retain(|branch, token| visible && tree.request_pending(branch, token));
        self.loading_text
            .schedule(cx, |this| &mut this.loading_text);
    }

    pub(super) fn track_favorite_toggle(
        &self,
        id: impl Into<gpui::ElementId>,
        track_id: i64,
        favorite: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        crate::gui::components::track::track_favorite_toggle(
            id,
            favorite,
            rivu_ui::IconButtonDensity::Compact,
            |key| self.tr(key),
            cx,
            move |this, _, cx| {
                this.send(
                    Command::SetFavorite {
                        track_ids: vec![track_id],
                        favorite: !favorite,
                    },
                    cx,
                )
            },
        )
    }

    pub(super) fn choose_library_root(&mut self, cx: &mut Context<Self>) {
        let i18n = Arc::clone(&self.i18n);
        cx.spawn(async move |this, cx| {
            let result = ashpd::desktop::file_chooser::SelectedFiles::open_file()
                .title(i18n.text("shell-add-library-folder"))
                .accept_label(i18n.text("shell-add"))
                .directory(true)
                .modal(true)
                .send()
                .await
                .and_then(|request| request.response());
            let _ = this.update(cx, |this, cx| {
                match crate::gui::chooser_result(result) {
                    Ok(Some(files)) => {
                        if let Some(path) = crate::gui::chooser_path(&files)
                            && let Err(error) = this.context.handle().update_gui_config(
                                crate::core::GuiConfigChange::AddLibraryRoot(path),
                            )
                        {
                            this.report_error(format!("{error:#}"), cx);
                        }
                    }
                    Ok(None) => return,
                    Err(error) => this.report_error(
                        this.i18n.format(
                            "shell-open-library-error",
                            &[("error", error.to_string().into())],
                        ),
                        cx,
                    ),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl Library {
    pub(super) fn library_drag_track_ids(&self, nodes: &[LibraryNode]) -> Vec<i64> {
        let mut ids = nodes
            .iter()
            .filter_map(|node| match node {
                LibraryNode::Track(id) => Some(*id),
                LibraryNode::Directory(_) | LibraryNode::OtherTracks => None,
            })
            .collect::<Vec<_>>();
        ids.sort_unstable_by_key(|id| {
            (
                self.library
                    .tree()
                    .index_of(&LibraryNode::Track(*id))
                    .unwrap_or(usize::MAX),
                *id,
            )
        });
        ids.dedup();
        ids
    }

    pub(super) fn resolve_library_drag(
        &mut self,
        nodes: &[LibraryNode],
        destination: super::DragDestination,
        cx: &mut Context<Self>,
    ) {
        if self.library_range_pending() || nodes.is_empty() || !destination.current(&self.state) {
            return;
        }
        let mut directories = nodes
            .iter()
            .filter_map(|node| match node {
                LibraryNode::Directory(path) => Some(path.to_path_buf()),
                LibraryNode::Track(_) | LibraryNode::OtherTracks => None,
            })
            .collect::<Vec<_>>();
        directories.sort();
        directories.dedup();
        let track_ids = self.library_drag_track_ids(nodes);
        let other_tracks = nodes.contains(&LibraryNode::OtherTracks);
        match destination {
            super::DragDestination::Queue => self.send(
                Command::EnqueueSources {
                    other_tracks,
                    directories,
                    track_ids,
                },
                cx,
            ),
            super::DragDestination::Playlist(playlist_id) => self.send(
                Command::AddPlaylistSources {
                    other_tracks,
                    playlist_id,
                    directories,
                    track_ids,
                },
                cx,
            ),
            super::DragDestination::QueueAt { index, revision } => self.send(
                Command::InsertQueueSources {
                    other_tracks,
                    directories,
                    track_ids,
                    index,
                    expected_revision: revision,
                },
                cx,
            ),
            super::DragDestination::PlaylistAt {
                playlist_id,
                index,
                revision,
            } => self.send(
                Command::InsertPlaylistSources {
                    other_tracks,
                    playlist_id,
                    directories,
                    track_ids,
                    index,
                    expected_revision: revision,
                },
                cx,
            ),
        }
    }

    fn selected_tree_index(&self) -> Option<usize> {
        self.library.tree().selected_index()
    }

    fn select_tree_row(&mut self, index: usize, cx: &mut Context<Self>) {
        cx.emit(LibraryEvent::Navigate(index));
    }

    pub(in crate::gui) fn prepare_tree_row(
        &mut self,
        id: super::coordination::manual_actions::ManualIntentId,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        self.selection_intent = Some(id);
        let Some(node) = self.library.navigate_tree_row(index) else {
            self.library.defer_manual_navigation(id);
            for (branch, offset) in self.library.demand_directories(index..index + 1) {
                self.request_directory_page(branch, offset, cx);
            }
            cx.notify();
            return;
        };
        self.prepare_library_row(
            id,
            LibraryRowRequest {
                node,
                index,
                shift: false,
                multi: false,
                scroll: true,
            },
            cx,
        );
    }

    fn resume_library_navigation(&mut self, cx: &mut Context<Self>) {
        if let Some((id, index)) = self.library.take_ready_manual_navigation() {
            if self.manual_intent_current(id) {
                self.prepare_tree_row(id, index, cx);
            }
        } else if let Some(id) = self.library.navigation_intent
            && let Some(index) = self.library.range_selection.target
            && matches!(
                self.library.tree().item(index),
                None | Some(TreeItem::Placeholder {
                    status: tree::BranchStatus::Error | tree::BranchStatus::Empty,
                    ..
                })
            )
        {
            self.fail_selection(id, cx);
        }
    }

    fn set_directory_expanded(
        &mut self,
        branch: &TreeBranch,
        expanded: bool,
        cx: &mut Context<Self>,
    ) {
        if self.library.expand_directory(branch, expanded) {
            self.request_directory_page(branch.clone(), 0, cx);
        }
        self.prune_directory_demand();
        self.sync_loading_text(cx);
        cx.notify();
    }

    pub(super) fn navigate_library_tree(&mut self, down: bool, cx: &mut Context<Self>) {
        let count = self.library.tree().row_count();
        if count == 0 {
            return;
        }
        let index = match self.library.manual_navigation_index() {
            Some(index) if down => (index + 1).min(count - 1),
            Some(index) => index.saturating_sub(1),
            None if down => 0,
            None => count - 1,
        };
        self.select_tree_row(index, cx);
    }

    pub(super) fn jump_library_edge(&mut self, last: bool, cx: &mut Context<Self>) {
        let count = self.library.tree().row_count();
        if count == 0 {
            return;
        }
        self.select_tree_row(if last { count - 1 } else { 0 }, cx);
    }

    pub(super) fn activate_library_tree(&mut self, cx: &mut Context<Self>) {
        if self.library_range_pending() {
            return;
        }
        let Some(index) = self.selected_tree_index() else {
            return;
        };
        let Some(TreeItem::Entry(visible)) = self.library.tree().item(index) else {
            return;
        };
        match visible.row {
            DirectoryRow::Directory { .. } | DirectoryRow::OtherTracks => {
                let branch = visible.branch.expect("directory has an occurrence");
                self.set_directory_expanded(&branch, !self.library.tree().expanded(&branch), cx);
            }
            DirectoryRow::Track(track) => self.send(Command::Play { track_id: track.id }, cx),
        }
    }

    pub(super) fn library_tree_key(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(index) = self.selected_tree_index() else {
            return;
        };
        let Some(TreeItem::Entry(visible)) = self.library.tree().item(index) else {
            return;
        };
        let depth = visible.depth;
        match key {
            "left" => {
                if visible.expanded
                    && let Some(branch) = &visible.branch
                {
                    self.set_directory_expanded(branch, false, cx);
                } else if let Some(parent) = self.library.tree().parent_index(index) {
                    self.select_tree_row(parent, cx);
                }
            }
            "right" => {
                if let Some(branch) = &visible.branch {
                    if !visible.expanded {
                        self.set_directory_expanded(branch, true, cx);
                    } else if self
                        .library
                        .tree()
                        .item(index + 1)
                        .is_some_and(|next| match next {
                            TreeItem::Entry(next) => next.depth > depth,
                            TreeItem::Placeholder {
                                depth: next_depth, ..
                            } => next_depth > depth,
                        })
                    {
                        self.select_tree_row(index + 1, cx);
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod library_root_tests {
    use super::{FeatureContext, Library};
    use crate::config::ConfigPersistence;
    use crate::{
        config::Config,
        core::{CoreHandle, CoreRuntime, GuiConfigChange},
        gui::library::tree::TreeItem,
        i18n::Localizer,
    };
    use gpui::{Entity, IntoElement, VisualTestContext, point, px, size};
    use rivu_protocol::model::{DirectoryRow, Query};
    use std::{path::PathBuf, sync::Arc};

    fn settle_config(handle: &CoreHandle) {
        // Core dispatch is FIFO: this response follows config publication.
        assert!(
            handle
                .query(Query::LibraryRootCounts { roots: Vec::new() })
                .result
                .is_ok()
        );
    }

    async fn draw_library(
        window: &mut VisualTestContext,
        app: &Entity<Library>,
        roots: &[PathBuf],
    ) {
        app.update(&mut window.cx, |this, cx| this.refresh(cx));
        window.draw(point(px(0.), px(0.)), size(px(900.), px(600.)), |_, _| {
            app.clone().into_any_element()
        });
        // Viewport demand starts native query-reader work. Wait for its accepted
        // projection rather than assuming a fixed number of draws completes it.
        window
            .condition(app, |this, _| {
                let tree = this.library.tree();
                this.library.search_counts() == Some((0, 0))
                    && tree.row_count() == roots.len()
                    && roots.iter().enumerate().all(|(index, root)| {
                        matches!(tree.item(index), Some(TreeItem::Entry(row))
                            if row.depth == 0
                                && matches!(&row.row, DirectoryRow::Directory { path } if path == root))
                    })
            })
            .await;
    }

    fn assert_roots(window: &mut VisualTestContext, app: &Entity<Library>, roots: &[PathBuf]) {
        app.update(&mut window.cx, |this, _| {
            let tree = this.library.tree();
            let rendered = (0..tree.row_count())
                .map(|index| match tree.item(index) {
                    Some(TreeItem::Entry(row)) if row.depth == 0 => match row.row {
                        DirectoryRow::Directory { path } => path,
                        DirectoryRow::OtherTracks => {
                            panic!("empty Other collection remains visible")
                        }
                        DirectoryRow::Track(_) => panic!("forest contains a track"),
                    },
                    _ => panic!("configured root was not projected by viewport demand"),
                })
                .collect::<Vec<_>>();
            assert_eq!(rendered, roots);
            assert_eq!(this.library.search_counts(), Some((0, 0)));
            assert_eq!(this.state.library.track_total, 0);
            assert!(!this.state.system.scanning);
            assert!(this.state.system.scan_progress.is_none());
        });
    }

    #[gpui::test]
    async fn empty_library_projects_first_added_appended_and_removed_roots(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            rivu_ui::theme::set_palette(crate::theme::palette(crate::theme::Theme::default()), cx);
            rivu_ui::theme::set_typography(
                gpui::font("sans-serif"),
                crate::gui::appearance::fonts::SYMBOLS_FAMILY,
                12,
                cx,
            );
        });
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        Config {
            volume: 0.0,
            ffmpeg_enabled: false,
            mpris_enabled: false,
            ..Config::default()
        }
        .save(&config_path)
        .unwrap();
        cx.executor().allow_parking();
        let runtime = CoreRuntime::start(directory.path(), &config_path).unwrap();
        settle_config(&runtime.handle);
        let i18n = Arc::new(Localizer::new("en-US"));
        let labels = std::rc::Rc::new(
            i18n.fixed_texts()
                .map(|(id, text)| (id.to_owned(), gpui::SharedString::from(text.to_owned())))
                .collect(),
        );
        let context = FeatureContext {
            snapshots: std::rc::Rc::new(crate::gui::feature::SnapshotSource::new(
                runtime.handle.clone(),
            )),
            i18n,
            labels,
            scroll: Default::default(),
        };
        let (app, window) = cx.add_window_view(|_, cx| Library::new(context, cx));
        // Snapshot publication uses explicit FIFO barriers and refreshes;
        // native query completion is awaited through the GPUI condition.
        runtime.handle.set_wakeup(|| {});
        app.update(&mut window.cx, |this, cx| {
            this.set_available(true);
            this.set_visible(true, cx);
        });
        draw_library(window, &app, &[]).await;
        assert_roots(window, &app, &[]);

        let first = directory.path().join("first");
        let second = directory.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        for (change, expected) in [
            (
                GuiConfigChange::AddLibraryRoot(first.clone()),
                vec![first.clone()],
            ),
            (
                GuiConfigChange::AddLibraryRoot(second.clone()),
                vec![first.clone(), second.clone()],
            ),
            (
                GuiConfigChange::RemoveLibraryRoot(first),
                vec![second.clone()],
            ),
            (GuiConfigChange::RemoveLibraryRoot(second), Vec::new()),
        ] {
            runtime.handle.update_gui_config(change).unwrap();
            settle_config(&runtime.handle);
            draw_library(window, &app, &expected).await;
            assert_roots(window, &app, &expected);
        }
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn unchanged_and_transport_only_publications_do_not_change_library_display() {
        let previous = GuiSnapshot::default();
        assert!(!display_changed(&previous, &previous));
        let mut next = previous.clone();
        next.playback.position += 1.;
        next.playback.last_heard_at = Some(1);
        next.playback.volume = 0.5;
        next.queue.revision += 1;
        next.library.playlist_revision += 1;
        next.system.revision += 1;
        assert!(!display_changed(&previous, &next));
    }

    #[test]
    fn library_content_scan_and_display_configuration_changes_repaint() {
        let previous = GuiSnapshot::default();
        let mut next = previous.clone();
        next.library.revision += 1;
        assert!(display_changed(&previous, &next));
        next = previous.clone();
        next.system.scanning = !previous.system.scanning;
        assert!(display_changed(&previous, &next));
        next = previous.clone();
        Arc::make_mut(&mut next.system.config)
            .library_columns
            .artist = !previous.system.config.library_columns.artist;
        assert!(display_changed(&previous, &next));
        next = previous.clone();
        next.playback.status = rivu_protocol::model::PlaybackStatus::Playing;
        assert!(display_changed(&previous, &next));
    }
}
