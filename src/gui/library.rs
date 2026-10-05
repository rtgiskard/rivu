use super::{
    GuiApp, ListFocus, TRACK_HEIGHT,
    components::{TreeKey, TreeRow, tree_row},
    icon_button, row, row_text,
};
use crate::model::{Command, Track};
use gpui::{
    AnyElement, Context, Render, UniformListScrollHandle, Window, prelude::*, px, uniform_list,
};
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone)]
pub(super) struct LibraryDrag {
    pub(super) node: LibraryNode,
}

impl Render for LibraryDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let label = match &self.node {
            LibraryNode::Directory(path) => path.to_string_lossy().into_owned(),
            LibraryNode::Track(_) => "Track".to_owned(),
        };
        super::drag_preview(label)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum LibraryNode {
    Directory(Arc<Path>),
    Track(i64),
}

#[derive(Default)]
struct Directory<'a> {
    directories: BTreeMap<OsString, Directory<'a>>,
    tracks: Vec<&'a Track>,
}

/// Build one directory tree per configured library root.
pub(super) fn library_rows(
    tracks: &[Track],
    library_roots: &[PathBuf],
) -> Vec<TreeRow<LibraryNode>> {
    let mut configured = library_roots
        .iter()
        .cloned()
        .map(|path| (path, Directory::default()))
        .collect::<Vec<_>>();
    let mut fallback = Directory::default();
    for track in tracks {
        let match_root = configured
            .iter()
            .enumerate()
            .filter_map(|(index, (root, _))| {
                track
                    .path
                    .strip_prefix(root)
                    .ok()
                    .map(|relative| (index, relative))
            })
            .max_by_key(|(_, relative)| relative.components().count());
        let (directory, relative) = if let Some((index, relative)) = match_root {
            (&mut configured[index].1, relative)
        } else {
            (&mut fallback, track.path.as_path())
        };
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        let mut directory = directory;
        for component in parent.components() {
            directory = directory
                .directories
                .entry(component.as_os_str().to_os_string())
                .or_default();
        }
        directory.tracks.push(track);
    }
    let mut rows = Vec::new();
    for (path, directory) in configured {
        append_root(directory, &path, &mut rows);
    }
    append_directory(fallback, Path::new(""), None, 0, &mut rows);
    rows
}

fn append_root(directory: Directory<'_>, path: &Path, rows: &mut Vec<TreeRow<LibraryNode>>) {
    let id = LibraryNode::Directory(Arc::from(path));
    let has_children = !directory.directories.is_empty() || !directory.tracks.is_empty();
    let label = path.file_name().map_or_else(
        || path.to_string_lossy().into_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    rows.push(TreeRow::new(id.clone(), None, 0, label, has_children));
    append_directory(directory, path, Some(id), 1, rows);
}

fn append_directory(
    directory: Directory<'_>,
    path: &Path,
    parent: Option<LibraryNode>,
    depth: usize,
    rows: &mut Vec<TreeRow<LibraryNode>>,
) {
    for (name, child) in directory.directories {
        let path = path.join(&name);
        let id = LibraryNode::Directory(Arc::from(path.as_path()));
        rows.push(TreeRow::new(
            id.clone(),
            parent.clone(),
            depth,
            name.to_string_lossy(),
            true,
        ));
        append_directory(child, &path, Some(id), depth + 1, rows);
    }
    let mut tracks = directory.tracks;
    tracks
        .sort_unstable_by_key(|track| (track.path.file_name().map(OsStr::to_os_string), track.id));
    rows.extend(tracks.into_iter().map(|track| {
        TreeRow::new(
            LibraryNode::Track(track.id),
            parent.clone(),
            depth,
            &track.title,
            false,
        )
    }));
}

impl GuiApp {
    pub(super) fn library_drag_track_ids(&self, node: &LibraryNode) -> Vec<i64> {
        self.state
            .library
            .tracks
            .iter()
            .filter_map(|track| match node {
                LibraryNode::Directory(path) if path.as_os_str().is_empty() => (!self
                    .state
                    .system
                    .config
                    .library_roots
                    .iter()
                    .any(|root| track.path.starts_with(root)))
                .then_some(track.id),
                LibraryNode::Directory(path) => {
                    track.path.starts_with(path.as_ref()).then_some(track.id)
                }
                LibraryNode::Track(id) => (track.id == *id).then_some(track.id),
            })
            .collect()
    }

    pub(super) fn rebuild_library_tree(&mut self) {
        self.library_tree.set_rows(library_rows(
            &self.state.library.tracks,
            &self.state.system.config.library_roots,
        ));
        self.library_tree_scroll = UniformListScrollHandle::new();
        if let Some(index) = self.library_tree.selected_index() {
            self.library_tree_scroll
                .scroll_to_item(index, gpui::ScrollStrategy::Nearest);
        }
    }

    pub(super) fn navigate_library_tree(&mut self, key: TreeKey, cx: &mut Context<Self>) {
        self.library_tree.handle_key(key);
        let selected = self.library_tree.selected().map(|row| row.id.clone());
        if let Some(index) = self.library_tree.selected_index() {
            self.library_tree_scroll
                .scroll_to_item(index, gpui::ScrollStrategy::Nearest);
        }
        match selected {
            Some(LibraryNode::Track(id)) => self.select_track(id, false, cx),
            _ => {
                self.selected.clear();
                self.library_selection.clear_selection();
                self.metadata_track = None;
                self.sync_waveform(cx);
                cx.notify();
            }
        }
    }

    pub(super) fn library_tree_list(&self, panel_id: u64, cx: &mut Context<Self>) -> AnyElement {
        uniform_list(
            ("library-tree", panel_id),
            self.library_tree.visible_indices().len(),
            cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                let focused = this.library_tree.selected().map(|row| row.id.clone());
                range
                    .filter_map(|index| {
                        let source_index = *this.library_tree.visible_indices().get(index)?;
                        let node = this.library_tree.rows().get(source_index)?;
                        let id = node.id.clone();
                        let selected = match &id {
                            LibraryNode::Track(id) => this.selected.contains(id),
                            _ => focused.as_ref() == Some(&id),
                        };
                        let (title, detail) = match &id {
                            LibraryNode::Track(id) => this.panel_track_text(*id),
                            _ => (node.label.clone(), String::new()),
                        };
                        let scan_path = match &id {
                            LibraryNode::Directory(path) => Some(path.to_path_buf()),
                            LibraryNode::Track(track_id) => this
                                .library_index
                                .get(track_id)
                                .and_then(|index| this.state.library.tracks.get(*index))
                                .map(|track| track.path.clone()),
                        };
                        let scan = scan_path.map(|path| {
                            icon_button(
                                ("library-scan", source_index),
                                "↻",
                                "Scan this path",
                                cx,
                                move |this, _, cx| {
                                    this.send(
                                        Command::Scan {
                                            paths: vec![path.clone()],
                                            force: this.force_scan,
                                        },
                                        cx,
                                    );
                                },
                            )
                            .opacity(0.35)
                            .hover(|style| style.opacity(1.0))
                        });
                        let content = row()
                            .flex_1()
                            .min_w_0()
                            .child(row_text(("library-node-text", source_index), title, detail))
                            .when_some(scan, |view, scan| view.child(scan));
                        Some(
                            tree_row(
                                ("library-node", source_index),
                                node.depth,
                                selected,
                                this.library_tree.expanded().contains(&id),
                                node.has_children,
                            )
                            .h(px(TRACK_HEIGHT))
                            .min_w_0()
                            .overflow_hidden()
                            .child(content)
                            .on_drag(LibraryDrag { node: id.clone() }, |drag, _, _, cx| {
                                cx.new(|_| drag.clone())
                            })
                            .on_click(cx.listener(
                                move |this, event: &gpui::ClickEvent, window, cx| {
                                    this.focus_workspace(window, cx);
                                    this.list_focus = Some(ListFocus::Library);
                                    this.library_tree.select(&id);
                                    match &id {
                                        LibraryNode::Track(track_id) => {
                                            let modifiers = event.modifiers();
                                            this.select_track(
                                                *track_id,
                                                modifiers.control || modifiers.platform,
                                                cx,
                                            );
                                            if event.click_count() == 2 {
                                                this.send(
                                                    Command::Play {
                                                        track_id: *track_id,
                                                    },
                                                    cx,
                                                );
                                            }
                                        }
                                        LibraryNode::Directory(_) => {
                                            if event.click_count() == 1 {
                                                this.navigate_library_tree(TreeKey::Toggle, cx);
                                            }
                                        }
                                    }
                                },
                            )),
                        )
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.library_tree_scroll)
        .flex_1()
        .min_h_0()
        .w_full()
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: i64, path: &str) -> Track {
        Track {
            id,
            path: path.into(),
            fingerprint: None,
            cue: None,
            title: format!("Track {id}"),
            artist: String::new(),
            album: String::new(),
            duration: None,
            codec: String::new(),
            channels: 2,
            sample_rate: 44100,
            bitrate_bps: None,
            track_number: None,
            disc_number: None,
            bits_per_sample: None,
            release_date: None,
            favorite: false,
            missing: false,
            play_count: 0,
            last_played: None,
        }
    }

    #[test]
    fn directories_precede_files_and_order_is_independent_of_library_order() {
        let mut tracks = vec![
            track(3, "Music/z.flac"),
            track(2, "Music/Album/b.flac"),
            track(4, "Music/a.flac"),
            track(1, "Music/Album/a.flac"),
        ];
        let rows = library_rows(&tracks, &[]);
        tracks.reverse();
        assert_eq!(rows, library_rows(&tracks, &[]));
        assert_eq!(rows.len(), 6);
        assert_eq!(
            rows[0].id,
            LibraryNode::Directory(Arc::from(Path::new("Music")))
        );
        assert_eq!(rows[1].parent.as_ref(), Some(&rows[0].id));
        assert_eq!(rows[1].depth, 1);
        assert!(rows[1].has_children);
        assert_eq!(rows[2].parent.as_ref(), Some(&rows[1].id));
        assert_eq!(rows[2].depth, 2);
        let ids = rows
            .iter()
            .filter_map(|row| match row.id {
                LibraryNode::Track(id) => Some(id),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(ids, vec![1, 2, 4, 3]);
    }

    #[test]
    fn rootless_tracks_and_shared_file_paths_keep_distinct_ids() {
        let rows = library_rows(&[track(2, "track.flac"), track(1, "track.flac")], &[]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, LibraryNode::Track(1));
        assert_eq!(rows[1].id, LibraryNode::Track(2));
        assert!(
            rows.iter()
                .all(|row| row.parent.is_none() && row.depth == 0)
        );
        assert!(library_rows(&[], &[]).is_empty());
    }
}
