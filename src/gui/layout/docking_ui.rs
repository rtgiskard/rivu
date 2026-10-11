use super::super::*;
use super::{docking, layout_geometry};

const DOCK_COLORS: [u32; 4] = [0x7aa2f7, 0x9ece6a, 0xbb9af7, 0xe0af68];

fn panel_group(node: &Node, panel: u64) -> Option<(u64, usize)> {
    match node {
        Node::Tabs { id, panels, .. } => panels
            .iter()
            .any(|item| item.id == panel)
            .then_some((*id, panels.len())),
        Node::Split { children, .. } => children.iter().find_map(|child| panel_group(child, panel)),
    }
}

impl GuiApp {
    pub(in crate::gui) fn panel_docking_blocked(&self) -> bool {
        self.layouts.current().locked
            || self.settings_open
            || self.about_open
            || self.layouts_open
            || self.app_menu_open
            || self.panel_menu.is_some()
            || self.catalog_open
    }

    pub(in crate::gui) fn start_panel_docking(
        &mut self,
        source: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.synchronize_interaction(window, cx);
        let cancellation = self.interaction.start_docking(
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
        if self.interaction.cancel_docking() {
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
        let Some(mut session) = self.interaction.take_docking() else {
            return;
        };
        let geometry = DockGeometry::current(window, self.state.system.config.ui_scale);
        if session.geometry != geometry || self.interaction.measured_geometry() != Some(geometry) {
            let had_preview = session.guide.is_some() || session.hover.is_some();
            session.guide = None;
            session.hover = None;
            if had_preview {
                cx.notify();
            }
        }
        // Bounds notifications precede repaint; never rebuild from the previous frame.
        if self.interaction.measured_geometry() != Some(geometry) {
            self.interaction.restore_docking(session);
            return;
        }
        session.geometry = geometry;
        let previous_guide = session.guide.map(|guide| (guide.leaf, guide.edge));
        let request = self.docking_request(&mut session, pointer);
        if session.hover.as_ref().map(|hover| hover.request) != request {
            session.hover =
                request.and_then(|request| self.prepare_docking(session.source, request));
            cx.notify();
        }
        if previous_guide != session.guide.map(|guide| (guide.leaf, guide.edge)) {
            cx.notify();
        }
        self.interaction.restore_docking(session);
    }

    fn docking_request(
        &self,
        session: &mut PanelDocking,
        pointer: Point<Pixels>,
    ) -> Option<DockRequest> {
        let root = self.layouts.current().root.as_ref()?;
        let measured = self.interaction.measurements();
        let bounds = |id| measured.get(&Measured::Node(id)).copied();
        let outer = bounds(layout_geometry::node_id(root))?;
        if !outer.contains(&pointer) {
            session.guide = None;
            return None;
        }
        if !session
            .guide
            .as_ref()
            .is_some_and(|guide| guide.contains(pointer, session.geometry.ui_scale))
        {
            session.guide =
                docking::Guide::build(root, pointer, session.geometry.ui_scale, &bounds);
        }
        if let Some(target) = session
            .guide
            .as_ref()
            .and_then(|guide| guide.target(pointer))
        {
            return Some(DockRequest {
                target,
                index: None,
            });
        }
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
        let index = docking::tab_index(node, pointer, &|id| {
            measured.get(&Measured::Tab(id)).copied()
        });
        let edge = if index.is_some() {
            Edge::Center
        } else {
            dock_edge(pointer, bounds(*id)?)?
        };
        Some(DockRequest {
            target: docking::Target { node: *id, edge },
            index,
        })
    }

    fn prepare_docking(&self, source: u64, request: DockRequest) -> Option<DockHover> {
        let current = self.layouts.current();
        let root = current.root.as_ref()?;
        let target = layout_geometry::find(root, request.target.node)?;
        let measured = self.interaction.measurements();
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
        let mut prepared = current.clone();
        let new_tabs = prepared.next_id;
        let needs_geometry =
            request.target.edge != Edge::Center || panel_group(root, source)?.1 == 1;
        let sizes = needs_geometry.then(|| {
            self.layout_edit_sizes(
                source,
                Some((request.target.node, request.target.edge, new_tabs)),
            )
        });
        if !prepared.move_panel(
            source,
            request.target.node,
            request.target.edge,
            request.index,
        ) {
            return Some(hover);
        }
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
            self.interaction.scale_factor(),
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
        let Some(session) = self.interaction.take_docking() else {
            return;
        };
        if let Some(hover) = session.hover {
            if let Some(prepared) = hover.prepared {
                *self.layouts.current_mut() = prepared;
                self.layout_changed(cx);
                self.interaction.set_target_group(
                    self.layouts
                        .current()
                        .root
                        .as_ref()
                        .and_then(|root| panel_group(root, source).map(|group| group.0)),
                );
                self.persist_layout(cx);
            }
        }
        cx.notify();
    }

    pub(in crate::gui) fn docking_overlay(&self) -> AnyElement {
        let Some(session) = self.interaction.docking() else {
            return div().into_any_element();
        };
        let mut overlay = div().absolute().top_0().left_0().size_full();
        if let Some(hover) = &session.hover {
            if !hover.unchanged {
                let owner = hover.owner;
                let preview = hover.preview;
                let selected_bar = session.guide.as_ref().and_then(|guide| {
                    guide
                        .bars
                        .iter()
                        .flatten()
                        .find(|bar| bar.target == hover.request.target)
                });
                let color = if preview.is_some() {
                    selected_bar.map_or(ACCENT, |bar| DOCK_COLORS[bar.depth % DOCK_COLORS.len()])
                } else {
                    0xf7768e
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
                let root = self.layouts.current().root.as_ref().unwrap();
                let label = if layout_geometry::node_id(root) == hover.request.target.node {
                    "Layout"
                } else {
                    layout_geometry::find(root, hover.request.target.node)
                        .map(|node| match node {
                            Node::Split {
                                axis: Axis::Horizontal,
                                ..
                            } => "Row",
                            Node::Split {
                                axis: Axis::Vertical,
                                ..
                            } => "Column",
                            Node::Tabs { .. } => "Panel",
                        })
                        .unwrap_or("Region")
                };
                let direction = match hover.request.target.edge {
                    Edge::Left => "left",
                    Edge::Right => "right",
                    Edge::Top => "above",
                    Edge::Bottom => "below",
                    Edge::Center => "tab",
                };
                let text = if preview.is_none() {
                    "Cannot dock here".to_owned()
                } else if let Some(bar) = selected_bar {
                    format!("{label} · level {} · {direction}", bar.depth + 1)
                } else {
                    format!("{label} · {direction}")
                };
                overlay = overlay.child(
                    div()
                        .absolute()
                        .left(owner.origin.x + px(6.))
                        .top(owner.origin.y + px(6.))
                        .px_2()
                        .py_1()
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgb(color))
                        .text_color(rgb(TEXT))
                        .text_xs()
                        .child(text),
                );
            }
        }
        if let Some(guide) = &session.guide {
            for bar in guide.bars.iter().flatten() {
                let selected = session
                    .hover
                    .as_ref()
                    .is_some_and(|hover| hover.request.target == bar.target);
                let color = DOCK_COLORS[bar.depth % DOCK_COLORS.len()];
                overlay = overlay.child(
                    div()
                        .absolute()
                        .left(bar.bounds.origin.x)
                        .top(bar.bounds.origin.y)
                        .w(bar.bounds.size.width)
                        .h(bar.bounds.size.height)
                        .rounded_sm()
                        .border_1()
                        .border_color(rgb(color))
                        .bg(rgb(color).alpha(if selected { 0.7 } else { 0.35 }))
                        .text_color(rgb(TEXT))
                        .text_xs()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(["1", "2", "3", "4"][bar.depth]),
                );
            }
        }
        overlay.into_any_element()
    }
}
