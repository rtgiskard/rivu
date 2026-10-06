mod tree;

use super::{GuiApp, ListFocus, icon_button, list_viewport, row};
use crate::model::{Command, DirectoryRow};
use gpui::{AnyElement, Context, Render, Window, div, prelude::*, px, uniform_list};
use std::{path::Path, sync::Arc};

pub(super) use tree::{BranchStatus, DirectoryTree};

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

    fn selected_tree_index(&self) -> Option<usize> {
        self.directory_tree
            .visible_rows()
            .iter()
            .position(|visible| self.directory_row_selected(&visible.row))
    }

    fn select_library_directory(&mut self, path: Arc<Path>, multi: bool, cx: &mut Context<Self>) {
        let node = LibraryNode::Directory(path);
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
        cx.notify();
    }

    fn select_tree_row(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(visible) = self.directory_tree.visible_rows().get(index) else {
            return;
        };
        match visible.row.clone() {
            DirectoryRow::Directory { path } => {
                self.select_library_directory(Arc::from(path), false, cx)
            }
            DirectoryRow::Track(track) => self.select_track(track.id, false, cx),
        }
        self.directory_tree
            .scroll
            .scroll_to_item(index, gpui::ScrollStrategy::Nearest);
    }

    fn set_directory_expanded(&mut self, path: &Path, expanded: bool, cx: &mut Context<Self>) {
        if expanded {
            self.directory_tree.expand(path);
            if matches!(
                self.directory_tree.status(path),
                BranchStatus::Unloaded | BranchStatus::Error
            ) {
                self.request_directory_page(path.to_path_buf(), 0, cx);
            }
        } else {
            self.directory_tree.collapse(path);
            self.directory_tree
                .retain_known_directories(&mut self.library_selected_nodes);
        }
        cx.notify();
    }

    pub(super) fn navigate_library_tree(&mut self, down: bool, cx: &mut Context<Self>) {
        let count = self.directory_tree.visible_rows().len();
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
        match self.directory_tree.visible_rows()[index].row.clone() {
            DirectoryRow::Directory { path } => {
                self.set_directory_expanded(&path, !self.directory_tree.expanded(&path), cx);
            }
            DirectoryRow::Track(track) => self.send(Command::Play { track_id: track.id }, cx),
        }
    }

    pub(super) fn library_tree_key(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(index) = self.selected_tree_index() else {
            return;
        };
        let rows = self.directory_tree.visible_rows();
        let visible = &rows[index];
        let depth = visible.depth;
        match key {
            "left" => {
                if let DirectoryRow::Directory { path } = &visible.row
                    && visible.expanded
                {
                    let path = path.clone();
                    self.set_directory_expanded(&path, false, cx);
                } else if let Some(parent) = rows[..index].iter().rposition(|row| row.depth < depth)
                {
                    self.select_tree_row(parent, cx);
                }
            }
            "right" => {
                if let DirectoryRow::Directory { path } = &visible.row {
                    if !visible.expanded {
                        let path = path.clone();
                        self.set_directory_expanded(&path, true, cx);
                    } else if rows.get(index + 1).is_some_and(|row| row.depth > depth) {
                        self.select_tree_row(index + 1, cx);
                    }
                }
            }
            "space" | " " => {
                if let DirectoryRow::Directory { path } = &visible.row {
                    let path = path.clone();
                    self.set_directory_expanded(&path, !self.directory_tree.expanded(&path), cx);
                }
            }
            _ => {}
        }
    }

    /// Render the visible flattened tree. Directory pages remain in the tree
    /// cache, so expanding a child never replaces its ancestors.
    pub(super) fn library_directory_list(
        &self,
        panel_id: u64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rows = self.directory_tree.visible_rows();
        let row_count = rows.len();
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
                    let rows = this.directory_tree.visible_rows();
                    let page_size = this.view_page_size();
                    range
                        .filter_map(|index| {
                            let visible = rows.get(index)?;
                            let entry = &visible.row;
                            let (node, title, detail, path) = match entry {
                                DirectoryRow::Directory { path } => {
                                    let title = path.file_name().map_or_else(
                                        || path.display().to_string(),
                                        |name| name.to_string_lossy().into_owned(),
                                    );
                                    let detail = match visible.status {
                                        BranchStatus::Loading => "Loading…".to_owned(),
                                        BranchStatus::Empty => "Empty".to_owned(),
                                        BranchStatus::Error => {
                                            "Unavailable — click to retry".to_owned()
                                        }
                                        BranchStatus::Unloaded => "Folder".to_owned(),
                                        BranchStatus::Ready => String::new(),
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
                            let row_path = path.clone();
                            let disclosure_path = path.clone();
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
                            let page_controls = row_path.clone().map(|path| {
                                let mut controls = row().gap_0().flex_shrink_0();
                                if visible.offset > 0 {
                                    let previous = visible.offset.saturating_sub(page_size);
                                    let previous_path = path.clone();
                                    controls = controls.child(icon_button(
                                        gpui::ElementId::named_usize("library-page-prev", index),
                                        "󰁍",
                                        "Browse earlier items",
                                        cx,
                                        move |this, _, cx| {
                                            this.request_directory_page(
                                                previous_path.clone(),
                                                previous,
                                                cx,
                                            );
                                        },
                                    ));
                                }
                                if visible.offset + page_size < visible.total {
                                    let next = visible.offset.saturating_add(page_size);
                                    let next_path = path;
                                    controls = controls.child(icon_button(
                                        gpui::ElementId::named_usize("library-page-next", index),
                                        "󰁔",
                                        "Browse more items",
                                        cx,
                                        move |this, _, cx| {
                                            this.request_directory_page(
                                                next_path.clone(),
                                                next,
                                                cx,
                                            );
                                        },
                                    ));
                                }
                                controls
                            });
                            let disclosure = if path.is_some() && visible.has_children {
                                let icon = if visible.expanded {
                                    "\u{f0140}"
                                } else {
                                    "\u{f0142}"
                                };
                                let disclosure_path =
                                    disclosure_path.clone().expect("path checked");
                                Some(icon_button(
                                    gpui::ElementId::named_usize("library-disclosure", index),
                                    icon,
                                    if visible.expanded {
                                        "Collapse folder"
                                    } else {
                                        "Expand folder"
                                    },
                                    cx,
                                    move |this, _, cx| {
                                        this.set_directory_expanded(
                                            &disclosure_path,
                                            !this.directory_tree.expanded(&disclosure_path),
                                            cx,
                                        );
                                    },
                                ))
                            } else {
                                None
                            };
                            let row_view = super::tree_row(
                                gpui::ElementId::named_usize("library-node", index),
                                visible.depth,
                                selected,
                                false,
                                false,
                            )
                            .h(px(32.0 * ui_scale.max(0.5)))
                            .child(
                                disclosure
                                    .map(IntoElement::into_any_element)
                                    .unwrap_or_else(|| div().w(gpui::px(14.)).into_any_element()),
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
                            .when_some(page_controls, |v, controls| v.child(controls))
                            .when_some(scan, |v, scan| v.child(scan))
                            .on_drag(LibraryDrag { nodes: drag_nodes }, |drag, _, _, cx| {
                                cx.new(|_| drag.clone())
                            })
                            .on_click(cx.listener(
                                move |this, event: &gpui::ClickEvent, window, cx| {
                                    this.focus_workspace(window, cx);
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
                                                this.set_directory_expanded(
                                                    path.as_ref(),
                                                    !this.directory_tree.expanded(path.as_ref()),
                                                    cx,
                                                );
                                            }
                                        }
                                    }
                                },
                            ));
                            Some(row_view)
                        })
                        .collect::<Vec<_>>()
                }),
            )
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
