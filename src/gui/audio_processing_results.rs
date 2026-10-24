//! Frozen processing-task results and explicit snapshot actions.
use super::*;

impl AudioProcessing {
    pub(in super::super) fn graph_task_summary(
        &mut self,
        panel: u64,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let Some(task) = self.state.system.processing.as_ref() else {
            return column();
        };
        let id = task.id;
        let phase = self.tr(match task.phase {
            ProcessingPhase::Queued => "processing-queued",
            ProcessingPhase::Rendering => "processing-rendering",
            ProcessingPhase::Processing => "processing-processing",
            ProcessingPhase::Complete => "processing-complete",
            ProcessingPhase::Cancelled => "processing-cancelled",
            ProcessingPhase::Failed => "processing-failed",
        });
        let source = task
            .source_path
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_else(|| "—".into());
        let mut summary = format!(
            "{} #{id} · {source} · {phase} · {} / {}",
            self.tr("processing-task"),
            task.completed,
            task.total
                .map(|v| v.to_string())
                .unwrap_or_else(|| "?".into())
        );
        if let Some(graph) = &task.settings.graph {
            let mode = task
                .settings
                .graph_mode
                .map(|mode| {
                    self.tr(match mode {
                        GraphExecutionMode::Auto => "processing-graph-auto",
                        GraphExecutionMode::Offline => "processing-graph-offline",
                        GraphExecutionMode::Realtime => "processing-graph-realtime",
                        GraphExecutionMode::Hybrid => "processing-graph-hybrid",
                    })
                })
                .unwrap_or_else(|| "—".into());
            summary.push_str(&format!(
                " · {mode} · {} {} → {} {} · {}",
                self.tr("processing-result-frozen-revision"),
                task.graph_revision
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "—".into()),
                self.tr("processing-result-current-revision"),
                self.state.system.config.processing_graph.revision,
                self.tr(
                    if graph.audio_eq(&self.state.system.config.processing_graph) {
                        "processing-result-current"
                    } else {
                        "processing-result-edited"
                    }
                )
            ));
            if let Some(target) = task.settings.graph_target {
                summary.push_str(&format!(
                    " · {} #{}:{}",
                    self.tr("processing-result-target"),
                    target.node,
                    target.port
                ));
            }
        }
        let mut content = row()
            .flex_wrap()
            .gap_2()
            .child(caption(theme, summary))
            .child(button(
                ("graph-show-results", panel),
                self.tr("processing-result-open"),
                cx,
                move |this, _, cx| this.show_graph_results(panel, cx),
            ));
        if !matches!(
            task.phase,
            ProcessingPhase::Complete | ProcessingPhase::Cancelled | ProcessingPhase::Failed
        ) {
            content = content.child(button(
                ("graph-cancel-task", panel),
                self.tr("processing-cancel"),
                cx,
                move |this, _, cx| this.send(Command::CancelProcessing { id }, cx),
            ));
        }
        content
    }

    fn choose_graph_export_path(&mut self, panel: u64, task_id: u64, cx: &mut Context<Self>) {
        if self.graph_ui.choosing_path.is_some() {
            return;
        }
        let Some(field) = self.fields.get("graph_export") else {
            return;
        };
        let original = field.input.read(cx).text().to_owned();
        let draft_generation = self.graph_ui.export_draft_generation;
        self.graph_ui.chooser_generation += 1;
        let chooser_generation = self.graph_ui.chooser_generation;
        self.graph_ui.choosing_path = Some("graph_export".to_owned());
        let i18n = Arc::clone(&self.context.i18n);
        cx.spawn(async move |this, cx| {
            let result = ashpd::desktop::file_chooser::SelectedFiles::save_file()
                .title(i18n.text("processing-result-choose-path"))
                .accept_label(i18n.text("processing-result-choose-path"))
                .modal(true)
                .send()
                .await
                .and_then(|request| request.response());
            let _ = this.update(cx, |this, cx| {
                if this.graph_ui.chooser_generation != chooser_generation {
                    return;
                }
                this.graph_ui.choosing_path = None;
                cx.notify();
                if !this.graph_ui.panel_inputs.contains_key(&panel)
                    || !super::inspector::path_snapshot_current(
                        (task_id, draft_generation, &original),
                        (
                            this.state
                                .system
                                .processing
                                .as_ref()
                                .map_or(0, |task| task.id),
                            this.graph_ui.export_draft_generation,
                            this.fields
                                .get("graph_export")
                                .map(|field| field.input.read(cx).text()),
                        ),
                    )
                {
                    return;
                }
                match crate::gui::chooser_result(result) {
                    Ok(Some(files)) => {
                        if let Some(path) = crate::gui::chooser_path(&files) {
                            match path.into_os_string().into_string() {
                                Ok(path) => {
                                    this.stage_graph_text("graph_export", path, cx);
                                    this.graph_ui.result_errors.remove(&panel);
                                }
                                Err(_) => {
                                    this.graph_ui.result_errors.insert(
                                        panel,
                                        this.tr("processing-result-path-encoding").to_string(),
                                    );
                                }
                            }
                        }
                    }
                    Ok(None) => return,
                    Err(error) => {
                        this.graph_ui.result_errors.insert(panel, error.to_string());
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(in super::super) fn graph_task_status(
        &mut self,
        panel: u64,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let export_input = if self
            .state
            .system
            .processing
            .as_ref()
            .is_some_and(|task| task.phase == ProcessingPhase::Complete)
        {
            Some(self.graph_panel_input(
                panel,
                "graph_export",
                &self.tr("processing-export-path"),
                cx,
            ))
        } else {
            None
        };
        let Some(task) = self.state.system.processing.as_ref() else {
            return column();
        };
        let id = task.id;
        let expanded = self.graph_ui.result_details.contains(&panel);
        let mut content = column().flex_shrink_0().gap_2();
        if let Some(error) = &task.error {
            content = content.child(caption(theme, error.clone()).text_color(rgb(theme.error)));
        }
        content = content.child(
            button(
                ("graph-result-details", panel),
                self.tr(if expanded {
                    "processing-result-hide-details"
                } else {
                    "processing-result-show-details"
                }),
                cx,
                move |this, _, cx| {
                    if !this.graph_ui.result_details.remove(&panel) {
                        this.graph_ui.result_details.insert(panel);
                    }
                    cx.notify();
                },
            )
            .self_start(),
        );
        if expanded {
            content = content.child(caption(
                theme,
                format!(
                    "{} · {} · {} Hz · {} ch · {} {}",
                    task.source_path.display(),
                    task.source_version,
                    task.sample_rate
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "—".into()),
                    task.channels
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "—".into()),
                    task.frames
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "—".into()),
                    self.tr("processing-result-frames"),
                ),
            ));
            for output in &task.graph_outputs {
                content = content.child(caption(
                    theme,
                    format!(
                        "#{}:{} · {}",
                        output.port.node,
                        output.port.port,
                        output.path.display()
                    ),
                ));
                if let Some(context) = &output.context {
                    content = content.child(caption(
                        theme,
                        format!(
                            "{} · {} {}",
                            context.path.display(),
                            context.start_frame,
                            self.tr("processing-result-frames")
                        ),
                    ));
                }
            }
            if let Some(path) = &task.settings.graph_export_path {
                content = content.child(caption(theme, path.display().to_string()));
            }
            if let Some(model) = &task.model_id {
                content = content.child(caption(
                    theme,
                    format!(
                        "{model} · SHA-256 {}",
                        task.model_sha256.as_deref().unwrap_or("—")
                    ),
                ));
            }
            if let Some(graph) = &task.settings.graph {
                let mode = task
                    .settings
                    .graph_mode
                    .map(|mode| {
                        self.tr(match mode {
                            GraphExecutionMode::Auto => "processing-graph-auto",
                            GraphExecutionMode::Offline => "processing-graph-offline",
                            GraphExecutionMode::Realtime => "processing-graph-realtime",
                            GraphExecutionMode::Hybrid => "processing-graph-hybrid",
                        })
                        .to_string()
                    })
                    .unwrap_or_else(|| "—".into());
                content = content.child(caption(
                    theme,
                    format!(
                        "{} · {}: {} · {}: {} · {mode}",
                        self.tr("processing-captured-settings"),
                        self.tr("processing-graph-nodes"),
                        graph.nodes.len(),
                        self.tr("processing-graph-connections"),
                        graph.edges.len()
                    ),
                ));
                for node in &graph.nodes {
                    content = content.child(caption(
                        theme,
                        format!("#{} · {}", node.id, self.tr(node_label(&node.kind))),
                    ));
                    let neural = match &node.kind {
                        GraphNodeKind::Speech { config } => Some((
                            config,
                            config.speech_manifest.as_ref(),
                            &config.speech_interpreter,
                        )),
                        GraphNodeKind::Separate { config } => Some((
                            config,
                            config.separation_manifest.as_ref(),
                            &config.separation_interpreter,
                        )),
                        _ => None,
                    };
                    if let Some((config, manifest, interpreter)) = neural {
                        content = content
                            .child(caption(
                                theme,
                                format!(
                                    "{} · {}",
                                    manifest
                                        .map(|p| p.display().to_string())
                                        .unwrap_or_else(|| "—".into()),
                                    interpreter.display()
                                ),
                            ))
                            .child(caption(
                                theme,
                                format!(
                                    "{}: {} · {:?} · {} {}",
                                    self.tr("processing-worker"),
                                    config.worker.display(),
                                    config.device,
                                    config.threads,
                                    self.tr("processing-threads")
                                ),
                            ));
                    }
                }
            }
            for node in &task.graph_nodes {
                let phase = self.tr(match node.phase {
                    ProcessingPhase::Queued => "processing-queued",
                    ProcessingPhase::Rendering => "processing-rendering",
                    ProcessingPhase::Processing => "processing-processing",
                    ProcessingPhase::Complete => "processing-complete",
                    ProcessingPhase::Cancelled => "processing-cancelled",
                    ProcessingPhase::Failed => "processing-failed",
                });
                content = content.child(caption(theme, format!("#{} · {phase}", node.node)));
                if let Some(model) = &node.model_id {
                    content = content.child(caption(
                        theme,
                        format!(
                            "{model} · SHA-256 {}",
                            node.model_sha256.as_deref().unwrap_or("—")
                        ),
                    ));
                }
                if let Some(error) = &node.error {
                    content =
                        content.child(caption(theme, error.clone()).text_color(rgb(theme.error)));
                }
            }
            if let Some(plan) = &task.graph_plan {
                content = content.child(caption(
                    theme,
                    format!(
                        "{}: {} B · {}: {} · {}: {} · {}: {}",
                        self.tr("processing-graph-plan-memory"),
                        plan.estimated_state_bytes,
                        self.tr("processing-graph-plan-work"),
                        plan.estimated_work_per_frame,
                        self.tr("processing-graph-plan-latency"),
                        plan.latency_frames,
                        self.tr("processing-graph-plan-tail"),
                        plan.tail_frames
                    ),
                ));
            }
        }
        if let Some(boundary) = task.settings.graph_boundary {
            content = content.child(caption(
                theme,
                format!(
                    "{} #{}:{}",
                    self.tr("processing-graph-boundary"),
                    boundary.node,
                    boundary.port
                ),
            ));
        }
        if let Some(path) = &task.graph_exported_path {
            content = content.child(caption(
                theme,
                format!(
                    "{}: {}",
                    self.tr("processing-graph-exported"),
                    path.display()
                ),
            ));
        }
        if task.phase == ProcessingPhase::Complete {
            let live = matches!(
                task.settings.graph_mode,
                Some(GraphExecutionMode::Realtime | GraphExecutionMode::Hybrid)
            );
            let ready = if task.graph_exported_path.is_some() {
                "processing-graph-export-ready"
            } else if live {
                "processing-result-live-prepared"
            } else {
                "processing-graph-file-ready"
            };
            content = content.child(caption(theme, self.tr(ready)));
            content = content.child(
                column()
                    .min_w_0()
                    .gap_1()
                    .child(caption(theme, self.tr("processing-export-path")))
                    .child(caption(theme, self.tr("processing-result-new-file")))
                    .child(
                        button(
                            ("graph-choose-export", panel),
                            self.tr("processing-result-choose-path"),
                            cx,
                            move |this, _, cx| this.choose_graph_export_path(panel, id, cx),
                        )
                        .self_start(),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(export_input.expect("complete graph task export input")),
                    ),
            );
            let ports = task
                .settings
                .graph
                .iter()
                .flat_map(|graph| graph.nodes.iter())
                .flat_map(|node| {
                    (0..output_count(&node.kind)).map(move |index| {
                        (
                            &node.kind,
                            GraphPort {
                                node: node.id,
                                port: index as u16,
                            },
                        )
                    })
                });
            for (kind, port) in ports {
                let file = task.graph_outputs.iter().find(|output| output.port == port);
                let label = self.tr(node_label(kind));
                let availability = file
                    .map(|output| {
                        format!(
                            "{} · {}",
                            self.tr("processing-graph-cached"),
                            output.path.display()
                        )
                    })
                    .unwrap_or_else(|| self.tr("processing-result-not-rendered").to_string());
                content = content.child(
                    row()
                        .flex_wrap()
                        .gap_1()
                        .child(caption(
                            theme,
                            format!(
                                "{label} #{} · {} {} · {availability}",
                                port.node,
                                self.tr("processing-result-port"),
                                port.port
                            ),
                        ))
                        .child(button(
                            SharedString::from(format!(
                                "graph-audition-{panel}-{id}-{}-{}",
                                port.node, port.port
                            )),
                            self.tr(if file.is_some() {
                                "processing-result-audition-cached"
                            } else {
                                "processing-result-calculate-audition"
                            }),
                            cx,
                            move |this, _, cx| {
                                this.send(Command::ProcessingGraphAudition { id, node: port }, cx)
                            },
                        ))
                        .child(button(
                            SharedString::from(format!(
                                "graph-export-{panel}-{id}-{}-{}",
                                port.node, port.port
                            )),
                            self.tr(if file.is_some() {
                                "processing-result-export-cached"
                            } else {
                                "processing-result-calculate-export"
                            }),
                            cx,
                            move |this, _, cx| {
                                let path = this
                                    .fields
                                    .get("graph_export")
                                    .map(|field| field.input.read(cx).text().to_owned())
                                    .unwrap_or_default();
                                if !path.is_empty() {
                                    this.graph_ui.result_errors.remove(&panel);
                                    this.send(
                                        Command::ProcessingGraphExport {
                                            id,
                                            node: port,
                                            path: PathBuf::from(path),
                                        },
                                        cx,
                                    );
                                } else {
                                    this.graph_ui.result_errors.insert(
                                        panel,
                                        this.context
                                            .i18n
                                            .text("processing-graph-export-required")
                                            .to_owned(),
                                    );
                                    cx.notify();
                                }
                            },
                        )),
                );
            }
        }
        if let Some(error) = self.graph_ui.result_errors.get(&panel) {
            content = content.child(caption(theme, error.clone()).text_color(rgb(theme.error)));
        }
        if let Some(error) = &self.error {
            content = content.child(caption(theme, error.clone()).text_color(rgb(theme.error)));
        }
        content
    }
}
