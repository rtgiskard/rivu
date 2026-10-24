//! Responsive graph workbench. View choices and scroll positions are panel-local;
//! committed graph, selection and task ownership remain with their existing owners.
use super::*;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Area {
    #[default]
    Graph,
    Parameters,
    Results,
}

#[derive(Default)]
pub(super) struct WorkbenchPanel {
    wide: bool,
    area: Area,
    connections: bool,
    pub(super) canvas_focus: Option<FocusHandle>,
}

impl AudioProcessing {
    pub(in crate::gui) fn deactivate_graph_inputs(
        &self,
        panel: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .graph_ui
            .panel_inputs
            .get(&panel)
            .is_some_and(|fields| {
                fields
                    .values()
                    .any(|field| field.input.read(cx).is_focused(window))
            })
        {
            window.blur(cx);
        }
    }
    pub(in crate::gui) fn show_graph_results(&mut self, panel: u64, cx: &mut Context<Self>) {
        self.cancel_pointer(cx);
        self.page = Page::Graph;
        self.graph_ui.workbenches.entry(panel).or_default().area = Area::Results;
        cx.notify();
    }
    pub(in crate::gui) fn processing_scroll(
        &mut self,
        panel: u64,
        region: &'static str,
        body: Div,
    ) -> AnyElement {
        let scroll = self.context.scroll.borrow_mut().content((region, panel));
        scrollable_area(
            (region, panel),
            div().id((region, panel)).size_full().child(body),
            &scroll,
        )
        .into_any_element()
    }

    pub(in crate::gui) fn processing_workbench_measure(
        &self,
        panel: u64,
        cx: &Context<Self>,
    ) -> AnyElement {
        let owner = cx.entity().downgrade();
        canvas(
            move |bounds, window, cx| {
                let wide = bounds.size.width >= window.rem_size() * 64.;
                let _ = owner.update(cx, |this, cx| {
                    let state = this.graph_ui.workbenches.entry(panel).or_default();
                    if state.wide != wide {
                        state.wide = wide;
                        this.cancel_pointer(cx);
                        cx.notify();
                    }
                });
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full()
        .into_any_element()
    }

    pub(super) fn graph_workspace(
        &mut self,
        panel: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        self.reconcile_graph(cx);
        let theme = rivu_ui::theme::palette(cx);
        let state = self.graph_ui.workbenches.entry(panel).or_default();
        let wide = state.wide;
        let area = state.area;
        let parameters_visible = area != Area::Results && (wide || area == Area::Parameters);
        if !parameters_visible {
            self.deactivate_equalizer_panel(panel, window, cx);
        }
        if self
            .graph_ui
            .panel_inputs
            .get(&panel)
            .is_some_and(|fields| {
                fields.iter().any(|(key, field)| {
                    let visible = if key == "graph_export" {
                        area == Area::Results
                    } else {
                        parameters_visible
                    };
                    !visible && field.input.read(cx).is_focused(window)
                })
            })
        {
            window.blur(cx);
        }
        let mut tabs = row().flex_wrap().gap_1();
        for (choice, label) in [
            (Area::Graph, "processing-workspace-graph"),
            (Area::Parameters, "processing-workspace-parameters"),
            (Area::Results, "processing-workspace-results"),
        ] {
            if wide && choice == Area::Parameters {
                continue;
            }
            tabs = tabs.child(
                button(
                    SharedString::from(format!("workbench-{panel}-{label}")),
                    self.tr(label),
                    cx,
                    move |this, _, cx| {
                        this.cancel_pointer(cx);
                        this.graph_ui.workbenches.entry(panel).or_default().area = choice;
                        cx.notify();
                    },
                )
                .when(
                    area == choice || (wide && area == Area::Parameters && choice == Area::Graph),
                    |v| v.bg(rgb(theme.selected)),
                ),
            );
        }
        let body = if area == Area::Results {
            let results = if self
                .state
                .system
                .processing
                .as_ref()
                .is_some_and(|task| task.operation != ProcessingOperation::Graph)
            {
                self.task_status(panel, cx)
            } else {
                self.graph_task_status(panel, cx)
            };
            self.processing_scroll(panel, "processing-results", results.p_2())
        } else if !wide && area == Area::Parameters {
            let inspector = self.graph_inspector(panel, window, cx);
            self.processing_scroll(panel, "processing-inspector", inspector)
        } else {
            let graph = self.graph_canvas_workspace(panel, cx).min_h(rems(24.));
            let graph = self.processing_scroll(panel, "processing-graph-canvas", graph);
            if wide {
                let inspector = self.graph_inspector(panel, window, cx);
                let inspector = self.processing_scroll(panel, "processing-inspector", inspector);
                row()
                    .size_full()
                    .min_h_0()
                    .gap_2()
                    .child(div().flex_1().min_w_0().h_full().child(graph))
                    .child(
                        div()
                            .w(rems(25.))
                            .max_w(relative(0.45))
                            .min_w_0()
                            .h_full()
                            .child(inspector),
                    )
                    .into_any_element()
            } else {
                graph
            }
        };
        column()
            .size_full()
            .min_h_0()
            .gap_2()
            .child(tabs.flex_shrink_0())
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .overflow_hidden()
                    .child(body),
            )
    }

    fn graph_canvas_workspace(&mut self, panel: u64, cx: &mut Context<Self>) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let busy = self.graph_busy();
        let drafts = self.graph_dirty_count(cx) > 0;
        let mut tools = row().flex_wrap().gap_1();
        for (redo, label) in [
            (false, "processing-graph-undo"),
            (true, "processing-graph-redo"),
        ] {
            let empty = if redo {
                self.graph_ui.redo.is_empty()
            } else {
                self.graph_ui.undo.is_empty()
            };
            tools = tools.child(disabled_control(
                theme,
                button(
                    SharedString::from(format!("history-{panel}-{redo}")),
                    self.tr(label),
                    cx,
                    move |this, _, cx| this.graph_history(redo, cx),
                ),
                busy || empty || drafts,
            ));
        }
        for (factor, label) in [(1. / 1.2, "−"), (1.2, "+")] {
            tools = tools.child(button(
                SharedString::from(format!("zoom-{panel}-{label}")),
                label,
                cx,
                move |this, _, cx| this.graph_zoom(factor, [250., 160.], cx),
            ));
        }
        tools = tools
            .child(caption(theme, format!("{:.0}%", self.graph_ui.zoom * 100.)))
            .child(button(
                ("fit-graph", panel),
                self.tr("processing-workspace-fit"),
                cx,
                move |this, _, cx| this.graph_fit(panel, None, cx),
            ))
            .child(button(
                ("focus-node", panel),
                self.tr("processing-workspace-locate"),
                cx,
                move |this, _, cx| {
                    if let Some(id) = this.graph_ui.selected {
                        this.graph_fit(panel, Some((id, id)), cx);
                    }
                },
            ))
            .child(button(
                ("reset-graph", panel),
                self.tr("processing-graph-reset-view"),
                cx,
                |this, _, cx| {
                    this.cancel_pointer(cx);
                    this.graph_ui.pan = [24., 24.];
                    this.graph_ui.zoom = 1.;
                    cx.notify();
                },
            ));
        tools = tools.child(
            caption(theme, "?")
                .id(("graph-workspace-help", panel))
                .tab_index(0)
                .aria_label(self.tr("processing-graph-editor-help"))
                .tooltip(rivu_ui::text_tooltip(
                    self.tr("processing-graph-editor-help"),
                )),
        );
        let mut library = row()
            .flex_wrap()
            .gap_1()
            .child(caption(theme, self.tr("processing-graph-add")));
        for (name, kind) in [
            (
                "eq",
                GraphNodeKind::Equalizer {
                    config: rivu_dsp::equalizer::config::EqualizerConfig {
                        enabled: true,
                        ..Default::default()
                    },
                    implementation: EqImplementation::Iir,
                },
            ),
            (
                "denoise",
                GraphNodeKind::Denoise {
                    config: self.state.system.config.denoise.clone(),
                    profile: None,
                },
            ),
            (
                "speech",
                GraphNodeKind::Speech {
                    config: self.state.system.config.neural.clone(),
                },
            ),
            (
                "separate",
                GraphNodeKind::Separate {
                    config: self.state.system.config.neural.clone(),
                },
            ),
            (
                "mix",
                GraphNodeKind::Mix {
                    gains_db: vec![0.; 2],
                    muted: vec![false; 2],
                    solo: vec![false; 2],
                },
            ),
            ("output", GraphNodeKind::Output),
        ] {
            library = library.child(disabled_control(
                theme,
                button(
                    SharedString::from(format!("workbench-add-{panel}-{name}")),
                    self.tr(node_label(&kind)),
                    cx,
                    move |this, _, cx| this.graph_add(panel, kind.clone(), cx),
                ),
                busy,
            ));
        }
        let selected = self
            .state
            .system
            .config
            .processing_graph
            .nodes
            .iter()
            .find(|node| Some(node.id) == self.graph_ui.selected);
        let selected_label = selected
            .map(|node| format!("{} #{}", self.tr(node_label(&node.kind)), node.id))
            .unwrap_or_else(|| "—".to_owned());
        let fixed = selected.is_none_or(|node| matches!(node.kind, GraphNodeKind::Source));
        let selected_drafts =
            selected.is_some_and(|node| self.graph_node_dirty_count(node.id, cx) > 0);
        let actions = row()
            .flex_wrap()
            .gap_1()
            .child(caption(theme, selected_label))
            .child(disabled_control(
                theme,
                button(
                    ("workbench-duplicate", panel),
                    self.tr("processing-graph-duplicate"),
                    cx,
                    |this, _, cx| this.graph_duplicate(cx),
                ),
                busy || fixed || selected_drafts,
            ))
            .child(disabled_control(
                theme,
                button(
                    ("workbench-remove", panel),
                    self.tr("processing-graph-delete"),
                    cx,
                    |this, _, cx| this.graph_remove(cx),
                ),
                busy || fixed || selected_drafts,
            ))
            .child(button(
                ("workbench-connections", panel),
                self.tr("processing-graph-connections"),
                cx,
                move |this, _, cx| {
                    let state = this.graph_ui.workbenches.entry(panel).or_default();
                    state.connections = !state.connections;
                    cx.notify();
                },
            ));
        let mut view = column()
            .size_full()
            .min_h_0()
            .gap_2()
            .child(library.flex_shrink_0())
            .child(tools.flex_shrink_0())
            .child(actions.flex_shrink_0())
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(self.routing_canvas(panel, cx)),
            );
        if let Some(port) = self.graph_ui.connecting {
            view = view.child(
                row()
                    .flex_wrap()
                    .gap_1()
                    .child(caption(
                        theme,
                        format!(
                            "{} #{}:{}",
                            self.tr("processing-graph-connect-from"),
                            port.node,
                            port.port
                        ),
                    ))
                    .child(button(
                        ("workbench-cancel-connect", panel),
                        self.tr("processing-cancel"),
                        cx,
                        |this, _, cx| {
                            this.graph_ui.connecting = None;
                            cx.notify();
                        },
                    )),
            );
        }
        if self.graph_ui.workbenches[&panel].connections {
            let mut connections = column().gap_1().p_1();
            for edge in &self.state.system.config.processing_graph.edges {
                let from = edge.from;
                let to = edge.to;
                connections = connections.child(
                    row()
                        .flex_wrap()
                        .gap_1()
                        .child(button(
                            SharedString::from(format!(
                                "locate-edge-{panel}-{}-{}",
                                to.node, to.port
                            )),
                            format!("#{}:{} → #{}:{}", from.node, from.port, to.node, to.port),
                            cx,
                            move |this, _, cx| {
                                this.graph_select(to.node, cx);
                                this.graph_ui.highlight_connection = Some(to);
                                this.graph_fit(panel, Some((from.node, to.node)), cx);
                            },
                        ))
                        .child(disabled_control(
                            theme,
                            button(
                                SharedString::from(format!(
                                    "remove-edge-{panel}-{}-{}",
                                    to.node, to.port
                                )),
                                self.tr("processing-graph-disconnect"),
                                cx,
                                move |this, _, cx| {
                                    this.graph_edit(GraphEdit::Disconnect { to }, cx)
                                },
                            ),
                            busy,
                        )),
                );
            }
            let connections = self.processing_scroll(panel, "processing-connections", connections);
            view = view.child(div().h(rems(7.)).flex_shrink_0().child(connections));
        }
        view
    }

    pub(in crate::gui) fn graph_execution_bar(
        &mut self,
        panel: u64,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let running = self
            .state
            .system
            .processing
            .as_ref()
            .is_some_and(|task| !task.phase.is_terminal());
        let busy = self.graph_busy() || running;
        let mut controls = row().flex_wrap().gap_1();
        for (mode, label) in [
            (GraphExecutionMode::Auto, "processing-graph-auto"),
            (GraphExecutionMode::Realtime, "processing-graph-realtime"),
            (GraphExecutionMode::Hybrid, "processing-graph-hybrid"),
            (GraphExecutionMode::Offline, "processing-graph-offline"),
        ] {
            controls = controls.child(
                button(
                    SharedString::from(format!("execution-{panel}-{mode:?}")),
                    self.tr(label),
                    cx,
                    move |this, _, cx| {
                        this.graph_ui.mode = mode;
                        cx.notify();
                    },
                )
                .when(self.graph_ui.mode == mode, |v| v.bg(rgb(theme.selected))),
            );
        }
        let no_input = self.input_track().is_none();
        for (output, label) in [
            (false, "processing-graph-preview"),
            (true, "processing-graph-run"),
        ] {
            let target = if output {
                self.state
                    .system
                    .config
                    .processing_graph
                    .nodes
                    .iter()
                    .find(|node| matches!(node.kind, GraphNodeKind::Output))
                    .map(|node| GraphPort {
                        node: node.id,
                        port: 0,
                    })
            } else {
                self.graph_ui.selected.map(|node| GraphPort {
                    node,
                    port: self.graph_ui.target_port,
                })
            };
            let label = target.map_or_else(
                || self.tr(label).to_string(),
                |port| format!("{} #{}:{}", self.tr(label), port.node, port.port),
            );
            controls = controls.child(disabled_control(
                theme,
                button(
                    SharedString::from(format!("execute-{panel}-{output}")),
                    label,
                    cx,
                    move |this, _, cx| this.graph_run_target(output, cx),
                ),
                busy || no_input || target.is_none(),
            ));
        }
        let mut footer = column()
            .flex_shrink_0()
            .gap_1()
            .p_2()
            .border_t_1()
            .border_color(rgb(theme.border))
            .child(controls);
        let drafts = self.graph_dirty_count(cx);
        if drafts > 0 {
            footer = footer.child(
                caption(
                    theme,
                    format!(
                        "{} · {drafts}",
                        self.tr("processing-workspace-run-committed")
                    ),
                )
                .text_color(rgb(theme.warning)),
            );
        }
        footer.child(self.graph_task_summary(panel, cx))
    }

    fn graph_fit(&mut self, panel: u64, nodes: Option<(u64, u64)>, cx: &mut Context<Self>) {
        let Some((bounds, _)) = self
            .graph_ui
            .bounds
            .get(&panel)
            .and_then(|bounds| bounds.get())
        else {
            return;
        };
        let size = [f32::from(bounds.size.width), f32::from(bounds.size.height)];
        if let Some((pan, zoom)) =
            fitted_view(&self.state.system.config.processing_graph, size, nodes)
        {
            self.cancel_pointer(cx);
            self.graph_ui.pan = pan;
            self.graph_ui.zoom = zoom;
            cx.notify();
        }
    }
}

fn fitted_view(
    graph: &ProcessingGraph,
    size: [f32; 2],
    nodes: Option<(u64, u64)>,
) -> Option<([f32; 2], f32)> {
    if size[0] <= 48. || size[1] <= 48. {
        return None;
    }
    let mut extent: Option<([f32; 2], [f32; 2])> = None;
    for node in &graph.nodes {
        if nodes.is_some_and(|(a, b)| node.id != a && node.id != b) {
            continue;
        }
        let min = node.position;
        let max = [min[0] + NODE_WIDTH, min[1] + node_height(&node.kind)];
        extent = Some(match extent {
            None => (min, max),
            Some((a, b)) => (
                [a[0].min(min[0]), a[1].min(min[1])],
                [b[0].max(max[0]), b[1].max(max[1])],
            ),
        });
    }
    let (min, max) = extent?;
    let zoom = ((size[0] - 48.) / (max[0] - min[0]))
        .min((size[1] - 48.) / (max[1] - min[1]))
        .min(1.);
    Some((
        [
            (size[0] - (max[0] - min[0]) * zoom) * 0.5 - min[0] * zoom,
            (size[1] - (max[1] - min[1]) * zoom) * 0.5 - min[1] * zoom,
        ],
        zoom,
    ))
}

#[cfg(test)]
mod tests {
    use super::{NODE_WIDTH, ProcessingGraph, fitted_view, node_height};

    #[test]
    fn fit_keeps_distant_nodes_inside_canvas_and_focuses_only_requested_nodes() {
        let mut graph = ProcessingGraph::default();
        graph.nodes[0].position = [-4000., -1500.];
        graph.nodes[1].position = [8000., 3500.];
        let size = [640., 320.];
        let (pan, zoom) = fitted_view(&graph, size, None).unwrap();
        for node in &graph.nodes {
            let left = pan[0] + node.position[0] * zoom;
            let top = pan[1] + node.position[1] * zoom;
            assert!(left >= 23.99 && top >= 23.99);
            assert!(left + NODE_WIDTH * zoom <= size[0] - 23.99);
            assert!(top + node_height(&node.kind) * zoom <= size[1] - 23.99);
        }
        let id = graph.nodes[0].id;
        let (pan, zoom) = fitted_view(&graph, size, Some((id, id))).unwrap();
        assert_eq!(zoom, 1.);
        let center = pan[0] + (graph.nodes[0].position[0] + NODE_WIDTH * 0.5) * zoom;
        assert!((center - size[0] * 0.5).abs() < 0.01);
        assert!(fitted_view(&graph, [40., 40.], None).is_none());
    }
}
