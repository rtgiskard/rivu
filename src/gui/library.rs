mod locate;
mod model;
mod owner;
mod queries;
mod requests;
pub(in crate::gui) mod selection;
pub(super) mod tree;

use super::components::track::{AudioSpecs, audio_specs, track_title};
use super::feature::FeatureContext;
use crate::config::{LibraryColumn, LibraryColumns};
use gpui::{AnyElement, Context, Div, Render, SharedString, Window, div, prelude::*, px, rgb};
use rivu_protocol::model::{Command, DirectoryRow, LibraryRow};
use rivu_ui::{
    IconButton, IconButtonDensity, IconButtonStyle, VirtualList, button, caption, lazy_row_tooltip,
    list_panel_header, list_viewport, panel_surface, row,
};
use std::{path::Path, sync::Arc};

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

pub(super) fn track_detail(
    i18n: &crate::i18n::Localizer,
    track: &rivu_protocol::model::LibraryRow,
) -> String {
    let mut detail = track
        .duration
        .map_or_else(|| "—".to_owned(), super::format_time);
    let specs = audio_specs(AudioSpecs::from(track));
    if !specs.is_empty() {
        detail.push_str(" · ");
        detail.push_str(&specs);
    }
    if track.missing {
        detail.push_str(i18n.text("gl-file-missing-suffix"));
    }
    detail.push('\n');
    detail.push_str(&track.path.to_string_lossy());
    detail
}

fn track_columns(
    theme: &'static rivu_ui::theme::Palette,
    track: &LibraryRow,
    columns: LibraryColumns,
) -> Option<gpui::Div> {
    if !LibraryColumn::ALL
        .into_iter()
        .any(|column| columns.enabled(column))
    {
        return None;
    }
    // Text fields share shrinkable space; scalar fields keep their intrinsic width.
    // Caption text is 0.75rem, so gap_3/px_3 leave one em between columns.
    let mut cells = row().gap_3().px_3().min_w_0().flex_initial();
    for column in LibraryColumn::ALL {
        if !columns.enabled(column) {
            continue;
        }
        let value: gpui::SharedString = match column {
            LibraryColumn::Artist => {
                if track.artist.is_empty() {
                    "-".into()
                } else {
                    track.artist.clone().into()
                }
            }
            LibraryColumn::Album => {
                if track.album.is_empty() {
                    "-".into()
                } else {
                    track.album.clone().into()
                }
            }
            LibraryColumn::Duration => track
                .duration
                .map_or_else(|| "-".into(), |value| super::format_time(value).into()),
            LibraryColumn::Codec => {
                if track.codec.is_empty() {
                    "-".into()
                } else {
                    track.codec.to_ascii_lowercase().into()
                }
            }
            LibraryColumn::Bitrate => track.bitrate_bps.map_or_else(
                || "-".into(),
                |value| {
                    format!(
                        "{}{:.0}",
                        if track.bitrate_exact { "" } else { "≈" },
                        value as f64 / 1000.0
                    )
                    .into()
                },
            ),
        };
        let cell = rivu_ui::caption(theme, value).truncate();
        cells = cells.child(match column {
            LibraryColumn::Artist | LibraryColumn::Album => {
                cell.max_w_24().flex_initial().min_w_0()
            }
            LibraryColumn::Duration | LibraryColumn::Bitrate => cell
                .font_features(super::appearance::fonts::tabular_numbers())
                .flex_shrink_0(),
            LibraryColumn::Codec => cell.flex_shrink_0(),
        });
    }
    Some(cells)
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum LibraryNode {
    OtherTracks,
    Directory(Arc<Path>),
    Track(i64),
}

pub(super) struct LibraryDrag {
    source: LibraryNode,
    nodes: std::cell::OnceCell<Vec<LibraryNode>>,
}

impl LibraryDrag {
    pub(in crate::gui) fn can_resolve(
        &self,
        _snapshot: &crate::projection::GuiSnapshot,
        selection_pending: bool,
    ) -> bool {
        !selection_pending && !self.nodes().is_empty()
    }

    pub(super) fn nodes(&self) -> &[LibraryNode] {
        self.nodes
            .get()
            .expect("drag nodes initialized at drag start")
    }
}

struct LibraryDragPreview {
    label: gpui::SharedString,
    icon: &'static str,
}

impl LibraryDragPreview {
    fn new(
        i18n: &crate::i18n::Localizer,
        nodes: &[LibraryNode],
        title: gpui::SharedString,
    ) -> Self {
        let directories = nodes
            .iter()
            .filter(|node| matches!(node, LibraryNode::Directory(_) | LibraryNode::OtherTracks))
            .count();
        let (icon, label) = if nodes.len() == 1 {
            match &nodes[0] {
                LibraryNode::OtherTracks => ("󰀻", i18n.text("library-other").to_owned().into()),
                LibraryNode::Directory(path) => (
                    "󰉋",
                    path.file_name()
                        .unwrap_or(path.as_os_str())
                        .to_string_lossy()
                        .into_owned()
                        .into(),
                ),
                LibraryNode::Track(_) => ("󰈙", title),
            }
        } else if directories == nodes.len() {
            (
                "󰉋",
                i18n.format("gl-folder-count", &[("count", nodes.len().into())])
                    .into(),
            )
        } else if directories == 0 {
            (
                "󰈙",
                i18n.format("gl-track-count", &[("count", nodes.len().into())])
                    .into(),
            )
        } else {
            (
                "󰉓",
                i18n.format("gl-entry-count", &[("count", nodes.len().into())])
                    .into(),
            )
        };
        Self { label, icon }
    }
}

impl Render for LibraryDragPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        rivu_ui::anchor_list_drag_preview(
            rivu_ui::drag_preview_surface(window, cx)
                .gap_2()
                .child(
                    div()
                        .flex_shrink_0()
                        .font_family(super::appearance::fonts::SYMBOLS_FAMILY)
                        .font_size_scale(1.)
                        .child(self.icon),
                )
                .child(div().min_w_0().truncate().child(self.label.clone())),
            window,
        )
    }
}

fn tree_guides(
    theme: &'static rivu_ui::theme::Palette,
    tree: &DirectoryTree,
    index: usize,
) -> gpui::Div {
    let mut guides = div().absolute().inset_0();
    tree.visit_guides(index, |column, continues, current| {
        let left = gpui::rems(0.5 + column as f32 * 1.25);
        if continues || current {
            guides = std::mem::replace(&mut guides, div()).child(
                div()
                    .absolute()
                    .left(left)
                    .top_0()
                    .w(px(1.0))
                    .h(gpui::relative(if continues { 1.0 } else { 0.5 }))
                    .bg(gpui::rgb(theme.muted).alpha(0.35)),
            );
        }
        if current {
            guides = std::mem::replace(&mut guides, div()).child(
                div()
                    .absolute()
                    .left(left)
                    .top(gpui::relative(0.5))
                    .w_2()
                    .h(px(1.0))
                    .bg(gpui::rgb(theme.muted).alpha(0.35)),
            );
        }
    });
    guides
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

    /// Render logical preorder slots, loading only the viewport's missing
    /// payloads. Expanding a branch never replaces its ancestors or siblings.
    pub(super) fn library_directory_list(
        &self,
        panel_id: u64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = rivu_ui::theme::palette(cx);
        let row_count = self.library.tree().row_count();
        let scroll = &self.library.tree().scroll;
        // Local forest acceptance must notify after prepaint to redraw the loaded rows.
        let demand_owner = cx.weak_entity();
        let view = list_viewport(
            VirtualList::new(("library-directory-tree", panel_id), row_count, scroll, cx.processor(move |this, range: std::ops::Range<usize>, window, cx| {
                let row_height = rivu_ui::tree_row_height(window, cx);
                let inline_action = |view: gpui::Stateful<Div>| {
                    view.min_h_0().h(row_height - px(2.)).py_0()
                        .min_w_0().w(row_height - px(2.)).px_0()
                };
                let playing_track = this.state.current_track()
                    .filter(|_| this.state.playback.status == rivu_protocol::model::PlaybackStatus::Playing)
                    .map(|track| track.id);
                let focused_index = this.focused
                    .then(|| this.library.tree().selected_index())
                    .flatten();
                range
                    .filter_map(|index| {
                        let item = this.library.tree().item(index)?;
                        let visible = match item {
                            TreeItem::Entry(visible) => visible,
                            TreeItem::Placeholder {
                                branch,
                                offset,
                                depth,
                                status,
                            } => {
                                let failed = status == BranchStatus::Error;
                                let placeholder = rivu_ui::tree_row(
                                    theme,
                                    gpui::ElementId::named_usize("library-loading", index),
                                    0,
                                    false,
                                )
                                .h(row_height)
                                .gap_0()
                                .overflow_hidden()
                                .child(
                                    div()
                                        .relative()
                                        .flex_1()
                                        .min_w_0()
                                        .h_full()
                                        .overflow_hidden()
                                        .child(tree_guides(theme, this.library.tree(), index))
                                        .child(
                                            row()
                                                .h_full()
                                                .pl(gpui::rems(depth as f32 * 1.25))
                                                .child(
                                                    rivu_ui::caption(theme, this.tr(if failed { "gl-unavailable" } else { "gl-loading" }))
                                                        .when(!this.loading_text.show(&branch, failed), |text| text.invisible())
                                                        .min_w_0()
                                                        .truncate(),
                                                )
                                                .when(failed, |view| {
                                                    view.child(
                                                        IconButton::new(gpui::ElementId::named_usize("library-retry", index), "󰑐", this.tr("gl-retry-folder")).build_with(IconButtonStyle::Quiet, IconButtonDensity::Regular, cx, move |this, _, cx| {
                                                            this.library.retry_directory(&branch);
                                                            this.request_directory_page(branch.clone(), offset, cx);
                                                        }, inline_action),
                                                    )
                                                }),
                                        ),
                                );
                                return Some(placeholder.into_any_element());
                            }
                        };
                        let entry = &visible.row;
                        let (node, title, detail, path) = match entry {
                            DirectoryRow::OtherTracks => (
                                LibraryNode::OtherTracks,
                                this.i18n.text("library-other").to_owned(),
                                String::new(),
                                None,
                            ),
                            DirectoryRow::Directory { path } => {
                                let title = if visible.depth == 0 {
                                    path.display().to_string()
                                } else {
                                    path.file_name().map_or_else(
                                        || path.display().to_string(),
                                        |name| name.to_string_lossy().into_owned(),
                                    )
                                };
                                let detail = match visible.status {
                                    BranchStatus::Loading => this.i18n.text("gl-loading").to_owned(),
                                    BranchStatus::Empty => this.i18n.text("gl-empty").to_owned(),
                                    BranchStatus::Error => this.i18n.text("gl-unavailable").to_owned(),
                                    BranchStatus::Unloaded | BranchStatus::Ready => {
                                        String::new()
                                    }
                                };
                                (
                                    LibraryNode::Directory(Arc::from(path.as_path())),
                                    title,
                                    detail,
                                    Some(path),
                                )
                            }
                            DirectoryRow::Track(track) => (
                                LibraryNode::Track(track.id),
                                track_title(&track.title, &track.path, track.cue_number).into_owned(),
                                String::new(),
                                None,
                            ),
                        };
                        let row_id = match &node {
                            LibraryNode::OtherTracks => gpui::ElementId::Name("library-other-tracks".into()),
                            LibraryNode::Track(id) => gpui::ElementId::NamedInteger("library-track".into(), *id as u64),
                            LibraryNode::Directory(path) => gpui::ElementId::Path(Arc::clone(path)),
                        };
                        let selected = this.directory_row_selected(entry);
                        let id = node;
                        let retry_counts = match entry {
                            DirectoryRow::OtherTracks => this.library.other_count_failed(),
                            DirectoryRow::Directory { path } => this.library.root_count_failed(path),
                            DirectoryRow::Track(_) => false,
                        };
                        let scan = path.map(|scan_path| {
                            let scan_path = scan_path.clone();
                            let hint = if retry_counts {
                                this.i18n.format("gl-retry-root-counts", &[("path", scan_path.display().to_string().into())])
                            } else {
                                this.i18n.format("gl-rescan-path", &[("path", scan_path.display().to_string().into())])
                            };
                            IconButton::new(gpui::ElementId::named_usize("library-scan", index), "󰑐", hint).build_with(IconButtonStyle::Quiet, IconButtonDensity::Regular, cx, move |this, _, cx| {
                                if this.library.root_count_failed(&scan_path) {
                                    this.library.retry_root_counts();
                                    this.request_root_counts(cx);
                                } else {
                                    this.send(
                                        Command::Scan {
                                            paths: vec![scan_path.clone()],
                                            force: this.library.force_scan(),
                                        },
                                        cx,
                                    );
                                }
                            }, inline_action)
                            .when(retry_counts, |view| view.text_color(gpui::rgb(theme.error)))
                        }).or_else(|| matches!(entry, DirectoryRow::OtherTracks).then(|| {
                            let hint = if retry_counts {
                                "gl-retry-other-counts"
                            } else if this.library.force_scan() {
                                "library-other-force"
                            } else {
                                "library-other-check"
                            };
                            IconButton::new(gpui::ElementId::named_usize("library-check-other", index), "󰑐", this.tr(hint))
                                .enabled(retry_counts || !this.state.system.scanning)
                                .build_with(IconButtonStyle::Quiet, IconButtonDensity::Regular, cx, |this, _, cx| {
                                    if this.library.other_count_failed() {
                                        this.library.retry_root_counts();
                                        this.request_root_counts(cx);
                                    } else {
                                        this.send(Command::CheckOtherTracks { force: this.library.force_scan() }, cx);
                                    }
                                }, inline_action)
                                .when(retry_counts, |view| view.text_color(gpui::rgb(theme.error)))
                        }));
                        let title = gpui::SharedString::from(title);
                        let name_content = row()
                            .w_full()
                            .h_full()
                            .gap_1()
                            .pl(gpui::rems(visible.depth as f32 * 1.25))
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_sm()
                                    .font_family(super::appearance::fonts::SYMBOLS_FAMILY)
                                    .font_size_scale(1.)
                                    .child(if matches!(entry, DirectoryRow::OtherTracks) { "󰀻" } else if visible.branch.is_some() {
                                        if visible.expanded {
                                            "󰝰"
                                        } else {
                                            "󰉋"
                                        }
                                    } else {
                                        "󰎇"
                                    }),
                            )
                            .child(
                                row()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_1()
                                    .overflow_hidden()
                                    .child(
                                        div()
                                            .min_w_0()
                                            .text_sm()
                                            .truncate()
                                            .when(matches!(entry, DirectoryRow::Track(track)
                                                if playing_track == Some(track.id)), |view| {
                                                view.text_color(gpui::rgb(theme.accent))
                                            })
                                            .text_overflow(gpui::TextOverflow::Truncate("..".into()))
                                            .child(title.clone())
                                            .when(matches!(entry, DirectoryRow::Track(_)), |view| view.flex_1())
                                            .when(matches!(entry, DirectoryRow::OtherTracks), |view| view.flex_shrink_0()),
                                    )
                                    .when(!detail.is_empty(), |view| {
                                        view.child(rivu_ui::caption(theme, detail.clone()).min_w_0().truncate()
                                            .when(path.is_none(), |text| text.flex_1())
                                            .when(visible.status == BranchStatus::Loading && !visible.branch.as_ref().is_some_and(|branch| this.loading_text.show(branch, false)), |text| text.invisible())
                                            .text_overflow(gpui::TextOverflow::Truncate("..".into())))
                                    })
                                    .when(matches!(entry, DirectoryRow::Track(track) if track.missing), |view| {
                                        view.child(
                                            div()
                                                .flex_shrink_0()
                                                .text_sm()
                                                .font_family(super::appearance::fonts::SYMBOLS_FAMILY)
                                                .font_size_scale(1.)
                                                .text_color(gpui::rgb(theme.error))
                                                .child("󰩋"),
                                        )
                                    }),
                            )
                            .when(visible.depth == 0 && path.is_some(), |view| {
                                let count = this.library.root_count_label(path.expect("root path"));
                                view.child(
                                    row()
                                        .gap_1()
                                        .flex_shrink_0()
                                        .child(rivu_ui::caption(theme, "󰎇").font_family(super::appearance::fonts::SYMBOLS_FAMILY).font_size_scale(1.))
                                        .child(rivu_ui::caption(theme, count)
                                            .font_features(super::appearance::fonts::tabular_numbers())),
                                )
                            })
                            .when(matches!(entry, DirectoryRow::OtherTracks), |view| {
                                view.child(rivu_ui::caption(theme, this.library.other_count_label())
                                    .font_features(super::appearance::fonts::tabular_numbers()))
                            });
                        let name_cell = div()
                            .w_56()
                            .flex_auto()
                            .min_w_0()
                            .h_full()
                            .relative()
                            .overflow_hidden()
                            .child(tree_guides(theme, this.library.tree(), index))
                            .child(name_content)
                            .when_some(scan, |view, scan| {
                                view.child(
                                    rivu_ui::hover_actions(
                                        theme,
                                        gpui::ElementId::named_usize("library-scan-overlay", index),
                                        "library-track-row",
                                        selected,
                                    )
                                        .when(retry_counts, |view| view.opacity(1.))
                                        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                        .on_click(|_, _, cx| cx.stop_propagation())
                                        .child(scan),
                                )
                            });
                        let menu_node = id.clone();
                        let menu_root = path.filter(|path| this.state.system.config.library_roots.contains(path)).cloned();
                        let row_view = lazy_row_tooltip(
                            rivu_ui::tree_row(
                                theme,
                                row_id,
                                0,
                                selected,
                            )
                            .h(row_height)
                            .gap_0()
                            .overflow_hidden()
                            .group("library-track-row")
                            .relative()
                            .when(focused_index == Some(index), |view| {
                                view.border_color(gpui::rgb(theme.accent))
                            })
                            .when(matches!(entry, DirectoryRow::Track(track)
                                if this.library.locate.highlight.is_some_and(|(id, started)|
                                    id == track.id && started.elapsed() < std::time::Duration::from_secs(2))), |view| {
                                view.bg(gpui::rgb(theme.highlight)).border_color(gpui::rgb(theme.accent))
                            })
                            .on_mouse_down(gpui::MouseButton::Right, cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                                cx.emit(LibraryEvent::Focus);
                                if this.library_range_pending() { cx.stop_propagation(); return; }
                                let selected = match &menu_node {
                                    LibraryNode::Track(id) => this.library.selected().contains(id),
                                    LibraryNode::Directory(_) | LibraryNode::OtherTracks => this.library.selected_nodes().contains(&menu_node),
                                };
                                if !selected { this.select_library_row(menu_node.clone(), index, false, false, cx); }
                                let nodes = this.library.drag_nodes(&menu_node);
                                let track_ids = this.library_drag_track_ids(&nodes);
                                let only_tracks = nodes.iter().all(|node| matches!(node, LibraryNode::Track(_)));
                                let play = match &menu_node { LibraryNode::Track(track_id) => Some(Command::Play { track_id: *track_id }), _ => None };
                                let remove = only_tracks.then(|| Command::RemoveTracks { track_ids: track_ids.clone() });
                                this.open_object_menu(super::ContentMenuRequest {
                                    source: super::TrackSource::Tracks(track_ids), position: event.position, play,
                                }, super::list_interaction::ContentGuard::Library(this.state.library.revision, this.state.library.structure_revision),
                                    remove, Some(nodes), menu_root.clone(), cx);
                                cx.stop_propagation();
                            }))
                            .child(name_cell)
                            .when_some(
                                match entry {
                                    DirectoryRow::Track(track) => track_columns(theme, track, this.state.system.config.library_columns),
                                    DirectoryRow::Directory { .. } | DirectoryRow::OtherTracks => None,
                                },
                                |view, columns| view.child(columns),
                            )
                            .when_some(
                                match entry {
                                    DirectoryRow::Track(track) => Some((track.id, track.favorite)),
                                    DirectoryRow::Directory { .. } | DirectoryRow::OtherTracks => None,
                                },
                                |view, (track_id, favorite)| {
                                    view.child(
                                        rivu_ui::hover_actions(theme, ("library-track-actions", track_id as u64), "library-track-row", selected).child(this.track_favorite_toggle(
                                            gpui::ElementId::NamedInteger("library-tree-favorite".into(), track_id as u64),
                                            track_id,
                                            favorite,
                                            cx,
                                        )),
                                    )
                                },
                            ),
                            {
                                let entry = visible.row;
                                let i18n = Arc::clone(&this.i18n);
                                let title = title.clone();
                                move || match &entry {
                                    DirectoryRow::Track(track) => {
                                        let mut heading = track_title(&track.title, &track.path, track.cue_number).into_owned();
                                        if !track.artist.trim().is_empty() {
                                            heading.push_str(" · ");
                                            heading.push_str(&track.artist);
                                        }
                                        if !track.album.trim().is_empty() {
                                            heading.push_str(if track.artist.trim().is_empty() { " · " } else { " / " });
                                            heading.push_str(&track.album);
                                        }
                                        let detail = track_detail(&i18n, track);
                                        (heading.into(), detail.into())
                                    }
                                    DirectoryRow::Directory { path } => (path.display().to_string().into(), detail.clone().into()),
                                    DirectoryRow::OtherTracks => (title.clone(), detail.clone().into()),
                                }
                            },
                        )
                        .when(!this.library_range_pending(), |view| {
                            let drag_owner = cx.weak_entity();
                            let drag_i18n = Arc::clone(&this.i18n);
                            view.on_drag(
                                LibraryDrag {
                                    source: id.clone(),
                                    nodes: std::cell::OnceCell::new(),
                                },
                                move |drag, _, window, cx| {
                                    rivu_ui::begin_list_drag(window, cx);
                                    let nodes = drag.nodes.get_or_init(|| {
                                        drag_owner
                                            .update(cx, |this, _| this.library.drag_nodes(&drag.source))
                                            .unwrap_or_default()
                                    });
                                    cx.new(|_| {
                                        LibraryDragPreview::new(&drag_i18n, nodes, title.clone())
                                    })
                                },
                            )
                        })
                        .on_click(cx.listener(
                            move |this, event: &gpui::ClickEvent, _, cx| {
                                this.library.cancel_navigation();
                                cx.emit(LibraryEvent::Focus);
                                match id.clone() {
                                    LibraryNode::Track(id) => {
                                        let modifiers = event.modifiers();
                                        this.select_library_row(
                                            LibraryNode::Track(id),
                                            index,
                                            modifiers.shift,
                                            modifiers.control,
                                            cx,
                                        );
                                        if event.click_count() == 2 {
                                            this.send(Command::Play { track_id: id }, cx);
                                        }
                                    }
                                    node @ (LibraryNode::Directory(_) | LibraryNode::OtherTracks) => {
                                        let modifiers = event.modifiers();
                                        let multi = modifiers.control;
                                        if !multi && !modifiers.shift {
                                            let branch = visible
                                                .branch
                                                .as_ref()
                                                .expect("directory occurrence");
                                            this.set_directory_expanded(
                                                branch,
                                                !this.library.tree().expanded(branch),
                                                cx,
                                            );
                                        }
                                        this.select_library_row(
                                            node,
                                            index,
                                            modifiers.shift,
                                            multi,
                                            cx,
                                        );
                                    }
                                }
                            },
                        ));
                        Some(row_view.into_any_element())
                    })
                    .collect::<Vec<_>>()
            })).on_viewport_deferred(demand_owner, |this, range, cx| {
                if !this.demanded() {
                    return;
                }
                this.request_root_counts(cx);
                for (path, offset) in this.library.demand_directories(range) {
                    this.request_directory_page(path, offset, cx);
                }
            })
            .h_full()
            .min_h_0()
            .w_full().scrollbar(("library-directory-scrollbar", panel_id)),
        );
        view.into_any_element()
    }
}

impl Library {
    pub(super) fn render_panel(
        &mut self,
        panel_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = rivu_ui::theme::palette(cx);
        self.finish_library_locate_render(window, cx);
        let total = self.library.tree().root_total();
        let range_pending = self.library_range_pending();
        let selected_directories = self
            .library
            .selected_nodes()
            .iter()
            .filter(|node| {
                matches!(
                    node,
                    super::library::LibraryNode::Directory(_)
                        | super::library::LibraryNode::OtherTracks
                )
            })
            .count();
        let selection_label = if range_pending {
            self.i18n.text("gl-selecting").to_owned()
        } else if selected_directories > 0 && self.library.selected().is_empty() {
            self.i18n.format(
                "gl-folders-selected",
                &[("count", (selected_directories).into())],
            )
        } else if selected_directories > 0 {
            self.i18n.format(
                "gl-mixed-selected",
                &[
                    ("tracks", (self.library.selected().len()).into()),
                    ("folders", (selected_directories).into()),
                ],
            )
        } else if !self.library.selected().is_empty() {
            self.i18n.format(
                "gl-tracks-selected",
                &[("count", (self.library.selected().len()).into())],
            )
        } else if let Some(total) = total {
            self.i18n
                .format("gl-entry-count", &[("count", (total).into())])
        } else {
            self.i18n.text("gl-loading").to_owned()
        };
        let heading = list_panel_header(theme, self.tr("gl-library"), selection_label.clone())
            .id(("library-selection-count", panel_id))
            .tooltip(rivu_ui::text_tooltip(selection_label));
        let tools = rivu_ui::panel_header_actions()
            .child(self.library_search.clone())
            .child(
                IconButton::new(
                    ("library-locate-playing", panel_id),
                    "",
                    self.tr("gl-locate-library"),
                )
                .enabled(self.state.current_track().is_some() && !self.library.locate.loading)
                .build(
                    IconButtonStyle::Surface,
                    IconButtonDensity::Regular,
                    cx,
                    |this, window, cx| this.locate_playing_library(window, cx),
                ),
            )
            .child(
                IconButton::new(
                    ("library-view", panel_id),
                    if self.library.tree_active() {
                        "󰉋"
                    } else {
                        "󰉢"
                    },
                    if self.library.tree_active() {
                        self.tr("gl-flat-view")
                    } else {
                        self.tr("gl-tree-view")
                    },
                )
                .build(
                    IconButtonStyle::Surface,
                    IconButtonDensity::Regular,
                    cx,
                    |this, _, cx| this.toggle_library_view(cx),
                ),
            )
            .child(
                IconButton::new(
                    ("library-filter-favorites", panel_id),
                    "󰓎",
                    self.tr("gl-filter-favorites"),
                )
                .toggled(self.library.favorites_only())
                .build(
                    IconButtonStyle::Surface,
                    IconButtonDensity::Regular,
                    cx,
                    |this, _, cx| {
                        this.refresh_favorite_filter(cx);
                    },
                ),
            )
            .child(
                IconButton::new(
                    ("library-filter-missing", panel_id),
                    "󰩋",
                    self.tr("gl-filter-missing"),
                )
                .toggled(self.library.missing_only())
                .build(
                    IconButtonStyle::Surface,
                    IconButtonDensity::Regular,
                    cx,
                    |this, _, cx| {
                        this.library.toggle_missing();
                        this.refresh_filter(cx);
                    },
                ),
            )
            .child(
                IconButton::new(
                    ("library-force-scan", panel_id),
                    "󱉶",
                    if self.library.force_scan() {
                        self.tr("gl-force-on")
                    } else {
                        self.tr("gl-force-off")
                    },
                )
                .toggled(self.library.force_scan())
                .build(
                    IconButtonStyle::Surface,
                    IconButtonDensity::Regular,
                    cx,
                    |this, _, cx| {
                        this.library.toggle_force_scan();
                        cx.notify();
                    },
                ),
            )
            .child(
                IconButton::new(("library-add-root", panel_id), "󰐕", self.tr("gl-add-root")).build(
                    IconButtonStyle::Surface,
                    IconButtonDensity::Regular,
                    cx,
                    |this, _, cx| this.choose_library_root(cx),
                ),
            );
        let mut panel = panel_surface(("library-panel", panel_id))
            .child(rivu_ui::panel_heading_row(heading, tools));
        if let Some((track_id, result)) = &self.library.locate.notice
            && self
                .state
                .current_track()
                .is_some_and(|track| track.id == *track_id)
        {
            use rivu_protocol::model::LibraryLocateResult;
            let message = match result {
                LibraryLocateResult::Filtered => Some("gl-locate-library-filtered"),
                LibraryLocateResult::OutsideRoot => Some("gl-locate-library-outside"),
                LibraryLocateResult::Unavailable => Some("gl-locate-library-unavailable"),
                LibraryLocateResult::Located(_) => None,
            };
            if let Some(message) = message {
                panel = panel.child(
                    row()
                        .flex_shrink_0()
                        .flex_wrap()
                        .child(caption(theme, self.tr(message)))
                        .when(matches!(result, LibraryLocateResult::Filtered), |view| {
                            view.child(button(
                                ("library-clear-filters-locate", panel_id),
                                self.tr("gl-locate-clear-filters"),
                                cx,
                                |this, window, cx| {
                                    this.clear_filters_and_locate_library(window, cx)
                                },
                            ))
                        }),
                );
            }
        }
        if self.state.library.track_total == 0 {
            panel = panel.child(caption(theme, self.tr("library-other-empty")));
        }
        if self
            .state
            .system
            .scan_visible(crate::core::state::MediaTaskOwner::Library)
        {
            panel = panel.child(crate::gui::components::media_task::media_task_status(
                &self.state.system,
                &self.i18n,
                panel_id,
                cx,
                Self::send,
            ));
        }
        if let Some(error) = &self.scan_error {
            let message: SharedString = error.clone().into();
            panel = panel.child(rivu_ui::panel_heading_row(
                caption(theme, message.clone())
                    .id(("library-scan-request-error", panel_id))
                    .text_color(rgb(theme.error))
                    .truncate()
                    .tooltip(rivu_ui::text_tooltip(message)),
                rivu_ui::panel_header_actions().child(
                    IconButton::new(
                        ("library-dismiss-scan-request-error", panel_id),
                        "",
                        self.tr("gl-dismiss-scan"),
                    )
                    .build(
                        IconButtonStyle::Quiet,
                        IconButtonDensity::Regular,
                        cx,
                        |this, _, cx| {
                            this.scan_error = None;
                            cx.notify();
                        },
                    ),
                ),
            ));
        }
        self.context.scroll.borrow_mut().register_list(
            ("library-projection", panel_id),
            &self.library.tree().scroll,
        );
        // Track counts describe contents, not the configured root projection.
        // Keep its viewport mounted so root changes can demand a fresh forest.
        if let Some((0, total)) = self.library.search_counts()
            && (total != 0 || self.library.tree().row_count() == 0)
        {
            panel = panel.child(caption(
                theme,
                self.i18n
                    .text(if total == 0 {
                        "search-empty"
                    } else {
                        "search-no-matches"
                    })
                    .to_owned(),
            ));
        }
        panel
            .child(self.library_directory_list(panel_id, cx))
            .into_any_element()
    }
}
#[cfg(test)]
mod library_root_tests {
    use super::{FeatureContext, Library};
    use crate::config::ConfigPersistence;
    use crate::{
        config::Config,
        core::{CoreHandle, GuiConfigChange, Runtime},
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
        let runtime = Runtime::start(directory.path(), &config_path).unwrap();
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
