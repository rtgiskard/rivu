//! Frozen processing-task results and explicit snapshot actions.
use super::graph_editor::{node_fields, node_label};
use super::*;
use rivu_format::config::processing_graph::{GraphNodeKind, GraphPort};
use rivu_protocol::model::GraphExecutionMode;
use serde_json::{Value, json};

#[derive(Default)]
pub(super) struct ResultsState {
    pub(super) export_draft_generation: u64,
    pub(super) details: std::collections::HashSet<u64>,
    pub(super) errors: HashMap<u64, String>,
    pub(super) task_details: bool,
}

impl ResultsState {
    pub(super) fn retain_panels(&mut self, layout: &crate::gui::workspace::WorkspacePanels) {
        self.details.retain(|id| layout.contains_panel(*id));
        self.errors.retain(|id, _| layout.contains_panel(*id));
    }
}

impl AudioProcessing {
    fn captured_parameters(&self, prefix: &str, value: &Value, cx: &App) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let mut content = column().min_w_0().gap_1();
        if let Some(object) = value.as_object() {
            for (key, value) in object {
                if value.is_object() {
                    content = content.child(self.captured_parameters(
                        &format!("{prefix}.{key}"),
                        value,
                        cx,
                    ));
                    continue;
                }
                let label = self.parameter_path_label(prefix, key);
                let text = match value {
                    Value::Null => "—".to_owned(),
                    Value::Bool(value) => self
                        .context
                        .i18n
                        .text(if *value {
                            "processing-enabled"
                        } else {
                            "processing-off"
                        })
                        .to_owned(),
                    Value::String(value) if key == "mode" => self
                        .context
                        .i18n
                        .text(match value.as_str() {
                            "off" => "processing-off",
                            "stationary" => "processing-stationary",
                            "profile" => "processing-profile",
                            "adaptive" => "processing-adaptive",
                            _ => unreachable!("serialized denoise mode"),
                        })
                        .to_owned(),
                    Value::String(value) => value.clone(),
                    _ => value.to_string(),
                };
                content = content.child(caption(theme, format!("{label}: {text}")));
            }
        }
        content
    }

    fn task_settings(&self, task: &rivu_protocol::model::ProcessingTaskStatus, cx: &App) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let neural = &task.settings.neural;
        let parameters = match task.operation {
            ProcessingOperation::Denoise => {
                serde_json::to_value(&task.settings.denoise).expect("serializable captured denoise")
            }
            ProcessingOperation::Speech => json!({
                "speech_interpreter":neural.speech_interpreter,"worker":neural.worker,
                "speech_manifest":neural.speech_manifest,"device":neural.device,"threads":neural.threads,
                "speech_wet_mix":neural.speech_wet_mix,"speech_attenuation_limit_db":neural.speech_attenuation_limit_db,
                "speech_post_filter":neural.speech_post_filter,
            }),
            ProcessingOperation::Separate => json!({
                "separation_interpreter":neural.separation_interpreter,"worker":neural.worker,
                "separation_manifest":neural.separation_manifest,"device":neural.device,"threads":neural.threads,
                "separation_overlap":neural.separation_overlap,
            }),
            ProcessingOperation::Graph => {
                serde_json::to_value(&task.settings.graph).expect("serializable graph snapshot")
            }
        };
        let mut content = column()
            .min_w_0()
            .gap_2()
            .p_2()
            .border_1()
            .border_color(rgb(theme.border))
            .child(
                caption(theme, self.tr("processing-captured-settings")).text_color(rgb(theme.text)),
            )
            .child(self.captured_parameters(
                if task.operation == ProcessingOperation::Denoise {
                    "denoise"
                } else {
                    "neural"
                },
                &parameters,
                cx,
            ));
        if let Some(profile) = task.settings.profile {
            content = content.child(caption(
                theme,
                format!(
                    "{}: {} · {}: {}",
                    self.context.i18n.text("processing-profile-start"),
                    profile.start_seconds,
                    self.context.i18n.text("processing-profile-end"),
                    profile.end_seconds
                ),
            ));
        }
        content
    }

    fn task_status(&self, panel: u64, cx: &mut Context<Self>) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let mut content = column().gap_2();
        if let Some(task) = &self.state.system.processing {
            let id = task.id;
            let terminal = matches!(
                task.phase,
                ProcessingPhase::Complete | ProcessingPhase::Cancelled | ProcessingPhase::Failed
            );
            let phase = self.tr(match task.phase {
                ProcessingPhase::Queued => "processing-queued",
                ProcessingPhase::Rendering => "processing-rendering",
                ProcessingPhase::Processing => "processing-processing",
                ProcessingPhase::Complete => "processing-complete",
                ProcessingPhase::Cancelled => "processing-cancelled",
                ProcessingPhase::Failed => "processing-failed",
            });
            let operation = self.tr(match task.operation {
                ProcessingOperation::Denoise => "processing-restoration",
                ProcessingOperation::Speech => "processing-speech",
                ProcessingOperation::Separate => "processing-separation",
                ProcessingOperation::Graph => "processing-routing-graph",
            });
            content = content.child(caption(
                theme,
                format!(
                    "{} #{} · {operation} · {} · {phase}",
                    self.context.i18n.text("processing-task"),
                    id,
                    task.source_path
                        .file_name()
                        .map(|name| name.to_string_lossy())
                        .unwrap_or_else(|| "—".into()),
                ),
            ));
            content = content.child(caption(
                theme,
                format!(
                    "{}: {} / {}",
                    self.context.i18n.text("processing-progress"),
                    task.completed,
                    task.total
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "?".to_owned())
                ),
            ));
            if let Some(total) = task.total.filter(|n| *n > 0) {
                content = content.child(
                    rivu_ui::ProgressBar::new(
                        ("processing-progress", panel),
                        (task.completed as f64 / total as f64) as f32,
                        self.tr("processing-progress"),
                    )
                    .render(cx),
                );
            }
            if !terminal {
                content = content.child(
                    button(
                        ("cancel-processing", panel),
                        self.tr("processing-cancel"),
                        cx,
                        move |this, _, cx| this.send(Command::CancelProcessing { id }, cx),
                    )
                    .self_start(),
                );
            }
            if let Some(error) = &task.error {
                content = content.child(caption(theme, error.clone()).text_color(rgb(theme.error)));
            }
            if task.phase == ProcessingPhase::Complete && task.outputs.is_some() {
                content = content.child(caption(theme, self.tr("processing-audition-source")));
                let mut auditions = row().flex_wrap().gap_1();
                let modes = if task.operation == ProcessingOperation::Separate {
                    vec![
                        (AuditionMode::Original, "processing-original"),
                        (AuditionMode::Vocals, "processing-vocals"),
                        (AuditionMode::Accompaniment, "processing-accompaniment"),
                        (AuditionMode::Mix, "processing-mix"),
                    ]
                } else {
                    vec![
                        (AuditionMode::Original, "processing-original"),
                        (AuditionMode::Denoised, "processing-processed"),
                    ]
                };
                for (mode, label) in modes {
                    auditions = auditions.child(button(
                        SharedString::from(format!("audition-{panel}-{mode:?}")),
                        self.tr(label),
                        cx,
                        move |this, _, cx| this.audition(id, mode, cx),
                    ));
                }
                content = content.child(auditions);
                if task.operation == ProcessingOperation::Separate {
                    content = content
                        .child(self.field_view(
                            FieldKey::VocalGain,
                            self.tr("processing-vocal-gain"),
                            true,
                            cx,
                        ))
                        .child(self.field_view(
                            FieldKey::AccompanimentGain,
                            self.tr("processing-accompaniment-gain"),
                            true,
                            cx,
                        ))
                        .child(
                            button(
                                ("apply-mix-gains", panel),
                                self.tr("processing-apply-mix"),
                                cx,
                                move |this, _, cx| this.audition(id, AuditionMode::Mix, cx),
                            )
                            .self_start(),
                        );
                }
            }
            content = content.child(activate_control(
                dropdown_trigger(
                    theme,
                    ("processing-task-details", panel),
                    self.tr(if self.results.task_details {
                        "processing-result-hide-details"
                    } else {
                        "processing-result-show-details"
                    }),
                    self.results.task_details,
                    cx,
                ),
                cx,
                |this, _, cx| {
                    this.results.task_details = !this.results.task_details;
                    cx.notify();
                },
            ));
            if self.results.task_details {
                if let Some(model) = &task.model_id {
                    content = content.child(caption(
                        theme,
                        format!(
                            "{} · {}",
                            model,
                            task.model_sha256.as_deref().unwrap_or("?")
                        ),
                    ));
                }
                if let (Some(rate), Some(channels), Some(frames)) =
                    (task.sample_rate, task.channels, task.frames)
                {
                    content = content.child(caption(
                        theme,
                        format!("{rate} Hz · {channels} ch · {frames} frames"),
                    ));
                }
                content = content.child(self.task_settings(task, cx));
            }
        }
        content
    }

    pub(in crate::gui) fn processing_results_view(
        &mut self,
        panel: u64,
        cx: &mut Context<Self>,
    ) -> Div {
        if self.state.system.processing.is_none() {
            return column().p_2().child(caption(
                rivu_ui::theme::palette(cx),
                self.tr("processing-result-empty"),
            ));
        }
        let results = if self
            .state
            .system
            .processing
            .as_ref()
            .is_some_and(|task| task.operation != ProcessingOperation::Graph)
        {
            self.task_status(panel, cx)
        } else {
            column()
                .gap_2()
                .child(self.graph_task_summary(panel, cx))
                .child(self.graph_task_status(panel, cx))
        };
        let body = self.processing_scroll(panel, "processing-results", results.p_2());
        column().size_full().min_h_0().child(body)
    }
    pub(super) fn graph_task_summary(&mut self, panel: u64, cx: &mut Context<Self>) -> Div {
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
                " · {mode} · {}",
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
        let mut actions = rivu_ui::panel_header_actions();
        if self.page != Page::Results {
            actions = actions.child(button(
                ("graph-show-results", panel),
                self.tr("processing-result-open"),
                cx,
                move |this, _, cx| this.show_graph_results(panel, cx),
            ));
        }
        if !matches!(
            task.phase,
            ProcessingPhase::Complete | ProcessingPhase::Cancelled | ProcessingPhase::Failed
        ) {
            actions = actions.child(button(
                ("graph-cancel-task", panel),
                self.tr("processing-cancel"),
                cx,
                move |this, _, cx| this.send(Command::CancelProcessing { id }, cx),
            ));
        }
        rivu_ui::panel_heading_row(
            caption(theme, summary.clone())
                .truncate()
                .id(("processing-task-summary", panel))
                .tooltip(rivu_ui::text_tooltip(summary)),
            actions,
        )
    }

    fn choose_graph_export_path(&mut self, panel: u64, task_id: u64, cx: &mut Context<Self>) {
        if self.choosing_path.is_some() {
            return;
        }
        let Some(field) = self.fields.get(&FieldKey::GraphExport) else {
            return;
        };
        let original = field.input.read(cx).text().to_owned();
        let draft_generation = self.results.export_draft_generation;
        let request = self.begin_path_request(panel, FieldKey::GraphExport);
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
                if !retire_path_request(&mut this.choosing_path, request) {
                    return;
                }
                cx.notify();
                if !this.panel_inputs.contains_key(&panel)
                    || !path_snapshot_current(
                        (task_id, draft_generation, &original),
                        (
                            this.state
                                .system
                                .processing
                                .as_ref()
                                .map_or(0, |task| task.id),
                            this.results.export_draft_generation,
                            this.fields
                                .get(&FieldKey::GraphExport)
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
                                    this.stage_graph_text(FieldKey::GraphExport, path, cx);
                                    this.results.errors.remove(&panel);
                                }
                                Err(_) => {
                                    this.results.errors.insert(
                                        panel,
                                        this.tr("processing-result-path-encoding").to_string(),
                                    );
                                }
                            }
                        }
                    }
                    Ok(None) => return,
                    Err(error) => {
                        this.results.errors.insert(panel, error.to_string());
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn graph_task_status(&mut self, panel: u64, cx: &mut Context<Self>) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let export_input = if self
            .state
            .system
            .processing
            .as_ref()
            .is_some_and(|task| task.phase == ProcessingPhase::Complete)
        {
            Some(self.panel_input(
                panel,
                FieldKey::GraphExport,
                &self.tr("processing-export-path"),
                cx,
            ))
        } else {
            None
        };
        let Some(task) = self.state.system.processing.as_ref() else {
            return column();
        };
        let task_id = task.id;
        let Some(id) = task.graph_run_id else {
            return column();
        };
        let export_receipt = id != task_id;
        let expanded = self.results.details.contains(&panel);
        let mut content = column().flex_shrink_0().gap_2();
        if let Some(error) = &task.error {
            content = content.child(caption(theme, error.clone()).text_color(rgb(theme.error)));
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
                "processing-graph-memory-ready"
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
                            move |this, _, cx| this.choose_graph_export_path(panel, task_id, cx),
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
                    (0..node.kind.outputs()).map(move |index| {
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
                let retained = task.graph_outputs.iter().any(|output| output.port == port);
                let label = self.tr(node_label(kind));
                let availability = if export_receipt {
                    format!("{} #{id}", self.tr("processing-result-source-run"))
                } else if retained {
                    self.tr("processing-graph-cached").to_string()
                } else {
                    self.tr("processing-result-not-rendered").to_string()
                };
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
                            self.tr(if export_receipt {
                                "processing-graph-audition"
                            } else if retained {
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
                            self.tr(if export_receipt {
                                "processing-graph-export"
                            } else if retained {
                                "processing-result-export-cached"
                            } else {
                                "processing-result-calculate-export"
                            }),
                            cx,
                            move |this, _, cx| {
                                let path = this
                                    .fields
                                    .get(&FieldKey::GraphExport)
                                    .map(|field| field.input.read(cx).text().to_owned())
                                    .unwrap_or_default();
                                if !path.is_empty() {
                                    this.results.errors.remove(&panel);
                                    this.send(
                                        Command::ProcessingGraphExport {
                                            id,
                                            node: port,
                                            path: PathBuf::from(path),
                                        },
                                        cx,
                                    );
                                } else {
                                    this.results.errors.insert(
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
        if let Some(error) = self.results.errors.get(&panel) {
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
                    if !this.results.details.remove(&panel) {
                        this.results.details.insert(panel);
                    }
                    cx.notify();
                },
            )
            .self_start(),
        );
        if expanded {
            if task.settings.graph.is_some() {
                content = content.child(caption(
                    theme,
                    format!(
                        "{} {} · {} {}",
                        self.tr("processing-result-frozen-revision"),
                        task.graph_revision
                            .map(|revision| revision.to_string())
                            .unwrap_or_else(|| "—".into()),
                        self.tr("processing-result-current-revision"),
                        self.state.system.config.processing_graph.revision
                    ),
                ));
            }
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
                        self.tr("processing-graph-cached")
                    ),
                ));
                if let Some(context) = &output.context {
                    content = content.child(caption(
                        theme,
                        format!(
                            "{} {}",
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
                    for field in node_fields(&node.kind) {
                        let text = field.text(&node.kind);
                        content = content.child(caption(
                            theme,
                            format!("{}: {text}", self.graph_parameter_label(field)),
                        ));
                    }
                    if let GraphNodeKind::Equalizer { config, .. } = &node.kind {
                        use rivu_dsp::equalizer::config::{ChannelTarget, FilterKind};
                        for (label, text) in [
                            ("processing-enabled", config.enabled.to_string()),
                            ("processing-preamp", config.preamp_db.to_string()),
                            ("processing-output-gain", config.output_gain_db.to_string()),
                        ] {
                            content = content
                                .child(caption(theme, format!("{}: {text}", self.tr(label))));
                        }
                        for (index, band) in config.bands.iter().enumerate() {
                            let kind = self.tr(match band.kind {
                                FilterKind::Peak => "processing-peak",
                                FilterKind::LowShelf => "processing-low-shelf",
                                FilterKind::HighShelf => "processing-high-shelf",
                                FilterKind::LowPass => "processing-low-pass",
                                FilterKind::HighPass => "processing-high-pass",
                                FilterKind::BandPass => "processing-band-pass",
                                FilterKind::Notch => "processing-notch",
                            });
                            let channels = match &band.channels {
                                ChannelTarget::All => "all".to_owned(),
                                ChannelTarget::Selected(channels) => channels
                                    .iter()
                                    .map(|channel| (channel + 1).to_string())
                                    .collect::<Vec<_>>()
                                    .join(","),
                            };
                            for (label, text) in [
                                ("processing-enabled", band.enabled.to_string()),
                                ("processing-graph-input-solo", band.solo.to_string()),
                                ("processing-graph-filter-kind", kind.to_string()),
                                ("processing-frequency", band.frequency_hz.to_string()),
                                ("processing-gain", band.gain_db.to_string()),
                                ("processing-q", band.q.to_string()),
                                ("processing-shelf-slope", band.shelf_slope.to_string()),
                                ("processing-stages", band.stages.to_string()),
                                ("processing-channels", channels),
                            ] {
                                content = content.child(caption(
                                    theme,
                                    format!(
                                        "{} {} · {}: {text}",
                                        self.tr("processing-graph-band"),
                                        index + 1,
                                        self.tr(label),
                                    ),
                                ));
                            }
                        }
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
        content
    }
}
