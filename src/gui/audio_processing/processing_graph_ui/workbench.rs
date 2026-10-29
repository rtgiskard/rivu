//! Responsive graph workbench. View choices and scroll positions are panel-local;
//! committed graph, selection and task ownership remain with their existing owners.
use super::*;
use rivu_ui::{
    ChoiceAction, ChoiceItem, ChoiceMode, ChoicePopup, IconButton, IconButtonDensity,
    IconButtonStyle, Slider, SliderInput, SliderKey, activate_control, dropdown_trigger,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum WorkbenchMenu {
    Add,
    Mode,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum WorkbenchChoice {
    Add(usize),
    Mode(GraphExecutionMode),
}

pub(super) struct WorkbenchPanel {
    wide: bool,
    parameters: bool,
    inspector_open: bool,
    inspector_width: f32,
    resize: Option<(Pixels, f32, DockGeometry)>,
    zoom_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    zoom_drag: Option<DockGeometry>,
    connections: bool,
    menu_bounds: Rc<Cell<[Option<Bounds<Pixels>>; 2]>>,
    pub(super) canvas_focus: Option<FocusHandle>,
}

impl Default for WorkbenchPanel {
    fn default() -> Self {
        Self {
            wide: false,
            parameters: false,
            inspector_open: true,
            inspector_width: 25.,
            resize: None,
            connections: false,
            zoom_bounds: Rc::new(Cell::new(None)),
            zoom_drag: None,
            menu_bounds: Rc::new(Cell::new([None; 2])),
            canvas_focus: None,
        }
    }
}

impl AudioProcessing {
    pub(in crate::gui) fn deactivate_graph_inputs(
        &self,
        panel: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let export_visible = self.page == Page::Results
            && self.state.system.processing.as_ref().is_some_and(|task| {
                task.operation == ProcessingOperation::Graph
                    && task.phase == ProcessingPhase::Complete
            });
        if self
            .graph_ui
            .panel_inputs
            .get(&panel)
            .is_some_and(|fields| {
                fields.iter().any(|(key, field)| {
                    // Results owns its export editor even though graph parameters are hidden.
                    (*key != FieldKey::GraphExport || !export_visible)
                        && field.input.read(cx).is_focused(window)
                })
            })
        {
            window.blur(cx);
        }
    }
    pub(in crate::gui) fn show_graph_results(&mut self, _panel: u64, cx: &mut Context<Self>) {
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
        canvas(
            move |bounds, window, cx| {
                let wide = bounds.size.width >= window.rem_size() * 64.;
                let _ = owner.update(cx, |this, cx| {
                    let state = this.graph_ui.workbenches.entry(panel).or_default();
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
        self.reconcile_graph(cx);
        let state = self.graph_ui.workbenches.entry(panel).or_default();
        let wide = state.wide;
        let parameters = state.parameters;
        let inspector_width = state.inspector_width;
        let selected = self.graph_ui.selected.is_some_and(|id| {
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
        if self
            .graph_ui
            .panel_inputs
            .get(&panel)
            .is_some_and(|fields| {
                fields.iter().any(|(key, field)| {
                    (*key == FieldKey::GraphExport || !parameters_visible)
                        && field.input.read(cx).is_focused(window)
                })
            })
        {
            window.blur(cx);
        }
        let toolbar = self.graph_toolbar(panel, selected, parameters_visible, cx);
        let body = if !wide && parameters_visible {
            let inspector = self.graph_inspector(panel, window, cx);
            self.processing_scroll(panel, "processing-inspector", inspector)
        } else {
            let graph = self.graph_canvas_workspace(panel, cx);
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
        let mut view = column()
            .size_full()
            .min_h_0()
            .gap_1()
            .child(
                canvas(
                    |_, _, _| {},
                    move |_, _, window, _| {
                        Self::inspector_resize_handlers(resize_owner.clone(), panel, window);
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
            .graph_ui
            .workbench_menu
            .is_some_and(|(owner, _)| owner == panel)
            && self.graph_ui.workbench_choices.is_open()
            && let Some(anchor) = self.graph_ui.workbench_anchor.get()
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
                    |this| &this.graph_ui.workbench_choices,
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
            let empty = if redo {
                self.graph_ui.redo.is_empty()
            } else {
                self.graph_ui.undo.is_empty()
            };
            editing = editing.child(
                IconButton::new(
                    SharedString::from(format!("history-{panel}-{redo}")),
                    glyph,
                    self.tr(label),
                )
                .enabled(!busy && !empty && !drafts)
                .build(
                    IconButtonStyle::Quiet,
                    IconButtonDensity::Compact,
                    cx,
                    move |this, _, cx| this.graph_history(redo, cx),
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
                .build(
                    IconButtonStyle::Quiet,
                    IconButtonDensity::Compact,
                    cx,
                    move |this, _, cx| this.graph_fit(panel, None, cx),
                ),
            )
            .child(
                IconButton::new(
                    ("focus-node", panel),
                    "",
                    self.tr("processing-workspace-locate"),
                )
                .enabled(selected)
                .build(
                    IconButtonStyle::Quiet,
                    IconButtonDensity::Compact,
                    cx,
                    move |this, _, cx| {
                        if let Some(id) = this.graph_ui.selected {
                            this.graph_fit(panel, Some((id, id)), cx);
                        }
                    },
                ),
            )
            .child(
                IconButton::new(
                    ("reset-graph-view", panel),
                    "󰑐",
                    self.tr("processing-graph-reset-view"),
                )
                .build(
                    IconButtonStyle::Quiet,
                    IconButtonDensity::Compact,
                    cx,
                    |this, _, cx| {
                        this.cancel_pointer(cx);
                        this.graph_ui.pan = [24., 24.];
                        this.graph_ui.zoom = 1.;
                        cx.notify();
                    },
                ),
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
                .toggled(self.graph_ui.workbenches[&panel].connections)
                .build(
                    IconButtonStyle::Quiet,
                    IconButtonDensity::Compact,
                    cx,
                    move |this, _, cx| {
                        this.cancel_pointer(cx);
                        let state = this.graph_ui.workbenches.entry(panel).or_default();
                        state.connections = !state.connections;
                        cx.notify();
                    },
                ),
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
                .enabled(selected)
                .toggled(parameters_visible)
                .build(
                    IconButtonStyle::Quiet,
                    IconButtonDensity::Compact,
                    cx,
                    move |this, _, cx| {
                        this.cancel_pointer(cx);
                        let state = this.graph_ui.workbenches.entry(panel).or_default();
                        if state.wide {
                            state.inspector_open = !state.inspector_open;
                        } else {
                            state.parameters = !state.parameters;
                        }
                        cx.notify();
                    },
                ),
            );
        if !self.graph_ui.workbenches[&panel].wide && parameters_visible {
            inspection = inspection.child(
                button(
                    ("workbench-back", panel),
                    self.tr("processing-workspace-back-graph"),
                    cx,
                    move |this, _, cx| {
                        this.cancel_pointer(cx);
                        this.graph_ui
                            .workbenches
                            .entry(panel)
                            .or_default()
                            .parameters = false;
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

    fn graph_canvas_workspace(&mut self, panel: u64, cx: &mut Context<Self>) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let busy = self.graph_busy();
        let mut view = column().size_full().min_h_0().gap_2().child(
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
        let mode = self.tr(mode_label(self.graph_ui.mode));
        let mode_control = self.workbench_dropdown(panel, WorkbenchMenu::Mode, mode, cx);
        let mut controls = row().flex_wrap().gap_2().child(
            row()
                .flex_shrink_0()
                .gap_1()
                .pr_2()
                .border_r_1()
                .border_color(rgb(theme.border))
                .child(caption(theme, self.tr("processing-workspace-mode")).whitespace_nowrap())
                .child(mode_control),
        );
        let no_input = self.input_track().is_none();
        for (output, label) in [
            (false, "processing-graph-preview"),
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
                        Some(node.id) == self.graph_ui.selected
                    }
                });
            let port = if output { 0 } else { self.graph_ui.target_port };
            let label = target.map_or_else(
                || self.tr(label).to_string(),
                |node| {
                    format!(
                        "{} · #{} · {}",
                        self.tr(label),
                        node.id,
                        self.graph_output_label(&node.kind, port as usize),
                    )
                },
            );
            controls = controls.child(disabled_control(
                theme,
                button(
                    SharedString::from(format!("execute-{panel}-{output}")),
                    label,
                    cx,
                    move |this, _, cx| this.graph_run_target(output, cx),
                )
                .gap_1()
                .flex_shrink_0()
                .whitespace_nowrap()
                .child(rivu_ui::icon(if output { "󰑮" } else { "" }, cx))
                .when(output, |view| {
                    view.bg(rgb(theme.selected)).text_color(rgb(theme.accent))
                }),
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
        footer
    }

    pub(in crate::gui) fn close_workbench_controls(&mut self) {
        self.graph_ui.workbench_choices.close();
        self.graph_ui.workbench_menu = None;
        self.graph_ui.workbench_anchor.set(None);
        self.cancel_workbench_resize();
    }

    pub(in crate::gui) fn cancel_workbench_resize(&mut self) {
        for state in self.graph_ui.workbenches.values_mut() {
            state.resize = None;
            state.zoom_drag = None;
        }
    }

    pub(in crate::gui) fn workbench_pointer_active(&self) -> bool {
        self.graph_ui
            .workbenches
            .values()
            .any(|state| state.resize.is_some() || state.zoom_drag.is_some())
    }

    pub(in crate::gui) fn retain_workbench_controls(
        &mut self,
        layout: &crate::gui::workspace::WorkspacePanels,
        visible: bool,
    ) {
        for (panel, state) in &mut self.graph_ui.workbenches {
            if !visible || !layout.is_panel_active(*panel) {
                state.resize = None;
                state.zoom_drag = None;
            }
        }
        if self
            .graph_ui
            .workbench_menu
            .is_some_and(|(panel, _)| !visible || !layout.is_panel_active(panel))
        {
            self.close_workbench_controls();
        }
    }

    fn workbench_dropdown(
        &mut self,
        panel: u64,
        menu: WorkbenchMenu,
        label: SharedString,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let open = self.graph_ui.workbench_menu == Some((panel, menu))
            && self.graph_ui.workbench_choices.is_open();
        let hint = self.tr(match menu {
            WorkbenchMenu::Add => "processing-workspace-add-node",
            WorkbenchMenu::Mode => "processing-workspace-mode",
        });
        let measured = Rc::clone(
            &self
                .graph_ui
                .workbenches
                .entry(panel)
                .or_default()
                .menu_bounds,
        );
        let trigger_bounds = Rc::clone(&measured);
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
                            if this.graph_ui.workbench_menu == Some((panel, menu)) {
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
        let trigger = dropdown_trigger(
            rivu_ui::theme::palette(cx),
            SharedString::from(format!("workbench-menu-{panel}-{}", menu as u8)),
            label,
            open,
            cx,
        )
        .w_auto()
        .flex_shrink_0()
        .relative()
        .role(Role::ComboBox)
        .aria_label(hint.clone())
        .tooltip(rivu_ui::text_tooltip(hint))
        .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
            match this
                .graph_ui
                .workbench_choices
                .key(event, ChoiceMode::Select, false, window)
                .apply(window, cx)
            {
                ChoiceAction::Commit(index) => this.choose_workbench_item(index, window, cx),
                ChoiceAction::Dismiss => {
                    this.close_workbench_controls();
                    cx.notify();
                }
                ChoiceAction::Navigated => cx.notify(),
                _ => {}
            }
        }))
        .child(measure);
        activate_control(trigger, cx, move |this, _, cx| {
            if this.graph_ui.workbench_menu == Some((panel, menu))
                && this.graph_ui.workbench_choices.is_open()
            {
                this.close_workbench_controls();
            } else {
                this.close_workbench_controls();
                let Some(anchor) = trigger_bounds.get()[menu as usize] else {
                    return;
                };
                this.graph_ui.workbench_anchor.set(Some(anchor));
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
                    WorkbenchMenu::Mode => {
                        let modes = [
                            GraphExecutionMode::Auto,
                            GraphExecutionMode::Realtime,
                            GraphExecutionMode::Hybrid,
                            GraphExecutionMode::Offline,
                        ];
                        let selected = modes
                            .iter()
                            .position(|mode| *mode == this.graph_ui.mode)
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
                this.graph_ui.workbench_menu = Some((panel, menu));
                this.graph_ui.workbench_choices.open(items, selected);
            }
            cx.notify();
        })
    }

    fn choose_workbench_item(&mut self, index: usize, _: &mut Window, cx: &mut Context<Self>) {
        let Some(choice) = self
            .graph_ui
            .workbench_choices
            .item(index)
            .map(|item| item.value)
        else {
            return;
        };
        let Some((panel, _)) = self.graph_ui.workbench_menu else {
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
                self.graph_add(panel, kind, cx);
            }
            WorkbenchChoice::Mode(mode) => self.graph_ui.mode = mode,
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
        div()
            .id(("inspector-resize", panel))
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
                    this.cancel_pointer(cx);
                    let state = this.graph_ui.workbenches.entry(panel).or_default();
                    state.resize = Some((
                        event.position.x,
                        state.inspector_width,
                        DockGeometry::current(window, this.state.system.config.ui_scale),
                    ));
                    cx.stop_propagation();
                }),
            )
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                let change = match event.keystroke.key.as_str() {
                    "left" => 1.,
                    "right" => -1.,
                    _ => return,
                };
                let state = this.graph_ui.workbenches.entry(panel).or_default();
                state.inspector_width = (state.inspector_width + change).clamp(16., 42.);
                this.cancel_pointer(cx);
                cx.stop_propagation();
                cx.notify();
            }))
    }

    fn inspector_resize_handlers(owner: WeakEntity<Self>, panel: u64, window: &mut Window) {
        let moved = owner.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase != DispatchPhase::Capture {
                return;
            }
            let _ = moved.update(cx, |this, cx| {
                let zoom_drag = this.graph_ui.workbenches[&panel].zoom_drag;
                if let Some(geometry) = zoom_drag {
                    if event.pressed_button != Some(MouseButton::Left)
                        || !window.is_window_active()
                        || geometry
                            != DockGeometry::current(window, this.state.system.config.ui_scale)
                    {
                        this.graph_ui.workbenches.get_mut(&panel).unwrap().zoom_drag = None;
                    } else if let Some(value) = this.graph_ui.workbenches[&panel]
                        .zoom_bounds
                        .get()
                        .and_then(|b| Slider::value_at(b, event.position))
                    {
                        this.graph_zoom_to(0.35 + value * 1.65, [250., 160.], cx);
                    }
                    window.prevent_default();
                    cx.notify();
                    return;
                }
                let state = this.graph_ui.workbenches.entry(panel).or_default();
                let Some((start, width, geometry)) = state.resize else {
                    return;
                };
                if event.pressed_button != Some(MouseButton::Left)
                    || !window.is_window_active()
                    || geometry != DockGeometry::current(window, this.state.system.config.ui_scale)
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
        window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
            if phase == DispatchPhase::Capture {
                let _ = released.update(cx, |this, cx| {
                    let state = this.graph_ui.workbenches.entry(panel).or_default();
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
                    let state = this.graph_ui.workbenches.entry(panel).or_default();
                    let active = state.resize.take().is_some() | state.zoom_drag.take().is_some();
                    if active {
                        cx.notify();
                    }
                });
            }
        });
    }

    fn graph_zoom_slider(&self, panel: u64, cx: &mut Context<Self>) -> Stateful<Div> {
        let state = &self.graph_ui.workbenches[&panel];
        let bounds = state.zoom_bounds.clone();
        let measured = bounds.clone();
        Slider::new(
            ("graph-zoom", panel),
            (self.graph_ui.zoom - 0.35) / 1.65,
            self.tr("processing-workspace-zoom"),
        )
        .dragging(state.zoom_drag.is_some())
        .render(cx, move |this, input, window, cx| {
            this.cancel_pointer(cx);
            let zoom = match input {
                SliderInput::PointerDown { position, .. } => {
                    let Some(value) = bounds.get().and_then(|b| Slider::value_at(b, position))
                    else {
                        return;
                    };
                    this.graph_ui.workbenches.get_mut(&panel).unwrap().zoom_drag = Some(
                        DockGeometry::current(window, this.state.system.config.ui_scale),
                    );
                    0.35 + value * 1.65
                }
                SliderInput::Key(key) => match key {
                    SliderKey::Start => 0.35,
                    SliderKey::End => 2.,
                    SliderKey::Decrease => this.graph_ui.zoom - 0.05,
                    SliderKey::Increase => this.graph_ui.zoom + 0.05,
                },
            };
            this.graph_zoom_to(zoom, [250., 160.], cx);
            cx.stop_propagation();
        })
        .w_24()
        .flex_shrink_0()
        .tooltip(rivu_ui::text_tooltip(format!(
            "{:.0}%",
            self.graph_ui.zoom * 100.
        )))
        .child(
            canvas(
                move |bounds, _, _| measured.set(Some(bounds)),
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
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
