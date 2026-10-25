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
    pub(super) key: String,
    pub(super) text: String,
    pub(super) generation: u64,
}

pub(super) fn submission_current(
    stamp: Option<&DraftStamp>,
    submitted: &SubmittedField,
    text: Option<&str>,
) -> bool {
    stamp.is_some_and(|stamp| stamp.generation == submitted.generation)
        && text == Some(submitted.text.as_str())
}

fn draft_revision_current(stamp: Option<&DraftStamp>, revision: u64) -> bool {
    stamp.is_none_or(|stamp| stamp.revision == revision)
}

pub(super) fn path_snapshot_current(
    requested: (u64, u64, &str),
    current: (u64, u64, Option<&str>),
) -> bool {
    requested.0 == current.0 && requested.1 == current.1 && current.2 == Some(requested.2)
}

fn field_identity(key: &str) -> Option<(u64, &str)> {
    let (id, path) = key.strip_prefix("graph.")?.split_once('/')?;
    Some((id.parse().ok()?, path))
}

fn stepped_field_text(key: &str, text: &str, step: f64) -> Option<String> {
    let value = text.parse::<f64>().ok()?;
    let stepped = value + step;
    if !value.is_finite() || !stepped.is_finite() {
        return None;
    }
    if step.fract() == 0. || key.ends_with("_seconds") {
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

fn patch_value(value: &mut Value, path: &str, text: &str) -> Result<(), String> {
    let text = if is_path(path) { text } else { text.trim() };
    if let Some(base) = path.strip_suffix("/implementation/taps") {
        let implementation = value
            .pointer_mut(&format!("/{base}/implementation"))
            .ok_or("Node field changed")?;
        let taps = text.parse::<usize>().map_err(|error| error.to_string())?;
        if implementation.as_str() == Some("iir") {
            return Err("Select linear-phase FIR and apply all to change its tap count".into());
        }
        *implementation = json!({"linear_phase": {"taps": taps}});
        return Ok(());
    }
    let slot = value
        .pointer_mut(&format!("/{path}"))
        .ok_or("Node field changed")?;
    *slot = if path.ends_with("/implementation") {
        match text {
            "iir" => json!("iir"),
            "linear_phase" => {
                let taps = slot
                    .pointer("/linear_phase/taps")
                    .and_then(Value::as_u64)
                    .unwrap_or(2049);
                json!({"linear_phase": {"taps": taps}})
            }
            _ => return Err("Unknown equalizer implementation".into()),
        }
    } else if path.ends_with("_manifest") && text.is_empty() {
        Value::Null
    } else if path.ends_with("/channels") {
        if text == "all" {
            json!("all")
        } else {
            let channels = text
                .split(',')
                .map(|part| {
                    part.trim()
                        .parse::<usize>()
                        .ok()
                        .and_then(|value| value.checked_sub(1))
                        .ok_or_else(|| {
                            "Use all or comma-separated, one-based channel numbers".to_owned()
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;
            json!({"selected": channels})
        }
    } else {
        match slot {
            Value::String(_) => Value::String(text.to_owned()),
            Value::Null if text.is_empty() => Value::Null,
            Value::Null => Value::String(text.to_owned()),
            Value::Number(number) if number.is_u64() => {
                Value::from(text.parse::<u64>().map_err(|error| error.to_string())?)
            }
            Value::Number(number) if number.is_i64() => {
                Value::from(text.parse::<i64>().map_err(|error| error.to_string())?)
            }
            Value::Number(_) => {
                let number = text.parse::<f64>().map_err(|error| error.to_string())?;
                Value::Number(
                    serde_json::Number::from_f64(number)
                        .ok_or_else(|| "Enter a finite number".to_owned())?,
                )
            }
            _ => serde_json::from_str(text).map_err(|error| error.to_string())?,
        }
    };
    Ok(())
}

/// Construct and validate a replacement without mutating the committed graph.
fn node_replacement(
    graph: &ProcessingGraph,
    id: u64,
    patches: &[(String, String)],
    equalizer: Option<rivu_dsp::equalizer::config::EqualizerConfig>,
) -> Result<GraphEdit, (Option<String>, String)> {
    let node = graph
        .nodes
        .iter()
        .find(|node| node.id == id)
        .ok_or_else(|| (None, "Node was removed".to_owned()))?;
    let mut serialized =
        serde_json::to_value(&node.kind).map_err(|error| (None, error.to_string()))?;
    for (key, text) in patches {
        let (field_id, path) = field_identity(key)
            .ok_or_else(|| (Some(key.clone()), "Invalid node field".to_owned()))?;
        if field_id != id {
            return Err((Some(key.clone()), "Fields belong to different nodes".into()));
        }
        patch_value(&mut serialized, path, text).map_err(|error| (Some(key.clone()), error))?;
    }
    let mut kind: GraphNodeKind =
        serde_json::from_value(serialized).map_err(|error| (None, error.to_string()))?;
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

fn enum_choices(path: &str) -> &'static [(&'static str, &'static str)] {
    if path.ends_with("/device") {
        &[
            ("cpu", "processing-fields-cpu"),
            ("cuda", "processing-fields-cuda"),
        ]
    } else if path.ends_with("/mode") {
        &[
            ("off", "processing-off"),
            ("stationary", "processing-stationary"),
            ("profile", "processing-profile"),
            ("adaptive", "processing-adaptive"),
        ]
    } else if path.ends_with("/implementation") {
        &[
            ("iir", "processing-graph-iir"),
            ("linear_phase", "processing-graph-fir"),
        ]
    } else {
        &[]
    }
}

fn is_path(path: &str) -> bool {
    path.ends_with("_interpreter") || path.ends_with("_manifest") || path.ends_with("/worker")
}

fn numeric_step(path: &str, value: &Value) -> Option<f64> {
    if !value.is_number() {
        return None;
    }
    Some(if path.ends_with("/taps") {
        2.
    } else if value.as_i64().is_some() || value.as_u64().is_some() {
        1.
    } else if path.ends_with("_mix") || path.ends_with("_overlap") {
        0.01
    } else if path.ends_with("_hz") {
        1.
    } else {
        0.1
    })
}

impl AudioProcessing {
    pub(in super::super) fn graph_dirty_count(&self, cx: &App) -> usize {
        self.fields
            .iter()
            .filter(|(key, field)| key.starts_with("graph.") && field.dirty)
            .count()
            + self.node_equalizer_dirty_count(cx)
    }

    pub(in super::super) fn graph_node_dirty_count(&self, id: u64, cx: &App) -> usize {
        let prefix = format!("graph.{id}/");
        self.fields
            .iter()
            .filter(|(key, field)| key.starts_with(&prefix) && field.dirty)
            .count()
            + self.node_equalizer_dirty_count_for(id, cx)
    }

    pub(in super::super) fn graph_has_drafts(&self) -> bool {
        self.fields
            .iter()
            .any(|(key, field)| key.starts_with("graph.") && field.dirty)
    }

    pub(super) fn stage_graph_text(&mut self, key: &str, text: String, cx: &mut Context<Self>) {
        let Some(field) = self.fields.get_mut(key) else {
            return;
        };
        field.dirty = true;
        field.input.update(cx, |input, cx| input.set_text(text, cx));
        if key.starts_with("graph.") {
            self.graph_ui.draft_generation += 1;
            let stamp = self
                .graph_ui
                .draft_stamps
                .entry(key.to_owned())
                .or_insert(DraftStamp {
                    revision: self.state.system.config.processing_graph.revision,
                    generation: 0,
                });
            stamp.generation = self.graph_ui.draft_generation;
            self.graph_ui.field_errors.remove(key);
        }
        if key == "graph_export" {
            self.graph_ui.export_draft_generation += 1;
        }
        cx.notify();
    }

    pub(super) fn submit_graph_fields(&mut self, keys: Vec<String>, cx: &mut Context<Self>) {
        let Some((id, _)) = keys.first().and_then(|key| field_identity(key)) else {
            return;
        };
        self.submit_node_drafts(id, keys, false, cx);
    }

    fn submit_node_drafts(
        &mut self,
        id: u64,
        mut keys: Vec<String>,
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
            .any(|key| !draft_revision_current(self.graph_ui.draft_stamps.get(key), revision));
        if stale {
            let error = self.tr("processing-fields-stale").to_string();
            for key in keys {
                self.graph_ui.field_errors.insert(key, error.clone());
            }
            cx.notify();
            return;
        }
        let patches: Vec<_> = keys
            .iter()
            .filter_map(|key| {
                self.fields
                    .get(key)
                    .map(|field| (key.clone(), field.input.read(cx).text().to_owned()))
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
                            .graph_ui
                            .draft_stamps
                            .get(&key)
                            .map_or(0, |stamp| stamp.generation),
                        key,
                        text,
                    })
                    .collect();
                for key in keys {
                    self.graph_ui.field_errors.remove(&key);
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
                    self.graph_ui.field_errors.insert(field, error);
                } else {
                    self.error = Some(error.clone());
                    for key in keys {
                        self.graph_ui.field_errors.insert(key, error.clone());
                    }
                }
                cx.notify();
            }
        }
    }

    fn apply_graph_node(&mut self, id: u64, cx: &mut Context<Self>) {
        let prefix = format!("graph.{id}/");
        let keys = self
            .fields
            .iter()
            .filter(|(key, field)| key.starts_with(&prefix) && field.dirty)
            .map(|(key, _)| key.clone())
            .collect();
        self.submit_node_drafts(id, keys, true, cx);
    }

    fn discard_graph_node(&mut self, id: u64, cx: &mut Context<Self>) {
        let prefix = format!("graph.{id}/");
        self.discard_node_equalizer_drafts(id, cx);
        if self
            .graph_ui
            .choosing_path
            .as_ref()
            .is_some_and(|key| key.starts_with(&prefix))
        {
            self.graph_ui.chooser_generation += 1;
        }
        for (key, field) in &mut self.fields {
            if key.starts_with(&prefix) {
                field.dirty = false;
                let committed = field.committed.clone();
                field
                    .input
                    .update(cx, |input, cx| input.set_text(committed, cx));
                self.graph_ui.draft_stamps.remove(key);
                self.graph_ui.field_errors.remove(key);
            }
        }
        cx.notify();
    }

    fn step_graph_field(&mut self, key: &str, step: f64, cx: &mut Context<Self>) {
        match stepped_field_text(key, &self.text(key, cx), step) {
            Some(text) => {
                self.stage_graph_text(key, text, cx);
            }
            None => {
                self.graph_ui.field_errors.insert(
                    key.to_owned(),
                    self.tr("processing-fields-number").to_string(),
                );
                cx.notify();
            }
        }
    }

    fn choose_graph_path(&mut self, panel: u64, key: String, cx: &mut Context<Self>) {
        if self.graph_ui.choosing_path.is_some() {
            return;
        }
        let Some(field) = self.fields.get(&key) else {
            return;
        };
        let original = field.input.read(cx).text().to_owned();
        let generation = self
            .graph_ui
            .draft_stamps
            .get(&key)
            .map_or(0, |stamp| stamp.generation);
        let revision = self.state.system.config.processing_graph.revision;
        let title = self.tr("processing-fields-browse").to_string();
        self.graph_ui.chooser_generation += 1;
        let chooser_generation = self.graph_ui.chooser_generation;
        self.graph_ui.choosing_path = Some(key.clone());
        cx.spawn(async move |this, cx| {
            let result = ashpd::desktop::file_chooser::SelectedFiles::open_file()
                .title(title.as_str())
                .modal(true)
                .send()
                .await
                .and_then(|request| request.response());
            let _ = this.update(cx, |this, cx| {
                if this.graph_ui.chooser_generation != chooser_generation {
                    cx.notify();
                    return;
                }
                this.graph_ui.choosing_path = None;
                if !this.graph_ui.panel_inputs.contains_key(&panel) {
                    cx.notify();
                    return;
                }
                match crate::gui::chooser_result(result) {
                    Ok(Some(files)) => {
                        let current = this
                            .fields
                            .get(&key)
                            .map(|field| field.input.read(cx).text());
                        let current_generation = this
                            .graph_ui
                            .draft_stamps
                            .get(&key)
                            .map_or(0, |stamp| stamp.generation);
                        if path_snapshot_current(
                            (revision, generation, &original),
                            (
                                this.state.system.config.processing_graph.revision,
                                current_generation,
                                current,
                            ),
                        ) && let Some(path) = crate::gui::chooser_path(&files)
                        {
                            match path.into_os_string().into_string() {
                                Ok(path) => this.stage_graph_text(&key, path, cx),
                                Err(_) => {
                                    this.graph_ui.field_errors.insert(
                                        key.clone(),
                                        this.tr("processing-result-path-encoding").to_string(),
                                    );
                                }
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        this.graph_ui.field_errors.insert(key, error.to_string());
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn graph_typed_control(
        &mut self,
        panel: u64,
        id: u64,
        path: &str,
        value: &Value,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let key = format!("graph.{id}{path}");
        let label = self.graph_parameter_label(path);
        let dirty = self.fields[&key].dirty;
        let label = if dirty {
            format!("{label} · {}", self.tr("processing-draft"))
        } else {
            label
        };
        let text = self.text(&key, cx);
        let choices = enum_choices(path);
        let control = if !choices.is_empty() {
            let mut options = row().flex_wrap().gap_1();
            for &(choice, label) in choices {
                let key = key.clone();
                options = options.child(
                    button(
                        SharedString::from(format!("graph-choice-{panel}-{key}-{choice}")),
                        self.tr(label),
                        cx,
                        move |this, _, cx| this.stage_graph_text(&key, choice.to_owned(), cx),
                    )
                    .when(text == choice, |view| view.bg(rgb(theme.selected))),
                );
            }
            options.into_any_element()
        } else if value.is_boolean() {
            let key = key.clone();
            checkbox(
                SharedString::from(format!("graph-bool-{panel}-{key}")),
                SharedString::from(self.tr("processing-fields-enabled").to_string()),
                text == "true",
                true,
                cx,
                move |this, enabled, cx| this.stage_graph_text(&key, enabled.to_string(), cx),
            )
            .into_any_element()
        } else {
            let input = self.graph_panel_input(panel, &key, &label, cx);
            let mut editor = row()
                .id(SharedString::from(format!("graph-path-help-{panel}-{key}")))
                .gap_1()
                .child(div().flex_1().min_w_0().child(input))
                .when_some(self.path_help(&key), |view, help| {
                    view.tooltip(rivu_ui::text_tooltip(help))
                });
            if let Some(step) = numeric_step(path, value) {
                for (sign, delta) in [("−", -step), ("+", step)] {
                    let key = key.clone();
                    editor = editor.child(button(
                        SharedString::from(format!("graph-step-{panel}-{key}-{sign}")),
                        SharedString::from(format!("{sign}{}", step)),
                        cx,
                        move |this, _, cx| this.step_graph_field(&key, delta, cx),
                    ));
                }
            }
            if is_path(path) {
                let key = key.clone();
                editor = editor.child(disabled_control(
                    theme,
                    button(
                        SharedString::from(format!("graph-browse-{panel}-{key}")),
                        self.tr("processing-fields-browse"),
                        cx,
                        move |this, _, cx| this.choose_graph_path(panel, key.clone(), cx),
                    ),
                    self.graph_ui.choosing_path.is_some(),
                ));
            }
            editor.into_any_element()
        };
        let apply_key = key.clone();
        let mut field = FormField::new(theme, label, control).action(disabled_control(
            theme,
            button(
                SharedString::from(format!("graph-apply-{panel}-{key}")),
                self.tr("processing-fields-apply-one"),
                cx,
                move |this, _, cx| this.apply_graph_field(&apply_key, cx),
            )
            .self_start(),
            self.graph_busy() || !dirty,
        ));
        if let Some(error) = self.graph_ui.field_errors.get(&key) {
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
            .find(|node| Some(node.id) == self.graph_ui.selected)
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
                .aria_label(self.tr("processing-fields-submit-help"))
                .tooltip(rivu_ui::text_tooltip(
                    self.tr("processing-fields-submit-help"),
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
        let mut fields = Vec::new();
        scalar_fields(
            &serde_json::to_value(&node.kind).expect("serializable node"),
            "",
            &mut fields,
        );
        let neural = matches!(
            node.kind,
            GraphNodeKind::Speech { .. } | GraphNodeKind::Separate { .. }
        );
        let expanded = self.graph_ui.expanded_backends.contains(&id);
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
                            if !this.graph_ui.expanded_backends.remove(&id) {
                                this.graph_ui.expanded_backends.insert(id);
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
            for (path, value) in &fields {
                let key = format!("graph.{id}{path}");
                if !self.fields.contains_key(&key) {
                    continue;
                }
                if path.ends_with("/taps") {
                    let implementation_key = format!("graph.{id}/equalizer/implementation");
                    if self.text(&implementation_key, cx) != "linear_phase" {
                        continue;
                    }
                }
                let name = path.rsplit('/').next().unwrap_or("");
                let inactive = match &node.kind {
                    GraphNodeKind::Speech { .. } => name.starts_with("separation_"),
                    GraphNodeKind::Separate { .. } => name.starts_with("speech_"),
                    _ => false,
                };
                let field_group = if neural && (is_path(path) || inactive) {
                    2
                } else if neural && matches!(name, "device" | "threads") {
                    1
                } else {
                    0
                };
                if group != field_group {
                    continue;
                }
                if inactive {
                    content = content.child(caption(theme, self.tr("processing-fields-inactive")));
                }
                content = content.child(self.graph_typed_control(panel, id, path, value, cx));
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
                            this.graph_ui.target_port = port as u16;
                            cx.notify();
                        },
                    )
                    .when(self.graph_ui.target_port == port as u16, |view| {
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
        DraftStamp, EqImplementation, GraphEdit, GraphNode, GraphNodeKind, PathBuf,
        ProcessingGraph, SubmittedField, draft_revision_current, node_replacement,
        path_snapshot_current, stepped_field_text, submission_current,
    };

    #[test]
    fn decimal_steps_preserve_finite_f64_range_and_draft_precision() {
        for (text, step, expected) in [
            ("0.2", 0.1, "0.3"),
            ("0.123456789", 0.1, "0.223456789"),
            ("2e-3", 0.001, "0.003"),
            ("-0.2", -0.1, "-0.3"),
        ] {
            assert_eq!(
                stepped_field_text("gain", text, step).as_deref(),
                Some(expected)
            );
        }
        for value in [1e100, -1e100, f64::MAX, -f64::MAX] {
            let stepped = stepped_field_text("gain", &value.to_string(), 0.1).unwrap();
            assert_eq!(stepped.parse::<f64>().unwrap(), value);
        }
        let small = stepped_field_text("gain", "1e-100", 1e-101)
            .unwrap()
            .parse::<f64>()
            .unwrap();
        assert!(((small - 1e-100) / 1e-101 - 1.).abs() < 1e-12);
        assert!(stepped_field_text("gain", &f64::MAX.to_string(), f64::MAX).is_none());
        assert!(stepped_field_text("gain", "inf", 0.1).is_none());
        assert!(stepped_field_text("gain", "invalid", 0.1).is_none());
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
    fn node_drafts_commit_as_one_validated_replacement() {
        let graph = speech_graph();
        let original = graph.clone();
        let patches = vec![
            ("graph.3/speech/config/threads".into(), " 4 ".into()),
            (
                "graph.3/speech/config/speech_wet_mix".into(),
                ".12345679".into(),
            ),
            (
                "graph.3/speech/config/worker".into(),
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
        let invalid = vec![
            (patches[0].0.clone(), "4".into()),
            (patches[1].0.clone(), "2".into()),
        ];
        assert!(node_replacement(&graph, 3, &invalid, None).is_err());
        assert_eq!(graph, original);
    }

    #[test]
    fn acknowledgement_cannot_clear_later_text_even_when_retyped_identically() {
        let submitted = SubmittedField {
            key: "graph.3/speech/config/threads".into(),
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
        let start = ("graph.3/denoise/profile/start_seconds".into(), "2".into());
        assert!(node_replacement(&graph, 3, std::slice::from_ref(&start), None).is_err());
        let edit = node_replacement(
            &graph,
            3,
            &[
                start,
                ("graph.3/denoise/profile/end_seconds".into(), "3".into()),
            ],
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
                "graph.3/equalizer/implementation".into(),
                "linear_phase".into(),
            ),
            (
                "graph.3/equalizer/implementation/taps".into(),
                "4097".into(),
            ),
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
            "graph.3/speech/config/threads".into(),
            "not a number".into(),
        )];
        assert_eq!(
            node_replacement(&graph, 3, &invalid, None).unwrap_err().0,
            Some(invalid[0].0.clone())
        );
        let mixed = vec![("graph.2/speech/config/threads".into(), "2".into())];
        assert!(node_replacement(&graph, 3, &mixed, None).is_err());
    }
}
