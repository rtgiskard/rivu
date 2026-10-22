use super::super::*;
use rivu_ui::{column, row};

impl GuiApp {
    pub(in crate::gui) fn measurement(&self, key: Measured) -> AnyElement {
        let measured = self.workspace.interaction.measurement_handle(key);
        let ui_scale = self.state.system.config.ui_scale;
        canvas(
            move |bounds, window, _| {
                measured.record(bounds, DockGeometry::current(window, ui_scale));
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .into_any_element()
    }
    pub(in crate::gui) fn layout_gutter(&self) -> f64 {
        (f64::from(SPLIT_GUTTER) * self.workspace.interaction.scale_factor()).round()
            / self.workspace.interaction.scale_factor()
    }
    pub(in crate::gui) fn layout_edit_sizes(
        &self,
        panel: u64,
        docking: Option<(u64, Edge, u64)>,
    ) -> HashMap<u64, (f64, f64)> {
        let sizes = self
            .workspace
            .interaction
            .measurements()
            .iter()
            .filter_map(|(key, bounds)| {
                let Measured::Node(id) = key else { return None };
                Some((
                    *id,
                    (
                        f64::from(f32::from(bounds.size.width)),
                        f64::from(f32::from(bounds.size.height)),
                    ),
                ))
            })
            .collect();
        self.workspace
            .layouts
            .current()
            .root
            .as_ref()
            .map(|root| {
                layout::layout_geometry::edit_sizes(
                    root,
                    sizes,
                    panel,
                    docking,
                    self.layout_gutter(),
                )
            })
            .unwrap_or_default()
    }
    pub(in crate::gui) fn reconcile_layout_sizes(&mut self, sizes: &HashMap<u64, (f64, f64)>) {
        let gutter = self.layout_gutter();
        self.workspace
            .layouts
            .current_mut()
            .preserve_region_sizes(sizes, gutter);
        // Structural callers invalidate through layout_changed after adjusting sizes.
    }
    pub(in crate::gui) fn remove_layout_panel(&mut self, panel: u64, cx: &mut Context<Self>) {
        if self.workspace.layouts.current().locked {
            return;
        }
        let cancellation = self.workspace.interaction.cancel();
        self.apply_interaction_cancellation(cancellation, cx);
        let sizes = self.layout_edit_sizes(panel, None);
        if self.workspace.layouts.current_mut().remove(panel) {
            self.reconcile_layout_sizes(&sizes);
            self.layout_changed(cx);
            self.persist_layout(cx);
        }
    }
    pub(in crate::gui) fn begin_split_drag(
        &mut self,
        id: u64,
        index: usize,
        pointer: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.synchronize_interaction(window, cx);
        if self.workspace.layouts.current().locked {
            return;
        }
        let Some(root) = self.workspace.layouts.current().root.as_ref() else {
            return;
        };
        let Some(Node::Split {
            axis,
            children,
            boundaries,
            ..
        }) = layout::layout_geometry::find(root, id)
        else {
            return;
        };
        let measured = self.workspace.interaction.measurements();
        let Some(bounds) = measured.get(&Measured::Node(id)).copied() else {
            return;
        };
        let Some(first) = children
            .get(index)
            .and_then(|node| measured.get(&Measured::Node(layout::layout_geometry::node_id(node))))
        else {
            return;
        };
        let Some(second) = children
            .get(index + 1)
            .and_then(|node| measured.get(&Measured::Node(layout::layout_geometry::node_id(node))))
        else {
            return;
        };
        let boundary = |index: usize| {
            if index == 0 {
                0.
            } else if index == children.len() {
                1.
            } else {
                boundaries
                    .get(index - 1)
                    .copied()
                    .unwrap_or(index as f64 / children.len() as f64)
            }
        };
        let length = if *axis == Axis::Horizontal {
            bounds.size.width
        } else {
            bounds.size.height
        };
        let center = if *axis == Axis::Horizontal {
            (first.origin.x + first.size.width + second.origin.x) * 0.5
        } else {
            (first.origin.y + first.size.height + second.origin.y) * 0.5
        };
        let mut drag = layout::layout_geometry::SplitDrag {
            id,
            index,
            axis: *axis,
            bounds,
            content_length: f64::from(f32::from(length))
                - self.layout_gutter() * children.len().saturating_sub(1) as f64,
            gutter: self.layout_gutter(),
            before: boundary(index),
            after: boundary(index + 2),
            pointer_offset: 0.,
            pointer,
        };
        drag.pointer_offset = drag.pointer_coordinate(pointer) - f64::from(f32::from(center));
        let original_fraction = boundary(index + 1);
        drop(measured);
        let cancellation = self
            .workspace
            .interaction
            .begin_split(drag, original_fraction, root);
        self.apply_interaction_cancellation(cancellation, cx);
        cx.notify();
    }
    pub(in crate::gui) fn render_node(
        &mut self,
        node: &Node,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = rivu_ui::theme::palette(cx);
        match node {
            Node::Split {
                id,
                axis,
                children,
                boundaries,
            } => {
                let id = *id;
                let axis = *axis;
                let boundaries = boundaries.clone();
                let mut panes = Vec::with_capacity(children.len());
                for (index, child) in children.iter().enumerate() {
                    let content = self.render_node(child, window, cx);
                    let gutter = (index + 1 < children.len()).then(|| {
                        let unlocked = !self.workspace.layouts.current().locked;
                        let dragging = matches!(
                            self.workspace.interaction.dragging(),
                            Some(Dragging::Split(drag)) if drag.id == id && drag.index == index
                        );
                        let indicator = div()
                            .bg(rgb(theme.accent))
                            .opacity(if unlocked && dragging { 1. } else { 0. })
                            .when(unlocked, |view| {
                                view.group_hover("split-gutter", |style| style.opacity(1.))
                            })
                            .when(axis == Axis::Horizontal, |view| view.w(px(2.)).h_full())
                            .when(axis == Axis::Vertical, |view| view.h(px(2.)).w_full());
                        div()
                            .id(("split", index))
                            .group("split-gutter")
                            .absolute()
                            .bg(rgb(theme.bg))
                            .flex()
                            .items_center()
                            .justify_center()
                            .when(unlocked, |view| {
                                view.when(axis == Axis::Horizontal, |view| view.cursor_col_resize())
                                    .when(axis == Axis::Vertical, |view| view.cursor_row_resize())
                            })
                            .child(indicator)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                    this.begin_split_drag(id, index, event.position, window, cx);
                                    cx.stop_propagation();
                                }),
                            )
                    });
                    panes.push((content, gutter));
                }
                let measurement = self.measurement(Measured::Node(id));
                container_query(move |size, window, _| {
                    let length = if axis == Axis::Horizontal {
                        size.width
                    } else {
                        size.height
                    };
                    let mut geometry = layout::split_geometry(
                        &boundaries,
                        panes.len(),
                        f64::from(f32::from(length)),
                        f64::from(SPLIT_GUTTER),
                        f64::from(window.scale_factor()),
                    )
                    .peekable();
                    let mut split = div()
                        .id(("node", id))
                        .relative()
                        .size_full()
                        .min_w_0()
                        .min_h_0();
                    for (content, gutter) in panes {
                        let Some((start, length)) = geometry.next() else {
                            break;
                        };
                        let pane = div()
                            .absolute()
                            .min_w_0()
                            .min_h_0()
                            .overflow_hidden()
                            .when(axis == Axis::Horizontal, |view| {
                                view.left(px(start as f32))
                                    .top_0()
                                    .w(px(length as f32))
                                    .h_full()
                            })
                            .when(axis == Axis::Vertical, |view| {
                                view.top(px(start as f32))
                                    .left_0()
                                    .h(px(length as f32))
                                    .w_full()
                            })
                            .child(content);
                        split = split.child(pane);
                        if let (Some(gutter), Some(&(next, _))) = (gutter, geometry.peek()) {
                            let end = start + length;
                            let gutter = gutter
                                .when(axis == Axis::Horizontal, |view| {
                                    view.left(px(end as f32))
                                        .top_0()
                                        .w(px((next - end) as f32))
                                        .h_full()
                                })
                                .when(axis == Axis::Vertical, |view| {
                                    view.top(px(end as f32))
                                        .left_0()
                                        .h(px((next - end) as f32))
                                        .w_full()
                                });
                            split = split.child(gutter);
                        }
                    }
                    split.child(measurement)
                })
                .into_any_element()
            }
            Node::Tabs { id, panels, active } => {
                let node_id = *id;
                let active = *active;
                let show_tab_bar = panels
                    .iter()
                    .find(|panel| panel.id == active)
                    .is_none_or(|panel| panel.show_tab_bar);
                let mut tabs = row()
                    .items_stretch()
                    .flex_shrink_0()
                    .min_w_0()
                    .px_2()
                    .py_0p5()
                    .gap_1()
                    .border_b_1()
                    .border_color(rgb(theme.structure_border));
                for panel in panels.iter().filter(|_| show_tab_bar) {
                    let panel_id = panel.id;
                    let title = panel_spec(&panel.kind).map_or_else(
                        || SharedString::from(panel.kind.clone()),
                        |spec| self.tr(spec.title_id),
                    );
                    let tab_bar_visible = panel.show_tab_bar;
                    let drag = PanelDrag {
                        source: DockSource::Existing(panel_id),
                        title: title.clone(),
                    };
                    tabs = tabs.child(
                        div()
                            .id(("tab", panel_id))
                            .tab_index(0)
                            .focus(|style| style.border_color(rgb(theme.accent)))
                            .relative()
                            .min_w_0()
                            .flex_shrink(1.)
                            .flex()
                            .items_center()
                            .gap_1()
                            .px_1()
                            .py_0p5()
                            .rounded_sm()
                            .border_1()
                            .border_color(gpui::transparent_black())
                            .text_xs()
                            .when(!self.workspace.layouts.current().locked, |view| {
                                view.cursor_move()
                            })
                            .bg(rgb(if active == panel_id {
                                theme.selected
                            } else {
                                theme.panel
                            }))
                            .hover(|style| {
                                style.bg(rgb(if active == panel_id {
                                    theme.selected_hover
                                } else {
                                    theme.highlight
                                }))
                            })
                            .text_color(rgb(if active == panel_id {
                                theme.text
                            } else {
                                theme.secondary
                            }))
                            .on_click(cx.listener(move |this, event, window, cx| {
                                this.cancel_interaction(window, cx);
                                this.workspace.layouts.current_mut().activate(panel_id);
                                this.search_panel = Some(panel_id);
                                if matches!(event, gpui::ClickEvent::Mouse(_)) {
                                    this.focus_layout(window, cx);
                                }
                                this.workspace.interaction.set_target_group(Some(node_id));
                                this.persist_layout(cx);
                            }))
                            .on_key_down(cx.listener(move |this, event, _, cx| {
                                if rivu_ui::context_menu_key(event) {
                                    this.open_keyboard_panel_menu(
                                        panel_id,
                                        tab_bar_visible,
                                        Measured::Tab(panel_id),
                                        cx,
                                    );
                                    cx.stop_propagation();
                                }
                            }))
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                    cx.stop_propagation();
                                    this.panel_menu = Some(PanelMenu {
                                        panel_id,
                                        tab_bar_visible,
                                        position: event.position,
                                        page: PanelMenuPage::Main,
                                    });
                                    cx.notify();
                                }),
                            )
                            .when(!self.workspace.layouts.current().locked, |view| {
                                let entity = cx.entity().downgrade();
                                view.on_drag(drag, move |drag, _, window, cx| {
                                    let _ = entity.update(cx, |this, cx| {
                                        this.start_panel_docking(drag.source, window, cx);
                                        if !this.panel_docking_blocked() {
                                            this.update_panel_docking(
                                                window.mouse_position(),
                                                window,
                                                cx,
                                            );
                                        }
                                    });
                                    cx.new(|_| drag.clone())
                                })
                            })
                            .tooltip(rivu_ui::text_tooltip(title.clone()))
                            .child(div().min_w_0().truncate().child(title))
                            .child(self.measurement(Measured::Tab(panel_id)))
                            .when(!self.workspace.layouts.current().locked, |view| {
                                view.child(rivu_ui::activate_control(
                                    div()
                                        .id(("close", panel_id))
                                        .role(Role::Button)
                                        .aria_label(self.tr("shell-remove-panel"))
                                        .tab_index(0)
                                        .flex_shrink_0()
                                        .size(rems(1.))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded_sm()
                                        .cursor_pointer()
                                        .hover(|style| style.bg(rgb(theme.highlight)))
                                        .focus(|style| style.bg(rgb(theme.highlight)))
                                        .tooltip(rivu_ui::text_tooltip(
                                            self.tr("shell-remove-panel"),
                                        ))
                                        .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                            cx.stop_propagation()
                                        })
                                        .child(
                                            div()
                                                .font_family(rivu_ui::theme::icon_family(cx))
                                                .font_size_scale(1.)
                                                .line_height(relative(1.))
                                                .child(""),
                                        ),
                                    cx,
                                    move |this, _, cx| {
                                        if !this.workspace.layouts.current().locked {
                                            this.remove_layout_panel(panel_id, cx);
                                        }
                                    },
                                ))
                            }),
                    );
                }
                let content = panels
                    .iter()
                    .find(|panel| panel.id == active)
                    .map(|panel| {
                        if self.state.system.config.still_mode
                            && matches!(
                                panel.kind.as_str(),
                                "spectrum" | "spectrogram" | "audio_visualizer" | "waveform"
                            )
                        {
                            return div()
                                .size_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_color(rgb(theme.muted))
                                .child(self.tr("shell-visualizations-paused"))
                                .into_any_element();
                        }
                        panel_spec(&panel.kind)
                            .map(|_| self.render_panel(panel, window, cx))
                            .unwrap_or_else(|| {
                                div()
                                    .child(self.i18n.format(
                                        "shell-unknown-panel",
                                        &[("kind", panel.kind.as_str().into())],
                                    ))
                                    .into_any_element()
                            })
                    })
                    .unwrap_or_else(|| div().into_any_element());
                let content_area = div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .p_2()
                    .overflow_hidden()
                    .capture_any_mouse_down(cx.listener(move |this, _: &MouseDownEvent, _, _| {
                        this.search_panel = Some(active);
                    }))
                    .child(content);
                let mut panel = column()
                    .id(("group", node_id))
                    .capture_key_down(cx.listener(move |this, _, _, _| {
                        this.search_panel = Some(active);
                    }))
                    .on_key_down(cx.listener(move |this, event, window, cx| {
                        if rivu_ui::context_menu_key(event) && !this.input_focused(window, cx) {
                            this.open_keyboard_panel_menu(
                                active,
                                show_tab_bar,
                                Measured::Node(node_id),
                                cx,
                            );
                            cx.stop_propagation();
                        }
                    }))
                    .relative()
                    .size_full()
                    .gap_0()
                    .bg(rgb(theme.panel))
                    .border_1()
                    .border_color(rgb(theme.structure_border))
                    .rounded_sm()
                    .overflow_hidden();
                if show_tab_bar {
                    panel = panel.child(tabs);
                }
                panel = panel.child(content_area).on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        this.panel_menu = Some(PanelMenu {
                            panel_id: active,
                            tab_bar_visible: show_tab_bar,
                            position: event.position,
                            page: PanelMenuPage::Main,
                        });
                        cx.notify();
                    }),
                );
                // Structural edits account for the outer pane, including its border.
                div()
                    .relative()
                    .size_full()
                    .child(panel)
                    .child(self.measurement(Measured::Node(node_id)))
                    .into_any_element()
            }
        }
    }
}

impl GuiApp {
    pub(in crate::gui) fn drag_split(
        &mut self,
        position: Point<Pixels>,
        alt: bool,
        cx: &mut Context<Self>,
    ) {
        if self.workspace.layouts.current().locked {
            return;
        }
        let config = &self.state.system.config;
        let Some((id, index, fraction)) = self.workspace.interaction.update_split(
            position,
            config.layout_snap_enabled ^ alt,
            f64::from(config.ui_scale),
            config.layout_snap_grid_px,
        ) else {
            return;
        };
        if self
            .workspace
            .layouts
            .current_mut()
            .set_boundary(id, index, fraction)
        {
            cx.notify();
        }
    }
}
