mod tree;

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
}

impl Render for LibraryDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let label = if self.nodes.len() == 1 {
            match &self.nodes[0] {
                LibraryNode::Directory(path) => path.to_string_lossy().into_owned(),
                LibraryNode::Track(_) => "Track".to_owned(),
            }
        } else {
            format!("{} library entries", self.nodes.len())
        };
        super::drag_preview(label)
    }
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

    fn select_library_directory(&mut self, path: Arc<Path>, multi: bool, cx: &mut Context<Self>) {
        let node = LibraryNode::Directory(path);
        self.directory_tree.cancel_navigation();
        if multi {
            if !self.library_selected_nodes.insert(node.clone()) {
                self.library_selected_nodes.remove(&node);
            }
        } else {
            self.selected.clear();
            self.library_selected_nodes.clear();
            self.library_selected_nodes.insert(node);
        }
        if self.selected.is_empty() {
            self.metadata_track = None;
            self.full_track = None;
            self.sync_metadata_default(cx);
        }
        self.sync_waveform(cx);
        self.remember_tree_selection();
        cx.notify();
    }

    fn select_tree_row(&mut self, index: usize, cx: &mut Context<Self>) {
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
        match &node {
            LibraryNode::Directory(path) => self.select_library_directory(path.clone(), false, cx),
            LibraryNode::Track(id) => self.select_track(*id, false, cx),
        }
        self.directory_tree.focus_selection(&node, index);
        self.remember_tree_selection();
    }

    fn set_directory_expanded(
        &mut self,
        branch: &TreeBranch,
        expanded: bool,
        cx: &mut Context<Self>,
    ) {
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
        let nerd_symbols = cx
            .try_global::<super::components::NerdSymbols>()
            .is_none_or(|settings| settings.0);
        let scroll = &self.directory_tree.scroll;
        let mut view = list_viewport(
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
                                    .child(div().w(gpui::rems(1.)).flex_shrink_0())
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
                                        BranchStatus::Unloaded | BranchStatus::Ready => String::new(),
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
                            let drag_nodes = if selected {
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
                                icon_button(
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
                                )
                            });
                            let disclosure = if path.is_some() && visible.has_children {
                                if visible.expanded { "▾" } else { "▸" }
                            } else {
                                ""
                            };
                            let row_view = super::tree_row(
                                gpui::ElementId::named_usize("library-node", index),
                                visible.depth,
                                selected,
                            )
                            .h(px(32.0 * ui_scale.max(0.5)))
                            .child(
                                div()
                                    .w(gpui::rems(1.))
                                    .flex_shrink_0()
                                    .text_color(gpui::rgb(super::MUTED))
                                    .child(disclosure),
                            )
                            .child(div().flex_shrink_0().text_sm().child(
                                match (path.is_some(), nerd_symbols) {
                                    (true, true) => "󰉋",
                                    (false, true) => "󰈙",
                                    (true, false) => "▸",
                                    (false, false) => "♪",
                                },
                            ))
                            .child(
                                row()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().min_w_0().truncate().child(title))
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
                            .on_drag(LibraryDrag { nodes: drag_nodes }, |drag, _, _, cx| {
                                cx.new(|_| drag.clone())
                            })
                            .on_click(cx.listener(
                                move |this, event: &gpui::ClickEvent, window, cx| {
                                    this.directory_tree.cancel_navigation();
                                    this.focus_layout(window, cx);
                                    this.list_focus = Some(ListFocus::Library);
                                    match id.clone() {
                                        LibraryNode::Track(id) => {
                                            let modifiers = event.modifiers();
                                            this.select_track(
                                                id,
                                                modifiers.control || modifiers.platform,
                                                cx,
                                            );
                                            if event.click_count() == 2 {
                                                this.send(Command::Play { track_id: id }, cx);
                                            }
                                        }
                                        LibraryNode::Directory(path) => {
                                            let modifiers = event.modifiers();
                                            let multi = modifiers.control
                                                || modifiers.platform
                                                || modifiers.shift;
                                            this.select_library_directory(path.clone(), multi, cx);
                                            if !multi {
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
                                        }
                                    }
                                    this.directory_tree.focus_selection(&id, index);
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
        if row_count == 0 {
            view = view.child(super::empty_state("No library directories or tracks"));
        }
        view.into_any_element()
    }
}
