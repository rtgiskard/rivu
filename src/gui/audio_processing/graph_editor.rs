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

mod inspector;
use inspector::{DraftStamp, SubmittedField};
mod fields;
pub(super) use fields::{GraphField, NodeField, node_fields};

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
    geometry: WindowGeometry,
    epoch: u64,
    revision: u64,
    button: MouseButton,
    _subscriptions: [Subscription; 2],
}

pub(super) struct GraphEditorState {
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
    draft_stamps: HashMap<FieldKey, DraftStamp>,
    draft_generation: u64,
    field_errors: HashMap<FieldKey, String>,
    expanded_backends: std::collections::HashSet<u64>,
    workbenches: HashMap<u64, WorkbenchPanel>,
    workbench_choices: rivu_ui::ChoiceState<WorkbenchChoice>,
    workbench_menu: Option<(u64, WorkbenchMenu)>,
    workbench_anchor: Cell<Option<Bounds<Pixels>>>,
}

impl Default for GraphEditorState {
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
            draft_stamps: HashMap::new(),
            draft_generation: 0,
            field_errors: HashMap::new(),
            expanded_backends: std::collections::HashSet::new(),
            workbenches: HashMap::new(),
            workbench_choices: rivu_ui::ChoiceState::default(),
            workbench_menu: None,
            workbench_anchor: Cell::new(None),
        }
    }
}

impl GraphEditorState {
    pub(super) fn retain_panels(&mut self, layout: &super::super::workspace::WorkspacePanels) {
        self.workbenches.retain(|id, _| layout.contains_panel(*id));
    }
}

pub(super) fn node_label(kind: &GraphNodeKind) -> &'static str {
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

    pub(super) fn graph_busy(&self) -> bool {
        self.graph_editor.pending.is_some() || self.pending > 0
    }

    pub(super) fn graph_collecting_equalizer_input(&self) -> Option<(u64, u64)> {
        let pending = self.graph_editor.pending.as_ref()?;
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
        if self.graph_editor.revision != Some(revision) {
            self.graph_editor.pointer = None;
            if self.graph_editor.pending.is_none() && self.graph_editor.revision.is_some() {
                self.graph_editor.undo.clear();
                self.graph_editor.redo.clear();
            }
            self.graph_editor.revision = Some(revision);
        }
        if let Some(pending) = self.graph_editor.pending.as_ref()
            && pending.accepted
            && revision > pending.before.revision
        {
            let pending = self
                .graph_editor
                .pending
                .take()
                .expect("pending graph edit");
            let accepted_revision = pending.before.revision + 1;
            equalizer_completed = pending
                .equalizer_submitted
                .map(|submission| (submission, revision == accepted_revision));
            if revision == pending.before.revision + 1 {
                equalizer_rebased = pending
                    .equalizer_target
                    .map(|id| (id, accepted_revision - 1, revision));
                if let Some(id) = pending.select_after {
                    self.graph_editor.selected = Some(id);
                    self.graph_editor.target_port = 0;
                }
                match pending.action {
                    HistoryAction::Edit => {
                        self.graph_editor.undo.push_back(pending.before);
                        self.graph_editor.redo.clear();
                    }
                    HistoryAction::Undo => {
                        self.graph_editor.undo.pop_back();
                        self.graph_editor.redo.push_back(pending.before);
                    }
                    HistoryAction::Redo => {
                        self.graph_editor.redo.pop_back();
                        self.graph_editor.undo.push_back(pending.before);
                    }
                }
                while self.graph_editor.undo.len() > HISTORY_LIMIT {
                    self.graph_editor.undo.pop_front();
                }
                while self.graph_editor.redo.len() > HISTORY_LIMIT {
                    self.graph_editor.redo.pop_front();
                }
            } else {
                self.graph_editor.undo.clear();
                self.graph_editor.redo.clear();
            }
            if revision == accepted_revision {
                for stamp in self.graph_editor.draft_stamps.values_mut() {
                    if stamp.revision + 1 == revision {
                        stamp.revision = revision;
                    }
                }
                for submitted in pending.submitted {
                    if inspector::submission_current(
                        self.graph_editor.draft_stamps.get(&submitted.key),
                        &submitted,
                        self.fields
                            .get(&submitted.key)
                            .map(|field| field.input.read(cx).text()),
                    ) && let Some(field) = self.fields.get_mut(&submitted.key)
                    {
                        field.dirty = false;
                        self.graph_editor.draft_stamps.remove(&submitted.key);
                        self.graph_editor.field_errors.remove(&submitted.key);
                    }
                }
            }
        }
        if self
            .graph_editor
            .selected
            .is_none_or(|id| !graph.nodes.iter().any(|n| n.id == id))
        {
            self.graph_editor.selected = graph.nodes.first().map(|n| n.id);
            self.graph_editor.target_port = 0;
        }
        if self
            .graph_editor
            .connecting
            .is_some_and(|p| !graph.nodes.iter().any(|n| n.id == p.node))
        {
            self.graph_editor.connecting = None;
        }
        let selected = self.graph_editor.selected;
        self.fields.retain(|key, field| {
            let Some(id) = key.graph_node() else {
                return true;
            };
            graph
                .nodes
                .iter()
                .any(|node| matches!(key, FieldKey::Graph(binding) if binding.matches(node)))
                && (Some(id) == selected || field.dirty)
        });
        self.graph_editor
            .draft_stamps
            .retain(|key, _| self.fields.contains_key(key));
        self.graph_editor
            .field_errors
            .retain(|key, _| self.fields.contains_key(key));
        self.graph_editor
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
            if let Some(pending) = self.graph_editor.pending.take() {
                if pending.before.revision != self.state.system.config.processing_graph.revision {
                    self.graph_editor.undo.clear();
                    self.graph_editor.redo.clear();
                }
                let error = self
                    .error
                    .clone()
                    .unwrap_or_else(|| self.tr("processing-command-failed").to_string());
                for submitted in pending.submitted {
                    if self.fields.contains_key(&submitted.key) {
                        self.graph_editor
                            .field_errors
                            .insert(submitted.key, error.clone());
                    }
                }
                if let Some(submission) = pending.equalizer_submitted {
                    self.finish_node_equalizer_submission(submission, false, cx);
                }
            }
        } else if let Some(pending) = self.graph_editor.pending.as_mut() {
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
        self.graph_editor.pending = Some(PendingEdit {
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
            self.graph_editor.redo.back()
        } else {
            self.graph_editor.undo.back()
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
        if self.graph_editor.selected != Some(id) {
            self.graph_editor.selected = Some(id);
            self.graph_editor.target_port = 0;
        }
        self.sync_graph_fields(cx);
        cx.notify();
    }

    fn sync_graph_fields(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.graph_editor.selected else {
            self.graph_editor.fields_revision = None;
            for fields in self.panel_inputs.values_mut() {
                fields.retain(|key, _| *key == FieldKey::GraphExport);
            }
            return;
        };
        let stamp = (self.state.system.config.processing_graph.revision, id);
        if self.graph_editor.fields_revision == Some(stamp) {
            return;
        }
        self.graph_editor.fields_revision = Some(stamp);
        let node = self
            .state
            .system
            .config
            .processing_graph
            .nodes
            .iter()
            .find(|n| Some(n.id) == self.graph_editor.selected)
            .cloned();
        let Some(node) = node else {
            return;
        };
        let fields = fields::node_fields(&node.kind);
        let mut active_keys = std::collections::HashSet::with_capacity(fields.size_hint().0);
        for field in fields {
            let text = field.text(&node.kind);
            let key = FieldKey::Graph(NodeField::new(&node, field));
            self.field(key, &self.graph_parameter_label(field), text, cx);
            active_keys.insert(key);
        }
        self.fields.retain(|key, field| match key.graph_node() {
            None => true,
            Some(node) if node == id => active_keys.contains(key),
            Some(_) => field.dirty,
        });
        // Keep only the current inspector's replicas. Drafts for other nodes
        // remain in the backing fields; removed controls drop subscriptions.
        for fields in self.panel_inputs.values_mut() {
            fields.retain(|key, _| *key == FieldKey::GraphExport || active_keys.contains(key));
        }
        if !self.fields.contains_key(&FieldKey::GraphExport) {
            self.field(
                FieldKey::GraphExport,
                &self.tr("processing-export-path"),
                String::new(),
                cx,
            );
        }
    }

    pub(super) fn graph_parameter_label(&self, field: GraphField) -> String {
        let label = self.tr(field.label()).to_string();
        match field {
            GraphField::MixGain(index)
            | GraphField::MixMuted(index)
            | GraphField::MixSolo(index) => {
                format!("{label} {}", index + 1)
            }
            GraphField::Denoise(field) => match field.group_label_id() {
                Some(group) => format!("{} · {label}", self.tr(group)),
                None => label,
            },
            _ => label,
        }
    }

    pub(super) fn apply_graph_field(&mut self, key: FieldKey, cx: &mut Context<Self>) {
        self.submit_graph_fields(vec![key], cx);
    }

    fn graph_add(&mut self, panel: u64, kind: GraphNodeKind, cx: &mut Context<Self>) {
        let viewport = self
            .graph_editor
            .bounds
            .get(&panel)
            .and_then(|b| b.get())
            .map(|(bounds, _)| [f32::from(bounds.size.width), f32::from(bounds.size.height)])
            .unwrap_or([600., 380.]);
        let position = [
            (viewport[0] * 0.5 - self.graph_editor.pan[0]) / self.graph_editor.zoom
                - NODE_WIDTH * 0.5,
            (viewport[1] * 0.5 - self.graph_editor.pan[1]) / self.graph_editor.zoom
                - node_height(&kind) * 0.5,
        ];
        self.graph_edit(GraphEdit::Add { kind, position }, cx);
    }

    fn graph_remove(&mut self, cx: &mut Context<Self>) {
        if self
            .graph_editor
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
                Some(n.id) == self.graph_editor.selected && !matches!(n.kind, GraphNodeKind::Source)
            })
        {
            self.graph_edit(GraphEdit::Remove { id: node.id }, cx);
        }
    }

    fn graph_duplicate(&mut self, cx: &mut Context<Self>) {
        if self
            .graph_editor
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
                Some(n.id) == self.graph_editor.selected && !matches!(n.kind, GraphNodeKind::Source)
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
            self.graph_editor.connecting = Some(port);
            self.graph_editor.target_port = port.port;
            cx.notify();
        } else if let Some(from) = self.graph_editor.connecting.take() {
            self.graph_edit(GraphEdit::Connect { from, to: port }, cx);
        }
    }

    fn graph_position(&self, node: &GraphNode) -> [f32; 2] {
        self.graph_editor
            .pointer
            .as_ref()
            .filter(|p| p.node == Some(node.id))
            .map(|p| p.preview)
            .unwrap_or(node.position)
    }

    fn graph_pan(&self) -> [f32; 2] {
        self.graph_editor
            .pointer
            .as_ref()
            .filter(|p| p.node.is_none())
            .map(|p| p.preview)
            .unwrap_or(self.graph_editor.pan)
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
        let Some((bounds, geometry)) = self.graph_editor.bounds.get(&panel).and_then(|b| b.get())
        else {
            return;
        };
        if geometry != WindowGeometry::current(window, self.state.system.config.ui_scale) {
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
            self.graph_editor.pan
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
        self.graph_editor.pointer = Some(GraphPointer {
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
        let Some(pointer) = self.graph_editor.pointer.as_mut() else {
            return;
        };
        if pointer.epoch != self.geometry_epoch
            || pointer.geometry
                != WindowGeometry::current(window, self.state.system.config.ui_scale)
            || pointer.revision != self.state.system.config.processing_graph.revision
        {
            self.cancel_pointer(cx);
            return;
        }
        let scale = if pointer.node.is_some() {
            self.graph_editor.zoom
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
                        .graph_editor
                        .pointer
                        .as_ref()
                        .is_some_and(|p| p.panel == panel)
                    {
                        if this
                            .graph_editor
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
                        .graph_editor
                        .pointer
                        .as_ref()
                        .is_some_and(|p| p.panel == panel && p.button == event.button)
                    {
                        this.graph_move_pointer(event.position, window, cx);
                        if let Some(pointer) = this.graph_editor.pointer.take() {
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
                                this.graph_editor.pan = pointer.preview;
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
        self.graph_zoom_to(self.graph_editor.zoom * factor, anchor, cx);
    }

    fn graph_zoom_to(&mut self, zoom: f32, anchor: [f32; 2], cx: &mut Context<Self>) {
        let old = self.graph_editor.zoom;
        let next = zoom.clamp(0.35, 2.);
        for (i, coordinate) in anchor.into_iter().enumerate() {
            self.graph_editor.pan[i] =
                coordinate - (coordinate - self.graph_editor.pan[i]) * next / old;
        }
        self.graph_editor.zoom = next;
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
                    self.graph_editor.connecting = None;
                    cx.notify();
                }
                "delete" | "backspace" => self.graph_remove(cx),
                "+" | "=" => self.graph_zoom(1.2, [250., 160.], cx),
                "-" => self.graph_zoom(1. / 1.2, [250., 160.], cx),
                "home" => {
                    self.cancel_pointer(cx);
                    self.graph_editor.pan = [24., 24.];
                    self.graph_editor.zoom = 1.;
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
                        self.graph_editor.pan[0] += delta[0];
                        self.graph_editor.pan[1] += delta[1];
                        cx.notify();
                    } else if let Some(node) = self
                        .state
                        .system
                        .config
                        .processing_graph
                        .nodes
                        .iter()
                        .find(|n| Some(n.id) == self.graph_editor.selected)
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
            .graph_editor
            .workbenches
            .entry(panel)
            .or_default()
            .canvas_focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        let theme = rivu_ui::theme::palette(cx);
        let graph = &self.state.system.config.processing_graph;
        let edges = graph.edges.clone();
        let highlighted = self.graph_editor.highlight_connection;
        let pan = self.graph_pan();
        let zoom = self.graph_editor.zoom;
        let measured = self
            .graph_editor
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
                .border_color(rgb(if self.graph_editor.selected == Some(id) {
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
                    let active = output && self.graph_editor.connecting == Some(endpoint);
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
                    this.graph_editor.pan[0] += delta[0];
                    this.graph_editor.pan[1] += delta[1];
                    cx.notify();
                }
                cx.stop_propagation();
            }))
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let geometry = WindowGeometry::current(window, scale);
                        measure.set(Some((bounds, geometry)));
                        let _ = owner.update(cx, |this, cx| {
                            if this.graph_editor.pointer.as_ref().is_some_and(|p| {
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
                .find(|n| Some(n.id) == self.graph_editor.selected)
        };
        let (Some(node), Some(track)) = (node, self.input_track()) else {
            return;
        };
        let target = GraphPort {
            node: node.id,
            port: if output {
                0
            } else {
                self.graph_editor.target_port
            },
        };
        self.send(
            Command::ProcessingGraphRun {
                track_id: track.id,
                target,
                mode: self.graph_editor.mode,
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
        core::CoreRuntime,
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
        CoreRuntime,
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
        let runtime = CoreRuntime::start(directory.path(), &path).unwrap();
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
            let bounds = this.graph_editor.bounds[&PANEL].get().unwrap().0;
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
    fn replace_parameter_text(
        cx: &mut TestAppContext,
        processing: &Entity<AudioProcessing>,
        handle: AnyWindowHandle,
        key: FieldKey,
        text: &str,
    ) {
        cx.update_window(handle, |_, window, cx| {
            use gpui::EntityInputHandler as _;
            let input = processing.read(cx).fields[&key].input.clone();
            let length = input.read(cx).text().encode_utf16().count();
            input.update(cx, |input, cx| {
                input.replace_text_in_range(Some(0..length), text, window, cx)
            });
        })
        .unwrap();
        draw(cx, handle);
    }

    #[gpui::test]
    fn removed_workbench_ignores_events_from_its_last_frame(cx: &mut TestAppContext) {
        use gpui::InputEvent as _;

        struct WorkbenchWindow(Entity<AudioProcessing>);
        impl Render for WorkbenchWindow {
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                self.0
                    .update(cx, |this, cx| this.graph_workspace(PANEL, window, cx))
            }
        }

        let (_directory, _runtime, processing, _) = fixture(cx);
        let workbench = cx.add_window({
            let processing = processing.clone();
            move |_, _| WorkbenchWindow(processing)
        });
        let handle = workbench.into();
        draw(cx, handle);
        cx.update_window(handle, |_, window, cx| {
            processing.update(cx, |this, cx| {
                this.sync_panels(&Default::default(), true, cx);
                assert!(!this.graph_editor.workbenches.contains_key(&PANEL));
            });
            // Keep the rendered frame: these are the listeners from the removed panel.
            window.dispatch_event(
                MouseMoveEvent {
                    position: point(px(40.), px(40.)),
                    pressed_button: Some(MouseButton::Left),
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
            window.dispatch_event(
                MouseUpEvent {
                    button: MouseButton::Left,
                    position: point(px(40.), px(40.)),
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
            processing.read_with(cx, |this, _| {
                assert!(!this.graph_editor.workbenches.contains_key(&PANEL));
                assert_eq!(this.graph_editor.zoom, 1.);
            });
        })
        .unwrap();
    }

    #[gpui::test]
    fn ordinary_ack_keeps_retyped_draft_and_ignores_programmatic_sync(cx: &mut TestAppContext) {
        let (_directory, _runtime, processing, handle) = fixture(cx);
        let key = FieldKey::Neural(NeuralField::Threads);
        processing.update(cx, |this, _| this.command_active = true);
        replace_parameter_text(cx, &processing, handle, key, "4");
        let submitted = processing.update(cx, |this, cx| {
            this.apply_field(key, cx);
            this.commands.pop_front().unwrap().field.unwrap()
        });
        replace_parameter_text(cx, &processing, handle, key, "5");
        replace_parameter_text(cx, &processing, handle, key, "4");
        let generation = processing.update(cx, |this, cx| {
            this.finish_parameter_fields(submitted, cx);
            assert!(this.fields[&key].dirty);
            assert_eq!(this.fields[&key].input.read(cx).text(), "4");
            this.apply_field(key, cx);
            let submitted = this.commands.pop_front().unwrap().field.unwrap();
            this.finish_parameter_fields(submitted, cx);
            assert!(!this.fields[&key].dirty);
            let generation = this.fields[&key].generation;
            this.fields[&key]
                .input
                .update(cx, |input, cx| input.set_text("6", cx));
            generation
        });
        cx.run_until_parked();
        processing.read_with(cx, |this, _| {
            assert!(!this.fields[&key].dirty);
            assert_eq!(this.fields[&key].generation, generation);
        });
    }

    #[gpui::test]
    fn stale_chooser_failure_cannot_pollute_new_draft_or_release_new_request(
        cx: &mut TestAppContext,
    ) {
        let (_directory, _runtime, processing, _handle) = fixture(cx);
        processing.update(cx, |this, cx| {
            this.graph_select(3, cx);
            let key = *this
                .fields
                .keys()
                .find(|key| {
                    matches!(
                        key,
                        FieldKey::Graph(NodeField {
                            node: 3,
                            field: GraphField::Neural(NeuralField::Worker),
                            ..
                        })
                    )
                })
                .unwrap();
            this.panel_input(PANEL, key, "Worker", cx);
            this.stage_graph_text(key, "python".into(), cx);
            let revision = this.state.system.config.processing_graph.revision;
            let generation = this.graph_editor.draft_stamps[&key].generation;
            let old = this.begin_path_request(PANEL, key);
            this.stage_graph_text(key, "new-worker.py".into(), cx);
            this.finish_graph_path_request(
                old,
                (revision, generation, "python"),
                Err::<_, &str>("old chooser failure"),
                cx,
            );
            assert!(
                this.choosing_path.is_none(),
                "the completed request retires its own busy state"
            );
            assert!(!this.graph_editor.field_errors.contains_key(&key));
            assert_eq!(this.text(key, cx), "new-worker.py");

            let generation = this.graph_editor.draft_stamps[&key].generation;
            let old = this.begin_path_request(PANEL, key);
            this.discard_graph_node(3, cx);
            this.stage_graph_text(key, "latest-worker.py".into(), cx);
            let current = this.begin_path_request(PANEL, key);
            this.finish_graph_path_request(
                old,
                (revision, generation, "new-worker.py"),
                Err::<_, &str>("obsolete failure"),
                cx,
            );
            assert_eq!(this.choosing_path, Some(current));
            assert_eq!(this.text(key, cx), "latest-worker.py");
            assert!(!this.graph_editor.field_errors.contains_key(&key));

            let generation = this.graph_editor.draft_stamps[&key].generation;
            this.finish_graph_path_request(
                current,
                (revision, generation, "latest-worker.py"),
                Err::<_, &str>("current failure"),
                cx,
            );
            assert!(this.choosing_path.is_none());
            assert_eq!(this.graph_editor.field_errors[&key], "current failure");
        });
    }

    #[gpui::test]
    async fn replacing_node_kind_drops_obsolete_drafts_before_new_edits(cx: &mut TestAppContext) {
        let (_directory, runtime, processing, handle) = fixture(cx);
        processing.update(cx, |this, cx| this.graph_select(3, cx));
        for (kind, threads) in [
            (
                GraphNodeKind::Speech {
                    config: rivu_format::config::NeuralConfig {
                        threads: 2,
                        ..Default::default()
                    },
                },
                2,
            ),
            (
                GraphNodeKind::Separate {
                    config: rivu_format::config::NeuralConfig {
                        threads: 3,
                        ..Default::default()
                    },
                },
                3,
            ),
        ] {
            processing.update(cx, |this, cx| {
                let key = *this
                    .fields
                    .keys()
                    .find(|key| {
                        matches!(
                            key,
                            FieldKey::Graph(NodeField {
                                node: 3,
                                field: GraphField::Neural(NeuralField::Threads),
                                ..
                            })
                        )
                    })
                    .unwrap();
                this.stage_graph_text(key, "8".into(), cx);
            });
            let revision = runtime.handle.config_snapshot().processing_graph.revision;
            let ack = futures::executor::block_on(runtime.handle.request_ack(
                Command::ProcessingGraphEdit {
                    expected_revision: revision,
                    edit: Box::new(GraphEdit::Replace { id: 3, kind }),
                },
            ));
            assert!(ack.is_success(), "{:?}", ack.error);
            processing.update(cx, |this, cx| {
                this.reconcile(this.context.snapshot(), cx);
                let key = *this
                    .fields
                    .keys()
                    .find(|key| {
                        matches!(
                            key,
                            FieldKey::Graph(NodeField {
                                node: 3,
                                field: GraphField::Neural(NeuralField::Threads),
                                ..
                            })
                        )
                    })
                    .unwrap();
                assert_eq!(this.fields[&key].input.read(cx).text(), threads.to_string());
                assert!(!this.fields[&key].dirty);
                this.stage_graph_text(key, "4".into(), cx);
                this.apply_graph_field(key, cx);
            });
            wait_for_commands(cx, &processing, handle).await;
            let snapshot = runtime.handle.config_snapshot();
            let node = snapshot.processing_graph.node(3).unwrap();
            let (GraphNodeKind::Speech { config } | GraphNodeKind::Separate { config }) =
                &node.kind
            else {
                panic!("neural node");
            };
            assert_eq!(config.threads, 4);
        }
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
        processing.read_with(cx, |this, _| {
            assert_eq!(this.graph_editor.selected, Some(4))
        });
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
        processing.read_with(cx, |this, _| assert!(this.graph_editor.pointer.is_none()));
        release(cx, handle, port);
        processing.read_with(cx, |this, _| {
            assert_eq!(
                this.graph_editor.connecting,
                Some(GraphPort { node: 3, port: 1 })
            );
            assert_eq!(this.graph_editor.target_port, 1);
        });
        key(cx, handle, "escape");
        processing.read_with(cx, |this, _| {
            assert_eq!(this.graph_editor.selected, Some(3));
            assert!(this.graph_editor.connecting.is_none());
            assert_eq!(this.graph_editor.target_port, 1);
        });
        assert_eq!(runtime.handle.config_snapshot().processing_graph, original);
        // An input port also owns selection, even without a pending connection.
        let input =
            node_point(&processing, 2, cx) + point(px(-68.), px(HEADER + PORT_ROW * 0.5 - 16.));
        press(cx, handle, input);
        processing.read_with(cx, |this, _| assert!(this.graph_editor.pointer.is_none()));
        release(cx, handle, input);
        processing.read_with(cx, |this, _| {
            assert_eq!(this.graph_editor.selected, Some(2))
        });
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
        processing.read_with(cx, |this, _| {
            assert_eq!(this.graph_editor.selected, Some(3))
        });
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
                graph_run_id: Some(1),
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
            this.panel_inputs[&PANEL][&FieldKey::GraphExport]
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
            assert_eq!(
                this.fields[&FieldKey::GraphExport].input.read(cx).text(),
                "out.wav"
            );
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
