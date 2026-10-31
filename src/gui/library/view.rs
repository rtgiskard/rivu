//! Library tree and panel rendering; feature lifecycle remains in the parent.
use super::*;
use crate::config::{LibraryColumn, LibraryColumns};
use crate::gui::components::track::{AudioSpecs, audio_specs, track_title};
use crate::gui::list_interaction::LibraryDrag;
use gpui::{AnyElement, Div, Render, div, px, rgb};
use rivu_protocol::model::LibraryRow;
use rivu_ui::{
    IconButton, IconButtonDensity, IconButtonStyle, VirtualList, button, caption, lazy_row_tooltip,
    list_panel_header, list_viewport, panel_surface, row,
};

pub(in crate::gui) fn track_detail(
    i18n: &crate::i18n::Localizer,
    track: &rivu_protocol::model::LibraryRow,
) -> String {
    let mut detail = track
        .duration
        .map_or_else(|| "—".to_owned(), crate::gui::format_time);
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
                .map_or_else(|| "-".into(), |value| crate::gui::format_time(value).into()),
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
                .font_features(crate::gui::appearance::fonts::tabular_numbers())
                .flex_shrink_0(),
            LibraryColumn::Codec => cell.flex_shrink_0(),
        });
    }
    Some(cells)
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
                        .font_family(crate::gui::appearance::fonts::SYMBOLS_FAMILY)
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
    /// Render logical preorder slots, loading only the viewport's missing
    /// payloads. Expanding a branch never replaces its ancestors or siblings.
    pub(in crate::gui) fn library_directory_list(
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
                                    row_height,
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
                                        .child(tree_guides(theme, this.library.tree(), index))
                                        .child(
                                            row()
                                                .h_full()
                                                .pl(gpui::rems(depth as f32 * 1.25))
                                                .child(
                                                    rivu_ui::caption(theme, this.tr(if failed { "gl-unavailable" } else { "shell-loading" }))
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
                                    BranchStatus::Loading => this.i18n.text("shell-loading").to_owned(),
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
                                    .font_family(crate::gui::appearance::fonts::SYMBOLS_FAMILY)
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
                                                .font_family(crate::gui::appearance::fonts::SYMBOLS_FAMILY)
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
                                        .child(rivu_ui::caption(theme, "󰎇").font_family(crate::gui::appearance::fonts::SYMBOLS_FAMILY).font_size_scale(1.))
                                        .child(rivu_ui::caption(theme, count)
                                            .font_features(crate::gui::appearance::fonts::tabular_numbers())),
                                )
                            })
                            .when(matches!(entry, DirectoryRow::OtherTracks), |view| {
                                view.child(rivu_ui::caption(theme, this.library.other_count_label())
                                    .font_features(crate::gui::appearance::fonts::tabular_numbers()))
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
                                row_height,
                            )
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
                                this.open_object_menu(crate::gui::ContentMenuRequest {
                                    source: crate::gui::TrackSource::Tracks(track_ids), position: event.position, play,
                                }, crate::gui::list_interaction::ContentGuard::Library(this.state.library.revision, this.state.library.structure_revision),
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
                            let drag_source = id.clone();
                            view.on_drag(
                                LibraryDrag {
                                    nodes: std::cell::OnceCell::new(),
                                },
                                move |drag, _, window, cx| {
                                    rivu_ui::begin_list_drag(window, cx);
                                    let nodes = drag.nodes.get_or_init(|| {
                                        drag_owner
                                            .update(cx, |this, _| this.library.drag_nodes(&drag_source))
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
    pub(in crate::gui) fn render_panel(
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
                    crate::gui::library::LibraryNode::Directory(_)
                        | crate::gui::library::LibraryNode::OtherTracks
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
            self.i18n.text("shell-loading").to_owned()
        };
        let heading = list_panel_header(
            theme,
            self.tr("shell-panel-library"),
            selection_label.clone(),
        )
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
                IconButton::new(
                    ("library-add-root", panel_id),
                    "󰐕",
                    self.tr("shell-add-library-folder"),
                )
                .build(
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
impl gpui::Render for Library {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_panel(0, window, cx)
    }
}
