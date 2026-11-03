//! Typed node drafts and their optimistic, atomic commit boundary.
use super::*;
use rivu_ui::{FormField, IconButton, IconButtonDensity, IconButtonStyle};

#[derive(Clone, Copy, Debug)]
pub(super) struct DraftStamp {
    pub(super) revision: u64,
    pub(super) generation: u64,
}

#[derive(Debug)]
pub(super) struct SubmittedField {
    pub(super) key: FieldKey,
    pub(super) text: String,
    pub(super) generation: u64,
}

pub(super) fn submission_current(
    stamp: Option<&DraftStamp>,
    submitted: &SubmittedField,
    text: Option<&str>,
) -> bool {
    stamp.is_some_and(|stamp| {
        text.is_some_and(|text| {
            submitted_text_current(
                stamp.generation,
                submitted.generation,
                text,
                &submitted.text,
            )
        })
    })
}

fn draft_revision_current(stamp: Option<&DraftStamp>, revision: u64) -> bool {
    stamp.is_none_or(|stamp| stamp.revision == revision)
}

fn stepped_field_text(timestamp: bool, text: &str, step: f64) -> Option<String> {
    let value = text.parse::<f64>().ok()?;
    let stepped = value + step;
    if !value.is_finite() || !stepped.is_finite() {
        return None;
    }
    if step.fract() == 0. || timestamp {
        return Some(stepped.to_string());
    }
    let decimal_places = |text: &str| {
        let (mantissa, exponent) = text.split_once(['e', 'E']).unwrap_or((text, "0"));
        let fraction = mantissa.split_once('.').map_or(0, |(_, part)| part.len());
        let exponent = exponent.parse::<i32>().unwrap_or(0);
        // f64 has no useful decimal precision beyond its smallest subnormal.
        (fraction as i64 - i64::from(exponent)).clamp(0, 324) as usize
    };
    let precision = decimal_places(text).max(decimal_places(&step.to_string()));
    // Round decimal addition noise without narrowing the finite f64 range or
    // discarding fractional digits already present in the draft.
    let mut text = format!("{stepped:.precision$}");
    if precision > 0 {
        text.truncate(text.trim_end_matches('0').trim_end_matches('.').len());
    }
    Some(text)
}

/// Construct and validate a replacement without mutating the committed graph.
fn node_replacement(
    graph: &ProcessingGraph,
    id: u64,
    patches: &[(FieldKey, String)],
    equalizer: Option<rivu_dsp::equalizer::config::EqualizerConfig>,
) -> Result<GraphEdit, (Option<FieldKey>, String)> {
    let node = graph
        .nodes
        .iter()
        .find(|node| node.id == id)
        .ok_or_else(|| (None, "Node was removed".to_owned()))?;
    let mut kind = node.kind.clone();
    for (key, text) in patches {
        let FieldKey::Graph(binding) = key else {
            return Err((Some(*key), "Invalid node field".to_owned()));
        };
        if binding.node != id {
            return Err((Some(*key), "Fields belong to different nodes".into()));
        }
        if !binding.matches(node) {
            return Err((Some(*key), "Node kind changed".into()));
        }
        binding
            .field
            .apply(&mut kind, text)
            .map_err(|error| (Some(*key), error))?;
    }
    if let Some(draft) = equalizer {
        let GraphNodeKind::Equalizer { config, .. } = &mut kind else {
            return Err((None, "Equalizer draft target changed".into()));
        };
        *config = draft;
    }
    let mut candidate = graph.clone();
    candidate
        .nodes
        .iter_mut()
        .find(|node| node.id == id)
        .expect("existing node")
        .kind = kind;
    candidate
        .validate()
        .map_err(|error| (None, error.to_string()))?;
    let kind = candidate
        .nodes
        .into_iter()
        .find(|node| node.id == id)
        .expect("existing node")
        .kind;
    Ok(GraphEdit::Replace { id, kind })
}

impl AudioProcessing {
    pub(in super::super) fn graph_dirty_count(&self, cx: &App) -> usize {
        self.fields
            .iter()
            .filter(|(key, field)| key.graph_node().is_some() && field.dirty)
            .count()
            + self.node_equalizer_dirty_count(cx)
    }

    pub(in super::super) fn graph_node_dirty_count(&self, id: u64, cx: &App) -> usize {
        self.fields
            .iter()
            .filter(|(key, field)| key.graph_node() == Some(id) && field.dirty)
            .count()
            + self.node_equalizer_dirty_count_for(id, cx)
    }

    pub(in super::super) fn graph_has_drafts(&self) -> bool {
        self.fields
            .iter()
            .any(|(key, field)| key.graph_node().is_some() && field.dirty)
    }

    pub(in crate::gui::audio_processing) fn stage_graph_text(
        &mut self,
        key: FieldKey,
        text: String,
        cx: &mut Context<Self>,
    ) {
        let Some(field) = self.fields.get_mut(&key) else {
            return;
        };
        field.dirty = true;
        field.generation += 1;
        field.input.update(cx, |input, cx| input.set_text(text, cx));
        if key.graph_node().is_some() {
            self.graph_editor.draft_generation += 1;
            let stamp = self
                .graph_editor
                .draft_stamps
                .entry(key)
                .or_insert(DraftStamp {
                    revision: self.state.system.config.processing_graph.revision,
                    generation: 0,
                });
            stamp.generation = self.graph_editor.draft_generation;
            self.graph_editor.field_errors.remove(&key);
        }
        if key == FieldKey::GraphExport {
            self.results.export_draft_generation += 1;
        }
        cx.notify();
    }

    pub(super) fn submit_graph_fields(&mut self, keys: Vec<FieldKey>, cx: &mut Context<Self>) {
        let Some(id) = keys.first().and_then(|key| key.graph_node()) else {
            return;
        };
        self.submit_node_drafts(id, keys, false, cx);
    }

    fn submit_node_drafts(
        &mut self,
        id: u64,
        mut keys: Vec<FieldKey>,
        include_equalizer: bool,
        cx: &mut Context<Self>,
    ) {
        if self.graph_busy() {
            return;
        }
        self.reconcile(self.context.snapshot(), cx);
        keys.sort();
        let revision = self.state.system.config.processing_graph.revision;
        let stale = keys
            .iter()
            .any(|key| !draft_revision_current(self.graph_editor.draft_stamps.get(key), revision));
        if stale {
            let error = self.tr("processing-fields-stale").to_string();
            for key in keys {
                self.graph_editor.field_errors.insert(key, error.clone());
            }
            cx.notify();
            return;
        }
        let patches: Vec<_> = keys
            .iter()
            .filter_map(|key| {
                self.fields
                    .get(key)
                    .map(|field| (*key, field.input.read(cx).text().to_owned()))
            })
            .collect();
        let mut equalizer_submitted = if include_equalizer {
            let prepared = match self
                .state
                .system
                .config
                .processing_graph
                .nodes
                .iter()
                .find(|node| node.id == id)
            {
                Some(GraphNode {
                    kind: GraphNodeKind::Equalizer { config, .. },
                    ..
                }) => self.prepare_node_equalizer_drafts(id, config, cx),
                _ => Ok(None),
            };
            match prepared {
                Ok(Some(submission)) if submission.revision != revision => {
                    self.error = Some(self.tr("processing-fields-stale").to_string());
                    cx.notify();
                    return;
                }
                Ok(submission) => submission,
                Err(error) => {
                    self.error = Some(error);
                    cx.notify();
                    return;
                }
            }
        } else {
            None
        };
        if patches.is_empty() && equalizer_submitted.is_none() {
            return;
        }
        match node_replacement(
            &self.state.system.config.processing_graph,
            id,
            &patches,
            equalizer_submitted
                .as_mut()
                .map(|submission| std::mem::take(&mut submission.config)),
        ) {
            Ok(edit) => {
                let submitted = patches
                    .into_iter()
                    .map(|(key, text)| SubmittedField {
                        generation: self
                            .graph_editor
                            .draft_stamps
                            .get(&key)
                            .map_or(0, |stamp| stamp.generation),
                        key,
                        text,
                    })
                    .collect();
                for key in keys {
                    self.graph_editor.field_errors.remove(&key);
                }
                self.graph_submit(
                    edit,
                    HistoryAction::Edit,
                    submitted,
                    equalizer_submitted,
                    cx,
                );
            }
            Err((field, error)) => {
                if let Some(field) = field {
                    self.graph_editor.field_errors.insert(field, error);
                } else {
                    self.error = Some(error.clone());
                    for key in keys {
                        self.graph_editor.field_errors.insert(key, error.clone());
                    }
                }
                cx.notify();
            }
        }
    }

    fn apply_graph_node(&mut self, id: u64, cx: &mut Context<Self>) {
        let keys = self
            .fields
            .iter()
            .filter(|(key, field)| key.graph_node() == Some(id) && field.dirty)
            .map(|(key, _)| *key)
            .collect();
        self.submit_node_drafts(id, keys, true, cx);
    }

    pub(in super::super) fn discard_graph_node(&mut self, id: u64, cx: &mut Context<Self>) {
        self.discard_node_equalizer_drafts(id, cx);
        if self
            .choosing_path
            .as_ref()
            .is_some_and(|request| request.key.graph_node() == Some(id))
        {
            self.choosing_path = None;
        }
        for (key, field) in &mut self.fields {
            if key.graph_node() == Some(id) {
                field.dirty = false;
                let committed = field.committed.clone();
                field
                    .input
                    .update(cx, |input, cx| input.set_text(committed, cx));
                self.graph_editor.draft_stamps.remove(key);
                self.graph_editor.field_errors.remove(key);
            }
        }
        cx.notify();
    }

    fn step_graph_field(&mut self, key: FieldKey, step: f64, cx: &mut Context<Self>) {
        let timestamp =
            matches!(key, FieldKey::Graph(NodeField { field, .. }) if field.is_timestamp());
        match stepped_field_text(timestamp, &self.text(key, cx), step) {
            Some(text) => {
                self.stage_graph_text(key, text, cx);
            }
            None => {
                self.graph_editor
                    .field_errors
                    .insert(key, self.tr("processing-fields-number").to_string());
                cx.notify();
            }
        }
    }

    fn choose_graph_path(&mut self, panel: u64, key: FieldKey, cx: &mut Context<Self>) {
        if self.choosing_path.is_some() {
            return;
        }
        let Some(field) = self.fields.get(&key) else {
            return;
        };
        let original = field.input.read(cx).text().to_owned();
        let generation = self
            .graph_editor
            .draft_stamps
            .get(&key)
            .map_or(0, |stamp| stamp.generation);
        let revision = self.state.system.config.processing_graph.revision;
        let title = self.tr("processing-fields-browse").to_string();
        let request = self.begin_path_request(panel, key);
        cx.spawn(async move |this, cx| {
            let result = ashpd::desktop::file_chooser::SelectedFiles::open_file()
                .title(title.as_str())
                .modal(true)
                .send()
                .await
                .and_then(|request| request.response());
            let _ = this.update(cx, |this, cx| {
                this.finish_graph_path_request(
                    request,
                    (revision, generation, &original),
                    crate::gui::chooser_result(result),
                    cx,
                );
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn finish_graph_path_request<E: std::fmt::Display>(
        &mut self,
        request: PathRequest,
        snapshot: (u64, u64, &str),
        result: Result<Option<ashpd::desktop::file_chooser::SelectedFiles>, E>,
        cx: &mut Context<Self>,
    ) {
        let PathRequest { panel, key, .. } = request;
        let owner_current = self.panel_inputs.contains_key(&panel)
            && matches!(key, FieldKey::Graph(binding) if self.state.system.config.processing_graph.nodes.iter().any(|node| binding.matches(node)))
            && path_snapshot_current(
                snapshot,
                (
                    self.state.system.config.processing_graph.revision,
                    self.graph_editor
                        .draft_stamps
                        .get(&key)
                        .map_or(0, |stamp| stamp.generation),
                    self.fields
                        .get(&key)
                        .map(|field| field.input.read(cx).text()),
                ),
            );
        if !retire_path_request(&mut self.choosing_path, request) {
            return;
        }
        cx.notify();
        if !owner_current {
            return;
        }
        match result {
            Ok(Some(files)) => {
                if let Some(path) = crate::gui::chooser_path(&files) {
                    match path.into_os_string().into_string() {
                        Ok(path) => self.stage_graph_text(key, path, cx),
                        Err(_) => {
                            self.graph_editor.field_errors.insert(
                                key,
                                self.tr("processing-result-path-encoding").to_string(),
                            );
                        }
                    }
                }
            }
            Ok(None) => {}
            Err(error) => {
                self.graph_editor
                    .field_errors
                    .insert(key, error.to_string());
            }
        }
        cx.notify();
    }

    fn graph_typed_control(
        &mut self,
        panel: u64,
        binding: NodeField,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let descriptor = binding.field;
        let key = FieldKey::Graph(binding);
        let label = self.graph_parameter_label(descriptor);
        let dirty = self.fields[&key].dirty;
        let label = if dirty {
            format!("{label} · {}", self.tr("processing-draft"))
        } else {
            label
        };
        let text = self.text(key, cx);
        let choices = descriptor.choices();
        let control = if descriptor == GraphField::Denoise(DenoiseField::FftSize) {
            self.workbench_dropdown(panel, super::workbench::WorkbenchMenu::Fft, text.into(), cx)
                .into_any_element()
        } else if !choices.is_empty() {
            let mut options = row().flex_wrap().gap_1();
            for &(choice, label) in choices {
                options = options.child(
                    button(
                        SharedString::from(format!("graph-choice-{panel}-{key:?}-{choice}")),
                        self.tr(label),
                        cx,
                        move |this, _, cx| this.stage_graph_text(key, choice.to_owned(), cx),
                    )
                    .when(text == choice, |view| view.bg(rgb(theme.selected))),
                );
            }
            options.into_any_element()
        } else if descriptor.is_boolean() {
            checkbox(
                SharedString::from(format!("graph-bool-{panel}-{key:?}")),
                SharedString::from(self.tr("processing-fields-enabled").to_string()),
                text == "true",
                true,
                cx,
                move |this, enabled, cx| this.stage_graph_text(key, enabled.to_string(), cx),
            )
            .into_any_element()
        } else {
            let input = self.panel_input(panel, key, &label, cx);
            let editor = if let Some(step) = descriptor.numeric_step() {
                let mut numeric = rivu_ui::NumericInput::new(input);
                for (sign, delta) in [("−", -step), ("+", step)] {
                    numeric = numeric.step(button(
                        SharedString::from(format!("graph-step-{panel}-{key:?}-{sign}")),
                        SharedString::from(format!("{sign}{}", step)),
                        cx,
                        move |this, _, cx| this.step_graph_field(key, delta, cx),
                    ));
                }
                numeric.into_div()
            } else {
                row().gap_1().child(div().flex_1().min_w_0().child(input))
            };
            let mut editor = editor
                .id(SharedString::from(format!(
                    "graph-path-help-{panel}-{key:?}"
                )))
                .when_some(self.path_help(key), |view, help| {
                    view.tooltip(rivu_ui::text_tooltip(help))
                });
            if descriptor.is_path() {
                editor = editor.child(disabled_control(
                    theme,
                    button(
                        SharedString::from(format!("graph-browse-{panel}-{key:?}")),
                        self.tr("processing-fields-browse"),
                        cx,
                        move |this, _, cx| this.choose_graph_path(panel, key, cx),
                    ),
                    self.choosing_path.is_some(),
                ));
            }
            editor.into_any_element()
        };
        let apply_key = key;
        let mut field = FormField::new(theme, label, control).action(disabled_control(
            theme,
            button(
                SharedString::from(format!("graph-apply-{panel}-{key:?}")),
                self.tr("processing-fields-apply-one"),
                cx,
                move |this, _, cx| this.apply_graph_field(apply_key, cx),
            )
            .self_start(),
            self.graph_busy() || !dirty,
        ));
        if let Some(error) = self.graph_editor.field_errors.get(&key) {
            field = field.error(error.clone());
        }
        field.into_div()
    }

    pub(super) fn graph_inspector(
        &mut self,
        panel: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let Some(node) = self
            .state
            .system
            .config
            .processing_graph
            .nodes
            .iter()
            .find(|node| Some(node.id) == self.graph_editor.selected)
            .cloned()
        else {
            self.deactivate_equalizer_panel(panel, window, cx);
            return column();
        };
        let id = node.id;
        let drafts = self.graph_node_dirty_count(id, cx);
        let busy = self.graph_busy();
        let fixed = matches!(node.kind, GraphNodeKind::Source);
        let mut content = column()
            .flex_shrink_0()
            .gap_2()
            .p_2()
            .border_1()
            .border_color(rgb(theme.border))
            .child(
                row()
                    .gap_1()
                    .child(
                        caption(
                            theme,
                            format!(
                                "{} · {} #{id}",
                                self.tr("processing-graph-inspector"),
                                self.tr(node_label(&node.kind))
                            ),
                        )
                        .flex_1()
                        .min_w_0(),
                    )
                    .child(
                        IconButton::new(
                            ("graph-duplicate", panel),
                            "",
                            self.tr("processing-graph-duplicate"),
                        )
                        .enabled(!busy && !fixed && drafts == 0)
                        .build(
                            IconButtonStyle::Quiet,
                            IconButtonDensity::Compact,
                            cx,
                            |this, _, cx| this.graph_duplicate(cx),
                        ),
                    )
                    .child(
                        IconButton::new(
                            ("graph-delete", panel),
                            "󰆴",
                            self.tr("processing-graph-delete"),
                        )
                        .enabled(!busy && !fixed && drafts == 0)
                        .build(
                            IconButtonStyle::Quiet,
                            IconButtonDensity::Compact,
                            cx,
                            |this, _, cx| this.graph_remove(cx),
                        ),
                    ),
            )
            .child(
                caption(
                    theme,
                    format!("{}: {drafts}", self.tr("processing-fields-drafts")),
                )
                .id(("graph-draft-help", panel))
                .tab_index(0)
                .aria_label(self.tr("processing-graph-fields-submit-help"))
                .tooltip(rivu_ui::text_tooltip(
                    self.tr("processing-graph-fields-submit-help"),
                )),
            )
            .child(
                row()
                    .flex_wrap()
                    .gap_1()
                    .child(disabled_control(
                        theme,
                        button(
                            ("graph-apply-all", panel),
                            self.tr("processing-fields-apply-all"),
                            cx,
                            move |this, _, cx| this.apply_graph_node(id, cx),
                        ),
                        busy || drafts == 0,
                    ))
                    .child(disabled_control(
                        theme,
                        button(
                            ("graph-discard", panel),
                            self.tr("processing-fields-discard"),
                            cx,
                            move |this, _, cx| this.discard_graph_node(id, cx),
                        ),
                        busy || drafts == 0,
                    )),
            );
        if let GraphNodeKind::Equalizer { config, .. } = &node.kind {
            content = content.child(self.node_equalizer_view(panel, id, config, window, cx));
        } else {
            self.deactivate_equalizer_panel(panel, window, cx);
        }
        if let GraphNodeKind::Denoise {
            config,
            profile: None,
        } = &node.kind
        {
            let config = config.clone();
            content = content.child(disabled_control(
                theme,
                button(
                    ("graph-profile", panel),
                    self.tr("processing-graph-add-profile"),
                    cx,
                    move |this, _, cx| {
                        this.graph_edit(
                            GraphEdit::Replace {
                                id,
                                kind: GraphNodeKind::Denoise {
                                    config: config.clone(),
                                    profile: Some(
                                        rivu_format::config::processing_graph::NoiseSelection {
                                            start_seconds: 0.,
                                            end_seconds: 1.,
                                        },
                                    ),
                                },
                            },
                            cx,
                        );
                    },
                )
                .self_start(),
                busy || drafts > 0,
            ));
        }
        if let GraphNodeKind::Mix {
            gains_db,
            muted,
            solo,
        } = &node.kind
        {
            content = content.child(
                caption(theme, self.tr("processing-graph-input-gain"))
                    .id(("graph-mix-help", panel))
                    .tab_index(0)
                    .aria_label(self.tr("processing-graph-mix-help"))
                    .tooltip(rivu_ui::text_tooltip(self.tr("processing-graph-mix-help"))),
            );
            let mut lanes = row().flex_wrap().gap_1();
            for add in [true, false] {
                let mut gains_db = gains_db.clone();
                let mut muted = muted.clone();
                let mut solo = solo.clone();
                let disabled = busy
                    || drafts > 0
                    || if add {
                        gains_db.len() >= 16
                    } else {
                        gains_db.len() <= 2
                    };
                if add {
                    gains_db.push(0.);
                    muted.push(false);
                    solo.push(false);
                } else {
                    gains_db.pop();
                    muted.pop();
                    solo.pop();
                }
                let kind = GraphNodeKind::Mix {
                    gains_db,
                    muted,
                    solo,
                };
                lanes = lanes.child(disabled_control(
                    theme,
                    button(
                        SharedString::from(format!("graph-lane-{panel}-{add}")),
                        self.tr(if add {
                            "processing-graph-add-lane"
                        } else {
                            "processing-graph-remove-lane"
                        }),
                        cx,
                        move |this, _, cx| {
                            this.graph_edit(
                                GraphEdit::Replace {
                                    id,
                                    kind: kind.clone(),
                                },
                                cx,
                            )
                        },
                    ),
                    disabled,
                ));
            }
            content = content.child(lanes);
        }
        let neural = matches!(
            node.kind,
            GraphNodeKind::Speech { .. } | GraphNodeKind::Separate { .. }
        );
        let expanded = self.graph_editor.expanded_backends.contains(&id);
        if neural {
            content = content.child(caption(theme, self.tr("processing-fields-active")));
        }
        for group in 0..3 {
            if group == 1 && neural {
                content = content.child(caption(theme, self.tr("processing-fields-runtime")));
            }
            if group == 2 && neural {
                content = content.child(
                    button(
                        ("graph-backends", panel),
                        self.tr(if expanded {
                            "processing-fields-hide-backends"
                        } else {
                            "processing-fields-show-backends"
                        }),
                        cx,
                        move |this, _, cx| {
                            if !this.graph_editor.expanded_backends.remove(&id) {
                                this.graph_editor.expanded_backends.insert(id);
                            }
                            cx.notify();
                        },
                    )
                    .self_start(),
                );
                if !expanded {
                    continue;
                }
                content = content.child(caption(theme, self.tr("processing-fields-backends-help")));
                content = content.child(caption(theme, self.tr("processing-graph-model-help")));
            }
            for descriptor in fields::node_fields(&node.kind) {
                let binding = NodeField::new(&node, descriptor);
                let key = FieldKey::Graph(binding);
                if !self.fields.contains_key(&key) {
                    continue;
                }
                if descriptor == GraphField::Taps {
                    let implementation_key =
                        FieldKey::Graph(NodeField::new(&node, GraphField::Implementation));
                    if self.text(implementation_key, cx) != "linear_phase" {
                        continue;
                    }
                }
                let inactive = descriptor.inactive(&node.kind);
                let field_group = descriptor.group(&node.kind);
                if group != field_group {
                    continue;
                }
                if inactive {
                    content = content.child(caption(theme, self.tr("processing-fields-inactive")));
                }
                content = content.child(self.graph_typed_control(panel, binding, cx));
            }
        }
        if output_count(&node.kind) > 1 {
            let mut ports = row().flex_wrap().gap_1();
            for port in 0..output_count(&node.kind) {
                ports = ports.child(
                    button(
                        SharedString::from(format!("graph-target-{panel}-{port}")),
                        self.tr(if port == 0 {
                            "processing-graph-vocals"
                        } else {
                            "processing-graph-accompaniment"
                        }),
                        cx,
                        move |this, _, cx| {
                            this.graph_editor.target_port = port as u16;
                            cx.notify();
                        },
                    )
                    .when(self.graph_editor.target_port == port as u16, |view| {
                        view.bg(rgb(theme.selected))
                    }),
                );
            }
            content = content.child(ports);
        }
        content
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DraftStamp, EqImplementation, FieldKey, GraphEdit, GraphField, GraphNode, GraphNodeKind,
        NeuralField, NodeField, PathBuf, ProcessingGraph, SubmittedField, draft_revision_current,
        node_replacement, path_snapshot_current, stepped_field_text, submission_current,
    };

    fn key(graph: &ProcessingGraph, node: u64, field: GraphField) -> FieldKey {
        FieldKey::Graph(NodeField::new(graph.node(node).unwrap(), field))
    }

    #[test]
    fn decimal_steps_preserve_finite_f64_range_and_draft_precision() {
        for (text, step, expected) in [
            ("0.2", 0.1, "0.3"),
            ("0.123456789", 0.1, "0.223456789"),
            ("2e-3", 0.001, "0.003"),
            ("-0.2", -0.1, "-0.3"),
        ] {
            assert_eq!(
                stepped_field_text(false, text, step).as_deref(),
                Some(expected)
            );
        }
        for value in [1e100, -1e100, f64::MAX, -f64::MAX] {
            let stepped = stepped_field_text(false, &value.to_string(), 0.1).unwrap();
            assert_eq!(stepped.parse::<f64>().unwrap(), value);
        }
        let small = stepped_field_text(false, "1e-100", 1e-101)
            .unwrap()
            .parse::<f64>()
            .unwrap();
        assert!(((small - 1e-100) / 1e-101 - 1.).abs() < 1e-12);
        assert!(stepped_field_text(false, &f64::MAX.to_string(), f64::MAX).is_none());
        assert!(stepped_field_text(false, "inf", 0.1).is_none());
        assert!(stepped_field_text(false, "invalid", 0.1).is_none());
    }

    fn speech_graph() -> ProcessingGraph {
        let mut graph = ProcessingGraph::default();
        graph.nodes.push(GraphNode {
            id: 3,
            position: [0., 0.],
            kind: GraphNodeKind::Speech {
                config: rivu_format::config::NeuralConfig::default(),
            },
        });
        graph.next_id = 4;
        graph
    }

    #[test]
    fn fft_choices_are_legal_and_commit_only_through_validated_node_replacement() {
        let mut graph = ProcessingGraph::default();
        graph.nodes.push(GraphNode {
            id: 3,
            position: [0., 0.],
            kind: GraphNodeKind::Denoise {
                config: Default::default(),
                profile: None,
            },
        });
        graph.next_id = 4;
        let field = GraphField::Denoise(super::DenoiseField::FftSize);
        for &(value, _) in field.choices() {
            let edit = node_replacement(&graph, 3, &[(key(&graph, 3, field), value.into())], None)
                .unwrap();
            let GraphEdit::Replace {
                kind: GraphNodeKind::Denoise { config, .. },
                ..
            } = edit
            else {
                panic!("denoise node replacement");
            };
            assert_eq!(config.fft_size, value.parse::<usize>().unwrap());
        }
        for value in ["128", "257", "2049", "16384"] {
            assert!(
                node_replacement(&graph, 3, &[(key(&graph, 3, field), value.into())], None)
                    .is_err()
            );
        }
    }

    #[test]
    fn node_drafts_commit_as_one_validated_replacement() {
        let graph = speech_graph();
        let original = graph.clone();
        let patches = vec![
            (
                key(&graph, 3, GraphField::Neural(NeuralField::Threads)),
                " 4 ".into(),
            ),
            (
                key(&graph, 3, GraphField::Neural(NeuralField::SpeechWetMix)),
                ".12345679".into(),
            ),
            (
                key(&graph, 3, GraphField::Neural(NeuralField::Worker)),
                " /home/user/worker with spaces.py ".into(),
            ),
        ];
        let edit = node_replacement(&graph, 3, &patches, None).unwrap();
        let GraphEdit::Replace {
            id,
            kind: GraphNodeKind::Speech { config },
        } = edit
        else {
            panic!("atomic replacement");
        };
        assert_eq!(id, 3);
        assert_eq!(config.threads, 4);
        assert_eq!(config.speech_wet_mix, 0.12345679);
        assert_eq!(
            config.worker,
            PathBuf::from(" /home/user/worker with spaces.py ")
        );
        assert_eq!(graph, original);
        let invalid = vec![(patches[0].0, "4".into()), (patches[1].0, "2".into())];
        assert!(node_replacement(&graph, 3, &invalid, None).is_err());
        assert_eq!(graph, original);
    }

    #[test]
    fn acknowledgement_cannot_clear_later_text_even_when_retyped_identically() {
        let graph = speech_graph();
        let submitted = SubmittedField {
            key: key(&graph, 3, GraphField::Neural(NeuralField::Threads)),
            text: "4".into(),
            generation: 2,
        };
        assert!(submission_current(
            Some(&DraftStamp {
                revision: 8,
                generation: 2
            }),
            &submitted,
            Some("4")
        ));
        assert!(!submission_current(
            Some(&DraftStamp {
                revision: 8,
                generation: 3
            }),
            &submitted,
            Some("4")
        ));
        assert!(!submission_current(
            Some(&DraftStamp {
                revision: 8,
                generation: 2
            }),
            &submitted,
            Some("5")
        ));
        assert!(!submission_current(None, &submitted, Some("4")));
    }

    #[test]
    fn stale_drafts_and_late_file_choices_reject_changed_intent() {
        assert!(draft_revision_current(
            Some(&DraftStamp {
                revision: 7,
                generation: 2
            }),
            7
        ));
        assert!(!draft_revision_current(
            Some(&DraftStamp {
                revision: 7,
                generation: 2
            }),
            8
        ));
        assert!(path_snapshot_current(
            (7, 2, "python"),
            (7, 2, Some("python"))
        ));
        assert!(!path_snapshot_current(
            (7, 2, "python"),
            (8, 2, Some("python"))
        ));
        assert!(!path_snapshot_current(
            (7, 2, "python"),
            (7, 3, Some("python"))
        ));
        assert!(!path_snapshot_current(
            (7, 2, "python"),
            (7, 2, Some("new text"))
        ));
    }

    #[test]
    fn dependent_range_fields_validate_together_not_in_field_order() {
        let mut graph = speech_graph();
        graph.nodes.last_mut().unwrap().kind = GraphNodeKind::Denoise {
            config: rivu_dsp::denoise::DenoiseConfig::default(),
            profile: Some(rivu_format::config::processing_graph::NoiseSelection {
                start_seconds: 0.,
                end_seconds: 1.,
            }),
        };
        let start = (key(&graph, 3, GraphField::ProfileStart), "2".into());
        assert!(node_replacement(&graph, 3, std::slice::from_ref(&start), None).is_err());
        let edit = node_replacement(
            &graph,
            3,
            &[start, (key(&graph, 3, GraphField::ProfileEnd), "3".into())],
            None,
        )
        .unwrap();
        let GraphEdit::Replace {
            kind:
                GraphNodeKind::Denoise {
                    profile: Some(range),
                    ..
                },
            ..
        } = edit
        else {
            panic!("range replacement");
        };
        assert_eq!(range.start_seconds, 2.);
        assert_eq!(range.end_seconds, 3.);
    }

    #[test]
    fn equalizer_and_implementation_drafts_share_one_validated_edit() {
        let mut graph = speech_graph();
        graph.nodes.last_mut().unwrap().kind = GraphNodeKind::Equalizer {
            config: rivu_dsp::equalizer::config::EqualizerConfig::default(),
            implementation: EqImplementation::Iir,
        };
        let original = graph.clone();
        let draft = rivu_dsp::equalizer::config::EqualizerConfig {
            preamp_db: -3.1234567890123,
            ..Default::default()
        };
        let mut patches = vec![
            (
                key(&graph, 3, GraphField::Implementation),
                "linear_phase".into(),
            ),
            (key(&graph, 3, GraphField::Taps), "4097".into()),
        ];
        let edit = node_replacement(&graph, 3, &patches, Some(draft.clone())).unwrap();
        let GraphEdit::Replace {
            kind:
                GraphNodeKind::Equalizer {
                    config,
                    implementation,
                },
            ..
        } = edit
        else {
            panic!("one equalizer replacement");
        };
        assert_eq!(config.preamp_db, draft.preamp_db);
        assert_eq!(implementation, EqImplementation::LinearPhase { taps: 4097 });
        patches[1].1 = "4096".into();
        assert!(node_replacement(&graph, 3, &patches, Some(draft)).is_err());
        assert_eq!(graph, original);
    }

    #[test]
    fn invalid_field_and_cross_node_batches_never_produce_an_edit() {
        let graph = speech_graph();
        let invalid = vec![(
            key(&graph, 3, GraphField::Neural(NeuralField::Threads)),
            "not a number".into(),
        )];
        assert_eq!(
            node_replacement(&graph, 3, &invalid, None).unwrap_err().0,
            Some(invalid[0].0)
        );
        let mixed = vec![(
            key(&graph, 2, GraphField::Neural(NeuralField::Threads)),
            "2".into(),
        )];
        assert!(node_replacement(&graph, 3, &mixed, None).is_err());
    }

    #[test]
    fn typed_targets_reject_changed_variants_and_lane_shapes() {
        let mut kind = GraphNodeKind::Mix {
            gains_db: vec![0., 0.],
            muted: vec![false; 2],
            solo: vec![false; 2],
        };
        let original = kind.clone();
        assert!(GraphField::MixGain(2).apply(&mut kind, "3").is_err());
        assert!(
            GraphField::Neural(NeuralField::Threads)
                .apply(&mut kind, "4")
                .is_err()
        );
        assert!(GraphField::MixGain(0).apply(&mut kind, "NaN").is_err());
        assert_eq!(kind, original);
        GraphField::MixGain(1).apply(&mut kind, "-6.25").unwrap();
        GraphField::MixMuted(1).apply(&mut kind, "true").unwrap();
        GraphField::MixSolo(0).apply(&mut kind, "true").unwrap();
        assert_eq!(
            kind,
            GraphNodeKind::Mix {
                gains_db: vec![0., -6.25],
                muted: vec![false, true],
                solo: vec![true, false],
            }
        );
        let mut equalizer = GraphNodeKind::Equalizer {
            config: Default::default(),
            implementation: EqImplementation::Iir,
        };
        assert!(GraphField::Taps.apply(&mut equalizer, "4097").is_err());
    }
}
