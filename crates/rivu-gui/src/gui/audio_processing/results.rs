//! Frozen processing-task results and explicit snapshot actions.
use super::graph_editor::{GraphField, node_fields, node_label};
use super::*;
use rivu_format::config::processing_graph::{GraphNodeKind, GraphPort};
use rivu_protocol::model::GraphExecutionMode;

#[derive(Default)]
pub(super) struct ResultsState {
    pub(super) details: bool,
    pub(super) task_details: bool,
    pub(super) graph_current: Option<bool>,
}

pub(super) fn graph_action_run(task: &rivu_protocol::model::ProcessingTaskStatus) -> Option<u64> {
    task.graph_run_id.filter(|_| task.graph_actions_available)
}

impl AudioProcessing {
    fn captured_denoise_line(
        &self,
        label: &str,
        field: DenoiseField,
        config: &DenoiseConfig,
    ) -> String {
        use DenoiseField::*;
        match field {
            AttackMs => format!("{label}: {} ms", config.attack_ms),
            ReleaseMs => format!("{label}: {} ms", config.release_ms),
            SpectralGainFloorDb => format!("{label}: {} dB", config.spectral_gain_floor_db),
            StationaryNoiseDbfs => format!("{label}: {} dBFS", config.stationary_noise_dbfs),
            ImpulseFloorDbfs => format!("{label}: {} dBFS", config.impulse.floor_dbfs),
            HumFundamentalHz => format!("{label}: {} Hz", config.hum.fundamental_hz),
            FftSize => self.captured_count(label, config.fft_size, "processing-result-samples"),
            ImpulseMaxWidth => {
                self.captured_count(label, config.impulse.max_width, "processing-result-samples")
            }
            SmoothingBins => {
                self.captured_count(label, config.smoothing_bins, "processing-result-bins")
            }
            _ => format!("{label}: {}", field.text(config)),
        }
    }

    fn captured_neural_line(label: &str, field: NeuralField, config: &NeuralConfig) -> String {
        match field {
            NeuralField::SpeechAttenuationLimitDb => {
                format!("{label}: {} dB", config.speech_attenuation_limit_db)
            }
            _ => format!("{label}: {}", field.text(config)),
        }
    }

    fn captured_graph_line(&self, field: GraphField, kind: &GraphNodeKind) -> String {
        let label = self.graph_parameter_label(field);
        match (field, kind) {
            (GraphField::Denoise(field), GraphNodeKind::Denoise { config, .. }) => {
                self.captured_denoise_line(&label, field, config)
            }
            (
                GraphField::Neural(field),
                GraphNodeKind::Speech { config } | GraphNodeKind::Separate { config },
            ) => Self::captured_neural_line(&label, field, config),
            (GraphField::ProfileStart | GraphField::ProfileEnd, _) => {
                format!("{label}: {} s", field.text(kind))
            }
            (GraphField::MixGain(index), GraphNodeKind::Mix { gains_db, .. }) => {
                format!("{label}: {} dB", gains_db[index])
            }
            _ => format!("{label}: {}", field.text(kind)),
        }
    }

    fn captured_toggle(&self, enabled: bool) -> SharedString {
        self.tr(if enabled {
            "processing-enabled"
        } else {
            "processing-off"
        })
    }

    fn captured_count(&self, label: &str, count: usize, message: &str) -> String {
        self.context
            .i18n
            .format(message, &[("label", label.into()), ("count", count.into())])
    }

    fn captured_denoise(&self, config: &DenoiseConfig, cx: &App) -> Div {
        use DenoiseField::*;
        let theme = rivu_ui::theme::palette(cx);
        let fields = [
            Mode,
            Strength,
            SpectralGainFloorDb,
            AttackMs,
            ReleaseMs,
            SmoothingBins,
            TransientProtection,
            Wet,
            FftSize,
            StationaryNoiseDbfs,
            HumEnabled,
            HumFundamentalHz,
            HumHarmonics,
            HumQ,
            ImpulseEnabled,
            ImpulseMaxWidth,
            ImpulseThreshold,
            ImpulseFloorDbfs,
            ImpulseTransientProtection,
        ];
        column()
            .min_w_0()
            .gap_1()
            .children(fields.into_iter().map(|field| {
                let label = self.tr(field.label_id());
                let label = if let Some(group) = field.group_label_id() {
                    SharedString::from(format!("{} · {label}", self.tr(group)))
                } else {
                    label
                };
                let line = match field {
                    Mode => {
                        let mode = self.tr(match config.mode {
                            DenoiseMode::Off => "processing-off",
                            DenoiseMode::Stationary => "processing-stationary",
                            DenoiseMode::Profile => "processing-profile",
                            DenoiseMode::Adaptive => "processing-adaptive",
                        });
                        format!("{label}: {mode}")
                    }
                    HumEnabled => format!("{label}: {}", self.captured_toggle(config.hum.enabled)),
                    ImpulseEnabled => {
                        format!("{label}: {}", self.captured_toggle(config.impulse.enabled))
                    }
                    _ => self.captured_denoise_line(&label, field, config),
                };
                caption(theme, line)
            }))
    }

    fn captured_neural(&self, config: &NeuralConfig, speech: bool, cx: &App) -> Div {
        use NeuralField::*;
        let theme = rivu_ui::theme::palette(cx);
        let fields: &[NeuralField] = if speech {
            &[
                SpeechInterpreter,
                Worker,
                SpeechManifest,
                Device,
                Threads,
                SpeechWetMix,
                SpeechAttenuationLimitDb,
                SpeechPostFilter,
            ]
        } else {
            &[
                SeparationInterpreter,
                Worker,
                SeparationManifest,
                Device,
                Threads,
                SeparationOverlap,
            ]
        };
        column()
            .min_w_0()
            .gap_1()
            .children(fields.iter().map(|&field| {
                let label = self.tr(field.label_id());
                let line = match field {
                    SpeechManifest if config.speech_manifest.is_none() => format!("{label}: —"),
                    SeparationManifest if config.separation_manifest.is_none() => {
                        format!("{label}: —")
                    }
                    SpeechPostFilter => format!(
                        "{label}: {}",
                        self.captured_toggle(config.speech_post_filter)
                    ),
                    _ => Self::captured_neural_line(&label, field, config),
                };
                caption(theme, line)
            }))
    }

    fn task_settings(&self, task: &rivu_protocol::model::ProcessingTaskStatus, cx: &App) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let parameters = match task.operation {
            ProcessingOperation::Denoise => self.captured_denoise(&task.settings.denoise, cx),
            ProcessingOperation::Speech | ProcessingOperation::Separate => self.captured_neural(
                &task.settings.neural,
                task.operation == ProcessingOperation::Speech,
                cx,
            ),
            ProcessingOperation::Graph => unreachable!("graph tasks have a separate results view"),
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
            .child(parameters);
        if let Some(profile) = task.settings.profile {
            content = content.child(caption(
                theme,
                format!(
                    "{}: {} s · {}: {} s",
                    self.context.i18n.text("processing-profile-start"),
                    profile.start_seconds,
                    self.context.i18n.text("processing-profile-end"),
                    profile.end_seconds
                ),
            ));
        }
        content
    }

    fn task_status(&mut self, panel: u64, cx: &mut Context<Self>) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let mut content = column().gap_2();
        let state = Rc::clone(&self.state);
        if let Some(task) = &state.system.processing {
            let id = task.id;
            let terminal = matches!(
                task.phase,
                ProcessingPhase::Complete | ProcessingPhase::Cancelled | ProcessingPhase::Failed
            );
            let phase = self.tr(rivu_presentation::processing::phase_key(task.phase));
            let operation = self.tr(match task.operation {
                ProcessingOperation::Denoise => "processing-restoration",
                ProcessingOperation::Speech => "processing-speech",
                ProcessingOperation::Separate => "processing-separation",
                ProcessingOperation::Graph => "processing-routing-graph",
            });
            content = content.child(
                caption(
                    theme,
                    format!(
                        "{operation} · {phase} · {} / {}",
                        task.completed,
                        task.total
                            .map(|total| total.to_string())
                            .unwrap_or_else(|| "?".into()),
                    ),
                )
                .id(("processing-task-status", panel))
                .tooltip(rivu_ui::text_tooltip(format!(
                    "{} #{id}\n{}",
                    self.tr("processing-task"),
                    task.source_path.display(),
                ))),
            );
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
                    crate::gui::appearance::material::button(
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
            if let Some(outputs) = &task.outputs {
                content = content.child(caption(theme, self.tr("processing-audition-source")));
                if state.system.config.processing_bypassed {
                    content =
                        content.child(caption(theme, self.tr("processing-audition-bypassed")));
                }
                let mut auditions = row().flex_wrap().gap_1();
                let modes = [
                    (AuditionMode::Original, "processing-original", true),
                    (
                        AuditionMode::Denoised,
                        "processing-processed",
                        outputs.denoised,
                    ),
                    (AuditionMode::Vocals, "processing-vocals", outputs.vocals),
                    (
                        AuditionMode::Accompaniment,
                        "processing-accompaniment",
                        outputs.accompaniment,
                    ),
                    (
                        AuditionMode::Mix,
                        "processing-mix",
                        outputs.vocals && outputs.accompaniment,
                    ),
                ];
                for (mode, label, _) in modes.into_iter().filter(|(_, _, available)| *available) {
                    auditions = auditions.child(disabled_control(
                        theme,
                        crate::gui::appearance::material::button(
                            SharedString::from(format!("audition-{panel}-{mode:?}")),
                            self.tr(label),
                            cx,
                            move |this, _, cx| this.audition(id, mode, cx),
                        )
                        .debug_selector(move || format!("processing-audition-{panel}-{mode:?}")),
                        state.system.config.processing_bypassed,
                    ));
                }
                content = content.child(auditions);
                if outputs.vocals && outputs.accompaniment {
                    content = content
                        .child(self.field_view(
                            panel,
                            FieldKey::VocalGain,
                            "processing-vocal-gain",
                            cx,
                        ))
                        .child(self.field_view(
                            panel,
                            FieldKey::AccompanimentGain,
                            "processing-accompaniment-gain",
                            cx,
                        ))
                        .child(disabled_control(
                            theme,
                            crate::gui::appearance::material::button(
                                ("apply-mix-gains", panel),
                                self.tr("processing-apply-mix"),
                                cx,
                                move |this, _, cx| this.audition(id, AuditionMode::Mix, cx),
                            )
                            .self_start(),
                            state.system.config.processing_bypassed,
                        ));
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
                    crate::gui::appearance::material::surface(cx),
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
        let body = self.processing_scroll(panel, "processing-results", results.p_2(), cx);
        column().size_full().min_h_0().child(body)
    }
    pub(super) fn graph_task_summary(&mut self, panel: u64, cx: &mut Context<Self>) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let Some(task) = self.state.system.processing.as_ref() else {
            return column();
        };
        let id = task.id;
        let phase = self.tr(rivu_presentation::processing::phase_key(task.phase));
        let source = task
            .source_path
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_else(|| "—".into());
        let export_label = self.tr("processing-tab-export");
        let (operation, separator) = if task.settings.audio_export.is_some() {
            (export_label.as_ref(), " · ")
        } else {
            ("", "")
        };
        let summary = format!(
            "{operation}{separator}{phase} · {} / {}",
            task.completed,
            task.total
                .map(|v| v.to_string())
                .unwrap_or_else(|| "?".into())
        );
        let mut details = format!(
            "{} #{id} · {source}\n{}\n{summary}",
            self.tr("processing-task"),
            task.source_path.display(),
        );
        if let Some(graph) = &task.settings.graph {
            let graph_current = &mut self.results.graph_current;
            let current = *graph_current
                .get_or_insert_with(|| graph.audio_eq(&self.state.system.config.processing_graph));
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
            details.push_str(&format!(
                " · {mode} · {}",
                self.tr(if current {
                    "processing-result-current"
                } else {
                    "processing-result-edited"
                })
            ));
            if let Some(target) = task.settings.graph_target {
                details.push_str(&format!(
                    " · {} #{}:{}",
                    self.tr("processing-result-target"),
                    target.node,
                    target.port
                ));
            }
        }
        let mut actions = row().min_w_0().flex_wrap().gap_1();
        if self.page != Page::Results {
            actions = actions.child(
                crate::gui::appearance::material::button(
                    ("graph-show-results", panel),
                    self.tr("processing-view-results"),
                    cx,
                    |this, _, cx| this.show_graph_results(cx),
                )
                .flex_shrink_0()
                .whitespace_nowrap(),
            );
        }
        if !matches!(
            task.phase,
            ProcessingPhase::Complete | ProcessingPhase::Cancelled | ProcessingPhase::Failed
        ) {
            actions = actions.child(
                crate::gui::appearance::material::button(
                    ("graph-cancel-task", panel),
                    self.tr("processing-cancel"),
                    cx,
                    move |this, _, cx| this.send(Command::CancelProcessing { id }, cx),
                )
                .debug_selector(move || format!("graph-cancel-task-{panel}"))
                .flex_shrink_0()
                .whitespace_nowrap(),
            );
        }
        let mut content = column().min_w_0().gap_1().child(
            row()
                .min_w_0()
                .flex_wrap()
                .items_center()
                .gap_1()
                .child(
                    caption(theme, summary)
                        .min_w(rems(8.))
                        .flex_1()
                        .truncate()
                        .id(("processing-task-summary", panel))
                        .tooltip(rivu_ui::text_tooltip(details)),
                )
                .child(actions),
        );
        if let Some(total) = task.total.filter(|total| *total > 0) {
            content = content.child(
                rivu_ui::ProgressBar::new(
                    ("processing-summary-progress", panel),
                    (task.completed as f64 / total as f64) as f32,
                    self.tr("processing-progress"),
                )
                .render(cx),
            );
        }
        content
    }

    pub(super) fn graph_task_status(&mut self, panel: u64, cx: &mut Context<Self>) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let Some(task) = self.state.system.processing.as_ref() else {
            return column();
        };
        let Some(id) = task.graph_run_id else {
            return column();
        };
        let export_receipt = id != task.id || task.settings.audio_export.is_some();
        let expanded = self.results.details;
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
        // Receipt descriptions are independent from the retained-resource action gate below.
        if task.phase == ProcessingPhase::Complete {
            let ready = if task.graph_exported_path.is_some() {
                "processing-graph-export-ready"
            } else if task.settings.graph_mode == Some(GraphExecutionMode::Hybrid) {
                "processing-result-hybrid-prepared"
            } else if task.settings.graph_mode == Some(GraphExecutionMode::Realtime) {
                "processing-result-live-prepared"
            } else {
                "processing-graph-memory-ready"
            };
            content = content.child(caption(theme, self.tr(ready)));
        }
        if self.state.system.config.processing_bypassed {
            content = content.child(caption(theme, self.tr("processing-audition-bypassed")));
        }
        if graph_action_run(task).is_some() {
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
                        .child(disabled_control(
                            theme,
                            crate::gui::appearance::material::button(
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
                                    this.send(
                                        Command::ProcessingGraphAudition { id, node: port },
                                        cx,
                                    )
                                },
                            )
                            .debug_selector(move || {
                                format!("graph-audition-{panel}-{id}-{}-{}", port.node, port.port)
                            }),
                            self.state.system.config.processing_bypassed,
                        ))
                        .child(
                            crate::gui::appearance::material::button(
                                SharedString::from(format!(
                                    "graph-export-{panel}-{id}-{}-{}",
                                    port.node, port.port
                                )),
                                self.tr("processing-export-configure"),
                                cx,
                                move |this, _, cx| this.open_frozen_export(id, port, cx),
                            )
                            .debug_selector(move || {
                                format!("graph-export-{panel}-{id}-{}-{}", port.node, port.port)
                            }),
                        ),
                );
            }
        }
        content = content.child(
            crate::gui::appearance::material::button(
                ("graph-result-details", panel),
                self.tr(if expanded {
                    "processing-result-hide-details"
                } else {
                    "processing-result-show-details"
                }),
                cx,
                move |this, _, cx| {
                    this.results.details = !this.results.details;
                    cx.notify();
                },
            )
            .self_start(),
        );
        if expanded {
            if let Some(request) = &task.settings.audio_export {
                content = content.child(self.export_receipt_settings(request, cx));
            }
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
                        content = content
                            .child(caption(theme, self.captured_graph_line(field, &node.kind)));
                    }
                    if let GraphNodeKind::Equalizer { config, .. } = &node.kind {
                        use rivu_dsp::equalizer::config::{ChannelTarget, FilterKind};
                        for (label, text) in [
                            ("processing-enabled", config.enabled.to_string()),
                            ("processing-preamp", format!("{} dB", config.preamp_db)),
                            (
                                "processing-output-gain",
                                format!("{} dB", config.output_gain_db),
                            ),
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
                                ("processing-frequency", format!("{} Hz", band.frequency_hz)),
                                ("processing-gain", format!("{} dB", band.gain_db)),
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
                let phase = self.tr(rivu_presentation::processing::phase_key(node.phase));
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
