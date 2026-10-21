//! Core owns the graph; this view retains drafts, selection, viewport and undo.
//! Hidden fields own shared drafts/commit state. Painted inputs belong to one
//! panel each and mirror text only on changes, never sharing focus or geometry.
use super::*;
use rivu_format::config::processing_graph::{
    EqImplementation, GraphEdit, GraphExecutionMode, GraphNode, GraphNodeKind, GraphPort,
    ProcessingGraph,
};

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
    submitted: Option<(String, String)>,
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
}

impl Default for GraphUi {
    fn default() -> Self {
        Self {
            selected: None,
            connecting: None,
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
        let input = cx.new(|cx| {
            Input::new(
                label.to_owned(),
                backing.read(cx).text().to_owned(),
                label.to_owned(),
                cx,
            )
        });
        let name = key.to_owned();
        let edited = cx.subscribe(&input, move |this, input, event, cx| match event {
            InputEvent::Changed { user: true } => {
                if let Some(field) = this.fields.get_mut(&name) {
                    field.dirty = true;
                    let text = input.read(cx).text().to_owned();
                    field.input.update(cx, |input, cx| input.set_text(text, cx));
                }
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

    fn graph_busy(&self) -> bool {
        self.graph_ui.pending.is_some() || self.pending > 0
    }

    pub(super) fn reconcile_graph(&mut self, cx: &mut Context<Self>) {
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
            if revision == pending.before.revision + 1 {
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
            if let Some((key, value)) = pending.submitted
                && let Some(field) = self.fields.get_mut(&key)
                && field.input.read(cx).text().trim() == value
            {
                field.dirty = false;
                let committed = field.committed.clone();
                field
                    .input
                    .update(cx, |input, cx| input.set_text(committed, cx));
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
        self.sync_graph_fields(cx);
    }

    pub(super) fn graph_ack(&mut self, success: bool, cx: &mut Context<Self>) {
        if !success {
            if self.graph_ui.pending.take().is_some_and(|pending| {
                pending.before.revision != self.state.system.config.processing_graph.revision
            }) {
                self.graph_ui.undo.clear();
                self.graph_ui.redo.clear();
            }
        } else if let Some(pending) = self.graph_ui.pending.as_mut() {
            pending.accepted = true;
        }
        self.reconcile_graph(cx);
    }

    fn graph_edit(&mut self, edit: GraphEdit, cx: &mut Context<Self>) {
        self.graph_submit(edit, HistoryAction::Edit, None, cx);
    }

    fn graph_submit(
        &mut self,
        edit: GraphEdit,
        action: HistoryAction,
        submitted: Option<(String, String)>,
        cx: &mut Context<Self>,
    ) {
        // Preserve the revision that produced the intent; Core rejects stale edits.
        if self.graph_busy() {
            return;
        }
        let graph = self.state.system.config.processing_graph.clone();
        let revision = graph.revision;
        let select_after = matches!(&edit, GraphEdit::Add { .. } | GraphEdit::Duplicate { .. })
            .then_some(graph.next_id);
        self.graph_ui.pending = Some(PendingEdit {
            before: graph,
            action,
            submitted,
            accepted: false,
            select_after,
        });
        self.send(
            Command::ProcessingGraphEdit {
                revision,
                edit: Box::new(edit),
            },
            cx,
        );
    }

    fn graph_history(&mut self, redo: bool, cx: &mut Context<Self>) {
        self.reconcile(self.context.snapshot(), cx);
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
                None,
                cx,
            );
        }
    }

    fn graph_select(&mut self, id: u64, cx: &mut Context<Self>) {
        self.graph_ui.selected = Some(id);
        self.graph_ui.target_port = 0;
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
                || path.ends_with("/implementation")
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
        let result = (|| -> Result<GraphEdit, String> {
            let (id, path) = key
                .strip_prefix("graph.")
                .and_then(|s| s.split_once('/'))
                .ok_or("Invalid node field")?;
            let id = id.parse::<u64>().map_err(|e| e.to_string())?;
            let node = self
                .state
                .system
                .config
                .processing_graph
                .nodes
                .iter()
                .find(|n| n.id == id)
                .ok_or("Node was removed")?;
            let mut value = serde_json::to_value(&node.kind).map_err(|e| e.to_string())?;
            let slot = value
                .pointer_mut(&format!("/{path}"))
                .ok_or("Node field changed")?;
            let text = self.text(key, cx);
            *slot = match slot {
                _ if path.ends_with("_manifest") && text.is_empty() => Value::Null,
                _ if path.ends_with("/channels") => {
                    if text == "all" {
                        json!("all")
                    } else {
                        let channels = text
                            .split(',')
                            .map(|part| {
                                part.trim()
                                    .parse::<usize>()
                                    .ok()
                                    .and_then(|n| n.checked_sub(1))
                                    .ok_or_else(|| {
                                        self.tr("processing-graph-channels-invalid").to_string()
                                    })
                            })
                            .collect::<Result<Vec<_>, _>>()?;
                        json!({"selected": channels})
                    }
                }
                Value::String(_) => Value::String(text),
                Value::Null if text.is_empty() => Value::Null,
                Value::Null => Value::String(text),
                _ => serde_json::from_str(&text).map_err(|e| e.to_string())?,
            };
            let kind = serde_json::from_value(value).map_err(|e| e.to_string())?;
            Ok(GraphEdit::Replace { id, kind })
        })();
        match result {
            Ok(edit) => self.graph_submit(
                edit,
                HistoryAction::Edit,
                Some((key.to_owned(), self.text(key, cx))),
                cx,
            ),
            Err(error) => {
                self.error = Some(error);
                cx.notify();
            }
        }
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
        if output {
            self.graph_ui.connecting = Some(port);
            self.graph_ui.selected = Some(port.node);
            self.graph_ui.target_port = port.port;
            self.sync_graph_fields(cx);
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
        cx.stop_propagation();
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
        let old = self.graph_ui.zoom;
        let next = (old * factor).clamp(0.35, 2.);
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
        let theme = rivu_ui::theme::palette(cx);
        let graph = &self.state.system.config.processing_graph;
        let edges = graph.edges.clone();
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
                        .px_2()
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
                    let label = if output && matches!(node.kind, GraphNodeKind::Separate { .. }) {
                        self.tr(if port == 0 {
                            "processing-graph-vocals"
                        } else {
                            "processing-graph-accompaniment"
                        })
                        .to_string()
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
                        .px_1()
                        .flex()
                        .items_center()
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
                        .child(if output {
                            format!("{label} ●")
                        } else {
                            format!("● {label}")
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_, _, _, cx| cx.stop_propagation()),
                        )
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
            .h(px(380.))
            .flex_shrink_0()
            .overflow_hidden()
            .bg(rgb(theme.bg))
            .border_1()
            .border_color(rgb(theme.border))
            .tab_index(0)
            .tab_stop(true)
            .aria_label(self.tr("processing-routing-graph"))
            .on_key_down(cx.listener(|this, event, _, cx| this.graph_key(event, cx)))
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
                            let mut path = PathBuilder::stroke(px(2.));
                            path.move_to(a);
                            path.cubic_bezier_to(
                                b,
                                a + point(reach, px(0.)),
                                b - point(reach, px(0.)),
                            );
                            if let Ok(path) = path.build() {
                                window.paint_path(path, rgb(theme.accent));
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
        if self.graph_busy() {
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
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        self.reconcile_graph(cx);
        let theme = rivu_ui::theme::palette(cx);
        let busy = self.graph_busy();
        let mut library = row()
            .flex_wrap()
            .gap_1()
            .child(caption(theme, self.tr("processing-graph-add")));
        for (name, kind) in [
            (
                "eq",
                GraphNodeKind::Equalizer {
                    config: rivu_dsp::equalizer::EqualizerConfig {
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
                    SharedString::from(format!("graph-add-{panel}-{name}")),
                    self.tr(node_label(&kind)),
                    cx,
                    move |this, _, cx| this.graph_add(panel, kind.clone(), cx),
                ),
                busy,
            ));
        }
        let tools = row()
            .flex_wrap()
            .gap_1()
            .child(disabled_control(
                theme,
                button(
                    ("graph-undo", panel),
                    self.tr("processing-graph-undo"),
                    cx,
                    |this, _, cx| this.graph_history(false, cx),
                ),
                busy || self.graph_ui.undo.is_empty(),
            ))
            .child(disabled_control(
                theme,
                button(
                    ("graph-redo", panel),
                    self.tr("processing-graph-redo"),
                    cx,
                    |this, _, cx| this.graph_history(true, cx),
                ),
                busy || self.graph_ui.redo.is_empty(),
            ))
            .child(button(
                ("graph-zoom-out", panel),
                "−",
                cx,
                |this, _, cx| this.graph_zoom(1. / 1.2, [250., 160.], cx),
            ))
            .child(caption(theme, format!("{:.0}%", self.graph_ui.zoom * 100.)))
            .child(button(("graph-zoom-in", panel), "+", cx, |this, _, cx| {
                this.graph_zoom(1.2, [250., 160.], cx)
            }))
            .child(button(
                ("graph-reset-view", panel),
                self.tr("processing-graph-reset-view"),
                cx,
                |this, _, cx| {
                    this.cancel_pointer(cx);
                    this.graph_ui.pan = [24., 24.];
                    this.graph_ui.zoom = 1.;
                    cx.notify();
                },
            ));
        let mut content = column()
            // The outer processing viewport scrolls this entire section. Its
            // bounds must include the inspector, not just the fixed canvas.
            .flex_shrink_0()
            .gap_2()
            .child(caption(theme, self.tr("processing-graph-independent")))
            .child(caption(
                theme,
                format!(
                    "{} {}",
                    self.tr("processing-graph-version"),
                    self.state.system.config.processing_graph.revision
                ),
            ))
            .child(library)
            .child(tools)
            .child(self.routing_canvas(panel, cx))
            .child(caption(theme, self.tr("processing-graph-editor-help")));
        if let Some(port) = self.graph_ui.connecting {
            content = content.child(
                row()
                    .flex_wrap()
                    .gap_1()
                    .child(caption(
                        theme,
                        format!(
                            "{} #{}:{} → {}",
                            self.tr("processing-graph-connect-from"),
                            port.node,
                            port.port,
                            self.tr("processing-graph-choose-input")
                        ),
                    ))
                    .child(button(
                        ("graph-cancel-connect", panel),
                        self.tr("processing-cancel"),
                        cx,
                        |this, _, cx| {
                            this.graph_ui.connecting = None;
                            cx.notify();
                        },
                    )),
            );
        }
        content = content.child(self.graph_inspector(panel, cx));
        let edges = self.state.system.config.processing_graph.edges.clone();
        let mut connections = column()
            .flex_shrink_0()
            .gap_1()
            .child(caption(theme, self.tr("processing-graph-connections")));
        for edge in edges {
            let to = edge.to;
            connections = connections.child(
                row()
                    .flex_wrap()
                    .gap_1()
                    .child(caption(
                        theme,
                        format!(
                            "#{}:{} PCM → #{}:{} PCM",
                            edge.from.node, edge.from.port, to.node, to.port
                        ),
                    ))
                    .child(disabled_control(
                        theme,
                        button(
                            SharedString::from(format!(
                                "graph-disconnect-{panel}-{}-{}",
                                to.node, to.port
                            )),
                            self.tr("processing-graph-disconnect"),
                            cx,
                            move |this, _, cx| this.graph_edit(GraphEdit::Disconnect { to }, cx),
                        ),
                        busy,
                    )),
            );
        }
        content = content.child(connections);
        let mut modes = row().flex_wrap().gap_1();
        for (mode, label) in [
            (GraphExecutionMode::Auto, "processing-graph-auto"),
            (GraphExecutionMode::Offline, "processing-graph-offline"),
            (GraphExecutionMode::Realtime, "processing-graph-realtime"),
            (GraphExecutionMode::Hybrid, "processing-graph-hybrid"),
        ] {
            modes = modes.child(
                button(
                    SharedString::from(format!("graph-mode-{panel}-{mode:?}")),
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
        let running = self.state.system.processing.as_ref().is_some_and(|t| {
            matches!(
                t.phase,
                ProcessingPhase::Queued | ProcessingPhase::Rendering | ProcessingPhase::Processing
            )
        });
        content
            .child(modes)
            .child(caption(theme, self.tr("processing-graph-mode-help")))
            .child(
                row()
                    .flex_wrap()
                    .gap_1()
                    .child(disabled_control(
                        theme,
                        button(
                            ("graph-preview", panel),
                            self.tr("processing-graph-preview"),
                            cx,
                            |this, _, cx| this.graph_run_target(false, cx),
                        ),
                        busy || running || self.input_track().is_none(),
                    ))
                    .child(disabled_control(
                        theme,
                        button(
                            ("graph-run", panel),
                            self.tr("processing-graph-run"),
                            cx,
                            |this, _, cx| this.graph_run_target(true, cx),
                        ),
                        busy || running
                            || self.input_track().is_none()
                            || !self
                                .state
                                .system
                                .config
                                .processing_graph
                                .nodes
                                .iter()
                                .any(|node| matches!(node.kind, GraphNodeKind::Output)),
                    )),
            )
    }

    fn graph_inspector(&mut self, panel: u64, cx: &mut Context<Self>) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let busy = self.graph_busy();
        let Some(node) = self
            .state
            .system
            .config
            .processing_graph
            .nodes
            .iter()
            .find(|n| Some(n.id) == self.graph_ui.selected)
            .cloned()
        else {
            return column();
        };
        let id = node.id;
        let fixed = matches!(node.kind, GraphNodeKind::Source);
        let mut content = column()
            .flex_shrink_0()
            .gap_2()
            .p_2()
            .border_1()
            .border_color(rgb(theme.border))
            .child(caption(
                theme,
                format!(
                    "{} · {} #{id}",
                    self.tr("processing-graph-inspector"),
                    self.tr(node_label(&node.kind))
                ),
            ))
            .child(
                row()
                    .flex_wrap()
                    .gap_1()
                    .child(disabled_control(
                        theme,
                        button(
                            ("graph-duplicate", panel),
                            self.tr("processing-graph-duplicate"),
                            cx,
                            |this, _, cx| this.graph_duplicate(cx),
                        ),
                        busy || fixed,
                    ))
                    .child(disabled_control(
                        theme,
                        button(
                            ("graph-delete", panel),
                            self.tr("processing-graph-delete"),
                            cx,
                            |this, _, cx| this.graph_remove(cx),
                        ),
                        busy || fixed,
                    )),
            );
        if let GraphNodeKind::Equalizer {
            config,
            implementation,
        } = &node.kind
        {
            let mut algorithms = row().flex_wrap().gap_1();
            for (choice, label) in [
                (EqImplementation::Iir, "processing-graph-iir"),
                (
                    EqImplementation::LinearPhase { taps: 2049 },
                    "processing-graph-fir",
                ),
            ] {
                let config = config.clone();
                algorithms = algorithms.child(disabled_control(
                    theme,
                    button(
                        SharedString::from(format!("graph-eq-{panel}-{choice:?}")),
                        self.tr(label),
                        cx,
                        move |this, _, cx| {
                            this.graph_edit(
                                GraphEdit::Replace {
                                    id,
                                    kind: GraphNodeKind::Equalizer {
                                        config: config.clone(),
                                        implementation: choice,
                                    },
                                },
                                cx,
                            )
                        },
                    )
                    .when(
                        std::mem::discriminant(implementation) == std::mem::discriminant(&choice),
                        |v| v.bg(rgb(theme.selected)),
                    ),
                    busy,
                ));
            }
            let mut next = config.clone();
            next.bands.push(EqBand::default());
            let add_kind = GraphNodeKind::Equalizer {
                config: next,
                implementation: *implementation,
            };
            let mut next = config.clone();
            next.bands.pop();
            let remove_kind = GraphNodeKind::Equalizer {
                config: next,
                implementation: *implementation,
            };
            content = content.child(algorithms).child(
                row()
                    .flex_wrap()
                    .gap_1()
                    .child(disabled_control(
                        theme,
                        button(
                            ("graph-add-band", panel),
                            self.tr("processing-add-band"),
                            cx,
                            move |this, _, cx| {
                                this.graph_edit(
                                    GraphEdit::Replace {
                                        id,
                                        kind: add_kind.clone(),
                                    },
                                    cx,
                                )
                            },
                        ),
                        busy || config.bands.len() >= MAX_BANDS,
                    ))
                    .child(disabled_control(
                        theme,
                        button(
                            ("graph-remove-band", panel),
                            self.tr("processing-remove-band"),
                            cx,
                            move |this, _, cx| {
                                this.graph_edit(
                                    GraphEdit::Replace {
                                        id,
                                        kind: remove_kind.clone(),
                                    },
                                    cx,
                                )
                            },
                        ),
                        busy || config.bands.is_empty(),
                    )),
            );
        }
        if let GraphNodeKind::Denoise { config, profile } = &node.kind {
            let mut modes = row().flex_wrap().gap_1();
            for (mode, label) in [
                ("off", "processing-off"),
                ("stationary", "processing-stationary"),
                ("profile", "processing-profile"),
                ("adaptive", "processing-adaptive"),
            ] {
                let mut value = serde_json::to_value(config).expect("serializable denoise");
                value["mode"] = json!(mode);
                let config: rivu_dsp::denoise::DenoiseConfig =
                    serde_json::from_value(value).expect("known denoise mode");
                let profile = *profile;
                modes = modes.child(disabled_control(
                    theme,
                    button(
                        SharedString::from(format!("graph-denoise-{panel}-{mode}")),
                        self.tr(label),
                        cx,
                        move |this, _, cx| {
                            this.graph_edit(
                                GraphEdit::Replace {
                                    id,
                                    kind: GraphNodeKind::Denoise {
                                        config: config.clone(),
                                        profile,
                                    },
                                },
                                cx,
                            )
                        },
                    ),
                    busy,
                ));
            }
            content = content.child(modes);
            if profile.is_none() {
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
                            )
                        },
                    ),
                    busy,
                ));
            }
        }
        if let GraphNodeKind::Mix {
            gains_db,
            muted,
            solo,
        } = &node.kind
        {
            let mut gains = gains_db.clone();
            gains.push(0.);
            let mut m = muted.clone();
            m.push(false);
            let mut s = solo.clone();
            s.push(false);
            let add = GraphNodeKind::Mix {
                gains_db: gains,
                muted: m,
                solo: s,
            };
            let mut gains = gains_db.clone();
            gains.pop();
            let mut m = muted.clone();
            m.pop();
            let mut s = solo.clone();
            s.pop();
            let remove = GraphNodeKind::Mix {
                gains_db: gains,
                muted: m,
                solo: s,
            };
            content = content
                .child(caption(theme, self.tr("processing-graph-mix-help")))
                .child(
                    row()
                        .flex_wrap()
                        .gap_1()
                        .child(disabled_control(
                            theme,
                            button(
                                ("graph-add-lane", panel),
                                self.tr("processing-graph-add-lane"),
                                cx,
                                move |this, _, cx| {
                                    this.graph_edit(
                                        GraphEdit::Replace {
                                            id,
                                            kind: add.clone(),
                                        },
                                        cx,
                                    )
                                },
                            ),
                            busy || gains_db.len() >= 16,
                        ))
                        .child(disabled_control(
                            theme,
                            button(
                                ("graph-remove-lane", panel),
                                self.tr("processing-graph-remove-lane"),
                                cx,
                                move |this, _, cx| {
                                    this.graph_edit(
                                        GraphEdit::Replace {
                                            id,
                                            kind: remove.clone(),
                                        },
                                        cx,
                                    )
                                },
                            ),
                            busy || gains_db.len() <= 2,
                        )),
                );
        }
        if matches!(
            node.kind,
            GraphNodeKind::Speech { .. } | GraphNodeKind::Separate { .. }
        ) {
            content = content.child(caption(theme, self.tr("processing-graph-model-help")));
        }
        let mut fields = Vec::new();
        scalar_fields(
            &serde_json::to_value(&node.kind).expect("serializable node"),
            "",
            &mut fields,
        );
        for (path, value) in fields {
            let key = format!("graph.{id}{path}");
            if !self.fields.contains_key(&key) {
                continue;
            }
            let label = self.graph_parameter_label(&path);
            if let Value::Bool(enabled) = value {
                let kind = node.kind.clone();
                let path_copy = path.clone();
                content = content.child(checkbox(
                    SharedString::from(format!("graph-bool-{panel}-{id}-{path}")),
                    SharedString::from(label),
                    enabled,
                    !busy,
                    cx,
                    move |this, value, cx| {
                        let mut serialized =
                            serde_json::to_value(&kind).expect("serializable node");
                        if let Some(slot) = serialized.pointer_mut(&path_copy) {
                            *slot = json!(value);
                        }
                        if let Ok(kind) = serde_json::from_value(serialized) {
                            this.graph_edit(GraphEdit::Replace { id, kind }, cx);
                        }
                    },
                ));
            } else {
                let input = self.graph_panel_input(panel, &key, &label, cx);
                let label = if self.fields[&key].dirty {
                    format!("{label} · {}", self.tr("processing-draft"))
                } else {
                    label
                };
                content = content.child(
                    column().gap_1().child(caption(theme, label)).child(
                        row()
                            .gap_1()
                            .child(div().flex_1().min_w_0().child(input))
                            .child(disabled_control(
                                theme,
                                button(
                                    SharedString::from(format!("graph-apply-{panel}-{key}")),
                                    self.tr("processing-apply"),
                                    cx,
                                    move |this, _, cx| this.apply_graph_field(&key, cx),
                                ),
                                busy,
                            )),
                    ),
                );
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
                    .when(self.graph_ui.target_port == port as u16, |v| {
                        v.bg(rgb(theme.selected))
                    }),
                );
            }
            content = content.child(ports);
        }
        content
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
        let revision = task
            .graph_revision
            .map(|v| v.to_string())
            .unwrap_or_else(|| "—".into());
        let mut content = column()
            .flex_shrink_0()
            .gap_2()
            .child(caption(
                theme,
                format!(
                    "{} #{id} · {} #{} · {} {revision} · {phase}",
                    self.tr("processing-task"),
                    self.tr("processing-source"),
                    task.track_id,
                    self.tr("processing-graph-version")
                ),
            ))
            .child(caption(
                theme,
                format!(
                    "{}: {} / {}",
                    self.tr("processing-progress"),
                    task.completed,
                    task.total
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "?".into())
                ),
            ));
        if task
            .settings
            .graph
            .as_ref()
            .is_some_and(|graph| !graph.audio_eq(&self.state.system.config.processing_graph))
        {
            content = content.child(caption(theme, self.tr("processing-graph-result-older")));
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
                content = content.child(caption(theme, error.clone()).text_color(rgb(theme.error)));
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
        if !terminal {
            content = content.child(button(
                ("graph-cancel-task", panel),
                self.tr("processing-cancel"),
                cx,
                move |this, _, cx| this.send(Command::CancelProcessing { id }, cx),
            ));
        }
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
                "processing-graph-live-ready"
            } else {
                "processing-graph-file-ready"
            };
            content = content.child(caption(theme, self.tr(ready)));
            if live {
                content = content.child(button(
                    ("graph-stop-live", panel),
                    self.tr("processing-graph-stop-live"),
                    cx,
                    move |this, _, cx| this.send(Command::CancelProcessing { id }, cx),
                ));
            }
            content = content.child(
                column()
                    .min_w_0()
                    .gap_1()
                    .child(caption(theme, self.tr("processing-export-path")))
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
                    (0..output_count(&node.kind)).map(move |index| GraphPort {
                        node: node.id,
                        port: index as u16,
                    })
                });
            for port in ports {
                let file = task.graph_outputs.iter().find(|output| output.port == port);
                let availability = file
                    .map(|output| {
                        format!(
                            "{} · {}",
                            self.tr("processing-graph-cached"),
                            output.path.display()
                        )
                    })
                    .unwrap_or_else(|| self.tr("processing-graph-not-cached").to_string());
                content = content.child(
                    row()
                        .flex_wrap()
                        .gap_1()
                        .child(caption(
                            theme,
                            format!("#{}:{} · {availability}", port.node, port.port),
                        ))
                        .child(button(
                            SharedString::from(format!(
                                "graph-audition-{panel}-{}-{}",
                                port.node, port.port
                            )),
                            self.tr("processing-graph-audition"),
                            cx,
                            move |this, _, cx| {
                                this.send(Command::ProcessingGraphAudition { id, node: port }, cx)
                            },
                        ))
                        .child(button(
                            SharedString::from(format!(
                                "graph-export-{panel}-{}-{}",
                                port.node, port.port
                            )),
                            self.tr("processing-graph-export"),
                            cx,
                            move |this, _, cx| {
                                let path = this.text("graph_export", cx);
                                if path.is_empty() {
                                    this.error = Some(
                                        this.context
                                            .i18n
                                            .text("processing-graph-export-required")
                                            .to_owned(),
                                    );
                                    cx.notify();
                                } else {
                                    this.send(
                                        Command::ProcessingGraphExport {
                                            id,
                                            node: port,
                                            path: PathBuf::from(path),
                                        },
                                        cx,
                                    );
                                }
                            },
                        )),
                );
            }
        }
        content
    }
}
