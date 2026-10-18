use super::super::*;
use super::{Layout, docking, layout_geometry};

fn panel_group(node: &Node, panel: u64) -> Option<(u64, usize)> {
    match node {
        Node::Tabs { id, panels, .. } => panels
            .iter()
            .any(|item| item.id == panel)
            .then_some((*id, panels.len())),
        Node::Split { children, .. } => children.iter().find_map(|child| panel_group(child, panel)),
    }
}

/// Translate measured insertion slots before building the shared preview/drop model.
fn docking_candidate(current: &Layout, source: u64, request: DockRequest) -> Option<Layout> {
    let mut index = request.index;
    if request.target.edge == Edge::Center
        && let Some(index) = &mut index
        && let Node::Tabs { panels, .. } =
            layout_geometry::find(current.root.as_ref()?, request.target.node)?
        && let Some(source_index) = panels.iter().position(|panel| panel.id == source)
        && *index > source_index
    {
        // Hit testing sees the source; move_panel inserts after removing it.
        *index -= 1;
    }
    let mut prepared = current.clone();
    prepared
        .move_panel(source, request.target.node, request.target.edge, index)
        .then_some(prepared)
}

impl GuiApp {
    pub(in crate::gui) fn panel_docking_blocked(&self) -> bool {
        self.workspace.layouts.current().locked
            || self.shell.is_open(shell::Surface::Settings)
            || self.shell.is_open(shell::Surface::About)
            || self.shell.help().is_some()
            || self.shell.is_open(shell::Surface::Layouts)
            || self.shell.is_open(shell::Surface::AppMenu)
            || self.panel_menu.is_some()
            || self.content_menu.is_some()
            || self.shell.is_open(shell::Surface::PanelCatalog)
    }

    pub(in crate::gui) fn start_panel_docking(
        &mut self,
        source: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.synchronize_interaction(window, cx);
        let cancellation = self.workspace.interaction.start_docking(
            source,
            DockGeometry::current(window, self.state.system.config.ui_scale),
        );
        self.apply_interaction_cancellation(cancellation, cx);
    }

    pub(in crate::gui) fn cancel_panel_docking(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.workspace.interaction.cancel_docking() {
            cx.stop_active_drag(window);
            cx.notify();
        }
    }

    pub(in crate::gui) fn update_panel_docking(
        &mut self,
        pointer: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.synchronize_interaction(window, cx);
        // Wayland's implicit button grab can deliver outside motion without MouseExit.
        if self.panel_docking_blocked()
            || !Bounds::new(Point::default(), window.viewport_size()).contains(&pointer)
        {
            self.cancel_panel_docking(window, cx);
            return;
        }
        let Some(mut session) = self.workspace.interaction.take_docking() else {
            return;
        };
        let geometry = DockGeometry::current(window, self.state.system.config.ui_scale);
        if session.geometry != geometry
            || self.workspace.interaction.measured_geometry() != Some(geometry)
        {
            let had_preview = session.hover.is_some();
            session.hover = None;
            if had_preview {
                cx.notify();
            }
        }
        // Bounds notifications precede repaint; never rebuild from the previous frame.
        if self.workspace.interaction.measured_geometry() != Some(geometry) {
            self.workspace.interaction.restore_docking(session);
            return;
        }
        session.geometry = geometry;
        let request = self.docking_request(pointer);
        if session.hover.as_ref().map(|hover| hover.request) != request {
            session.hover =
                request.and_then(|request| self.prepare_docking(session.source, request));
            cx.notify();
        }
        self.workspace.interaction.restore_docking(session);
    }

    fn docking_request(&self, pointer: Point<Pixels>) -> Option<DockRequest> {
        let root = self.workspace.layouts.current().root.as_ref()?;
        let measured = self.workspace.interaction.measurements();
        let bounds = |id| measured.get(&Measured::Node(id)).copied();
        fn leaf<'a>(
            node: &'a Node,
            pointer: Point<Pixels>,
            bounds: &impl Fn(u64) -> Option<Bounds<Pixels>>,
        ) -> Option<&'a Node> {
            if !bounds(layout_geometry::node_id(node))?.contains(&pointer) {
                return None;
            }
            match node {
                Node::Tabs { .. } => Some(node),
                Node::Split { children, .. } => children
                    .iter()
                    .find_map(|child| leaf(child, pointer, bounds)),
            }
        }
        let node = leaf(root, pointer, &bounds)?;
        let Node::Tabs { id, .. } = node else {
            return None;
        };
        let panel_bounds = bounds(*id)?;
        let index = docking::tab_index(node, pointer, panel_bounds, &|id| {
            measured.get(&Measured::Tab(id)).copied()
        });
        let edge = if index.is_some() {
            Edge::Center
        } else {
            dock_edge(pointer, panel_bounds)?
        };
        Some(DockRequest {
            target: docking::Target { node: *id, edge },
            index,
        })
    }

    fn prepare_docking(&self, source: u64, request: DockRequest) -> Option<DockHover> {
        let current = self.workspace.layouts.current();
        let root = current.root.as_ref()?;
        let target = layout_geometry::find(root, request.target.node)?;
        let measured = self.workspace.interaction.measurements();
        let owner = *measured.get(&Measured::Node(request.target.node))?;
        let outer = *measured.get(&Measured::Node(layout_geometry::node_id(root)))?;
        drop(measured);
        let mut hover = DockHover {
            request,
            owner,
            preview: None,
            prepared: None,
            unchanged: false,
        };
        if matches!(target, Node::Tabs { panels, .. } if panels.len() == 1 && panels[0].id == source)
        {
            hover.unchanged = true;
            return Some(hover);
        }
        // One owned candidate per target transition, never per pointer update.
        // The preview and drop consume the same model result, including source collapse.
        let new_tabs = current.next_id;
        let needs_geometry =
            request.target.edge != Edge::Center || panel_group(root, source)?.1 == 1;
        let sizes = needs_geometry.then(|| {
            self.layout_edit_sizes(
                source,
                Some((request.target.node, request.target.edge, new_tabs)),
            )
        });
        let Some(mut prepared) = docking_candidate(current, source, request) else {
            return Some(hover);
        };
        if let Some(sizes) = sizes {
            prepared.preserve_region_sizes(&sizes, self.layout_gutter());
        }
        hover.unchanged = prepared == *current;
        if hover.unchanged {
            return Some(hover);
        }
        let preview_node = if request.target.edge == Edge::Center {
            request.target.node
        } else {
            new_tabs
        };
        let preview = docking::node_bounds(
            prepared.root.as_ref()?,
            outer,
            preview_node,
            f64::from(SPLIT_GUTTER),
            self.workspace.interaction.scale_factor(),
        )?;
        hover.preview = Some(preview);
        hover.prepared = Some(prepared);
        Some(hover)
    }

    pub(in crate::gui) fn drop_panel_docking(
        &mut self,
        source: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.panel_docking_blocked()
            || self
                .workspace
                .interaction
                .docking()
                .is_none_or(|session| session.source != source)
        {
            self.cancel_panel_docking(window, cx);
            return;
        }
        // Mouse-up can arrive without an intervening move (or after a resize).
        // Resolve through exactly the same path as the displayed preview.
        self.update_panel_docking(window.mouse_position(), window, cx);
        let Some(session) = self.workspace.interaction.take_docking() else {
            return;
        };
        if let Some(hover) = session.hover
            && let Some(prepared) = hover.prepared
        {
            *self.workspace.layouts.current_mut() = prepared;
            self.layout_changed(cx);
            self.workspace.interaction.set_target_group(
                self.workspace
                    .layouts
                    .current()
                    .root
                    .as_ref()
                    .and_then(|root| panel_group(root, source).map(|group| group.0)),
            );
            self.persist_layout(cx);
        }
        cx.notify();
    }

    pub(in crate::gui) fn docking_overlay(
        &self,
        theme: &'static crate::theme::Palette,
    ) -> AnyElement {
        let Some(session) = self.workspace.interaction.docking() else {
            return div().into_any_element();
        };
        let mut overlay = div().absolute().top_0().left_0().size_full();
        if let Some(hover) = &session.hover
            && !hover.unchanged
        {
            let owner = hover.owner;
            let preview = hover.preview;
            let color = if preview.is_some() {
                theme.accent
            } else {
                theme.error
            };
            overlay = overlay.child(
                div()
                    .absolute()
                    .left(owner.origin.x)
                    .top(owner.origin.y)
                    .w(owner.size.width)
                    .h(owner.size.height)
                    .border_2()
                    .border_color(rgb(color)),
            );
            overlay = overlay.child(
                canvas(
                    |_, _, _| (),
                    move |_, _, window, _| {
                        if let Some(preview) = preview {
                            window.paint_quad(fill(preview, rgb(color).alpha(0.22)));
                        }
                    },
                )
                .absolute()
                .size_full(),
            );
            let text = self.tr(if preview.is_none() {
                "shell-cannot-dock"
            } else {
                match hover.request.target.edge {
                    Edge::Left => "shell-dock-left",
                    Edge::Right => "shell-dock-right",
                    Edge::Top => "shell-dock-above",
                    Edge::Bottom => "shell-dock-below",
                    Edge::Center => "shell-dock-tab",
                }
            });
            overlay = overlay.child(
                div()
                    .absolute()
                    .left(owner.origin.x + px(6.))
                    .top(owner.origin.y + px(6.))
                    .px_2()
                    .py_1()
                    .bg(rgb(theme.panel))
                    .border_1()
                    .border_color(rgb(color))
                    .text_color(rgb(theme.text))
                    .text_xs()
                    .child(text),
            );
        }
        overlay.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::layout::Panel;
    use core::prelude::v1::test;

    fn tabs(id: u64, ids: &[u64], active: u64) -> Node {
        Node::Tabs {
            id,
            active,
            panels: ids
                .iter()
                .map(|&id| Panel {
                    id,
                    kind: "library".into(),
                    show_tab_bar: true,
                    audio_visualizer: None,
                    album_artwork: None,
                })
                .collect(),
        }
    }

    fn panel_ids(layout: &Layout, group: u64) -> Vec<u64> {
        let Node::Tabs { panels, .. } =
            layout_geometry::find(layout.root.as_ref().unwrap(), group).unwrap()
        else {
            panic!("expected tab group");
        };
        panels.iter().map(|panel| panel.id).collect()
    }

    #[test]
    fn measured_tab_slots_build_the_preview_and_drop_candidate() {
        let owner = Bounds::new(point(px(0.), px(0.)), size(px(500.), px(300.)));
        let tab_bounds = |id| {
            (101..=104).contains(&id).then(|| {
                Bounds::new(
                    point(px((id - 101) as f32 * 100.), px(0.)),
                    size(px(100.), px(30.)),
                )
            })
        };
        for (label, source, x, y, slot, expected) in [
            (
                "rightward",
                101,
                200.,
                10.,
                Some(2),
                vec![102, 101, 103, 104],
            ),
            (
                "leftward",
                104,
                100.,
                10.,
                Some(1),
                vec![101, 104, 102, 103],
            ),
            (
                "self before",
                102,
                100.,
                10.,
                Some(1),
                vec![101, 102, 103, 104],
            ),
            (
                "self after",
                102,
                200.,
                10.,
                Some(2),
                vec![101, 102, 103, 104],
            ),
            ("start", 103, 0., 10., Some(0), vec![103, 101, 102, 104]),
            ("end", 102, 499., 10., Some(4), vec![101, 103, 104, 102]),
            (
                "cross group",
                201,
                200.,
                10.,
                Some(2),
                vec![101, 102, 201, 103, 104],
            ),
            (
                "center append",
                102,
                200.,
                100.,
                None,
                vec![101, 103, 104, 102],
            ),
        ] {
            let current = Layout {
                root: Some(Node::Split {
                    id: 1,
                    axis: Axis::Horizontal,
                    children: vec![
                        tabs(
                            2,
                            &[101, 102, 103, 104],
                            if source == 201 { 101 } else { source },
                        ),
                        tabs(3, &[201, 202], 201),
                    ],
                    boundaries: Vec::new(),
                }),
                next_id: 1000,
                locked: false,
            };
            let target = layout_geometry::find(current.root.as_ref().unwrap(), 2).unwrap();
            let index = docking::tab_index(target, point(px(x), px(y)), owner, &tab_bounds);
            assert_eq!(index, slot, "{label}: measured slot");
            let candidate = docking_candidate(
                &current,
                source,
                DockRequest {
                    target: docking::Target {
                        node: 2,
                        edge: Edge::Center,
                    },
                    index,
                },
            )
            .unwrap();
            assert_eq!(panel_ids(&candidate, 2), expected, "{label}");
            assert_eq!(candidate.next_id, current.next_id, "{label}: allocator");
            if label.starts_with("self") {
                assert_eq!(candidate, current, "{label}: unchanged preview/drop");
            }
            if source == 201 {
                assert_eq!(panel_ids(&candidate, 3), vec![202]);
            }
        }
    }
}
