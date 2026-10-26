//! Core owns the graph; this view retains drafts, selection, viewport and undo.
//! Hidden fields own shared drafts/commit state. Painted inputs belong to one
//! panel each and mirror text only on changes, never sharing focus or geometry.
use super::*;
use rivu_format::config::processing_graph::{
    EqImplementation, GraphEdit, GraphNode, GraphNodeKind, GraphPort, ProcessingGraph,
};
use rivu_protocol::model::GraphExecutionMode;

mod workbench;
use workbench::{WorkbenchChoice, WorkbenchMenu, WorkbenchPanel};
mod results;

mod inspector;
use inspector::{DraftStamp, SubmittedField};

const NODE_WIDTH: f32 = 220.;
const HEADER: f32 = 36.;
const PORT_ROW: f32 = 28.;
const HISTORY_LIMIT: usize = 64;

#[derive(Clone, Copy)]
enum HistoryAction {
    Edit,
    Undo,
    Redo,
}

struct PendingEdit {
    before: ProcessingGraph,
    action: HistoryAction,
    submitted: Vec<SubmittedField>,
    equalizer_submitted: Option<super::equalizer_editor::NodeEqualizerSubmission>,
    equalizer_target: Option<u64>,
    accepted: bool,
    select_after: Option<u64>,
}

pub(super) struct GraphPointer {
    pub(super) panel: u64,
    start: Point<Pixels>,
    origin: [f32; 2],
    node: Option<u64>,
    preview: [f32; 2],
    bounds: Bounds<Pixels>,
    geometry: DockGeometry,
    epoch: u64,
    revision: u64,
    button: MouseButton,
    _subscriptions: [Subscription; 2],
}

struct PanelInput {
    input: Entity<Input>,
    _subscriptions: [Subscription; 2],
}

pub(super) struct GraphUi {
    selected: Option<u64>,
    connecting: Option<GraphPort>,
    highlight_connection: Option<GraphPort>,
    target_port: u16,
    pan: [f32; 2],
    zoom: f32,
    mode: GraphExecutionMode,
    revision: Option<u64>,
    fields_revision: Option<(u64, u64)>,
    undo: VecDeque<ProcessingGraph>,
    redo: VecDeque<ProcessingGraph>,
    pending: Option<PendingEdit>,
    pub(super) pointer: Option<GraphPointer>,
    pub(super) bounds: HashMap<u64, MeasuredGraph>,
    panel_inputs: HashMap<u64, HashMap<String, PanelInput>>,
    draft_stamps: HashMap<String, DraftStamp>,
    draft_generation: u64,
    field_errors: HashMap<String, String>,
    expanded_backends: std::collections::HashSet<u64>,
    choosing_path: Option<String>,
    chooser_generation: u64,
    export_draft_generation: u64,
    workbenches: HashMap<u64, WorkbenchPanel>,
    workbench_choices: rivu_ui::ChoiceState<WorkbenchChoice>,
    workbench_menu: Option<(u64, WorkbenchMenu)>,
    workbench_anchor: Cell<Option<Bounds<Pixels>>>,
    result_details: std::collections::HashSet<u64>,
    result_errors: HashMap<u64, String>,
}

impl Default for GraphUi {
    fn default() -> Self {
        Self {
            selected: None,
            connecting: None,
            highlight_connection: None,
            target_port: 0,
            pan: [24., 24.],
            zoom: 1.,
            mode: GraphExecutionMode::Auto,
            revision: None,
            undo: VecDeque::new(),
            fields_revision: None,
            redo: VecDeque::new(),
            pending: None,
            pointer: None,
            bounds: HashMap::new(),
            panel_inputs: HashMap::new(),
            draft_stamps: HashMap::new(),
            draft_generation: 0,
            field_errors: HashMap::new(),
            expanded_backends: std::collections::HashSet::new(),
            choosing_path: None,
            chooser_generation: 0,
            export_draft_generation: 0,
            workbenches: HashMap::new(),
            workbench_choices: rivu_ui::ChoiceState::default(),
            workbench_menu: None,
            workbench_anchor: Cell::new(None),
            result_details: std::collections::HashSet::new(),
            result_errors: HashMap::new(),
        }
    }
}

impl GraphUi {
    pub(super) fn input_focused(&self, window: &Window, cx: &App) -> bool {
        self.panel_inputs.values().any(|fields| {
            fields
                .values()
                .any(|field| field.input.read(cx).is_focused(window))
        })
    }

    pub(super) fn retain_panel_inputs(&mut self, layout: &super::super::layout::Layout) {
        self.panel_inputs.retain(|id, _| layout.contains_panel(*id));
        self.workbenches.retain(|id, _| layout.contains_panel(*id));
        self.result_details.retain(|id| layout.contains_panel(*id));
        self.result_errors
            .retain(|id, _| layout.contains_panel(*id));
    }
}

fn node_label(kind: &GraphNodeKind) -> &'static str {
    match kind {
        GraphNodeKind::Source => "processing-source",
        GraphNodeKind::Equalizer { .. } => "processing-equalizer",
        GraphNodeKind::Denoise { .. } => "processing-restoration",
        GraphNodeKind::Speech { .. } => "processing-speech",
        GraphNodeKind::Separate { .. } => "processing-separation",
        GraphNodeKind::Mix { .. } => "processing-mix",
        GraphNodeKind::Output => "processing-graph-output",
    }
}

fn input_count(kind: &GraphNodeKind) -> usize {
    kind.inputs()
}

fn output_count(kind: &GraphNodeKind) -> usize {
    kind.outputs()
}

fn node_height(kind: &GraphNodeKind) -> f32 {
    HEADER + PORT_ROW * input_count(kind).max(output_count(kind)) as f32 + 12.
}

fn scalar_fields(value: &Value, path: &str, fields: &mut Vec<(String, Value)>) {
    if path.ends_with("/implementation") {
        let fir = value.pointer("/linear_phase/taps").and_then(Value::as_u64);
        fields.push((
            path.to_owned(),
            json!(if fir.is_some() { "linear_phase" } else { "iir" }),
        ));
        fields.push((format!("{path}/taps"), json!(fir.unwrap_or(2049))));
        return;
    }
    if path.ends_with("/channels") {
        fields.push((path.to_owned(), value.clone()));
        return;
    }
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                scalar_fields(value, &format!("{path}/{key}"), fields);
            }
        }
        Value::Array(array) => {
            for (index, value) in array.iter().enumerate() {
                scalar_fields(value, &format!("{path}/{index}"), fields);
            }
        }
        _ => fields.push((path.to_owned(), value.clone())),
    }
}

impl AudioProcessing {
    fn graph_output_label(&self, kind: &GraphNodeKind, port: usize) -> String {
        if matches!(kind, GraphNodeKind::Separate { .. }) {
            self.tr(if port == 0 {
                "processing-graph-vocals"
            } else {
                "processing-graph-accompaniment"
            })
            .to_string()
        } else {
            format!("PCM {}", port + 1)
        }
    }

    fn graph_panel_input(
        &mut self,
        panel: u64,
        key: &str,
        label: &str,
        cx: &mut Context<Self>,
    ) -> Entity<Input> {
        if let Some(field) = self
            .graph_ui
            .panel_inputs
            .get(&panel)
            .and_then(|fields| fields.get(key))
        {
            return field.input.clone();
        }
        let backing = self.fields[key].input.clone();
        let placeholder = self
            .path_placeholder(key)
            .unwrap_or_else(|| label.to_owned().into());
        let input = cx.new(|cx| {
            Input::new(
                label.to_owned(),
                backing.read(cx).text().to_owned(),
                placeholder,
                cx,
            )
        });
        let name = key.to_owned();
        let edited = cx.subscribe(&input, move |this, input, event, cx| match event {
            InputEvent::Changed {
                user: true,
                composing: false,
            } => {
                let text = input.read(cx).text().to_owned();
                this.stage_graph_text(&name, text, cx);
                cx.notify();
            }
            InputEvent::Submit => this.apply_field(&name, cx),
            _ => {}
        });
        let replica = input.downgrade();
        let changed = cx.subscribe(&backing, move |_, backing, event, cx| {
            if matches!(event, InputEvent::Changed { .. })
                && let Some(input) = replica.upgrade()
                && input.read(cx).text() != backing.read(cx).text()
            {
                let text = backing.read(cx).text().to_owned();
                input.update(cx, |input, cx| input.set_text(text, cx));
            }
        });
        self.graph_ui.panel_inputs.entry(panel).or_default().insert(
            key.to_owned(),
            PanelInput {
                input: input.clone(),
                _subscriptions: [edited, changed],
            },
        );
        input
    }

    pub(super) fn graph_busy(&self) -> bool {
        self.graph_ui.pending.is_some() || self.pending > 0
    }

    pub(super) fn graph_collecting_equalizer_input(&self) -> Option<(u64, u64)> {
        let pending = self.graph_ui.pending.as_ref()?;
        pending
            .equalizer_submitted
            .as_ref()?
            .live_input
            .then_some(())?;
        Some((pending.equalizer_target?, pending.before.revision))
    }

    pub(super) fn reconcile_graph(&mut self, cx: &mut Context<Self>) {
        let mut equalizer_completed = None;
        let mut equalizer_rebased = None;
        let graph = &self.state.system.config.processing_graph;
        let revision = graph.revision;
        if self.graph_ui.revision != Some(revision) {
            self.graph_ui.pointer = None;
            if self.graph_ui.pending.is_none() && self.graph_ui.revision.is_some() {
                self.graph_ui.undo.clear();
                self.graph_ui.redo.clear();
            }
            self.graph_ui.revision = Some(revision);
        }
        if let Some(pending) = self.graph_ui.pending.as_ref()
            && pending.accepted
            && revision > pending.before.revision
        {
            let pending = self.graph_ui.pending.take().expect("pending graph edit");
            let accepted_revision = pending.before.revision + 1;
            equalizer_completed = pending
                .equalizer_submitted
                .map(|submission| (submission, revision == accepted_revision));
            if revision == pending.before.revision + 1 {
                equalizer_rebased = pending
                    .equalizer_target
                    .map(|id| (id, accepted_revision - 1, revision));
                if let Some(id) = pending.select_after {
                    self.graph_ui.selected = Some(id);
                    self.graph_ui.target_port = 0;
                }
                match pending.action {
                    HistoryAction::Edit => {
                        self.graph_ui.undo.push_back(pending.before);
                        self.graph_ui.redo.clear();
                    }
                    HistoryAction::Undo => {
                        self.graph_ui.undo.pop_back();
                        self.graph_ui.redo.push_back(pending.before);
                    }
                    HistoryAction::Redo => {
                        self.graph_ui.redo.pop_back();
                        self.graph_ui.undo.push_back(pending.before);
                    }
                }
                while self.graph_ui.undo.len() > HISTORY_LIMIT {
                    self.graph_ui.undo.pop_front();
                }
                while self.graph_ui.redo.len() > HISTORY_LIMIT {
                    self.graph_ui.redo.pop_front();
                }
            } else {
                self.graph_ui.undo.clear();
                self.graph_ui.redo.clear();
            }
            if revision == accepted_revision {
                for stamp in self.graph_ui.draft_stamps.values_mut() {
                    if stamp.revision + 1 == revision {
                        stamp.revision = revision;
                    }
                }
                for submitted in pending.submitted {
                    if inspector::submission_current(
                        self.graph_ui.draft_stamps.get(&submitted.key),
                        &submitted,
                        self.fields
                            .get(&submitted.key)
                            .map(|field| field.input.read(cx).text()),
                    ) && let Some(field) = self.fields.get_mut(&submitted.key)
                    {
                        field.dirty = false;
                        self.graph_ui.draft_stamps.remove(&submitted.key);
                        self.graph_ui.field_errors.remove(&submitted.key);
                    }
                }
            }
        }
        if self
            .graph_ui
            .selected
            .is_none_or(|id| !graph.nodes.iter().any(|n| n.id == id))
        {
            self.graph_ui.selected = graph.nodes.first().map(|n| n.id);
            self.graph_ui.target_port = 0;
        }
        if self
            .graph_ui
            .connecting
            .is_some_and(|p| !graph.nodes.iter().any(|n| n.id == p.node))
        {
            self.graph_ui.connecting = None;
        }
        let selected = self.graph_ui.selected;
        self.fields.retain(|key, field| {
            if !key.starts_with("graph.") {
                return true;
            }
            let id = key
                .strip_prefix("graph.")
                .and_then(|s| s.split_once('/'))
                .and_then(|(id, _)| id.parse::<u64>().ok());
            graph.nodes.iter().any(|n| Some(n.id) == id) && (id == selected || field.dirty)
        });
        self.graph_ui
            .draft_stamps
            .retain(|key, _| self.fields.contains_key(key));
        self.graph_ui
            .field_errors
            .retain(|key, _| self.fields.contains_key(key));
        self.graph_ui
            .expanded_backends
            .retain(|id| graph.nodes.iter().any(|node| node.id == *id));
        self.sync_graph_fields(cx);
        if let Some((id, from_revision, to_revision)) = equalizer_rebased {
            self.rebase_node_equalizer_drafts(id, from_revision, to_revision, cx);
        }
        if let Some((submission, accepted)) = equalizer_completed {
            self.finish_node_equalizer_submission(submission, accepted, cx);
        }
    }

    pub(super) fn graph_ack(&mut self, success: bool, cx: &mut Context<Self>) {
        if !success {
            if let Some(pending) = self.graph_ui.pending.take() {
                if pending.before.revision != self.state.system.config.processing_graph.revision {
                    self.graph_ui.undo.clear();
                    self.graph_ui.redo.clear();
                }
                let error = self
                    .error
                    .clone()
                    .unwrap_or_else(|| self.tr("processing-command-failed").to_string());
                for submitted in pending.submitted {
                    if self.fields.contains_key(&submitted.key) {
                        self.graph_ui
                            .field_errors
                            .insert(submitted.key, error.clone());
                    }
                }
                if let Some(submission) = pending.equalizer_submitted {
                    self.finish_node_equalizer_submission(submission, false, cx);
                }
            }
        } else if let Some(pending) = self.graph_ui.pending.as_mut() {
            pending.accepted = true;
        }
        self.reconcile_graph(cx);
    }

    pub(super) fn graph_edit(&mut self, edit: GraphEdit, cx: &mut Context<Self>) {
        self.graph_submit(edit, HistoryAction::Edit, Vec::new(), None, cx);
    }

    pub(super) fn graph_edit_equalizer(
        &mut self,
        edit: GraphEdit,
        submitted: Option<super::equalizer_editor::NodeEqualizerSubmission>,
        cx: &mut Context<Self>,
    ) {
        self.graph_submit(edit, HistoryAction::Edit, Vec::new(), submitted, cx);
    }

    fn graph_submit(
        &mut self,
        edit: GraphEdit,
        action: HistoryAction,
        submitted: Vec<SubmittedField>,
        equalizer_submitted: Option<super::equalizer_editor::NodeEqualizerSubmission>,
        cx: &mut Context<Self>,
    ) {
        // Preserve the revision that produced the intent; Core rejects stale edits.
        if self.graph_busy() {
            return;
        }
        let graph = self.state.system.config.processing_graph.clone();
        let expected_revision = graph.revision;
        let select_after = matches!(&edit, GraphEdit::Add { .. } | GraphEdit::Duplicate { .. })
            .then_some(graph.next_id);
        let equalizer_target = match &edit {
            GraphEdit::Replace {
                id,
                kind: GraphNodeKind::Equalizer { .. },
            } => Some(*id),
            _ => None,
        };
        self.graph_ui.pending = Some(PendingEdit {
            before: graph,
            action,
            submitted,
            equalizer_submitted,
            equalizer_target,
            accepted: false,
            select_after,
        });
        self.send(
            Command::ProcessingGraphEdit {
                expected_revision,
                edit: Box::new(edit),
            },
            cx,
        );
    }

    fn graph_history(&mut self, redo: bool, cx: &mut Context<Self>) {
        self.reconcile(self.context.snapshot(), cx);
        if self.graph_dirty_count(cx) > 0 {
            return;
        }
        let graph = if redo {
            self.graph_ui.redo.back()
        } else {
            self.graph_ui.undo.back()
        }
        .cloned();
        if let Some(graph) = graph {
            self.graph_submit(
                GraphEdit::ReplaceGraph { graph },
                if redo {
                    HistoryAction::Redo
                } else {
                    HistoryAction::Undo
                },
                Vec::new(),
                None,
                cx,
            );
        }
    }

    fn graph_select(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.graph_ui.selected != Some(id) {
            self.graph_ui.selected = Some(id);
            self.graph_ui.target_port = 0;
        }
        self.sync_graph_fields(cx);
        cx.notify();
    }

    fn sync_graph_fields(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.graph_ui.selected else {
            self.graph_ui.fields_revision = None;
            for fields in self.graph_ui.panel_inputs.values_mut() {
                fields.retain(|key, _| key == "graph_export");
            }
            return;
        };
        let stamp = (self.state.system.config.processing_graph.revision, id);
        if self.graph_ui.fields_revision == Some(stamp) {
            return;
        }
        self.graph_ui.fields_revision = Some(stamp);
        let node = self
            .state
            .system
            .config
            .processing_graph
            .nodes
            .iter()
            .find(|n| Some(n.id) == self.graph_ui.selected)
            .cloned();
        let Some(node) = node else {
            return;
        };
        let mut fields = Vec::new();
        scalar_fields(
            &serde_json::to_value(&node.kind).expect("serializable graph node"),
            "",
            &mut fields,
        );
        let mut active_keys = std::collections::HashSet::with_capacity(fields.len());
        for (path, value) in fields {
            if path.is_empty()
                || (matches!(node.kind, GraphNodeKind::Equalizer { .. })
                    && path.starts_with("/equalizer/config/"))
                || (path.ends_with("/profile") && value.is_null())
            {
                continue;
            }
            let text = if path.ends_with("/channels") {
                match value.get("selected").and_then(Value::as_array) {
                    Some(channels) => channels
                        .iter()
                        .filter_map(Value::as_u64)
                        .map(|c| (c + 1).to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                    None => "all".to_owned(),
                }
            } else {
                match value {
                    Value::String(s) => s,
                    Value::Null => String::new(),
                    // DSP parameters are f32; source-relative timestamps are f64.
                    Value::Number(number) if number.is_f64() && !path.ends_with("_seconds") => {
                        (number.as_f64().expect("floating point number") as f32).to_string()
                    }
                    _ => value.to_string(),
                }
            };
            let key = format!("graph.{}{path}", node.id);
            self.field(&key, &self.graph_parameter_label(&path), text, cx);
            active_keys.insert(key);
        }
        let prefix = format!("graph.{id}/");
        self.fields.retain(|key, field| {
            if !key.starts_with("graph.") {
                true
            } else if key.starts_with(&prefix) {
                active_keys.contains(key)
            } else {
                field.dirty
            }
        });
        // Keep only the current inspector's replicas. Drafts for other nodes
        // remain in the backing fields; removed controls drop subscriptions.
        for fields in self.graph_ui.panel_inputs.values_mut() {
            fields.retain(|key, _| key == "graph_export" || active_keys.contains(key));
        }
        if !self.fields.contains_key("graph_export") {
            self.field(
                "graph_export",
                &self.tr("processing-export-path"),
                String::new(),
                cx,
            );
        }
    }

    fn graph_parameter_label(&self, path: &str) -> String {
        let key = path.rsplit('/').next().unwrap_or(path);
        if let Some((group, index)) = path.rsplit_once('/')
            && let Ok(index) = index.parse::<usize>()
        {
            let id = match group.rsplit('/').next().unwrap_or("") {
                "gains_db" => Some("processing-graph-input-gain"),
                "muted" => Some("processing-graph-input-muted"),
                "solo" => Some("processing-graph-input-solo"),
                _ => None,
            };
            if let Some(id) = id {
                return format!("{} {}", self.tr(id), index + 1);
            }
        }
        let label = match key {
            "preamp_db" => self.tr("processing-preamp").to_string(),
            "output_gain_db" => self.tr("processing-output-gain").to_string(),
            "gain_db" => self.tr("processing-gain").to_string(),
            "kind" => self.tr("processing-graph-filter-kind").to_string(),
            "shelf_slope" => self.tr("processing-shelf-slope").to_string(),
            "stages" => self.tr("processing-stages").to_string(),
            "channels" => self.tr("processing-channels").to_string(),
            "taps" => self.tr("processing-graph-taps").to_string(),
            "implementation" => self.tr("processing-fields-implementation").to_string(),
            "mode" => self.tr("processing-denoise-mode").to_string(),
            "device" => self.tr("processing-device").to_string(),
            "start_seconds" => self.tr("processing-profile-start").to_string(),
            "end_seconds" => self.tr("processing-profile-end").to_string(),
            _ => self.parameter_label(key),
        };
        let components = path.trim_start_matches('/').split('/').collect::<Vec<_>>();
        if let Some(index) = components.iter().position(|p| *p == "bands") {
            return format!(
                "{} {} · {label}",
                self.tr("processing-graph-band"),
                components
                    .get(index + 1)
                    .and_then(|n| n.parse::<usize>().ok())
                    .unwrap_or(0)
                    + 1
            );
        }
        if let Some(group) = components.iter().find(|p| matches!(**p, "hum" | "impulse")) {
            return format!("{} · {label}", self.parameter_label(group));
        }
        label
    }

    pub(super) fn apply_graph_field(&mut self, key: &str, cx: &mut Context<Self>) {
        self.submit_graph_fields(vec![key.to_owned()], cx);
    }

    fn graph_add(&mut self, panel: u64, kind: GraphNodeKind, cx: &mut Context<Self>) {
        let viewport = self
            .graph_ui
            .bounds
            .get(&panel)
            .and_then(|b| b.get())
            .map(|(bounds, _)| [f32::from(bounds.size.width), f32::from(bounds.size.height)])
            .unwrap_or([600., 380.]);
        let position = [
            (viewport[0] * 0.5 - self.graph_ui.pan[0]) / self.graph_ui.zoom - NODE_WIDTH * 0.5,
            (viewport[1] * 0.5 - self.graph_ui.pan[1]) / self.graph_ui.zoom
                - node_height(&kind) * 0.5,
        ];
        self.graph_edit(GraphEdit::Add { kind, position }, cx);
    }

    fn graph_remove(&mut self, cx: &mut Context<Self>) {
        if self
            .graph_ui
            .selected
            .is_some_and(|id| self.graph_node_dirty_count(id, cx) > 0)
        {
            return;
        }
        if let Some(node) = self
            .state
            .system
            .config
            .processing_graph
            .nodes
            .iter()
            .find(|n| {
                Some(n.id) == self.graph_ui.selected && !matches!(n.kind, GraphNodeKind::Source)
            })
        {
            self.graph_edit(GraphEdit::Remove { id: node.id }, cx);
        }
    }

    fn graph_duplicate(&mut self, cx: &mut Context<Self>) {
        if self
            .graph_ui
            .selected
            .is_some_and(|id| self.graph_node_dirty_count(id, cx) > 0)
        {
            return;
        }
        if let Some(node) = self
            .state
            .system
            .config
            .processing_graph
            .nodes
            .iter()
            .find(|n| {
                Some(n.id) == self.graph_ui.selected && !matches!(n.kind, GraphNodeKind::Source)
            })
        {
            self.graph_edit(
                GraphEdit::Duplicate {
                    id: node.id,
                    position: [node.position[0] + 32., node.position[1] + 48.],
                },
                cx,
            );
        }
    }

    fn graph_connect(&mut self, port: GraphPort, output: bool, cx: &mut Context<Self>) {
        self.graph_select(port.node, cx);
        if output {
            self.graph_ui.connecting = Some(port);
            self.graph_ui.target_port = port.port;
            cx.notify();
        } else if let Some(from) = self.graph_ui.connecting.take() {
            self.graph_edit(GraphEdit::Connect { from, to: port }, cx);
        }
    }

    fn graph_position(&self, node: &GraphNode) -> [f32; 2] {
        self.graph_ui
            .pointer
            .as_ref()
            .filter(|p| p.node == Some(node.id))
            .map(|p| p.preview)
            .unwrap_or(node.position)
    }

    fn graph_pan(&self) -> [f32; 2] {
        self.graph_ui
            .pointer
            .as_ref()
            .filter(|p| p.node.is_none())
            .map(|p| p.preview)
            .unwrap_or(self.graph_ui.pan)
    }

    fn graph_start_pointer(
        &mut self,
        panel: u64,
        node: Option<u64>,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A child port's default focus handler claims its press before the card.
        if node.is_some() && window.default_prevented() {
            return;
        }
        if self.graph_busy() && node.is_some() {
            return;
        }
        let Some((bounds, geometry)) = self.graph_ui.bounds.get(&panel).and_then(|b| b.get())
        else {
            return;
        };
        if geometry != DockGeometry::current(window, self.state.system.config.ui_scale) {
            return;
        }
        let origin = if let Some(id) = node {
            self.graph_select(id, cx);
            let Some(node) = self
                .state
                .system
                .config
                .processing_graph
                .nodes
                .iter()
                .find(|n| n.id == id)
            else {
                return;
            };
            node.position
        } else {
            self.graph_ui.pan
        };
        let subscriptions = [
            cx.observe_window_activation(window, |this, window, cx| {
                if !window.is_window_active() {
                    this.cancel_pointer(cx);
                }
            }),
            cx.observe_window_visibility(window, |this, visibility, _, cx| {
                if !matches!(visibility, WindowVisibility::Visible) {
                    this.cancel_pointer(cx);
                }
            }),
        ];
        self.graph_ui.pointer = Some(GraphPointer {
            panel,
            node,
            start: event.position,
            origin,
            preview: origin,
            bounds,
            geometry,
            epoch: self.geometry_epoch,
            revision: self.state.system.config.processing_graph.revision,
            button: event.button,
            _subscriptions: subscriptions,
        });
        cx.notify();
    }

    fn graph_move_pointer(
        &mut self,
        position: Point<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pointer) = self.graph_ui.pointer.as_mut() else {
            return;
        };
        if pointer.epoch != self.geometry_epoch
            || pointer.geometry != DockGeometry::current(window, self.state.system.config.ui_scale)
            || pointer.revision != self.state.system.config.processing_graph.revision
        {
            self.cancel_pointer(cx);
            return;
        }
        let scale = if pointer.node.is_some() {
            self.graph_ui.zoom
        } else {
            1.
        };
        pointer.preview = [
            pointer.origin[0] + f32::from(position.x - pointer.start.x) / scale,
            pointer.origin[1] + f32::from(position.y - pointer.start.y) / scale,
        ];
        cx.notify();
    }

    fn graph_pointer_handlers(weak: WeakEntity<Self>, panel: u64, window: &mut Window) {
        let entity = weak.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase == DispatchPhase::Capture {
                let _ = entity.update(cx, |this, cx| {
                    if this
                        .graph_ui
                        .pointer
                        .as_ref()
                        .is_some_and(|p| p.panel == panel)
                    {
                        if this
                            .graph_ui
                            .pointer
                            .as_ref()
                            .is_some_and(|pointer| event.pressed_button == Some(pointer.button))
                        {
                            this.graph_move_pointer(event.position, window, cx);
                        } else {
                            this.cancel_pointer(cx);
                        }
                        window.prevent_default();
                    }
                });
            }
        });
        let entity = weak.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
            if phase == DispatchPhase::Capture {
                let _ = entity.update(cx, |this, cx| {
                    if this
                        .graph_ui
                        .pointer
                        .as_ref()
                        .is_some_and(|p| p.panel == panel && p.button == event.button)
                    {
                        this.graph_move_pointer(event.position, window, cx);
                        if let Some(pointer) = this.graph_ui.pointer.take() {
                            if let Some(id) = pointer.node {
                                if pointer.preview != pointer.origin {
                                    this.graph_edit(
                                        GraphEdit::Move {
                                            id,
                                            position: pointer.preview,
                                        },
                                        cx,
                                    );
                                }
                            } else {
                                this.graph_ui.pan = pointer.preview;
                            }
                        }
                        cx.notify();
                    }
                });
            }
        });
        window.on_mouse_event(move |_: &MouseExitEvent, phase, _, cx| {
            if phase == DispatchPhase::Capture {
                let _ = weak.update(cx, |this, cx| this.cancel_pointer(cx));
            }
        });
    }

    fn graph_zoom(&mut self, factor: f32, anchor: [f32; 2], cx: &mut Context<Self>) {
        self.cancel_pointer(cx);
        self.graph_zoom_to(self.graph_ui.zoom * factor, anchor, cx);
    }

    fn graph_zoom_to(&mut self, zoom: f32, anchor: [f32; 2], cx: &mut Context<Self>) {
        let old = self.graph_ui.zoom;
        let next = zoom.clamp(0.35, 2.);
        for (i, coordinate) in anchor.into_iter().enumerate() {
            self.graph_ui.pan[i] = coordinate - (coordinate - self.graph_ui.pan[i]) * next / old;
        }
        self.graph_ui.zoom = next;
        cx.notify();
    }

    fn graph_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        if modifiers.control || modifiers.platform {
            match key {
                "z" => self.graph_history(modifiers.shift, cx),
                "y" => self.graph_history(true, cx),
                "d" => self.graph_duplicate(cx),
                _ => return,
            }
        } else {
            match key {
                "escape" => {
                    self.cancel_pointer(cx);
                    self.graph_ui.connecting = None;
                    cx.notify();
                }
                "delete" | "backspace" => self.graph_remove(cx),
                "+" | "=" => self.graph_zoom(1.2, [250., 160.], cx),
                "-" => self.graph_zoom(1. / 1.2, [250., 160.], cx),
                "home" => {
                    self.cancel_pointer(cx);
                    self.graph_ui.pan = [24., 24.];
                    self.graph_ui.zoom = 1.;
                    cx.notify();
                }
                "left" | "right" | "up" | "down" => {
                    let step = if modifiers.shift { 40. } else { 10. };
                    let delta = match key {
                        "left" => [-step, 0.],
                        "right" => [step, 0.],
                        "up" => [0., -step],
                        _ => [0., step],
                    };
                    if modifiers.alt {
                        self.cancel_pointer(cx);
                        self.graph_ui.pan[0] += delta[0];
                        self.graph_ui.pan[1] += delta[1];
                        cx.notify();
                    } else if let Some(node) = self
                        .state
                        .system
                        .config
                        .processing_graph
                        .nodes
                        .iter()
                        .find(|n| Some(n.id) == self.graph_ui.selected)
                    {
                        self.graph_edit(
                            GraphEdit::Move {
                                id: node.id,
                                position: [
                                    node.position[0] + delta[0],
                                    node.position[1] + delta[1],
                                ],
                            },
                            cx,
                        );
                    }
                }
                _ => return,
            }
        }
        cx.stop_propagation();
    }

    fn routing_canvas(&mut self, panel: u64, cx: &mut Context<Self>) -> AnyElement {
        let focus = self
            .graph_ui
            .workbenches
            .entry(panel)
            .or_default()
            .canvas_focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        let theme = rivu_ui::theme::palette(cx);
        let graph = &self.state.system.config.processing_graph;
        let edges = graph.edges.clone();
        let highlighted = self.graph_ui.highlight_connection;
        let pan = self.graph_pan();
        let zoom = self.graph_ui.zoom;
        let measured = self
            .graph_ui
            .bounds
            .entry(panel)
            .or_insert_with(|| Rc::new(Cell::new(None)))
            .clone();
        let measure = measured.clone();
        let epoch = self.geometry_epoch;
        let scale = self.state.system.config.ui_scale;
        let weak = cx.entity().downgrade();
        let owner = weak.clone();
        let mut nodes = Vec::with_capacity(graph.nodes.len());
        let mut positions = HashMap::new();
        for node in &graph.nodes {
            let id = node.id;
            let p = self.graph_position(node);
            positions.insert(id, p);
            let mut card = div()
                .id(SharedString::from(format!("graph-card-{panel}-{id}")))
                .absolute()
                .left(px(pan[0] + p[0] * zoom))
                .top(px(pan[1] + p[1] * zoom))
                .w(px(NODE_WIDTH * zoom))
                .h(px(node_height(&node.kind) * zoom))
                .bg(rgb(theme.panel))
                .border_1()
                .border_color(rgb(if self.graph_ui.selected == Some(id) {
                    theme.accent
                } else {
                    theme.border
                }))
                .rounded_md()
                .tab_index(0)
                .tab_stop(true)
                .role(Role::Button)
                .focus(|v| v.bg(rgb(theme.selected)))
                .aria_label(SharedString::from(format!(
                    "{} #{id}",
                    self.tr(node_label(&node.kind))
                )))
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                    this.graph_select(id, cx);
                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                        cx.stop_propagation();
                    }
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event, window, cx| {
                        this.graph_start_pointer(panel, Some(id), event, window, cx)
                    }),
                )
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .w_full()
                        .h(px(HEADER * zoom))
                        .px(px(8. * zoom))
                        .flex()
                        .items_center()
                        .truncate()
                        .text_size(px(12. * zoom))
                        .child(format!("{} #{id}", self.tr(node_label(&node.kind)))),
                );
            for output in [false, true] {
                let count = if output {
                    output_count(&node.kind)
                } else {
                    input_count(&node.kind)
                };
                for port in 0..count {
                    let endpoint = GraphPort {
                        node: id,
                        port: port as u16,
                    };
                    let label = if output {
                        self.graph_output_label(&node.kind, port)
                    } else {
                        format!("PCM {}", port + 1)
                    };
                    let active = output && self.graph_ui.connecting == Some(endpoint);
                    let control = div()
                        .id(SharedString::from(format!(
                            "graph-port-{panel}-{id}-{output}-{port}"
                        )))
                        .absolute()
                        .top(px((HEADER + PORT_ROW * port as f32) * zoom))
                        .h(px(PORT_ROW * zoom))
                        .max_w(px(NODE_WIDTH * zoom / 2.))
                        .px(px(4. * zoom))
                        .flex()
                        .items_center()
                        .gap(px(3. * zoom))
                        .text_size(px(11. * zoom))
                        .text_color(rgb(if active { theme.accent } else { theme.text }))
                        .when(output, |v| v.right_0().justify_end())
                        .when(!output, |v| v.left_0())
                        .tab_index(0)
                        .tab_stop(true)
                        .role(Role::Button)
                        .aria_label(SharedString::from(format!(
                            "{} #{id} · {} · {label}",
                            self.tr(node_label(&node.kind)),
                            self.tr(if output {
                                "processing-graph-port-out"
                            } else {
                                "processing-graph-port-in"
                            })
                        )))
                        .focus(|v| v.bg(rgb(theme.selected)))
                        .when(!output, |view| view.child(div().flex_shrink_0().child("●")))
                        .child(div().min_w_0().truncate().child(label))
                        .when(output, |view| view.child(div().flex_shrink_0().child("●")))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.graph_connect(endpoint, output, cx);
                            cx.stop_propagation();
                        }));
                    card = card.child(control);
                }
            }
            nodes.push(card.into_any_element());
        }
        div()
            .id(("routing-canvas", panel))
            .relative()
            .w_full()
            .h_full()
            .min_h_0()
            .overflow_hidden()
            .bg(rgb(theme.bg))
            .border_1()
            .border_color(rgb(theme.border))
            .tab_index(0)
            .tab_stop(true)
            .track_focus(&focus)
            .aria_label(self.tr("processing-routing-graph"))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                this.graph_key(event, cx);
                let key = event.keystroke.key.as_str();
                let editing =
                    if event.keystroke.modifiers.control || event.keystroke.modifiers.platform {
                        matches!(key, "d" | "y" | "z")
                    } else {
                        matches!(key, "delete" | "backspace")
                    };
                if editing {
                    // Editing may remove the focused card or select a new identity.
                    focus.focus(window, cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |this, event, window, cx| {
                    this.graph_start_pointer(panel, None, event, window, cx)
                }),
            )
            .on_scroll_wheel(cx.listener(move |this, event: &ScrollWheelEvent, _, cx| {
                let delta = match event.delta {
                    ScrollDelta::Pixels(p) => [f32::from(p.x), f32::from(p.y)],
                    ScrollDelta::Lines(p) => [p.x * 24., p.y * 24.],
                };
                if event.modifiers.control {
                    let anchor = measured
                        .get()
                        .map(|(b, _)| {
                            [
                                f32::from(event.position.x - b.origin.x),
                                f32::from(event.position.y - b.origin.y),
                            ]
                        })
                        .unwrap_or([250., 160.]);
                    this.graph_zoom((delta[1] * 0.006).exp(), anchor, cx);
                } else {
                    this.cancel_pointer(cx);
                    this.graph_ui.pan[0] += delta[0];
                    this.graph_ui.pan[1] += delta[1];
                    cx.notify();
                }
                cx.stop_propagation();
            }))
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let geometry = DockGeometry::current(window, scale);
                        measure.set(Some((bounds, geometry)));
                        let _ = owner.update(cx, |this, cx| {
                            if this.graph_ui.pointer.as_ref().is_some_and(|p| {
                                p.panel == panel
                                    && (p.bounds != bounds
                                        || p.geometry != geometry
                                        || p.epoch != epoch)
                            }) {
                                this.cancel_pointer(cx);
                            }
                        });
                        bounds
                    },
                    move |bounds, _, window, _| {
                        Self::graph_pointer_handlers(weak.clone(), panel, window);
                        let mut grid = PathBuilder::stroke(px(1.));
                        let spacing = 32. * zoom;
                        let mut x = pan[0].rem_euclid(spacing);
                        while x < f32::from(bounds.size.width) {
                            grid.move_to(point(bounds.left() + px(x), bounds.top()));
                            grid.line_to(point(bounds.left() + px(x), bounds.bottom()));
                            x += spacing;
                        }
                        let mut y = pan[1].rem_euclid(spacing);
                        while y < f32::from(bounds.size.height) {
                            grid.move_to(point(bounds.left(), bounds.top() + px(y)));
                            grid.line_to(point(bounds.right(), bounds.top() + px(y)));
                            y += spacing;
                        }
                        if let Ok(path) = grid.build() {
                            window.paint_path(path, rgb(theme.border));
                        }
                        for edge in &edges {
                            let (Some(from), Some(to)) =
                                (positions.get(&edge.from.node), positions.get(&edge.to.node))
                            else {
                                continue;
                            };
                            let a = bounds.origin
                                + point(
                                    px(pan[0] + (from[0] + NODE_WIDTH) * zoom),
                                    px(pan[1]
                                        + (from[1]
                                            + HEADER
                                            + PORT_ROW * (edge.from.port as f32 + 0.5))
                                            * zoom),
                                );
                            let b = bounds.origin
                                + point(
                                    px(pan[0] + to[0] * zoom),
                                    px(pan[1]
                                        + (to[1]
                                            + HEADER
                                            + PORT_ROW * (edge.to.port as f32 + 0.5))
                                            * zoom),
                                );
                            let reach = px((f32::from(b.x - a.x).abs() * 0.5).max(40.));
                            let selected = highlighted == Some(edge.to);
                            let mut path = PathBuilder::stroke(px(if selected { 4. } else { 2. }));
                            path.move_to(a);
                            path.cubic_bezier_to(
                                b,
                                a + point(reach, px(0.)),
                                b - point(reach, px(0.)),
                            );
                            if let Ok(path) = path.build() {
                                window.paint_path(
                                    path,
                                    rgb(if selected {
                                        theme.highlight
                                    } else {
                                        theme.accent
                                    }),
                                );
                            }
                        }
                    },
                )
                .size_full(),
            )
            .children(nodes)
            .into_any_element()
    }

    fn graph_run_target(&mut self, output: bool, cx: &mut Context<Self>) {
        if self.graph_busy()
            || self
                .state
                .system
                .processing
                .as_ref()
                .is_some_and(|task| !task.phase.is_terminal())
        {
            return;
        }
        let graph = &self.state.system.config.processing_graph;
        let node = if output {
            graph
                .nodes
                .iter()
                .find(|n| matches!(n.kind, GraphNodeKind::Output))
        } else {
            graph
                .nodes
                .iter()
                .find(|n| Some(n.id) == self.graph_ui.selected)
        };
        let (Some(node), Some(track)) = (node, self.input_track()) else {
            return;
        };
        let target = GraphPort {
            node: node.id,
            port: if output { 0 } else { self.graph_ui.target_port },
        };
        self.send(
            Command::ProcessingGraphRun {
                track_id: track.id,
                target,
                mode: self.graph_ui.mode,
            },
            cx,
        );
    }

    pub(super) fn routing_graph_view(
        &mut self,
        panel: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        self.graph_workspace(panel, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Config, ConfigPersistence},
        core::Runtime,
        gui::feature::SnapshotSource,
        i18n::Localizer,
    };
    use core::prelude::v1::test;

    const PANEL: u64 = 7;

    struct GraphWindow {
        processing: Entity<AudioProcessing>,
        input: Entity<Input>,
    }

    impl Render for GraphWindow {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            column().size_full().child(self.input.clone()).child(
                div().w_full().h(px(500.)).child(
                    self.processing
                        .update(cx, |this, cx| this.routing_canvas(PANEL, cx)),
                ),
            )
        }
    }

    fn fixture(
        cx: &mut TestAppContext,
    ) -> (
        tempfile::TempDir,
        Runtime,
        Entity<AudioProcessing>,
        AnyWindowHandle,
    ) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            rivu_ui::theme::set_palette(crate::theme::palette(crate::theme::Theme::default()), cx);
            rivu_ui::theme::set_typography(
                gpui::font("sans-serif"),
                crate::gui::appearance::fonts::SYMBOLS_FAMILY,
                12,
                cx,
            );
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mut config = Config {
            volume: 0.,
            ffmpeg_enabled: false,
            mpris_enabled: false,
            tray_enabled: false,
            ..Default::default()
        };
        config.processing_graph.nodes.push(GraphNode {
            id: 3,
            position: [360., 240.],
            kind: GraphNodeKind::Separate {
                config: Default::default(),
            },
        });
        config.processing_graph.next_id = 4;
        config.save(&path).unwrap();
        let runtime = Runtime::start(directory.path(), &path).unwrap();
        let i18n = Arc::new(Localizer::new("en-US"));
        let context = FeatureContext {
            snapshots: Rc::new(SnapshotSource::new(runtime.handle.clone())),
            labels: Rc::new(
                i18n.fixed_texts()
                    .map(|(id, text)| (id.to_owned(), SharedString::from(text.to_owned())))
                    .collect(),
            ),
            i18n,
            scroll: Default::default(),
        };
        let processing = cx.new(|cx| AudioProcessing::new(context, cx));
        let input = cx.new(|cx| Input::new("Editor", "12", "", cx));
        let window = cx.add_window({
            let processing = processing.clone();
            let input = input.clone();
            move |_, _| GraphWindow { processing, input }
        });
        let handle = window.into();
        draw(cx, handle);
        cx.update_window(handle, |_, window, cx| {
            input.read(cx).focus_handle(cx).focus(window, cx);
        })
        .unwrap();
        (directory, runtime, processing, handle)
    }

    fn draw(cx: &mut TestAppContext, handle: AnyWindowHandle) {
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
    }

    async fn wait_for_commands(
        cx: &mut TestAppContext,
        processing: &Entity<AudioProcessing>,
        handle: AnyWindowHandle,
    ) {
        cx.condition(processing, |this, _| this.pending == 0).await;
        draw(cx, handle);
    }

    fn node_point(
        processing: &Entity<AudioProcessing>,
        id: u64,
        cx: &TestAppContext,
    ) -> Point<Pixels> {
        processing.read_with(cx, |this, _| {
            let bounds = this.graph_ui.bounds[&PANEL].get().unwrap().0;
            let node = this
                .state
                .system
                .config
                .processing_graph
                .nodes
                .iter()
                .find(|n| n.id == id)
                .unwrap();
            bounds.origin
                + point(
                    px(24. + node.position[0] + 80.),
                    px(24. + node.position[1] + 16.),
                )
        })
    }

    fn press(cx: &mut TestAppContext, handle: AnyWindowHandle, position: Point<Pixels>) {
        use gpui::InputEvent as _;
        cx.update_window(handle, |_, window, cx| {
            window.dispatch_event(
                MouseDownEvent {
                    button: MouseButton::Left,
                    position,
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
        })
        .unwrap();
    }

    fn release(cx: &mut TestAppContext, handle: AnyWindowHandle, position: Point<Pixels>) {
        use gpui::InputEvent as _;
        cx.update_window(handle, |_, window, cx| {
            window.dispatch_event(
                MouseUpEvent {
                    button: MouseButton::Left,
                    position,
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
        })
        .unwrap();
        draw(cx, handle);
    }

    fn key(cx: &mut TestAppContext, handle: AnyWindowHandle, key: &str) {
        cx.update_window(handle, |_, window, cx| {
            window.dispatch_keystroke(Keystroke::parse(key).unwrap(), cx);
        })
        .unwrap();
        draw(cx, handle);
    }

    #[gpui::test]
    async fn clicking_nodes_moves_keyboard_ownership_and_delete_respects_source_protection(
        cx: &mut TestAppContext,
    ) {
        let (_directory, runtime, processing, handle) = fixture(cx);
        let original = runtime.handle.config_snapshot().processing_graph.clone();
        let output = node_point(&processing, 2, cx);
        press(cx, handle, output);
        release(cx, handle, output);
        cx.update_window(handle, |root, window, cx| {
            let root = root.downcast::<GraphWindow>().unwrap();
            assert!(!root.read(cx).input.read(cx).is_focused(window));
        })
        .unwrap();
        // A different card must replace the previous card's keyboard target.
        let source = node_point(&processing, 1, cx);
        press(cx, handle, source);
        release(cx, handle, source);
        key(cx, handle, "delete");
        wait_for_commands(cx, &processing, handle).await;
        assert_eq!(runtime.handle.config_snapshot().processing_graph, original);
        press(cx, handle, output);
        release(cx, handle, output);
        key(cx, handle, "delete");
        wait_for_commands(cx, &processing, handle).await;
        let graph = &runtime.handle.config_snapshot().processing_graph;
        assert_eq!(graph.nodes.iter().map(|n| n.id).collect::<Vec<_>>(), [1, 3]);
        assert!(graph.edges.is_empty());
        assert_eq!(graph.revision, 1);
        key(cx, handle, "ctrl-z");
        wait_for_commands(cx, &processing, handle).await;
        let restored = &runtime.handle.config_snapshot().processing_graph;
        assert_eq!(restored.nodes, original.nodes);
        assert_eq!(restored.edges, original.edges);
        key(cx, handle, "ctrl-y");
        wait_for_commands(cx, &processing, handle).await;
        assert_eq!(
            runtime
                .handle
                .config_snapshot()
                .processing_graph
                .nodes
                .iter()
                .map(|n| n.id)
                .collect::<Vec<_>>(),
            [1, 3]
        );
        key(cx, handle, "ctrl-z");
        wait_for_commands(cx, &processing, handle).await;
        press(cx, handle, output);
        release(cx, handle, output);
        key(cx, handle, "ctrl-d");
        wait_for_commands(cx, &processing, handle).await;
        processing.read_with(cx, |this, _| assert_eq!(this.graph_ui.selected, Some(4)));
        assert!(
            runtime
                .handle
                .config_snapshot()
                .processing_graph
                .nodes
                .iter()
                .any(|n| n.id == 4)
        );
        key(cx, handle, "delete");
        wait_for_commands(cx, &processing, handle).await;
        let after_duplicate_delete = &runtime.handle.config_snapshot().processing_graph;
        assert_eq!(after_duplicate_delete.nodes, original.nodes);
        assert_eq!(after_duplicate_delete.edges, original.edges);
    }

    #[gpui::test]
    async fn port_press_keeps_drag_idle_and_escape_preserves_the_selected_output(
        cx: &mut TestAppContext,
    ) {
        let (_directory, runtime, processing, handle) = fixture(cx);
        let original = runtime.handle.config_snapshot().processing_graph.clone();
        let port = node_point(&processing, 3, cx)
            + point(px(NODE_WIDTH - 92.), px(HEADER + PORT_ROW * 1.5 - 16.));
        press(cx, handle, port);
        processing.read_with(cx, |this, _| assert!(this.graph_ui.pointer.is_none()));
        release(cx, handle, port);
        processing.read_with(cx, |this, _| {
            assert_eq!(
                this.graph_ui.connecting,
                Some(GraphPort { node: 3, port: 1 })
            );
            assert_eq!(this.graph_ui.target_port, 1);
        });
        key(cx, handle, "escape");
        processing.read_with(cx, |this, _| {
            assert_eq!(this.graph_ui.selected, Some(3));
            assert!(this.graph_ui.connecting.is_none());
            assert_eq!(this.graph_ui.target_port, 1);
        });
        assert_eq!(runtime.handle.config_snapshot().processing_graph, original);
        // An input port also owns selection, even without a pending connection.
        let input =
            node_point(&processing, 2, cx) + point(px(-68.), px(HEADER + PORT_ROW * 0.5 - 16.));
        press(cx, handle, input);
        processing.read_with(cx, |this, _| assert!(this.graph_ui.pointer.is_none()));
        release(cx, handle, input);
        processing.read_with(cx, |this, _| assert_eq!(this.graph_ui.selected, Some(2)));
        let output = node_point(&processing, 2, cx)
            + point(px(NODE_WIDTH - 92.), px(HEADER + PORT_ROW * 0.5 - 16.));
        let free_input =
            node_point(&processing, 3, cx) + point(px(-68.), px(HEADER + PORT_ROW * 0.5 - 16.));
        press(cx, handle, output);
        release(cx, handle, output);
        press(cx, handle, free_input);
        release(cx, handle, free_input);
        wait_for_commands(cx, &processing, handle).await;
        let graph = &runtime.handle.config_snapshot().processing_graph;
        assert_eq!(graph.nodes, original.nodes);
        assert_eq!(
            graph.edges,
            [
                rivu_format::config::processing_graph::GraphEdge {
                    from: GraphPort { node: 1, port: 0 },
                    to: GraphPort { node: 2, port: 0 },
                },
                rivu_format::config::processing_graph::GraphEdge {
                    from: GraphPort { node: 2, port: 0 },
                    to: GraphPort { node: 3, port: 0 },
                },
            ]
        );
        processing.read_with(cx, |this, _| assert_eq!(this.graph_ui.selected, Some(3)));
    }
    struct WorkbenchWindow(Entity<AudioProcessing>);

    impl Render for WorkbenchWindow {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.0
                .update(cx, |this, cx| this.render_panel(PANEL, window, cx))
        }
    }

    #[gpui::test]
    fn completed_graph_results_keep_export_focus_across_redraws_and_typing(
        cx: &mut TestAppContext,
    ) {
        cx.update(Input::init);
        let (_directory, _runtime, processing, _) = fixture(cx);
        processing.update(cx, |this, cx| {
            this.reconcile_graph(cx);
            let config = &this.state.system.config;
            let task = rivu_protocol::model::ProcessingTaskStatus {
                id: 1,
                track_id: 1,
                operation: ProcessingOperation::Graph,
                phase: ProcessingPhase::Complete,
                completed: 1,
                total: Some(1),
                error: None,
                outputs: None,
                sample_rate: Some(48_000),
                channels: Some(2),
                frames: Some(48_000),
                model_id: None,
                model_sha256: None,
                source_path: "source.wav".into(),
                source_version: "source".into(),
                source_cue: None,
                settings: rivu_protocol::model::ProcessingSettings {
                    denoise: config.denoise.clone(),
                    neural: config.neural.clone(),
                    profile: None,
                    graph: Some(config.processing_graph.clone()),
                    graph_target: None,
                    graph_mode: Some(GraphExecutionMode::Offline),
                    graph_boundary: None,
                    graph_export_path: None,
                },
                graph_revision: Some(config.processing_graph.revision),
                graph_outputs: Vec::new(),
                graph_exported_path: None,
                graph_nodes: Vec::new(),
                graph_plan: None,
            };
            Rc::make_mut(&mut this.state).system.processing = Some(task);
            this.page = Page::Results;
        });
        let window = cx.open_window(size(px(1200.), px(900.)), {
            let processing = processing.clone();
            move |_, _| WorkbenchWindow(processing)
        });
        let handle = window.into();
        draw(cx, handle);
        let input = processing.read_with(cx, |this, _| {
            this.graph_ui.panel_inputs[&PANEL]["graph_export"]
                .input
                .clone()
        });
        cx.update_window(handle, |_, window, cx| {
            input.read(cx).focus_handle(cx).focus(window, cx);
        })
        .unwrap();
        for key in ["o", "u", "t", ".", "w", "a", "v"] {
            cx.update_window(handle, |_, window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
                assert!(input.read(cx).is_focused(window));
                window.dispatch_keystroke(Keystroke::parse(key).unwrap(), cx);
            })
            .unwrap();
            draw(cx, handle);
        }
        processing.read_with(cx, |this, cx| {
            assert_eq!(input.read(cx).text(), "out.wav");
            assert_eq!(this.fields["graph_export"].input.read(cx).text(), "out.wav");
        });
        // The same retained input is no longer a focus owner when its result disappears.
        processing.update(cx, |this, _| {
            Rc::make_mut(&mut this.state)
                .system
                .processing
                .as_mut()
                .unwrap()
                .phase = ProcessingPhase::Failed;
        });
        cx.update_window(handle, |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
            assert!(!input.read(cx).is_focused(window));
        })
        .unwrap();
    }
}
