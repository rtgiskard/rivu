//! Responsive graph workbench. View choices belong to the window's processing
//! owner; panel identity only fences controls from a retired layout frame.
use super::*;
use rivu_ui::{
    ChoiceItem, ChoicePopup, ChoiceSelect, IconButton, IconButtonDensity, IconButtonStyle, Slider,
    SliderInput, SliderKey,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum WorkbenchMenu {
    Add,
    Mode,
    Fft,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum WorkbenchChoice {
    Add(usize),
    Mode(GraphExecutionMode),
    Fft(NodeField, &'static str),
}

/// Last-frame controls cannot borrow a replacement panel's input authority.
#[derive(Clone)]
pub(super) struct WorkbenchInput {
    panel: u64,
    bounds: MeasuredGraph,
    input_epoch: u64,
    geometry_epoch: u64,
}

impl WorkbenchInput {
    pub(super) fn current(&self, owner: &AudioProcessing) -> bool {
        owner.page == Page::Graph
            && self.geometry_epoch == owner.geometry_epoch
            && owner.graph_editor.workbench.panel == Some(self.panel)
            && owner.graph_editor.workbench.input_epoch == self.input_epoch
            && Rc::ptr_eq(&owner.graph_editor.workbench.zoom_bounds, &self.bounds)
    }
}

pub(super) struct WorkbenchPanel {
    pub(super) panel: Option<u64>,
    wide: bool,
    parameters: bool,
    inspector_open: bool,
    inspector_width: f32,
    resize: Option<(Pixels, f32, WindowGeometry)>,
    input_epoch: u64,
    zoom_bounds: MeasuredGraph,
    zoom_drag: Option<WindowGeometry>,
    connections: bool,
    menu_bounds: Rc<Cell<[Option<Bounds<Pixels>>; 3]>>,
    pub(super) canvas_focus: Option<FocusHandle>,
}

impl Default for WorkbenchPanel {
    fn default() -> Self {
        Self {
            panel: None,
            wide: false,
            parameters: false,
            inspector_open: true,
            inspector_width: 25.,
            resize: None,
            input_epoch: 0,
            connections: false,
            zoom_bounds: Rc::new(Cell::new(None)),
            zoom_drag: None,
            menu_bounds: Rc::new(Cell::new([None; 3])),
            canvas_focus: None,
        }
    }
}

impl AudioProcessing {
    pub(in crate::gui) fn deactivate_graph_inputs(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let export_visible = self.page == Page::Results
            && self.state.system.processing.as_ref().is_some_and(|task| {
                task.operation == ProcessingOperation::Graph
                    && super::super::results::graph_action_run(task).is_some()
            });
        self.retire_graph_inputs(None, export_visible, window, cx);
    }

    fn retire_graph_inputs(
        &mut self,
        visible_node: Option<u64>,
        export_visible: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut blur = false;
        for (key, field) in &mut self.fields {
            let hidden = key.graph_node().is_some_and(|id| Some(id) != visible_node)
                || (*key == FieldKey::GraphExport && !export_visible);
            if hidden {
                blur |= field.input.read(cx).is_focused(window);
                field.retire(cx);
            }
        }
        if blur {
            window.blur(cx);
        }
    }
    pub(in crate::gui) fn show_graph_results(&mut self, cx: &mut Context<Self>) {
        self.cancel_pointer(cx);
        self.close_workbench_controls();
        self.page = Page::Results;
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
        let target = self.workbench_input(panel);
        canvas(
            move |bounds, window, cx| {
                let wide = bounds.size.width >= window.rem_size() * 64.;
                let _ = owner.update(cx, |this, cx| {
                    if !target.current(this) {
                        return;
                    }
                    let state = &mut this.graph_editor.workbench;
                    if state.wide != wide {
                        state.wide = wide;
                        this.cancel_pointer(cx);
                        this.close_workbench_controls();
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
        self.bind_panel(panel, cx);
        self.reconcile_graph(cx);
        let state = &mut self.graph_editor.workbench;
        state.panel = Some(panel);
        let wide = state.wide;
        let parameters = state.parameters;
        let inspector_width = state.inspector_width;
        let selected = self.graph_editor.selected.is_some_and(|id| {
            self.state
                .system
                .config
                .processing_graph
                .nodes
                .iter()
                .any(|node| node.id == id)
        });
        let parameters_visible = selected
            && if wide {
                state.inspector_open
            } else {
                parameters
            };
        if !parameters_visible {
            self.deactivate_equalizer_panel(panel, window, cx);
        }
        self.retire_graph_inputs(
            self.graph_editor.selected.filter(|_| parameters_visible),
            false,
            window,
            cx,
        );
        let toolbar = self.graph_toolbar(panel, selected, parameters_visible, cx);
        let body = if !wide && parameters_visible {
            let inspector = self.graph_inspector(panel, window, cx);
            self.processing_scroll(panel, "processing-inspector", inspector)
        } else {
            let graph = self.graph_canvas_workspace(panel, window, cx);
            if wide && parameters_visible {
                let inspector = self.graph_inspector(panel, window, cx);
                let inspector = self.processing_scroll(panel, "processing-inspector", inspector);
                row()
                    .size_full()
                    .min_h_0()
                    .child(div().flex_1().min_w_0().h_full().child(graph))
                    .child(self.inspector_resize_handle(panel, cx))
                    .child(
                        div()
                            .w(rems(inspector_width))
                            .max_w(relative(0.45))
                            .min_w_0()
                            .h_full()
                            .child(inspector),
                    )
                    .into_any_element()
            } else {
                graph.into_any_element()
            }
        };
        let resize_owner = cx.entity().downgrade();
        let resize_target = self.workbench_input(panel);
        let mut view = column()
            .size_full()
            .min_h_0()
            .gap_1()
            .child(
                canvas(
                    |_, _, _| {},
                    move |_, _, window, _| {
                        Self::inspector_resize_handlers(
                            resize_owner.clone(),
                            resize_target.clone(),
                            window,
                        );
                    },
                )
                .absolute()
                .size_full(),
            )
            .child(toolbar.flex_shrink_0())
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .overflow_hidden()
                    .child(body),
            );
        if self
            .graph_editor
            .workbench_menu
            .is_some_and(|(owner, _)| owner == panel)
            && self.graph_editor.workbench_choices.is_open()
            && let Some(anchor) = self.graph_editor.workbench_anchor.get()
        {
            let mut anchor = anchor;
            anchor.size.width = anchor.size.width.max(window.rem_size() * 16.);
            view = view.child(
                ChoicePopup::new(
                    ("graph-choice", panel),
                    ("graph-choice-scroll", panel),
                    anchor,
                )
                .render(
                    self,
                    |this| &this.graph_editor.workbench_choices,
                    Self::choose_workbench_item,
                    Self::dismiss_workbench_menu,
                    window,
                    cx,
                ),
            );
        }
        view
    }

    fn graph_toolbar(
        &mut self,
        panel: u64,
        selected: bool,
        parameters_visible: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let busy = self.graph_busy();
        let drafts = self.graph_dirty_count(cx) > 0;
        let target = self.workbench_input(panel);
        let mut editing = row()
            .flex_shrink_0()
            .max_w_full()
            .flex_wrap()
            .gap_1()
            .child(disabled_control(
                theme,
                self.workbench_dropdown(
                    panel,
                    WorkbenchMenu::Add,
                    self.tr("processing-workspace-add-node"),
                    cx,
                ),
                busy,
            ));
        for (redo, glyph, label) in [
            (false, "󰕌", "processing-graph-undo"),
            (true, "󰑎", "processing-graph-redo"),
        ] {
            let target = target.clone();
            let empty = if redo {
                self.graph_editor.redo.is_empty()
            } else {
                self.graph_editor.undo.is_empty()
            };
            editing = editing.child(
                IconButton::new(
                    SharedString::from(format!("history-{panel}-{redo}")),
                    glyph,
                    self.tr(label),
                )
                .surface(crate::gui::appearance::material::surface(cx))
                .enabled(!busy && !empty && !drafts)
                .build(
                    IconButtonStyle::Quiet,
                    IconButtonDensity::Compact,
                    cx,
                    move |this, _, cx| {
                        if target.current(this) {
                            this.graph_history(redo, cx);
                        }
                    },
                ),
            );
        }
        let navigation = row()
            .flex_shrink_0()
            .max_w_full()
            .flex_wrap()
            .gap_1()
            .child(self.graph_zoom_slider(panel, cx))
            .child(
                IconButton::new(
                    ("fit-graph", panel),
                    "󰘖",
                    self.tr("processing-workspace-fit"),
                )
                .surface(crate::gui::appearance::material::surface(cx))
                .build(IconButtonStyle::Quiet, IconButtonDensity::Compact, cx, {
                    let target = target.clone();
                    move |this, _, cx| {
                        if target.current(this) {
                            this.graph_fit(None, cx);
                        }
                    }
                }),
            )
            .child(
                IconButton::new(
                    ("focus-node", panel),
                    "",
                    self.tr("processing-workspace-locate"),
                )
                .surface(crate::gui::appearance::material::surface(cx))
                .enabled(selected)
                .build(IconButtonStyle::Quiet, IconButtonDensity::Compact, cx, {
                    let target = target.clone();
                    move |this, _, cx| {
                        if target.current(this)
                            && let Some(id) = this.graph_editor.selected
                        {
                            this.graph_fit(Some((id, id)), cx);
                        }
                    }
                }),
            )
            .child(
                IconButton::new(
                    ("reset-graph-view", panel),
                    "󰑐",
                    self.tr("processing-graph-reset-view"),
                )
                .surface(crate::gui::appearance::material::surface(cx))
                .build(IconButtonStyle::Quiet, IconButtonDensity::Compact, cx, {
                    let target = target.clone();
                    move |this, _, cx| {
                        if !target.current(this) {
                            return;
                        }
                        this.cancel_pointer(cx);
                        this.graph_editor.view = GraphView::default();
                        cx.notify();
                    }
                }),
            )
            .child(
                rivu_ui::icon("", cx)
                    .id(("graph-workspace-help", panel))
                    .opacity(0.6)
                    .hover(|style| style.opacity(1.))
                    .tooltip(rivu_ui::text_tooltip(
                        self.tr("processing-graph-editor-help"),
                    )),
            );
        let mut inspection = row()
            .flex_shrink_0()
            .max_w_full()
            .flex_wrap()
            .gap_1()
            .child(
                IconButton::new(
                    ("workbench-connections", panel),
                    "󰌷",
                    self.tr("processing-graph-connections"),
                )
                .surface(crate::gui::appearance::material::surface(cx))
                .toggled(self.graph_editor.workbench.connections)
                .build(IconButtonStyle::Quiet, IconButtonDensity::Compact, cx, {
                    let target = target.clone();
                    move |this, _, cx| {
                        if !target.current(this) {
                            return;
                        }
                        this.cancel_pointer(cx);
                        let state = &mut this.graph_editor.workbench;
                        state.connections = !state.connections;
                        state.input_epoch = state.input_epoch.wrapping_add(1);
                        cx.notify();
                    }
                }),
            )
            .child(
                IconButton::new(
                    ("workbench-parameters", panel),
                    "󰒓",
                    self.tr(if parameters_visible {
                        "processing-workspace-hide-inspector"
                    } else {
                        "processing-workspace-show-inspector"
                    }),
                )
                .surface(crate::gui::appearance::material::surface(cx))
                .enabled(selected)
                .toggled(parameters_visible)
                .build(IconButtonStyle::Quiet, IconButtonDensity::Compact, cx, {
                    let target = target.clone();
                    move |this, _, cx| {
                        if !target.current(this) {
                            return;
                        }
                        this.cancel_pointer(cx);
                        let state = &mut this.graph_editor.workbench;
                        if state.wide {
                            state.inspector_open = !state.inspector_open;
                        } else {
                            state.parameters = !state.parameters;
                        }
                        state.input_epoch = state.input_epoch.wrapping_add(1);
                        cx.notify();
                    }
                }),
            );
        if !self.graph_editor.workbench.wide && parameters_visible {
            inspection = inspection.child(
                crate::gui::appearance::material::button(
                    ("workbench-back", panel),
                    self.tr("processing-workspace-back-graph"),
                    cx,
                    move |this, _, cx| {
                        if !target.current(this) {
                            return;
                        }
                        this.cancel_pointer(cx);
                        let state = &mut this.graph_editor.workbench;
                        state.parameters = false;
                        state.input_epoch = state.input_epoch.wrapping_add(1);
                        cx.notify();
                    },
                )
                .flex_shrink_0()
                .whitespace_nowrap(),
            );
        }
        row()
            .flex_wrap()
            .gap_2()
            .child(editing.pr_2().border_r_1().border_color(rgb(theme.border)))
            .child(
                navigation
                    .pr_2()
                    .border_r_1()
                    .border_color(rgb(theme.border)),
            )
            .child(inspection)
    }

    fn graph_canvas_workspace(
        &mut self,
        panel: u64,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let busy = self.graph_busy();
        let mut view = column().size_full().min_h_0().gap_2().child(
            div()
                .flex_1()
                .min_h_0()
                .child(self.routing_canvas(panel, window, cx)),
        );
        if let Some(port) = self.graph_editor.connecting {
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
                    .child(crate::gui::appearance::material::button(
                        ("workbench-cancel-connect", panel),
                        self.tr("action-cancel"),
                        cx,
                        |this, _, cx| {
                            this.graph_editor.connecting = None;
                            cx.notify();
                        },
                    )),
            );
        }
        if self.graph_editor.workbench.connections {
            let mut connections = column().gap_1().p_1();
            for edge in &self.state.system.config.processing_graph.edges {
                let from = edge.from;
                let to = edge.to;
                connections = connections.child(
                    row()
                        .flex_wrap()
                        .gap_1()
                        .child(crate::gui::appearance::material::button(
                            SharedString::from(format!(
                                "locate-edge-{panel}-{}-{}",
                                to.node, to.port
                            )),
                            format!("#{}:{} → #{}:{}", from.node, from.port, to.node, to.port),
                            cx,
                            move |this, _, cx| {
                                this.graph_select(to.node, cx);
                                this.graph_editor.highlight_connection = Some(to);
                                this.graph_fit(Some((from.node, to.node)), cx);
                            },
                        ))
                        .child(disabled_control(
                            theme,
                            crate::gui::appearance::material::button(
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
        let mode = self.tr(if self.graph_editor.mode == GraphExecutionMode::Auto {
            "processing-graph-auto-short"
        } else {
            mode_label(self.graph_editor.mode)
        });
        let mode_control = self.workbench_dropdown(panel, WorkbenchMenu::Mode, mode, cx);
        let mut controls = row()
            .min_w_0()
            .flex_wrap()
            .items_center()
            .gap_1()
            .child(mode_control);
        let no_input = self.input_track().is_none();
        for (output, label) in [
            (
                false,
                if self.graph_editor.mode == GraphExecutionMode::Offline {
                    "processing-graph-calculate-selected"
                } else {
                    "processing-graph-preview"
                },
            ),
            (true, "processing-graph-run"),
        ] {
            let target = self
                .state
                .system
                .config
                .processing_graph
                .nodes
                .iter()
                .find(|node| {
                    if output {
                        matches!(node.kind, GraphNodeKind::Output)
                    } else {
                        Some(node.id) == self.graph_editor.selected
                    }
                });
            let port = if output {
                0
            } else {
                self.graph_editor.target_port
            };
            let audition_blocked = !output
                && self.graph_editor.mode != GraphExecutionMode::Offline
                && self.state.system.config.processing_bypassed;
            let mut hint = target.map_or_else(
                || self.tr(label).to_string(),
                |node| {
                    format!(
                        "{} · {} #{} · {} {}",
                        self.tr(label),
                        self.tr(node_label(&node.kind)),
                        node.id,
                        self.tr("processing-result-port"),
                        port,
                    )
                },
            );
            if audition_blocked {
                hint.push_str(&format!("\n{}", self.tr("processing-audition-bypassed")));
            }
            controls = controls.child(disabled_control(
                theme,
                rivu_ui::button_with_hint(
                    SharedString::from(format!("execute-{panel}-{output}")),
                    self.tr(label),
                    hint,
                    cx,
                    move |this, _, cx| this.graph_run_target(output, cx),
                )
                .flex_shrink_0()
                .whitespace_nowrap()
                .debug_selector(move || format!("execute-{panel}-{output}"))
                .when(output, |view| {
                    view.bg(rgb(theme.selected)).text_color(rgb(theme.accent))
                }),
                busy || no_input || target.is_none() || audition_blocked,
            ));
        }
        let mut footer = column()
            .flex_shrink_0()
            .gap_1()
            .py_1()
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
        footer
    }

    pub(in crate::gui) fn close_workbench_controls(&mut self) {
        let state = &mut self.graph_editor.workbench;
        state.input_epoch = state.input_epoch.wrapping_add(1);
        self.graph_editor.workbench_choices.close();
        self.graph_editor.workbench_menu = None;
        self.graph_editor.workbench_anchor.set(None);
        self.cancel_workbench_resize();
    }

    pub(in crate::gui) fn cancel_workbench_resize(&mut self) {
        let state = &mut self.graph_editor.workbench;
        state.resize = None;
        state.zoom_drag = None;
    }

    pub(in crate::gui) fn workbench_pointer_active(&self) -> bool {
        let state = &self.graph_editor.workbench;
        state.resize.is_some() || state.zoom_drag.is_some()
    }

    pub(in crate::gui) fn retain_workbench_controls(
        &mut self,
        layout: &crate::gui::workspace::WorkspacePanels,
        visible: bool,
    ) {
        if self
            .graph_editor
            .workbench
            .panel
            .is_some_and(|panel| !visible || !layout.is_panel_active(panel))
        {
            self.close_workbench_controls();
            let state = &mut self.graph_editor.workbench;
            state.panel = None;
            state.zoom_bounds.set(None);
            state.menu_bounds.set([None; 3]);
            state.canvas_focus = None;
            self.graph_editor.bounds.set(None);
        }
    }

    pub(super) fn workbench_input(&self, panel: u64) -> WorkbenchInput {
        WorkbenchInput {
            panel,
            bounds: Rc::clone(&self.graph_editor.workbench.zoom_bounds),
            input_epoch: self.graph_editor.workbench.input_epoch,
            geometry_epoch: self.geometry_epoch,
        }
    }

    pub(super) fn workbench_dropdown(
        &mut self,
        panel: u64,
        menu: WorkbenchMenu,
        label: SharedString,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let open = self.graph_editor.workbench_menu == Some((panel, menu))
            && self.graph_editor.workbench_choices.is_open();
        let hint = self.tr(match menu {
            WorkbenchMenu::Add => "processing-workspace-add-node",
            WorkbenchMenu::Mode => "processing-workspace-mode-help",
            WorkbenchMenu::Fft => "processing-fft",
        });
        let measured = Rc::clone(&self.graph_editor.workbench.menu_bounds);
        let trigger_bounds = Rc::clone(&measured);
        let target = self.workbench_input(panel);
        let owner = cx.entity().downgrade();
        let close_on_move = open;
        let measure = canvas(
            move |bounds, _, cx| {
                let mut anchors = measured.get();
                let previous = anchors[menu as usize].replace(bounds);
                measured.set(anchors);
                if close_on_move && previous.is_some_and(|previous| previous != bounds) {
                    let owner = owner.clone();
                    cx.defer(move |cx| {
                        let _ = owner.update(cx, |this, cx| {
                            if this.graph_editor.workbench_menu == Some((panel, menu)) {
                                this.close_workbench_controls();
                                cx.notify();
                            }
                        });
                    });
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        ChoiceSelect::new(
            SharedString::from(format!("workbench-menu-{panel}-{}", menu as u8)),
            label,
        )
        .build(
            self,
            move |this| {
                (this.graph_editor.workbench_menu == Some((panel, menu)))
                    .then_some(&mut this.graph_editor.workbench_choices)
            },
            cx,
            move |this, _, cx| {
                if !target.current(this) {
                    return;
                }
                if this.graph_editor.workbench_menu == Some((panel, menu))
                    && this.graph_editor.workbench_choices.is_open()
                {
                    this.close_workbench_controls();
                } else {
                    this.close_workbench_controls();
                    let Some(anchor) = trigger_bounds.get()[menu as usize] else {
                        return;
                    };
                    this.graph_editor.workbench_anchor.set(Some(anchor));
                    let (items, selected) = match menu {
                        WorkbenchMenu::Add => (
                            [
                                "processing-equalizer",
                                "processing-restoration",
                                "processing-speech",
                                "processing-separation",
                                "processing-mix",
                                "processing-graph-output",
                            ]
                            .into_iter()
                            .enumerate()
                            .map(|(index, label)| {
                                ChoiceItem::new(WorkbenchChoice::Add(index), this.tr(label))
                            })
                            .collect::<Vec<_>>(),
                            0,
                        ),
                        WorkbenchMenu::Fft => {
                            let Some(node) = this.graph_editor.selected.and_then(|id| {
                                this.state.system.config.processing_graph.node(id).ok()
                            }) else {
                                return;
                            };
                            if !matches!(node.kind, GraphNodeKind::Denoise { .. }) {
                                return;
                            }
                            let field = GraphField::Denoise(DenoiseField::FftSize);
                            let binding = NodeField::new(node, field);
                            let text = this.text(FieldKey::Graph(binding), cx);
                            let choices = field.choices();
                            let selected = choices
                                .iter()
                                .position(|(value, _)| *value == text)
                                .unwrap_or(0);
                            (
                                choices
                                    .iter()
                                    .map(|&(value, _)| {
                                        ChoiceItem::new(WorkbenchChoice::Fft(binding, value), value)
                                    })
                                    .collect(),
                                selected,
                            )
                        }
                        WorkbenchMenu::Mode => {
                            let modes = [
                                GraphExecutionMode::Auto,
                                GraphExecutionMode::Realtime,
                                GraphExecutionMode::Hybrid,
                                GraphExecutionMode::Offline,
                            ];
                            let selected = modes
                                .iter()
                                .position(|mode| *mode == this.graph_editor.mode)
                                .unwrap_or(0);
                            (
                                modes
                                    .into_iter()
                                    .map(|mode| {
                                        ChoiceItem::new(
                                            WorkbenchChoice::Mode(mode),
                                            this.tr(mode_label(mode)),
                                        )
                                    })
                                    .collect(),
                                selected,
                            )
                        }
                    };
                    this.graph_editor.workbench_menu = Some((panel, menu));
                    this.graph_editor.workbench_choices.open(items, selected);
                }
                cx.notify();
            },
            Self::choose_workbench_item,
            |this, _, cx| {
                this.close_workbench_controls();
                cx.notify();
            },
        )
        .w_auto()
        .flex_shrink_0()
        .relative()
        .aria_label(hint.clone())
        .tooltip(rivu_ui::text_tooltip(hint))
        .child(measure)
    }

    fn choose_workbench_item(&mut self, index: usize, _: &mut Window, cx: &mut Context<Self>) {
        let Some(choice) = self
            .graph_editor
            .workbench_choices
            .item(index)
            .map(|item| item.value)
        else {
            return;
        };
        let Some(_) = self.graph_editor.workbench_menu else {
            return;
        };
        self.close_workbench_controls();
        match choice {
            WorkbenchChoice::Add(index) => {
                if self.graph_busy() {
                    return;
                }
                let kind = match index {
                    0 => GraphNodeKind::Equalizer {
                        config: rivu_dsp::equalizer::config::EqualizerConfig {
                            enabled: true,
                            ..Default::default()
                        },
                        implementation: EqImplementation::Iir,
                    },
                    1 => GraphNodeKind::Denoise {
                        config: self.state.system.config.denoise.clone(),
                        profile: None,
                    },
                    2 => GraphNodeKind::Speech {
                        config: self.state.system.config.neural.clone(),
                    },
                    3 => GraphNodeKind::Separate {
                        config: self.state.system.config.neural.clone(),
                    },
                    4 => GraphNodeKind::Mix {
                        gains_db: vec![0.; 2],
                        muted: vec![false; 2],
                        solo: vec![false; 2],
                    },
                    _ => GraphNodeKind::Output,
                };
                self.graph_add(kind, cx);
            }
            WorkbenchChoice::Mode(mode) => self.graph_editor.mode = mode,
            WorkbenchChoice::Fft(binding, value) => {
                if self.graph_editor.selected == Some(binding.node)
                    && self
                        .state
                        .system
                        .config
                        .processing_graph
                        .node(binding.node)
                        .is_ok_and(|node| binding.matches(node))
                {
                    self.stage_graph_text(FieldKey::Graph(binding), value.to_owned(), cx);
                }
            }
        }
        cx.notify();
    }

    fn dismiss_workbench_menu(
        &mut self,
        _: &MouseDownEvent,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> bool {
        self.close_workbench_controls();
        true
    }

    fn inspector_resize_handle(&self, panel: u64, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = rivu_ui::theme::palette(cx);
        let pointer_target = self.workbench_input(panel);
        let key_target = pointer_target.clone();
        div()
            .id(("inspector-resize", panel))
            .debug_selector(move || format!("inspector-resize-{panel}"))
            .w_2()
            .h_full()
            .flex_shrink_0()
            .cursor_col_resize()
            .tab_index(0)
            .role(Role::Splitter)
            .aria_label(self.tr("processing-workspace-resize-inspector"))
            .hover(move |style| style.bg(rgb(theme.border)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    if !pointer_target.current(this) {
                        return;
                    }
                    this.cancel_pointer(cx);
                    let state = &mut this.graph_editor.workbench;
                    state.resize = Some((
                        event.position.x,
                        state.inspector_width,
                        WindowGeometry::current(window, this.state.system.config.ui_scale),
                    ));
                    cx.stop_propagation();
                }),
            )
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.modifiers != Modifiers::default() {
                    return;
                }
                let change = match event.keystroke.key.as_str() {
                    "left" => 1.,
                    "right" => -1.,
                    _ => return,
                };
                if !this.resize_inspector(&key_target, change, cx) {
                    return;
                }
                cx.stop_propagation();
                cx.notify();
            }))
    }

    fn resize_inspector(
        &mut self,
        target: &WorkbenchInput,
        change: f32,
        cx: &mut Context<Self>,
    ) -> bool {
        if !target.current(self) {
            return false;
        }
        let state = &mut self.graph_editor.workbench;
        if !state.wide || !state.inspector_open {
            return false;
        }
        state.inspector_width = (state.inspector_width + change).clamp(16., 42.);
        self.cancel_pointer(cx);
        cx.notify();
        true
    }

    fn inspector_resize_handlers(
        owner: WeakEntity<Self>,
        target: WorkbenchInput,
        window: &mut Window,
    ) {
        let move_target = target.clone();
        let moved = owner.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase != DispatchPhase::Capture {
                return;
            }
            let _ = moved.update(cx, |this, cx| {
                if !move_target.current(this) {
                    return;
                }
                // A removed panel can still have listeners in the last rendered frame.
                let state = &mut this.graph_editor.workbench;
                if let Some(geometry) = state.zoom_drag {
                    if event.pressed_button != Some(MouseButton::Left)
                        || !window.is_window_active()
                        || geometry
                            != WindowGeometry::current(window, this.state.system.config.ui_scale)
                    {
                        state.zoom_drag = None;
                    } else if let Some((bounds, measured_geometry)) = state.zoom_bounds.get()
                        && geometry == measured_geometry
                        && let Some(value) = Slider::value_at(bounds, event.position)
                    {
                        this.graph_zoom_to(0.35 + f64::from(value) * 1.65, [250., 160.], cx);
                    }
                    window.prevent_default();
                    cx.notify();
                    return;
                }
                let Some((start, width, geometry)) = state.resize else {
                    return;
                };
                if event.pressed_button != Some(MouseButton::Left)
                    || !window.is_window_active()
                    || geometry
                        != WindowGeometry::current(window, this.state.system.config.ui_scale)
                {
                    state.resize = None;
                    return;
                }
                state.inspector_width = (width
                    - f32::from(event.position.x - start) / f32::from(window.rem_size()))
                .clamp(16., 42.);
                window.prevent_default();
                cx.notify();
            });
        });
        let released = owner.clone();
        let release_target = target.clone();
        window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
            if phase == DispatchPhase::Capture {
                let _ = released.update(cx, |this, cx| {
                    if !release_target.current(this) {
                        return;
                    }
                    let state = &mut this.graph_editor.workbench;
                    let active = state.resize.take().is_some() | state.zoom_drag.take().is_some();
                    if active {
                        cx.notify();
                    }
                });
            }
        });
        window.on_mouse_event(move |_: &MouseExitEvent, phase, _, cx| {
            if phase == DispatchPhase::Capture {
                let _ = owner.update(cx, |this, cx| {
                    if !target.current(this) {
                        return;
                    }
                    let state = &mut this.graph_editor.workbench;
                    let active = state.resize.take().is_some() | state.zoom_drag.take().is_some();
                    if active {
                        cx.notify();
                    }
                });
            }
        });
    }

    fn graph_zoom_slider(&self, panel: u64, cx: &mut Context<Self>) -> Stateful<Div> {
        let state = &self.graph_editor.workbench;
        let bounds = state.zoom_bounds.clone();
        let measured = bounds.clone();
        let target = self.workbench_input(panel);
        let measured_target = target.clone();
        let owner = cx.entity().downgrade();
        let ui_scale = self.state.system.config.ui_scale;
        Slider::new(
            ("graph-zoom", panel),
            ((self.graph_editor.view.zoom - 0.35) / 1.65) as f32,
            self.tr("processing-workspace-zoom"),
        )
        .dragging(state.zoom_drag.is_some())
        .render(cx, move |this, input, window, cx| {
            if !target.current(this) {
                return;
            }
            this.cancel_pointer(cx);
            let zoom = match input {
                SliderInput::PointerDown { position, .. } => {
                    let Some((bounds, geometry)) = bounds.get() else {
                        return;
                    };
                    if geometry
                        != WindowGeometry::current(window, this.state.system.config.ui_scale)
                    {
                        return;
                    }
                    let Some(value) = Slider::value_at(bounds, position) else {
                        return;
                    };
                    let state = &mut this.graph_editor.workbench;
                    state.zoom_drag = Some(WindowGeometry::current(
                        window,
                        this.state.system.config.ui_scale,
                    ));
                    0.35 + f64::from(value) * 1.65
                }
                SliderInput::Key(key) => match key {
                    SliderKey::Start => 0.35,
                    SliderKey::End => 2.,
                    SliderKey::Decrease => (this.graph_editor.view.zoom - 0.05).max(0.35),
                    SliderKey::Increase => (this.graph_editor.view.zoom + 0.05).clamp(0.35, 2.),
                },
            };
            this.graph_zoom_to(zoom, [250., 160.], cx);
            cx.stop_propagation();
        })
        .debug_selector(move || format!("graph-zoom-{panel}"))
        .w_24()
        .flex_shrink_0()
        .tooltip(rivu_ui::text_tooltip(format!(
            "{:.0}%",
            self.graph_editor.view.zoom * 100.
        )))
        .child(
            canvas(
                move |bounds, window, cx| {
                    if let Some(owner) = owner.upgrade()
                        && measured_target.current(owner.read(cx))
                    {
                        measured.set(Some((bounds, WindowGeometry::current(window, ui_scale))));
                    }
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
    }

    fn graph_fit(&mut self, nodes: Option<(u64, u64)>, cx: &mut Context<Self>) {
        let Some((bounds, _)) = self.graph_editor.bounds.get() else {
            return;
        };
        let size = [f32::from(bounds.size.width), f32::from(bounds.size.height)];
        if let Some(view) = fitted_view(&self.state.system.config.processing_graph, size, nodes) {
            self.cancel_pointer(cx);
            self.graph_editor.view = view;
            cx.notify();
        }
    }
}

fn mode_label(mode: GraphExecutionMode) -> &'static str {
    match mode {
        GraphExecutionMode::Auto => "processing-graph-auto",
        GraphExecutionMode::Realtime => "processing-graph-realtime",
        GraphExecutionMode::Hybrid => "processing-graph-hybrid",
        GraphExecutionMode::Offline => "processing-graph-offline",
    }
}

fn fitted_view(
    graph: &ProcessingGraph,
    size: [f32; 2],
    nodes: Option<(u64, u64)>,
) -> Option<GraphView> {
    if size[0] <= 48. || size[1] <= 48. {
        return None;
    }
    let size = size.map(f64::from);
    let origin = graph
        .nodes
        .iter()
        .find(|node| nodes.is_none_or(|(a, b)| node.id == a || node.id == b))?
        .position
        .map(f64::from);
    let mut extent: Option<([f64; 2], [f64; 2])> = None;
    for node in &graph.nodes {
        if nodes.is_some_and(|(a, b)| node.id != a && node.id != b) {
            continue;
        }
        // Rebase before adding card dimensions: adding 220 to f32::MAX even
        // in f64 would erase the entire width and incorrectly fit at infinity.
        let min = std::array::from_fn(|axis| f64::from(node.position[axis]) - origin[axis]);
        let max = [
            min[0] + f64::from(NODE_WIDTH),
            min[1] + f64::from(node_height(&node.kind)),
        ];
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
    Some(GraphView {
        origin,
        pan: std::array::from_fn(|axis| {
            (size[axis] - (max[axis] - min[axis]) * zoom) * 0.5 - min[axis] * zoom
        }),
        zoom,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::CoreRuntime;
    use core::prelude::v1::test;
    use gpui::InputEvent as _;

    const PANEL: u64 = 7;

    #[derive(Clone, Copy)]
    enum ControlTransition {
        Hide,
        Replace,
    }

    struct ControlsWindow {
        owner: Entity<AudioProcessing>,
        zoom_focus: FocusHandle,
        resize_focus: FocusHandle,
        inspector: bool,
        canvas: bool,
        before_key: Rc<Cell<Option<ControlTransition>>>,
    }

    impl Render for ControlsWindow {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.owner.update(cx, |this, cx| {
                if self.inspector {
                    return this.graph_inspector(PANEL, window, cx);
                }
                let before_key = self.before_key.clone();
                column()
                    .size_full()
                    .capture_key_down(cx.listener(move |this, _: &KeyDownEvent, _, _| {
                        match before_key.take() {
                            Some(ControlTransition::Hide) => {
                                this.retain_workbench_controls(&Default::default(), false);
                            }
                            Some(ControlTransition::Replace) => {
                                this.graph_editor.workbench = WorkbenchPanel {
                                    panel: Some(PANEL),
                                    wide: true,
                                    ..Default::default()
                                };
                            }
                            None => {}
                        }
                    }))
                    .child(
                        this.graph_zoom_slider(PANEL, cx)
                            .track_focus(&self.zoom_focus),
                    )
                    .child(
                        this.inspector_resize_handle(PANEL, cx)
                            .h(px(80.))
                            .track_focus(&self.resize_focus),
                    )
                    .when(self.canvas, |view| {
                        view.child(
                            div()
                                .flex_1()
                                .min_h_0()
                                .w_full()
                                .child(this.routing_canvas(PANEL, window, cx)),
                        )
                    })
            })
        }
    }

    fn fixture(
        cx: &mut TestAppContext,
        inspector: bool,
    ) -> (
        tempfile::TempDir,
        CoreRuntime,
        Entity<AudioProcessing>,
        AnyWindowHandle,
        FocusHandle,
        FocusHandle,
    ) {
        cx.executor().allow_parking();
        cx.update(Input::init);
        let mut config = crate::gui::audio_processing::test_support::config();
        for id in [3, 4] {
            config.processing_graph.nodes.push(GraphNode {
                id,
                position: [360., 240.],
                kind: GraphNodeKind::Separate {
                    config: Default::default(),
                },
            });
        }
        config.processing_graph.next_id = 5;
        let (directory, runtime, owner) =
            crate::gui::audio_processing::test_support::owner(cx, config);
        owner.update(cx, |this, cx| {
            this.graph_select(3, cx);
            this.graph_editor.workbench.wide = true;
            this.graph_editor.workbench.panel = Some(PANEL);
            this.command_active = true;
        });
        let zoom_focus = cx.update(|cx| cx.focus_handle());
        let resize_focus = cx.update(|cx| cx.focus_handle());
        let window = cx.open_window(size(px(900.), px(1200.)), {
            let owner = owner.clone();
            let zoom_focus = zoom_focus.clone();
            let resize_focus = resize_focus.clone();
            move |_, _| ControlsWindow {
                owner,
                zoom_focus,
                resize_focus,
                inspector,
                canvas: false,
                before_key: Default::default(),
            }
        });
        let handle = window.into();
        draw(cx, handle);
        (directory, runtime, owner, handle, zoom_focus, resize_focus)
    }

    fn draw(cx: &mut TestAppContext, handle: AnyWindowHandle) {
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        })
        .unwrap();
    }

    #[gpui::test]
    fn fit_extreme_origin_renders_scaled_cards_and_preserves_pan_and_node_drag_contracts(
        cx: &mut TestAppContext,
    ) {
        let (_directory, _runtime, owner, handle, _, _) = fixture(cx, false);
        cx.update_window(handle, |root, _, cx| {
            root.downcast::<ControlsWindow>()
                .unwrap()
                .update(cx, |this, cx| {
                    this.canvas = true;
                    cx.notify();
                });
        })
        .unwrap();
        draw(cx, handle);
        owner.update(cx, |this, cx| {
            let graph = &mut Arc::make_mut(&mut Rc::make_mut(&mut this.state).system.config)
                .processing_graph;
            for node in &mut graph.nodes {
                node.position = [f32::MAX, f32::MAX];
            }
            graph.validate().unwrap();
            this.command_active = false;
            this.graph_fit(None, cx);
        });
        draw(cx, handle);
        let fitted = VisualTestContext::from_window(handle, cx)
            .debug_bounds("graph-card-7-4")
            .unwrap();
        assert!((f32::from(fitted.size.width) - NODE_WIDTH).abs() < 0.1);
        owner.update(cx, |this, cx| {
            let anchor = this
                .graph_editor
                .view
                .screen([f64::from(f32::MAX); 2], [0.; 2])
                .map(|v| v as f32);
            this.graph_zoom_to(1.2, anchor, cx);
        });
        draw(cx, handle);
        let zoomed = VisualTestContext::from_window(handle, cx)
            .debug_bounds("graph-card-7-4")
            .unwrap();
        assert!((f32::from(zoomed.size.width) - NODE_WIDTH * 1.2).abs() < 0.1);
        assert!((f32::from(zoomed.origin.x - fitted.origin.x)).abs() < 0.1);
        assert!((f32::from(zoomed.origin.y - fitted.origin.y)).abs() < 0.1);
        let start = zoomed.origin + point(px(16.), px(16.));
        let end = start + point(px(48.), px(-24.));
        cx.update_window(handle, |_, window, cx| {
            owner.update(cx, |this, cx| {
                this.graph_start_pointer(
                    PANEL,
                    None,
                    &MouseDownEvent {
                        button: MouseButton::Middle,
                        position: start,
                        ..Default::default()
                    },
                    window,
                    cx,
                );
                this.graph_move_pointer(end, window, cx);
                let before = this
                    .graph_editor
                    .view
                    .screen([f64::from(f32::MAX); 2], [0.; 2]);
                let preview = this.graph_view().screen([f64::from(f32::MAX); 2], [0.; 2]);
                assert_eq!(preview, [before[0] + 48., before[1] - 24.]);
            });
            window.dispatch_event(
                MouseUpEvent {
                    button: MouseButton::Middle,
                    position: end,
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
        })
        .unwrap();
        draw(cx, handle);
        let panned = VisualTestContext::from_window(handle, cx)
            .debug_bounds("graph-card-7-4")
            .unwrap();
        assert!((f32::from(panned.origin.x - zoomed.origin.x) - 48.).abs() < 0.1);
        assert!((f32::from(panned.origin.y - zoomed.origin.y) + 24.).abs() < 0.1);
        cx.update_window(handle, |_, window, cx| {
            owner.update(cx, |this, cx| {
                assert!(this.graph_editor.pointer.is_none());
                this.graph_start_pointer(
                    PANEL,
                    Some(4),
                    &MouseDownEvent {
                        button: MouseButton::Left,
                        position: end,
                        ..Default::default()
                    },
                    window,
                    cx,
                );
                this.graph_move_pointer(end + point(px(32.), px(24.)), window, cx);
                assert_eq!(
                    this.graph_editor.pointer.as_ref().unwrap().preview,
                    [f64::from(f32::MAX); 2]
                );
            });
            window.dispatch_event(
                MouseUpEvent {
                    button: MouseButton::Left,
                    position: end + point(px(32.), px(24.)),
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
        })
        .unwrap();
        owner.read_with(cx, |this, _| {
            assert!(this.graph_editor.pointer.is_none());
            assert!(this.graph_editor.pending.is_none());
            assert_eq!(
                this.state
                    .system
                    .config
                    .processing_graph
                    .node(4)
                    .unwrap()
                    .position,
                [f32::MAX; 2]
            );
        });
    }

    fn dispatch_key(window: &mut Window, cx: &mut App, key: &str) {
        let keystroke = Keystroke::parse(key).unwrap();
        window.dispatch_event(
            KeyDownEvent {
                keystroke: keystroke.clone(),
                is_held: false,
                prefer_character_input: false,
            }
            .to_platform_input(),
            cx,
        );
        window.dispatch_event(KeyUpEvent { keystroke }.to_platform_input(), cx);
    }

    fn click(window: &mut Window, cx: &mut App, position: Point<Pixels>) {
        window.dispatch_event(
            MouseDownEvent {
                button: MouseButton::Left,
                position,
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
        window.dispatch_event(
            MouseUpEvent {
                button: MouseButton::Left,
                position,
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
    }

    #[gpui::test]
    fn zoom_and_inspector_keys_preserve_ranges_without_a_source_format(cx: &mut TestAppContext) {
        let (_directory, _runtime, owner, handle, zoom_focus, resize_focus) = fixture(cx, false);
        owner.read_with(cx, |this, _| {
            assert!(this.state.current_track().is_none());
            assert!(this.state.playback.duration.is_none());
        });
        cx.update_window(handle, |_, window, cx| {
            window.focus(&zoom_focus, cx);
            for _ in 0..80 {
                dispatch_key(window, cx, "right");
            }
            assert_eq!(owner.read(cx).graph_editor.view.zoom, 2.);
            dispatch_key(window, cx, "home");
            for _ in 0..80 {
                dispatch_key(window, cx, "left");
            }
            assert_eq!(owner.read(cx).graph_editor.view.zoom, 0.35);
            window.focus(&resize_focus, cx);
            for _ in 0..80 {
                dispatch_key(window, cx, "left");
            }
            assert_eq!(owner.read(cx).graph_editor.workbench.inspector_width, 42.);
            for _ in 0..80 {
                dispatch_key(window, cx, "right");
            }
            dispatch_key(window, cx, "ctrl-left");
            assert_eq!(owner.read(cx).graph_editor.workbench.inspector_width, 16.);
        })
        .unwrap();
    }

    #[gpui::test]
    fn old_zoom_and_resize_callbacks_cannot_edit_hidden_or_replaced_panels(
        cx: &mut TestAppContext,
    ) {
        let (_directory, _runtime, owner, handle, zoom_focus, resize_focus) = fixture(cx, false);
        // GPUI redraws dirty windows before keyboard dispatch. Invalidate in capture,
        // after that redraw but before the real, now-stale control listener bubbles.
        for transition in [ControlTransition::Hide, ControlTransition::Replace] {
            for (focus, key) in [(&zoom_focus, "end"), (&resize_focus, "left")] {
                cx.update_window(handle, |root, window, cx| {
                    let root = root.downcast::<ControlsWindow>().unwrap();
                    let before_key = root.read(cx).before_key.clone();
                    before_key.set(Some(transition));
                    window.focus(focus, cx);
                    dispatch_key(window, cx, key);
                    assert!(before_key.get().is_none());
                    owner.read_with(cx, |this, _| {
                        assert_eq!(this.graph_editor.view.zoom, 1.);
                        assert_eq!(this.graph_editor.workbench.inspector_width, 25.);
                    });
                })
                .unwrap();
            }
        }
        draw(cx, handle);
        cx.update_window(handle, |_, window, cx| {
            window.focus(&resize_focus, cx);
            dispatch_key(window, cx, "left");
            assert_eq!(owner.read(cx).graph_editor.workbench.inspector_width, 26.);
            window.focus(&zoom_focus, cx);
            dispatch_key(window, cx, "end");
            assert_eq!(owner.read(cx).graph_editor.view.zoom, 2.);
        })
        .unwrap();
    }

    #[gpui::test]
    fn stale_inspector_delete_cannot_remove_the_new_selection(cx: &mut TestAppContext) {
        let (_directory, _runtime, owner, handle, _, _) = fixture(cx, true);
        let position = VisualTestContext::from_window(handle, cx)
            .debug_bounds("graph-delete-7")
            .unwrap()
            .center();
        cx.update_window(handle, |_, window, cx| {
            owner.update(cx, |this, cx| this.graph_select(4, cx));
            click(window, cx, position);
            assert!(owner.read(cx).commands.is_empty());
        })
        .unwrap();
        draw(cx, handle);
        click_window(cx, handle, position);
        owner.read_with(cx, |this, _| {
            assert!(matches!(
                &this.commands.front().unwrap().command,
                Command::ProcessingGraphEdit { edit, .. } if matches!(edit.as_ref(), GraphEdit::Remove { id: 4 })
            ));
        });
    }

    fn click_window(cx: &mut TestAppContext, handle: AnyWindowHandle, position: Point<Pixels>) {
        cx.update_window(handle, |_, window, cx| click(window, cx, position))
            .unwrap();
    }

    #[gpui::test]
    fn stale_inspector_delete_cannot_remove_a_replaced_node_kind(cx: &mut TestAppContext) {
        let (_directory, runtime, owner, handle, _, _) = fixture(cx, true);
        let position = VisualTestContext::from_window(handle, cx)
            .debug_bounds("graph-delete-7")
            .unwrap()
            .center();
        let revision = runtime.handle.config_snapshot().processing_graph.revision;
        let response =
            futures::executor::block_on(runtime.handle.request_ack(Command::ProcessingGraphEdit {
                expected_revision: revision,
                edit: Box::new(GraphEdit::Replace {
                    id: 3,
                    kind: GraphNodeKind::Output,
                }),
            }));
        assert!(response.ack.is_success());
        click_window(cx, handle, position);
        assert!(owner.read_with(cx, |this, _| this.commands.is_empty()));
        assert!(matches!(
            runtime
                .handle
                .config_snapshot()
                .processing_graph
                .node(3)
                .unwrap()
                .kind,
            GraphNodeKind::Output
        ));
    }

    #[test]
    fn fit_keeps_distant_nodes_inside_canvas_and_focuses_only_requested_nodes() {
        let mut graph = ProcessingGraph::default();
        graph.nodes[0].position = [-4000., -1500.];
        graph.nodes[1].position = [8000., 3500.];
        let size = [640., 320.];
        let view = fitted_view(&graph, size, None).unwrap();
        for node in &graph.nodes {
            let [left, top] = view.screen(node.position.map(f64::from), [0.; 2]);
            assert!(left >= 0. && top >= 0.);
            assert!(left + f64::from(NODE_WIDTH) * view.zoom <= f64::from(size[0]));
            assert!(top + f64::from(node_height(&node.kind)) * view.zoom <= f64::from(size[1]));
        }
        let id = graph.nodes[0].id;
        let view = fitted_view(&graph, size, Some((id, id))).unwrap();
        let center = view.screen(
            graph.nodes[0].position.map(f64::from),
            [f64::from(NODE_WIDTH) * 0.5, 0.],
        )[0];
        assert!((center - f64::from(size[0]) * 0.5).abs() < 0.01);
        assert!(fitted_view(&graph, [40., 40.], None).is_none());
    }

    #[test]
    fn fit_coincident_extreme_nodes_preserves_card_and_port_geometry_after_zoom() {
        let mut graph = ProcessingGraph::default();
        for node in &mut graph.nodes {
            node.position = [f32::MAX, -f32::MAX];
        }
        graph.validate().unwrap();
        let size = [640., 320.];
        let mut view = fitted_view(&graph, size, None).unwrap();
        assert_eq!(view.zoom, 1.);
        let position = graph.nodes[0].position.map(f64::from);
        let height = graph
            .nodes
            .iter()
            .map(|node| node_height(&node.kind))
            .fold(0., f32::max);
        let local_center = [f64::from(NODE_WIDTH) * 0.5, f64::from(height) * 0.5];
        let anchor = [320., 160.];
        assert_eq!(view.screen(position, local_center), anchor);
        view.zoom_to(1.2, anchor);
        assert_eq!(view.screen(position, local_center), anchor);
        let left = view.screen(position, [0.; 2]);
        let port = view.screen(
            position,
            [f64::from(NODE_WIDTH), f64::from(HEADER + PORT_ROW * 0.5)],
        );
        assert!((left[0] - (320. - f64::from(NODE_WIDTH) * 0.6)).abs() < 1e-10);
        assert!((port[0] - left[0] - f64::from(NODE_WIDTH) * 1.2).abs() < 1e-10);
        assert!((port[1] - left[1] - f64::from(HEADER + PORT_ROW * 0.5) * 1.2).abs() < 1e-10);
        assert_eq!(
            view.placement(anchor, local_center),
            graph.nodes[0].position
        );
        // A sub-ULP drag must preview the same f32 position it can persist.
        assert_eq!(view.dragged(position, [32., -24.]), position);
    }

    #[test]
    fn fit_extreme_span_supports_relative_zoom_drag_and_absolute_anchor_zoom() {
        let mut graph = ProcessingGraph::default();
        graph.nodes[0].position = [-f32::MAX, -f32::MAX];
        graph.nodes[1].position = [f32::MAX, f32::MAX];
        let mut view = fitted_view(&graph, [640., 320.], None).unwrap();
        let position = graph.nodes[1].position.map(f64::from);
        let anchor = view.screen(position, [0.; 2]);
        let fit_zoom = view.zoom;
        view.zoom_to(fit_zoom * 1.2, anchor);
        assert!((view.zoom / fit_zoom - 1.2).abs() < 1e-12);
        assert_eq!(view.screen(position, [0.; 2]), anchor);
        let dragged = view.dragged(position, [-32., -24.]);
        let screen = view.screen(dragged, [0.; 2]);
        assert!((screen[0] - anchor[0] + 32.).abs() < 0.001);
        assert!((screen[1] - anchor[1] + 24.).abs() < 0.001);
        graph.nodes[1].position = graph_stored_position(dragged);
        graph.validate().unwrap();
        view.zoom_to(1.2, anchor);
        assert_eq!(view.screen(position, [0.; 2]), anchor);
        let edge = view.screen(position, [f64::from(NODE_WIDTH), f64::from(HEADER)]);
        assert!((edge[0] - anchor[0] - f64::from(NODE_WIDTH) * 1.2).abs() < 1e-10);
        assert!((edge[1] - anchor[1] - f64::from(HEADER) * 1.2).abs() < 1e-10);
    }

    #[test]
    fn ordinary_zoom_keeps_anchor_and_add_placement_in_world_coordinates() {
        let mut view = GraphView {
            origin: [2000., -1500.],
            ..GraphView::default()
        };
        let position = [1234., 512.];
        let anchor = [250., 160.];
        let before = view.screen(position, [0.; 2]);
        view.zoom_to(1.2, anchor);
        let after = view.screen(position, [0.; 2]);
        for axis in 0..2 {
            assert!(
                (after[axis] - (anchor[axis] + (before[axis] - anchor[axis]) * 1.2)).abs() < 1e-10
            );
        }
        let local = [f64::from(NODE_WIDTH) * 0.5, 38.];
        let placed = view.placement(anchor, local).map(f64::from);
        let center = view.screen(placed, local);
        assert!((center[0] - anchor[0]).abs() < 0.001);
        assert!((center[1] - anchor[1]).abs() < 0.001);
        let dragged = view.dragged(position, [48., -24.]);
        assert_eq!(dragged, [1274., 492.]);
    }

    #[test]
    fn edge_clipping_preserves_visible_cubic_and_extreme_cross_canvas_segments() {
        let clip = [[0., 0.], [640., 320.]];
        let ordinary = [[20., 40.], [80., 40.], [160., 100.], [220., 100.]];
        let mut segments = Vec::new();
        clipped_graph_curve(ordinary, clip, &mut |curve| segments.push(curve));
        assert_eq!(segments, [ordinary]);
        segments.clear();
        let extent = f64::from(f32::MAX) * 2.;
        clipped_graph_curve(
            [
                [-extent, 160.],
                [-extent * 0.5, 160.],
                [extent * 0.5, 160.],
                [extent, 160.],
            ],
            clip,
            &mut |curve| segments.push(curve),
        );
        assert!(!segments.is_empty());
        assert!(segments.first().unwrap()[0][0] <= 0.25);
        assert!(segments.last().unwrap()[3][0] >= 639.75);
        for curve in segments {
            for point in curve {
                assert!((0. ..=640.).contains(&point[0]));
                assert_eq!(point[1], 160.);
            }
        }
    }

    #[test]
    fn fit_keeps_valid_extreme_coordinate_spans_finite_and_visible() {
        let mut graph = ProcessingGraph::default();
        graph.nodes[0].position = [-f32::MAX, -f32::MAX];
        graph.nodes[1].position = [f32::MAX, f32::MAX];
        graph.validate().unwrap();
        let size = [640., 320.];
        let view = fitted_view(&graph, size, None).unwrap();
        assert!(view.zoom > 0. && view.zoom < 0.35);
        for node in &graph.nodes {
            let screen = view.screen(node.position.map(f64::from), [0.; 2]);
            for axis in 0..2 {
                assert!((0. ..=f64::from(size[axis])).contains(&screen[axis]));
            }
        }
    }

    #[gpui::test]
    fn selecting_another_node_retires_native_edits_and_preedit_without_losing_drafts(
        cx: &mut TestAppContext,
    ) {
        use gpui::EntityInputHandler as _;

        let (_directory, _runtime, owner, handle, _, _) = fixture(cx, true);
        let key = owner.read_with(cx, |this, _| {
            FieldKey::Graph(NodeField::new(
                this.state.system.config.processing_graph.node(3).unwrap(),
                GraphField::Neural(NeuralField::Threads),
            ))
        });
        let old = owner.read_with(cx, |this, _| this.fields[&key].input.clone());
        cx.update_window(handle, |_, window, cx| {
            old.update(cx, |input, cx| {
                input.focus_and_select_all(window, cx);
                input.replace_text_in_range(None, "2", window, cx);
            });
        })
        .unwrap();
        draw(cx, handle);
        cx.update_window(handle, |_, window, cx| {
            old.update(cx, |input, cx| {
                input.replace_and_mark_text_in_range(None, "临", None, window, cx);
                assert!(input.is_composing());
            });
        })
        .unwrap();
        draw(cx, handle);

        // Selection invalidates native authority before the next frame is drawn.
        owner.update(cx, |this, cx| this.graph_select(4, cx));
        cx.update_window(handle, |_, window, cx| {
            old.update(cx, |input, cx| {
                input.replace_text_in_range(None, "99", window, cx);
                input.replace_and_mark_text_in_range(None, "迟到", None, window, cx);
                assert_eq!(input.text(), "2");
                assert!(!input.is_composing());
            });
        })
        .unwrap();
        draw(cx, handle);
        owner.read_with(cx, |this, cx| {
            assert!(this.fields[&key].dirty);
            assert_eq!(this.text(key, cx), "2");
        });

        owner.update(cx, |this, cx| this.graph_select(3, cx));
        draw(cx, handle);
        let current = owner.read_with(cx, |this, _| this.fields[&key].input.clone());
        cx.update_window(handle, |_, window, cx| {
            old.update(cx, |input, cx| {
                input.replace_text_in_range(None, "88", window, cx);
            });
            current.update(cx, |input, cx| {
                assert_eq!(input.text(), "2");
                assert!(!input.is_composing());
                input.focus_and_select_all(window, cx);
                input.replace_text_in_range(None, "6", window, cx);
            });
        })
        .unwrap();
        draw(cx, handle);
        owner.read_with(cx, |this, cx| assert_eq!(this.text(key, cx), "6"));
    }
}
