mod selection;
mod tree;

pub(super) use selection::LibrarySelection;

use super::{GuiApp, ListFocus, icon_button, list_viewport, row};
use crate::model::{Command, DirectoryRow};
use gpui::{AnyElement, Context, Render, Window, div, prelude::*, px, uniform_list};
use std::{path::Path, sync::Arc};

use tree::TreeItem;
pub(super) use tree::{BranchStatus, DirectoryTree, TreeBranch};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum LibraryNode {
    Directory(Arc<Path>),
    Track(i64),
}

#[derive(Clone)]
pub(super) struct LibraryDrag {
    pub(super) nodes: Vec<LibraryNode>,
    label: gpui::SharedString,
    icon: &'static str,
}

impl LibraryDrag {
    fn new(nodes: Vec<LibraryNode>, title: gpui::SharedString) -> Self {
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
        Self { nodes, label, icon }
    }
}

impl Render for LibraryDrag {
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
        let left = gpui::rems(1.0 + column as f32 * 1.25);
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

    fn remember_tree_selection(&mut self) {
        self.directory_tree
            .remember_selection(&self.library_selected_nodes, &self.selected);
    }

    fn selected_tree_index(&self) -> Option<usize> {
        self.directory_tree.selected_index()
    }

    fn select_tree_row(&mut self, index: usize, cx: &mut Context<Self>) {
        self.invalidate_library_selection();
        self.directory_tree
            .scroll
            .scroll_to_item(index, gpui::ScrollStrategy::Nearest);
        let Some(TreeItem::Entry(visible)) = self.directory_tree.item(index) else {
            self.directory_tree.defer_selection(index);
            for (branch, offset) in self.directory_tree.requests_for_range(index..index + 1) {
                self.request_directory_page(branch, offset, cx);
            }
            cx.notify();
            return;
        };
        self.directory_tree.cancel_navigation();
        let node = match visible.row {
            DirectoryRow::Directory { path } => LibraryNode::Directory(Arc::from(path)),
            DirectoryRow::Track(track) => LibraryNode::Track(track.id),
        };
        self.select_library_row(node, index, false, false, cx);
    }

    fn set_directory_expanded(
        &mut self,
        branch: &TreeBranch,
        expanded: bool,
        cx: &mut Context<Self>,
    ) {
        self.invalidate_library_selection();
        if expanded {
            self.directory_tree.expand(branch);
            if matches!(
                self.directory_tree.status(branch),
                BranchStatus::Unloaded | BranchStatus::Error
            ) {
                self.request_directory_page(branch.clone(), 0, cx);
            }
        } else {
            self.directory_tree.collapse(branch);
        }
        cx.notify();
    }

    pub(super) fn navigate_library_tree(&mut self, down: bool, cx: &mut Context<Self>) {
        let count = self.directory_tree.row_count();
        if count == 0 {
            return;
        }
        let index = match self.selected_tree_index() {
            Some(index) if down => (index + 1).min(count - 1),
            Some(index) => index.saturating_sub(1),
            None if down => 0,
            None => count - 1,
        };
        self.select_tree_row(index, cx);
    }

    pub(super) fn activate_library_tree(&mut self, cx: &mut Context<Self>) {
        if self.library_range_pending() {
            return;
        }
        let Some(index) = self.selected_tree_index() else {
            return;
        };
        let Some(TreeItem::Entry(visible)) = self.directory_tree.item(index) else {
            return;
        };
        match visible.row {
            DirectoryRow::Directory { .. } => {
                let branch = visible.branch.expect("directory has an occurrence");
                self.set_directory_expanded(&branch, !self.directory_tree.expanded(&branch), cx);
            }
            DirectoryRow::Track(track) => self.send(Command::Play { track_id: track.id }, cx),
        }
    }

    pub(super) fn library_tree_key(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(index) = self.selected_tree_index() else {
            return;
        };
        let Some(TreeItem::Entry(visible)) = self.directory_tree.item(index) else {
            return;
        };
        let depth = visible.depth;
        match key {
            "left" => {
                if visible.expanded
                    && let Some(branch) = &visible.branch
                {
                    self.set_directory_expanded(branch, false, cx);
                } else if let Some(parent) = self.directory_tree.parent_index(index) {
                    self.select_tree_row(parent, cx);
                }
            }
            "right" => {
                if let Some(branch) = &visible.branch {
                    if !visible.expanded {
                        self.set_directory_expanded(branch, true, cx);
                    } else if matches!(&self.directory_tree.item(index + 1),
                        Some(TreeItem::Entry(next)) if next.depth > depth)
                        || matches!(&self.directory_tree.item(index + 1),
                            Some(TreeItem::Placeholder { depth: next_depth, .. }) if *next_depth > depth)
                    {
                        self.select_tree_row(index + 1, cx);
                    }
                }
            }
            "space" | " " => {
                if let Some(branch) = &visible.branch {
                    self.set_directory_expanded(branch, !self.directory_tree.expanded(branch), cx);
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
        let row_count = self.directory_tree.row_count();
        let ui_scale = self.state.system.config.ui_scale;
        let scroll = &self.directory_tree.scroll;
        let view = list_viewport(
            uniform_list(
                ("library-directory-tree", panel_id),
                row_count,
                cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                    range
                        .filter_map(|index| {
                            let item = this.directory_tree.item(index)?;
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
                                        depth,
                                        false,
                                    )
                                    .h(px(32.0 * ui_scale.max(0.5)))
                                    .relative()
                                    .child(tree_guides(&this.directory_tree, index))
                                    .child(
                                        div().text_color(gpui::rgb(super::MUTED)).child(
                                            if failed { "Unavailable" } else { "Loading…" },
                                        ),
                                    )
                                    .when(failed, |view| {
                                        view.child(icon_button(
                                            gpui::ElementId::named_usize("library-retry", index),
                                            "󰑐",
                                            "Retry loading folder",
                                            cx,
                                            move |this, _, cx| {
                                                this.directory_tree.retry(&branch);
                                                this.request_directory_page(
                                                    branch.clone(),
                                                    offset,
                                                    cx,
                                                );
                                            },
                                        ))
                                    });
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
                                    format!(
                                        "{}{}{} · {}{}",
                                        track.artist,
                                        if track.artist.is_empty() || track.album.is_empty() {
                                            ""
                                        } else {
                                            " / "
                                        },
                                        track.album,
                                        super::format_time(track.duration.unwrap_or(0.)),
                                        if track.missing {
                                            " · File missing"
                                        } else {
                                            ""
                                        },
                                    ),
                                    None,
                                ),
                            };
                            let selected = this.directory_row_selected(entry);
                            let id = node.clone();
                            let drag_nodes = if selected && !this.library_range_pending() {
                                let mut selected_nodes = this.library_selected_nodes.clone();
                                selected_nodes
                                    .extend(this.selected.iter().copied().map(LibraryNode::Track));
                                if selected_nodes.is_empty() {
                                    vec![id.clone()]
                                } else {
                                    selected_nodes.into_iter().collect()
                                }
                            } else {
                                vec![id.clone()]
                            };
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
                                                force: this.force_scan,
                                            },
                                            cx,
                                        );
                                    },
                                ))
                            });
                            let drag = LibraryDrag {
                                nodes: drag_nodes,
                                label: title.into(),
                                icon: "",
                            };
                            let row_view = super::tree_row(
                                gpui::ElementId::named_usize("library-node", index),
                                visible.depth,
                                selected,
                            )
                            .h(px(32.0 * ui_scale.max(0.5)))
                            .relative()
                            .child(tree_guides(&this.directory_tree, index))
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_sm()
                                    .font_family(super::fonts::SYMBOLS_FAMILY)
                                    .child(if let Some(branch) = &visible.branch {
                                        if this.directory_tree.expanded(branch) {
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
                                    .child(div().min_w_0().truncate().child(drag.label.clone()))
                                    .when(!detail.is_empty(), |v| {
                                        v.child(
                                            div()
                                                .min_w_0()
                                                .text_xs()
                                                .text_color(gpui::rgb(super::MUTED))
                                                .truncate()
                                                .child(detail),
                                        )
                                    }),
                            )
                            .when_some(scan, |v, scan| v.child(scan))
                            .when_some(
                                match entry {
                                    DirectoryRow::Track(track) => Some((track.id, track.favorite)),
                                    DirectoryRow::Directory { .. } => None,
                                },
                                |view, (track_id, favorite)| {
                                    view.group("library-track-row").child(
                                        this.library_favorite_toggle(
                                            gpui::ElementId::named_usize(
                                                "library-tree-favorite",
                                                index,
                                            ),
                                            track_id,
                                            favorite,
                                            cx,
                                        ),
                                    )
                                },
                            )
                            .when(!this.library_range_pending(), |view| {
                                view.on_drag(drag, |drag, _, _, cx| {
                                    cx.new(|_| {
                                        LibraryDrag::new(drag.nodes.clone(), drag.label.clone())
                                    })
                                })
                            })
                            .on_click(cx.listener(
                                move |this, event: &gpui::ClickEvent, window, cx| {
                                    this.directory_tree.cancel_navigation();
                                    this.focus_layout(window, cx);
                                    this.list_focus = Some(ListFocus::Library);
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
                                                    !this.directory_tree.expanded(branch),
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
                                    this.remember_tree_selection();
                                },
                            ));
                            Some(row_view.into_any_element())
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .with_decoration(super::paged::viewport_demand(cx.processor(
                |this, range: std::ops::Range<usize>, _, cx| {
                    this.remember_tree_selection();
                    for (path, offset) in this.directory_tree.requests_for_range(range) {
                        this.request_directory_page(path, offset, cx);
                    }
                    if let Some(index) = this.directory_tree.ready_selection() {
                        this.select_tree_row(index, cx);
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
