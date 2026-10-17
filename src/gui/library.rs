mod locate;
pub(super) mod tree;

use super::{
    GuiApp, ListFocus,
    browser::selection::LibraryRowRequest,
    components::{
        active_control, button, caption, column, disabled_control, icon_button, list_viewport,
        panel_header, panel_surface, panel_toolbar, quiet_icon_button, row,
    },
};
use crate::{
    config::{LibraryColumn, LibraryColumns},
    model::{Command, DirectoryRow, LibraryRow},
};
use gpui::{
    AnyElement, Context, Div, Render, SharedString, Window, div, prelude::*, px, rgb, uniform_list,
};
use std::{path::Path, sync::Arc};

use tree::TreeItem;
pub(super) use tree::{BranchStatus, DirectoryTree, TreeBranch};

pub(super) use locate::LibraryLocateState;

pub(super) fn track_detail(
    i18n: &crate::i18n::Localizer,
    track: &crate::model::LibraryRow,
) -> String {
    let mut detail = track.artist.clone();
    if !track.album.is_empty() {
        if !detail.is_empty() {
            detail.push_str(" / ");
        }
        detail.push_str(&track.album);
    }
    if !detail.is_empty() {
        detail.push_str(" · ");
    }
    match track.duration {
        Some(duration) => detail.push_str(&super::format_time(duration)),
        None => detail.push('-'),
    }
    detail.push_str(" · ");
    if track.codec.is_empty() {
        detail.push('-');
    } else {
        detail.extend(track.codec.chars().map(|ch| ch.to_ascii_lowercase()));
    }
    detail.push_str(" · ");
    match track.bitrate_bps {
        Some(value) => {
            use std::fmt::Write;
            let _ = write!(
                detail,
                "{}{:.0} kbps",
                if track.bitrate_exact { "" } else { "≈" },
                value as f64 / 1000.0
            );
        }
        None => detail.push('-'),
    }
    if track.missing {
        detail.push_str(i18n.text("gl-file-missing-suffix"));
    }
    detail
}

fn track_columns(
    theme: &'static crate::theme::Palette,
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
        let cell = crate::gui::components::caption(theme, value).truncate();
        cells = cells.child(match column {
            LibraryColumn::Artist | LibraryColumn::Album => {
                cell.max_w(gpui::rems(6.0)).flex_initial().min_w_0()
            }
            LibraryColumn::Duration | LibraryColumn::Bitrate => cell
                .font_features(super::fonts::tabular_numbers())
                .flex_shrink_0(),
            LibraryColumn::Codec => cell.flex_shrink_0(),
        });
    }
    Some(cells)
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum LibraryNode {
    Directory(Arc<Path>),
    Track(i64),
}

pub(super) struct LibraryDrag {
    source: LibraryNode,
    nodes: std::cell::OnceCell<Vec<LibraryNode>>,
}

impl LibraryDrag {
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
            .filter(|node| matches!(node, LibraryNode::Directory(_)))
            .count();
        let (icon, label) = if nodes.len() == 1 {
            match &nodes[0] {
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
        super::components::anchor_list_drag_preview(
            super::components::drag_preview_surface(window, cx)
                .gap_2()
                .child(
                    div()
                        .flex_shrink_0()
                        .font_family(super::fonts::SYMBOLS_FAMILY)
                        .child(self.icon),
                )
                .child(div().min_w_0().truncate().child(self.label.clone())),
            window,
        )
    }
}

fn tree_guides(
    theme: &'static crate::theme::Palette,
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
                    .w(gpui::rems(0.5))
                    .h(px(1.0))
                    .bg(gpui::rgb(theme.muted).alpha(0.35)),
            );
        }
    });
    guides
}

impl GuiApp {
    pub(super) fn can_resolve_library_drag(&self, drag: &LibraryDrag) -> bool {
        !self.library_range_pending() && !drag.nodes().is_empty()
    }

    pub(super) fn library_drag_track_ids(&self, nodes: &[LibraryNode]) -> Vec<i64> {
        let mut ids = nodes
            .iter()
            .filter_map(|node| match node {
                LibraryNode::Track(id) => Some(*id),
                LibraryNode::Directory(_) => None,
            })
            .collect::<Vec<_>>();
        ids.sort_unstable_by_key(|id| {
            (
                self.browser
                    .library
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
        if self.library_range_pending() || nodes.is_empty() {
            return;
        }
        let mut directories = nodes
            .iter()
            .filter_map(|node| match node {
                LibraryNode::Directory(path) => Some(path.to_path_buf()),
                LibraryNode::Track(_) => None,
            })
            .collect::<Vec<_>>();
        directories.sort();
        directories.dedup();
        let track_ids = self.library_drag_track_ids(nodes);
        match destination {
            super::DragDestination::Queue => self.send(
                Command::EnqueueSources {
                    directories,
                    track_ids,
                },
                cx,
            ),
            super::DragDestination::Playlist(playlist_id) => self.send(
                Command::AddPlaylistSources {
                    playlist_id,
                    directories,
                    track_ids,
                },
                cx,
            ),
        }
    }

    fn selected_tree_index(&self) -> Option<usize> {
        self.browser.library.tree().selected_index()
    }

    fn select_tree_row(&mut self, index: usize, cx: &mut Context<Self>) {
        let id = self.begin_manual_intent();
        self.prepare_tree_row(id, index, cx);
    }

    fn prepare_tree_row(
        &mut self,
        id: super::metadata::ManualIntentId,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        if !self.manual_intent_current(id) {
            return;
        }
        let Some(node) = self.browser.library.navigate_tree_row(index) else {
            self.browser.library.defer_manual_navigation(id);
            for (branch, offset) in self.browser.library.demand_directories(index..index + 1) {
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

    fn set_directory_expanded(
        &mut self,
        branch: &TreeBranch,
        expanded: bool,
        cx: &mut Context<Self>,
    ) {
        if self.browser.library.expand_directory(branch, expanded) {
            self.request_directory_page(branch.clone(), 0, cx);
        }
        cx.notify();
    }

    pub(super) fn navigate_library_tree(&mut self, down: bool, cx: &mut Context<Self>) {
        let count = self.browser.library.tree().row_count();
        if count == 0 {
            return;
        }
        let index = match self.selected_tree_index() {
            Some(index) if down => (index + 1).min(count - 1),
            Some(index) => index.saturating_sub(1),
            None if down => 0,
            None => count - 1,
        };
        let id = self.begin_manual_intent();
        self.prepare_tree_row(id, index, cx);
    }

    pub(super) fn jump_library_edge(&mut self, last: bool, cx: &mut Context<Self>) {
        let count = self.browser.library.tree().row_count();
        if count == 0 {
            return;
        }
        let intent = self.begin_manual_intent();
        self.prepare_tree_row(intent, if last { count - 1 } else { 0 }, cx);
    }

    pub(super) fn activate_library_tree(&mut self, cx: &mut Context<Self>) {
        if self.library_range_pending() {
            return;
        }
        let Some(index) = self.selected_tree_index() else {
            return;
        };
        let Some(TreeItem::Entry(visible)) = self.browser.library.tree().item(index) else {
            return;
        };
        match visible.row {
            DirectoryRow::Directory { .. } => {
                let branch = visible.branch.expect("directory has an occurrence");
                self.set_directory_expanded(
                    &branch,
                    !self.browser.library.tree().expanded(&branch),
                    cx,
                );
            }
            DirectoryRow::Track(track) => self.send(Command::Play { track_id: track.id }, cx),
        }
    }

    pub(super) fn library_tree_key(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(index) = self.selected_tree_index() else {
            return;
        };
        let Some(TreeItem::Entry(visible)) = self.browser.library.tree().item(index) else {
            return;
        };
        let depth = visible.depth;
        match key {
            "left" => {
                if visible.expanded
                    && let Some(branch) = &visible.branch
                {
                    self.set_directory_expanded(branch, false, cx);
                } else if let Some(parent) = self.browser.library.tree().parent_index(index) {
                    self.select_tree_row(parent, cx);
                }
            }
            "right" => {
                if let Some(branch) = &visible.branch {
                    if !visible.expanded {
                        self.set_directory_expanded(branch, true, cx);
                    } else if matches!(&self.browser.library.tree().item(index + 1),
                        Some(TreeItem::Entry(next)) if next.depth > depth)
                        || matches!(&self.browser.library.tree().item(index + 1),
                            Some(TreeItem::Placeholder { depth: next_depth, .. }) if *next_depth > depth)
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
        let theme = crate::gui::theme::palette(cx);
        let row_count = self.browser.library.tree().row_count();
        let scroll = &self.browser.library.tree().scroll;
        // Local forest acceptance must notify after prepaint to redraw the loaded rows.
        let demand_owner = cx.entity().downgrade();
        let view = list_viewport(
            super::components::scrollable_list(
                ("library-directory-scrollbar", panel_id),
            uniform_list(
                ("library-directory-tree", panel_id),
                row_count,
                cx.processor(move |this, range: std::ops::Range<usize>, window, cx| {
                    let playing_track = this.state.current_track()
                        .filter(|_| this.state.playback.status == crate::model::PlaybackStatus::Playing)
                        .map(|track| track.id);
                    let focused_index = (this.layout_focus.is_focused(window)
                        && this.browser.focus() == Some(super::BrowserFocus::List(ListFocus::Library)))
                        .then(|| this.browser.library.tree().selected_index())
                        .flatten();
                    range
                        .filter_map(|index| {
                            let item = this.browser.library.tree().item(index)?;
                            let visible = match item {
                                TreeItem::Entry(visible) => visible,
                                TreeItem::Placeholder {
                                    branch,
                                    offset,
                                    depth,
                                    status,
                                } => {
                                    let failed = status == BranchStatus::Error;
                                    let placeholder = crate::gui::components::tree_row(
                                        theme,
                                        gpui::ElementId::named_usize("library-loading", index),
                                        0,
                                        false,
                                    )
                                    .gap_0()
                                    .overflow_hidden()
                                    .child(
                                        div()
                                            .relative()
                                            .flex_1()
                                            .min_w_0()
                                            .h_full()
                                            .overflow_hidden()
                                            .child(tree_guides(theme, this.browser.library.tree(), index))
                                            .child(
                                                row()
                                                    .h_full()
                                                    .pl(gpui::rems(depth as f32 * 1.25))
                                                    .child(
                                                        crate::gui::components::caption(theme, this.tr(if failed { "gl-unavailable" } else { "gl-loading" }))
                                                            .when(!this.loading_text.show_branch(&branch, failed), |text| text.invisible())
                                                            .min_w_0()
                                                            .truncate(),
                                                    )
                                                    .when(failed, |view| {
                                                        view.child(
                                                            super::components::quiet_icon_button(theme, icon_button(
                                                                gpui::ElementId::named_usize("library-retry", index),
                                                                "󰑐",
                                                                this.tr("gl-retry-folder"),
                                                                cx,
                                                                move |this, _, cx| {
                                                                    this.browser.library.retry_directory(&branch);
                                                                    this.request_directory_page(branch.clone(), offset, cx);
                                                                },
                                                            ))
                                                            .min_h_0()
                                                            .size(gpui::rems(1.5)),
                                                        )
                                                    }),
                                            ),
                                    );
                                    return Some(placeholder.into_any_element());
                                }
                            };
                            let entry = &visible.row;
                            let (node, title, detail, path) = match entry {
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
                                        Some(path.clone()),
                                    )
                                }
                                DirectoryRow::Track(track) => (
                                    LibraryNode::Track(track.id),
                                    track.title.clone(),
                                    String::new(),
                                    None,
                                ),
                            };
                            let selected = this.directory_row_selected(entry);
                            let id = node.clone();
                            let retry_counts = path.as_deref().is_some_and(|path| {
                                this.browser.library.root_count_failed(path)
                            });
                            let scan = path.clone().map(|scan_path| {
                                let hint = if retry_counts {
                                    this.i18n.format("gl-retry-root-counts", &[("path", scan_path.display().to_string().into())])
                                } else {
                                    this.i18n.format("gl-rescan-path", &[("path", scan_path.display().to_string().into())])
                                };
                                super::components::quiet_icon_button(theme, icon_button(
                                    gpui::ElementId::named_usize("library-scan", index),
                                    "󰑐",
                                    hint,
                                    cx,
                                    move |this, _, cx| {
                                        if this.browser.library.root_count_failed(&scan_path) {
                                            this.browser.library.retry_root_counts();
                                            this.request_root_counts(cx);
                                        } else {
                                            this.send(
                                                Command::Scan {
                                                    paths: vec![scan_path.clone()],
                                                    force: this.browser.library.force_scan(),
                                                },
                                                cx,
                                            );
                                        }
                                    },
                                ))
                                .when(retry_counts, |view| view.text_color(gpui::rgb(theme.error)))
                            });
                            let title = gpui::SharedString::from(title);
                            let tooltip_title = path.as_ref().map_or_else(
                                || title.clone(),
                                |path| path.display().to_string().into(),
                            );
                            let name_content = row()
                                .w_full()
                                .h_full()
                                .gap_1()
                                .pl(gpui::rems(visible.depth as f32 * 1.25))
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .text_sm()
                                        .font_family(super::fonts::SYMBOLS_FAMILY)
                                        .child(if let Some(branch) = &visible.branch {
                                            if this.browser.library.tree().expanded(branch) {
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
                                                .when(path.is_none(), |view| view.flex_1()),
                                        )
                                        .when(path.is_some() && !detail.is_empty(), |view| {
                                            view.child(crate::gui::components::caption(theme, detail.clone()).min_w_0().truncate()
                                                .when(visible.status == BranchStatus::Loading && !visible.branch.as_ref().is_some_and(|branch| this.loading_text.show_branch(branch, false)), |text| text.invisible())
                                                .text_overflow(gpui::TextOverflow::Truncate("..".into())))
                                        })
                                        .when(matches!(entry, DirectoryRow::Track(track) if track.missing), |view| {
                                            view.child(
                                                div()
                                                    .flex_shrink_0()
                                                    .text_sm()
                                                    .font_family(super::fonts::SYMBOLS_FAMILY)
                                                    .text_color(gpui::rgb(theme.error))
                                                    .child("󰌶"),
                                            )
                                        }),
                                )
                                .when(visible.depth == 0 && path.is_some(), |view| {
                                    let count = this.browser.library.root_count(path.as_deref().expect("root path"));
                                    view.child(
                                        row()
                                            .gap_1()
                                            .flex_shrink_0()
                                            .child(crate::gui::components::caption(theme, "󰎇").font_family(super::fonts::SYMBOLS_FAMILY))
                                            .child(crate::gui::components::caption(theme, count.map_or_else(|| "—".to_owned(), |total| total.to_string()))
                                                .font_features(super::fonts::tabular_numbers())),
                                    )
                                });
                            let name_cell = div()
                                .w(gpui::rems(14.0))
                                .flex_auto()
                                .min_w_0()
                                .h_full()
                                .relative()
                                .overflow_hidden()
                                .child(tree_guides(theme, this.browser.library.tree(), index))
                                .child(name_content)
                                .when_some(scan, |view, scan| {
                                    view.child(
                                        row()
                                            .id(gpui::ElementId::named_usize("library-scan-overlay", index))
                                            .absolute()
                                            .right_0()
                                            .top_0()
                                            .h_full()
                                            .bg(super::components::row_background(theme, selected, false))
                                            .when(!retry_counts, |view| view.invisible())
                                            .group_hover("library-track-row", move |style| {
                                                style.visible().bg(super::components::row_background(theme, selected, true))
                                            })
                                            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                            .on_click(|_, _, cx| cx.stop_propagation())
                                            .child(scan.min_h_0().h(gpui::rems(1.5)).w(gpui::rems(1.4))),
                                    )
                                });
                            let menu_node = id.clone();
                            let menu_root = path.clone().filter(|path| this.state.system.config.library_roots.contains(path));
                            let row_view = super::components::lazy_row_tooltip(
                                crate::gui::components::tree_row(
                                    theme,
                                    gpui::ElementId::named_usize("library-node", index),
                                    0,
                                    selected,
                                )
                                .gap_0()
                                .overflow_hidden()
                                .group("library-track-row")
                                .relative()
                                .when(focused_index == Some(index), |view| {
                                    view.border_color(gpui::rgb(theme.accent))
                                })
                                .when(matches!(entry, DirectoryRow::Track(track)
                                    if this.browser.library.locate.highlight.is_some_and(|(id, started)|
                                        id == track.id && started.elapsed() < std::time::Duration::from_secs(2))), |view| {
                                    view.bg(gpui::rgb(theme.highlight)).border_color(gpui::rgb(theme.accent))
                                })
                                .on_mouse_down(gpui::MouseButton::Right, cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                                    this.focus_layout(window, cx);
                                    this.browser.focus_list(ListFocus::Library);
                                    if this.library_range_pending() { cx.stop_propagation(); return; }
                                    let selected = match &menu_node {
                                        LibraryNode::Track(id) => this.browser.library.selected().contains(id),
                                        LibraryNode::Directory(_) => this.browser.library.selected_nodes().contains(&menu_node),
                                    };
                                    if !selected { this.select_library_row(menu_node.clone(), index, false, false, cx); }
                                    let nodes = this.browser.library.drag_nodes(&menu_node);
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
                                        DirectoryRow::Directory { .. } => None,
                                    },
                                    |view, columns| view.child(columns),
                                )
                                .when_some(
                                    match entry {
                                        DirectoryRow::Track(track) => Some((track.id, track.favorite)),
                                        DirectoryRow::Directory { .. } => None,
                                    },
                                    |view, (track_id, favorite)| {
                                        view.child(
                                            super::components::hover_actions(theme, "library-track-row", selected).child(this.track_favorite_toggle(
                                                gpui::ElementId::named_usize("library-tree-favorite", index),
                                                "library-track-row",
                                                track_id,
                                                favorite,
                                                cx,
                                            )),
                                        )
                                    },
                                ),
                                tooltip_title,
                                {
                                    let entry = visible.row;
                                    let i18n = Arc::clone(&this.i18n);
                                    move || match &entry {
                                        DirectoryRow::Track(track) => track_detail(&i18n, track).into(),
                                        DirectoryRow::Directory { .. } => detail.clone().into(),
                                    }
                                },
                            )
                            .when(!this.library_range_pending(), |view| {
                                let app = cx.entity();
                                view.on_drag(
                                    LibraryDrag {
                                        source: id.clone(),
                                        nodes: std::cell::OnceCell::new(),
                                    },
                                    move |drag, _, window, cx| {
                                        super::components::begin_list_drag(window, cx);
                                        app.update(cx, |this, cx| {
                                            let nodes = drag.nodes.get_or_init(|| {
                                                this.browser.library.drag_nodes(&drag.source)
                                            });
                                            cx.new(|_| {
                                                LibraryDragPreview::new(&this.i18n, nodes, title.clone())
                                            })
                                        })
                                    },
                                )
                            })
                            .on_click(cx.listener(
                                move |this, event: &gpui::ClickEvent, window, cx| {
                                    this.browser.library.cancel_navigation();
                                    this.focus_layout(window, cx);
                                    this.browser.focus_list(ListFocus::Library);
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
                                        LibraryNode::Directory(path) => {
                                            let modifiers = event.modifiers();
                                            let multi = modifiers.control;
                                            if !multi && !modifiers.shift {
                                                let branch = visible
                                                    .branch
                                                    .as_ref()
                                                    .expect("directory occurrence");
                                                this.set_directory_expanded(
                                                    branch,
                                                    !this.browser.library.tree().expanded(branch),
                                                    cx,
                                                );
                                            }
                                            this.select_library_row(
                                                LibraryNode::Directory(path),
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
                }),
            )
            .with_decoration(super::paged::viewport_demand(move |range, _, cx| {
                let owner = demand_owner.clone();
                cx.defer(move |cx| {
                    let _ = owner.update(cx, |this, cx| {
                        if !this.panel_demand("library") {
                            return;
                        }
                        this.request_root_counts(cx);
                        for (path, offset) in this.browser.library.demand_directories(range) {
                            this.request_directory_page(path, offset, cx);
                        }
                        if let Some((id, index)) = this.browser.library.take_ready_manual_navigation() {
                            this.prepare_tree_row(id, index, cx);
                        }
                    });
                });
            }))
            .h_full()
            .min_h_0()
            .w_full()
            .track_scroll(scroll),
                scroll,
            ),
        );
        view.into_any_element()
    }
}

impl GuiApp {
    fn library_scan_status(&self, panel_id: u64, cx: &mut Context<Self>) -> Div {
        let theme = crate::gui::theme::palette(cx);
        use crate::model::ScanPhase;
        let progress = self.state.system.scan_progress.as_ref();
        let header_text = progress.map_or_else(
            || self.i18n.text("gl-scanning").to_owned(),
            |progress| {
                let phase = match progress.phase {
                    ScanPhase::Discovering => self.i18n.text("gl-scan-discovering"),
                    ScanPhase::ReadingMetadata => self.i18n.text("gl-scan-metadata"),
                    ScanPhase::Saving => self.i18n.text("gl-scan-saving"),
                    ScanPhase::Completed => self.i18n.text("gl-scan-completed"),
                    ScanPhase::Failed => self.i18n.text("gl-scan-failed"),
                    ScanPhase::Cancelled => self.i18n.text("gl-scan-cancelled"),
                };
                match progress.total {
                    Some(total) => self.i18n.format(
                        "gl-scan-progress",
                        &[
                            ("phase", (phase).into()),
                            ("processed", (progress.processed).into()),
                            ("total", (total).into()),
                            ("errors", (progress.errors).into()),
                        ],
                    ),
                    None => self.i18n.format(
                        "gl-scan-found",
                        &[
                            ("phase", (phase).into()),
                            ("processed", (progress.processed).into()),
                            ("errors", (progress.errors).into()),
                        ],
                    ),
                }
            },
        );
        let dismissible = !self.state.system.scanning
            && progress.is_some_and(|progress| {
                matches!(
                    progress.phase,
                    ScanPhase::Completed | ScanPhase::Failed | ScanPhase::Cancelled
                )
            });
        let dismiss = div()
            .size(gpui::rems(2.))
            .flex_shrink_0()
            .when(dismissible, |slot| {
                slot.child(quiet_icon_button(
                    theme,
                    icon_button(
                        ("dismiss-scan-result", panel_id),
                        "",
                        self.tr("gl-dismiss-scan"),
                        cx,
                        |this, _, cx| this.send(Command::DismissScanResult, cx),
                    ),
                ))
            });
        let mut bar = div()
            .w_full()
            .h(px(3.))
            .flex_shrink_0()
            .bg(rgb(theme.border));
        if let Some(progress) = progress
            && let Some(total) = progress.total.filter(|total| *total > 0)
        {
            let fraction = (progress.processed as f32 / total as f32).clamp(0., 1.);
            bar = bar.child(div().h_full().w(gpui::relative(fraction)).bg(rgb(
                if progress.phase == ScanPhase::Failed {
                    theme.error
                } else {
                    theme.accent
                },
            )));
        }
        let path: SharedString = progress
            .and_then(|progress| progress.path.as_ref())
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default()
            .into();
        let message: SharedString = self.state.system.scan_message.clone().into();
        // Keep every scan phase in the same cells; only dismissal removes the surface.
        column()
            .gap_1()
            .min_w_0()
            .flex_shrink_0()
            .child(
                row()
                    .h(gpui::rems(2.))
                    .flex_shrink_0()
                    .child(caption(theme, header_text).flex_1().min_w_0().truncate())
                    .child(dismiss),
            )
            .child(bar)
            .child(
                caption(theme, path.clone())
                    .id(("scan-path", panel_id))
                    .h(gpui::rems(1.25))
                    .flex_shrink_0()
                    .min_w_0()
                    .truncate()
                    .tooltip(move |_, cx| {
                        cx.new(|_| crate::gui::components::ButtonTooltip { text: path.clone() })
                            .into()
                    }),
            )
            .child(
                caption(theme, message.clone())
                    .id(("scan-message", panel_id))
                    .h(gpui::rems(1.25))
                    .flex_shrink_0()
                    .min_w_0()
                    .truncate()
                    .tooltip(move |_, cx| {
                        cx.new(|_| crate::gui::components::ButtonTooltip {
                            text: message.clone(),
                        })
                        .into()
                    }),
            )
    }

    pub(super) fn library_panel(
        &mut self,
        panel_id: u64,
        _tab_bar_visible: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = crate::gui::theme::palette(cx);
        self.finish_library_locate_render(window, cx);
        let total = self.browser.library.tree().root_total();
        let range_pending = self.library_range_pending();
        let selected_directories = self
            .browser
            .library
            .selected_nodes()
            .iter()
            .filter(|node| matches!(node, super::library::LibraryNode::Directory(_)))
            .count();
        let selection_label = if range_pending {
            self.i18n.text("gl-selecting").to_owned()
        } else if selected_directories > 0 && self.browser.library.selected().is_empty() {
            self.i18n.format(
                "gl-folders-selected",
                &[("count", (selected_directories).into())],
            )
        } else if selected_directories > 0 {
            self.i18n.format(
                "gl-mixed-selected",
                &[
                    ("tracks", (self.browser.library.selected().len()).into()),
                    ("folders", (selected_directories).into()),
                ],
            )
        } else if !self.browser.library.selected().is_empty() {
            self.i18n.format(
                "gl-tracks-selected",
                &[("count", (self.browser.library.selected().len()).into())],
            )
        } else {
            self.i18n
                .format("gl-entry-count", &[("count", (total).into())])
        };
        let heading = panel_header(theme, self.tr("gl-library"), selection_label.clone())
            .flex_1()
            .min_w_0()
            .id(("library-selection-count", panel_id))
            .tooltip(move |_, cx| {
                cx.new(|_| crate::gui::components::ButtonTooltip {
                    text: selection_label.clone().into(),
                })
                .into()
            });
        let tools = panel_toolbar()
            .min_w_0()
            .flex_shrink(1.)
            .child(self.library_search.clone())
            .child(disabled_control(
                theme,
                icon_button(
                    ("library-locate-playing", panel_id),
                    "",
                    self.tr("gl-locate-library"),
                    cx,
                    |this, window, cx| this.locate_playing_library(window, cx),
                ),
                self.state.current_track().is_none() || self.browser.library.locate.pending,
            ))
            .child(icon_button(
                ("library-view", panel_id),
                if self.browser.library.tree_active() {
                    "󰉋"
                } else {
                    "󰉢"
                },
                if self.browser.library.tree_active() {
                    self.tr("gl-flat-view")
                } else {
                    self.tr("gl-tree-view")
                },
                cx,
                |this, _, cx| this.toggle_library_view(cx),
            ))
            .child(active_control(
                theme,
                icon_button(
                    ("library-filter-favorites", panel_id),
                    "󰓎",
                    self.tr("gl-filter-favorites"),
                    cx,
                    |this, _, cx| {
                        this.refresh_favorite_filter(cx);
                    },
                ),
                self.browser.library.favorites_only(),
            ))
            .child(active_control(
                theme,
                icon_button(
                    ("library-filter-missing", panel_id),
                    "󰌶",
                    self.tr("gl-filter-missing"),
                    cx,
                    |this, _, cx| {
                        this.browser.library.toggle_missing();
                        this.refresh_filter(cx);
                    },
                ),
                self.browser.library.missing_only(),
            ))
            .child(active_control(
                theme,
                icon_button(
                    ("library-force-scan", panel_id),
                    "󰑐",
                    if self.browser.library.force_scan() {
                        self.tr("gl-force-on")
                    } else {
                        self.tr("gl-force-off")
                    },
                    cx,
                    |this, _, cx| {
                        this.browser.library.toggle_force_scan();
                        cx.notify();
                    },
                ),
                self.browser.library.force_scan(),
            ))
            .child(icon_button(
                ("library-add-root", panel_id),
                "󰐕",
                self.tr("gl-add-root"),
                cx,
                |this, _, cx| this.choose_library_root(cx),
            ));
        let mut panel = panel_surface(("library-panel", panel_id))
            .child(row().items_end().child(heading).child(tools));
        if let Some((track_id, result)) = &self.browser.library.locate.notice
            && self
                .state
                .current_track()
                .is_some_and(|track| track.id == *track_id)
        {
            use crate::model::LibraryLocateResult;
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
        if self.state.system.scan_progress.is_some() || self.state.system.scanning {
            panel = panel.child(self.library_scan_status(panel_id, cx));
        }
        self.scroll_views.register_list(
            ("library-projection", panel_id),
            &self.browser.library.tree().scroll,
        );
        // Track counts describe contents, not the configured root projection.
        // Keep its viewport mounted so root changes can demand a fresh forest.
        if let Some((0, total)) = self.browser.library.search_counts
            && (total != 0 || self.browser.library.tree().row_count() == 0)
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
    use super::GuiApp;
    use crate::{
        config::Config,
        core::{CoreHandle, GuiConfigChange, Runtime},
        gui::{
            layout::{Node, Panel, save_owner::LayoutSaveOwner},
            library::tree::TreeItem,
        },
        i18n::Localizer,
        model::{DirectoryRow, Query},
    };
    use gpui::{AppContext, Entity, IntoElement, VisualTestContext, point, px, size};
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

    fn draw_library(window: &mut VisualTestContext, app: &Entity<GuiApp>) {
        // The first draw demands the forest; the second renders accepted roots
        // after the independently completed zero-track count.
        for _ in 0..2 {
            app.update(&mut window.cx, |this, cx| this.refresh(cx));
            window.draw(point(px(0.), px(0.)), size(px(900.), px(600.)), |_, _| {
                app.clone().into_any_element()
            });
            window.run_until_parked();
        }
    }

    fn assert_roots(window: &mut VisualTestContext, app: &Entity<GuiApp>, roots: &[PathBuf]) {
        app.update(&mut window.cx, |this, _| {
            let tree = this.browser.library.tree();
            let rendered = (0..tree.row_count())
                .map(|index| match tree.item(index) {
                    Some(TreeItem::Entry(row)) if row.depth == 0 => match row.row {
                        DirectoryRow::Directory { path } => path,
                        DirectoryRow::Track(_) => panic!("forest contains a track"),
                    },
                    _ => panic!("configured root was not projected by viewport demand"),
                })
                .collect::<Vec<_>>();
            assert_eq!(rendered, roots);
            assert_eq!(this.browser.library.search_counts, Some((0, 0)));
            assert_eq!(this.state.library.track_total, 0);
            assert!(!this.state.system.scanning);
            assert!(this.state.system.scan_progress.is_none());
        });
    }

    #[gpui::test]
    fn empty_library_projects_first_added_appended_and_removed_roots(
        cx: &mut gpui::TestAppContext,
    ) {
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
        let runtime = Runtime::start(directory.path(), &config_path).unwrap();
        settle_config(&runtime.handle);
        let layouts = LayoutSaveOwner::new(directory.path().join("layouts.toml"));
        // Finish constructor-time layout migration before installing GUI wakers.
        let (mut initial_layouts, _) = layouts.handle().snapshot();
        let config = runtime.handle.config_snapshot();
        initial_layouts
            .current_mut()
            .initialize_audio_visualizers(config.as_ref().into());
        initial_layouts
            .current_mut()
            .initialize_album_artworks(config.as_ref().into());
        layouts.handle().submit(&initial_layouts);
        layouts.flush().unwrap();
        let artwork = Arc::new(crate::artwork::ArtworkManager::new(directory.path()));
        let window = cx.add_empty_window();
        let app = window.update(|window, cx| {
            cx.new(|cx| {
                GuiApp::new(
                    runtime.handle.clone(),
                    layouts.handle(),
                    artwork,
                    std::rc::Weak::new(),
                    Arc::new(Localizer::new("en-US")),
                    window,
                    cx,
                )
            })
        });
        // Native worker threads cannot wake GPUI's deterministic scheduler.
        // FIFO Core barriers and draw_library's refresh own test synchronization.
        runtime.handle.set_wakeup(|| {});
        layouts.handle().set_wakeup(|| {});
        app.update(&mut window.cx, |this, _| {
            this.window_visible = true;
            this.layouts.current_mut().root = Some(Node::Tabs {
                id: 1,
                active: 2,
                panels: vec![Panel {
                    id: 2,
                    kind: "library".into(),
                    show_tab_bar: false,
                    audio_visualizer: None,
                    album_artwork: None,
                }],
            });
            this.browser.library.search_counts = Some((0, 0));
        });
        draw_library(window, &app);
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
            draw_library(window, &app);
            assert_roots(window, &app, &expected);
        }
    }
}
