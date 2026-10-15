pub(super) mod tree;

use super::{
    GuiApp, ListFocus, browser::selection::LibraryRowRequest, icon_button, list_viewport, row,
};
use crate::{
    config::{LibraryColumn, LibraryColumns},
    model::{Command, DirectoryRow, LibraryRow},
};
use gpui::{AnyElement, Context, Render, Window, div, prelude::*, px, uniform_list};
use std::{path::Path, sync::Arc};

use tree::TreeItem;
pub(super) use tree::{BranchStatus, DirectoryTree, TreeBranch};

pub(super) fn track_detail(track: &crate::model::LibraryRow) -> String {
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
    detail.push_str(if track.codec.is_empty() {
        "-"
    } else {
        &track.codec
    });
    detail.push_str(" · ");
    match track.bitrate_bps {
        Some(value) => {
            use std::fmt::Write;
            let _ = write!(
                detail,
                "{}{:.1} kbps",
                if track.bitrate_exact { "" } else { "≈" },
                value as f64 / 1000.0
            );
        }
        None => detail.push('-'),
    }
    if track.missing {
        detail.push_str(" · File missing");
    }
    detail
}

fn track_columns(track: &LibraryRow, columns: LibraryColumns) -> Option<gpui::Div> {
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
                    track.codec.clone().into()
                }
            }
            LibraryColumn::Bitrate => track.bitrate_bps.map_or_else(
                || "-".into(),
                |value| {
                    format!(
                        "{}{:.1} kbps",
                        if track.bitrate_exact { "" } else { "≈" },
                        value as f64 / 1000.0
                    )
                    .into()
                },
            ),
        };
        let cell = super::caption(value).truncate();
        cells = cells.child(match column {
            LibraryColumn::Artist | LibraryColumn::Album => {
                cell.max_w(gpui::rems(6.0)).flex_initial().min_w_0()
            }
            LibraryColumn::Duration | LibraryColumn::Codec | LibraryColumn::Bitrate => {
                cell.flex_shrink_0()
            }
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
    fn new(nodes: &[LibraryNode], title: gpui::SharedString) -> Self {
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
            ("󰉋", format!("{} folders", nodes.len()).into())
        } else if directories == 0 {
            ("󰈙", format!("{} tracks", nodes.len()).into())
        } else {
            ("󰉓", format!("{} items", nodes.len()).into())
        };
        Self { label, icon }
    }
}

impl Render for LibraryDragPreview {
    fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        // Window coordinates avoid the source row's grab offset; GPUI fits actual bounds.
        gpui::anchored()
            .position(window.mouse_position() + gpui::point(px(10.), px(10.)))
            .child(
                row()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .max_w(px(260.))
                    .bg(gpui::rgb(super::PANEL))
                    .border_1()
                    .border_color(gpui::rgb(super::ACCENT))
                    .rounded_md()
                    .text_color(gpui::rgb(super::TEXT))
                    .child(
                        div()
                            .flex_shrink_0()
                            .font_family(super::fonts::SYMBOLS_FAMILY)
                            .child(self.icon),
                    )
                    .child(div().min_w_0().truncate().child(self.label.clone())),
            )
    }
}

fn tree_guides(tree: &DirectoryTree, index: usize) -> gpui::Div {
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
                    .bg(gpui::rgb(super::MUTED).alpha(0.35)),
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
                    .bg(gpui::rgb(super::MUTED).alpha(0.35)),
            );
        }
    });
    guides
}

impl GuiApp {
    pub(super) fn library_drag_track_ids(&self, nodes: &[LibraryNode]) -> Vec<i64> {
        let mut ids = nodes
            .iter()
            .filter_map(|node| match node {
                LibraryNode::Track(id) => Some(*id),
                LibraryNode::Directory(_) => None,
            })
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    pub(super) fn resolve_library_drag(
        &mut self,
        nodes: &[LibraryNode],
        destination: super::DragDestination,
        cx: &mut Context<Self>,
    ) {
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
        if self.library_range_pending() {
            return;
        }
        if directories.is_empty() && track_ids.is_empty() {
            return;
        }
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
            "space" | " " => {
                if let Some(branch) = &visible.branch {
                    self.set_directory_expanded(
                        branch,
                        !self.browser.library.tree().expanded(branch),
                        cx,
                    );
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
        let row_count = self.browser.library.tree().row_count();
        let scroll = &self.browser.library.tree().scroll;
        let view = list_viewport(
            uniform_list(
                ("library-directory-tree", panel_id),
                row_count,
                cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
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
                                    let placeholder = super::tree_row(
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
                                            .child(tree_guides(this.browser.library.tree(), index))
                                            .child(
                                                row()
                                                    .h_full()
                                                    .pl(gpui::rems(depth as f32 * 1.25))
                                                    .child(
                                                        super::caption(if failed { "Unavailable" } else { "Loading…" })
                                                            .min_w_0()
                                                            .truncate(),
                                                    )
                                                    .when(failed, |view| {
                                                        view.child(
                                                            super::components::quiet_icon_button(icon_button(
                                                                gpui::ElementId::named_usize("library-retry", index),
                                                                "󰑐",
                                                                "Retry loading folder",
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
                                        BranchStatus::Loading => "Loading…".to_owned(),
                                        BranchStatus::Empty => "Empty".to_owned(),
                                        BranchStatus::Error => "Unavailable".to_owned(),
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
                            let scan = path.clone().filter(|_| selected).map(|scan_path| {
                                let hint = format!("Rescan {}", scan_path.display());
                                super::components::quiet_icon_button(icon_button(
                                    gpui::ElementId::named_usize("library-scan", index),
                                    "󰑐",
                                    hint,
                                    cx,
                                    move |this, _, cx| {
                                        this.send(
                                            Command::Scan {
                                                paths: vec![scan_path.clone()],
                                                force: this.browser.library.force_scan(),
                                            },
                                            cx,
                                        );
                                    },
                                ))
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
                                            "󰈙"
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
                                                .text_overflow(gpui::TextOverflow::Truncate("..".into()))
                                                .child(title.clone())
                                                .when(path.is_none(), |view| view.flex_1()),
                                        )
                                        .when(path.is_some() && !detail.is_empty(), |view| {
                                            view.child(super::caption(detail.clone()).min_w_0().truncate()
                                                .text_overflow(gpui::TextOverflow::Truncate("..".into())))
                                        })
                                        .when(matches!(entry, DirectoryRow::Track(track) if track.missing), |view| {
                                            view.child(
                                                div()
                                                    .flex_shrink_0()
                                                    .text_sm()
                                                    .font_family(super::fonts::SYMBOLS_FAMILY)
                                                    .text_color(gpui::rgb(super::ERROR))
                                                    .child("󰌶"),
                                            )
                                        }),
                                )
                                .when(visible.depth == 0 && path.is_some(), |view| {
                                    let count = this.browser.library.root_count(path.as_deref().expect("root path"));
                                    view.child(
                                        super::caption(count.map_or_else(|| "—".to_owned(), |total| total.to_string()))
                                            .flex_shrink_0(),
                                    )
                                    .when(this.browser.library.root_counts_failed(), |view| {
                                        view.child(
                                            super::components::quiet_icon_button(icon_button(
                                                gpui::ElementId::named_usize("library-count-retry", index),
                                                "󰑐",
                                                "Retry scanned track counts",
                                                cx,
                                                |this, _, cx| this.request_root_counts(cx),
                                            ))
                                            .min_h_0()
                                            .size(gpui::rems(1.5)),
                                        )
                                    })
                                })
                                .when_some(scan, |view, scan| {
                                    view.child(scan.min_h_0().size(gpui::rems(1.5)))
                                });
                            let name_cell = div()
                                .w(gpui::rems(14.0))
                                .flex_auto()
                                .min_w_0()
                                .h_full()
                                .relative()
                                .overflow_hidden()
                                .child(tree_guides(this.browser.library.tree(), index))
                                .child(name_content);
                            let row_view = super::components::lazy_row_tooltip(
                                super::tree_row(
                                    gpui::ElementId::named_usize("library-node", index),
                                    0,
                                    selected,
                                )
                                .gap_0()
                                .overflow_hidden()
                                .group("library-track-row")
                                .child(name_cell)
                                .when_some(
                                    match entry {
                                        DirectoryRow::Track(track) => track_columns(track, this.state.system.config.library_columns),
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
                                            this.track_favorite_toggle(
                                                gpui::ElementId::named_usize("library-tree-favorite", index),
                                                "library-track-row",
                                                track_id,
                                                favorite,
                                                cx,
                                            )
                                            .flex_shrink_0()
                                            .min_h_0()
                                            .size(gpui::rems(1.5)),
                                        )
                                    },
                                ),
                                tooltip_title,
                                {
                                    let entry = visible.row;
                                    move || match &entry {
                                        DirectoryRow::Track(track) => track_detail(track).into(),
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
                                    move |drag, _, _, cx| {
                                        app.update(cx, |this, cx| {
                                            let nodes = drag.nodes.get_or_init(|| {
                                                this.browser.library.drag_nodes(&drag.source)
                                            });
                                            cx.new(|_| {
                                                LibraryDragPreview::new(nodes, title.clone())
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
                                                modifiers.control || modifiers.platform,
                                                cx,
                                            );
                                            if event.click_count() == 2 {
                                                this.send(Command::Play { track_id: id }, cx);
                                            }
                                        }
                                        LibraryNode::Directory(path) => {
                                            let modifiers = event.modifiers();
                                            let multi = modifiers.control || modifiers.platform;
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
            .with_decoration(super::paged::viewport_demand(cx.processor(
                |this, range: std::ops::Range<usize>, _, cx| {
                    for (path, offset) in this.browser.library.demand_directories(range) {
                        this.request_directory_page(path, offset, cx);
                    }
                    if let Some((id, index)) = this.browser.library.take_ready_manual_navigation() {
                        this.prepare_tree_row(id, index, cx);
                    }
                },
            )))
            .h_full()
            .min_h_0()
            .w_full()
            .track_scroll(scroll),
        );
        view.into_any_element()
    }
}
