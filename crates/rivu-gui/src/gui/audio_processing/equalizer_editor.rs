//! One editor per explicit EQ target, reused by the window's processing panel.
//! Core owns committed parameters; editors retain drafts and cancellable pointer intent.
//! Gain and response-channel controls share a wrapping header above the editor body.
use super::*;
use crate::projection::EqualizerFormat;
use rivu_dsp::equalizer::config::{
    ChannelTarget, EqBand, EqualizerConfig, FilterKind, MAX_BANDS, MAX_FREQUENCY_RATE_RATIO,
};
use rivu_format::config::processing_graph::{EqImplementation, GraphEdit, GraphNodeKind};
use rivu_protocol::model::EqBandEdit;
use rivu_ui::{Slider, SliderInput, SliderKey};

/// Realized-vs-target FIR gaps at least this large are shaded on the curve.
/// A display aid, not an audibility or acceptance threshold.
const FIR_DEVIATION_DB: f64 = 0.5;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum EqTarget {
    Monitor,
    Node(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum BandField {
    FrequencyHz,
    GainDb,
    Q,
    ShelfSlope,
    Stages,
    Channels,
}

#[derive(Clone)]
struct FieldControl {
    key: EqField,
    input: Entity<Input>,
    epoch: u64,
    revision: u64,
    format: EqFormat,
    geometry_epoch: u64,
}

impl BandField {
    fn name(self) -> &'static str {
        match self {
            Self::FrequencyHz => "frequency_hz",
            Self::GainDb => "gain_db",
            Self::Q => "q",
            Self::ShelfSlope => "shelf_slope",
            Self::Stages => "stages",
            Self::Channels => "channels",
        }
    }

    fn numeric_edit(self, value: f64) -> EqBandEdit {
        match self {
            Self::FrequencyHz => EqBandEdit::FrequencyHz(value),
            Self::GainDb => EqBandEdit::GainDb(value),
            Self::Q => EqBandEdit::Q(value),
            Self::ShelfSlope => EqBandEdit::ShelfSlope(value),
            Self::Stages => EqBandEdit::Stages(value as u8),
            Self::Channels => unreachable!("channels are not a numeric field"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum EqField {
    PreampDb,
    OutputGainDb,
    BandCount,
    Band(usize, BandField),
}

impl EqField {
    fn band_index(self) -> Option<usize> {
        match self {
            Self::Band(index, _) => Some(index),
            _ => None,
        }
    }
}

// Strings are presentation-only widget IDs; bindings never parse them back.
impl std::fmt::Display for EqField {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PreampDb => output.write_str("preamp_db"),
            Self::OutputGainDb => output.write_str("output_gain_db"),
            Self::BandCount => output.write_str("band_count"),
            Self::Band(index, field) => write!(output, "band.{index}.{}", field.name()),
        }
    }
}

type EqFormat = Option<EqualizerFormat>;

struct EqAvailability {
    blocked: bool,
    pending: usize,
    format: EqFormat,
    collecting_revision: Option<u64>,
}

#[derive(Default)]
pub(super) struct EqualizerEditors {
    editors: HashMap<EqTarget, Entity<EqualizerEditor>>,
    active: Option<(u64, EqTarget)>,
    released_target: Option<EqTarget>,
    pointer_count: Rc<Cell<usize>>,
    removing: HashMap<EqTarget, (EqualizerConfig, usize)>,
    // Replacement intent follows serialized command start/ack, not refreshes.
    reset_band_focus: Option<EqTarget>,
    pending_node_focus_reset: Option<(u64, u64)>,
    reconciled: Option<EqualizerReconcileInputs>,
    loaded_preset: Option<EqualizerConfig>,
    loading_preset: Option<EqualizerConfig>,
}

// Publications also carry transport position. Retain only the last dependency
// source plus local queue/draft fences, not a cache of editor projections.
struct EqualizerReconcileInputs {
    config: Arc<rivu_format::config::ConfigSettings>,
    format: EqFormat,
    pending: usize,
    command_active: bool,
    graph_pending: bool,
    graph_drafts: bool,
    collecting: Option<(u64, u64)>,
    geometry_epoch: u64,
    reset_band_focus: Option<EqTarget>,
}

impl EqualizerReconcileInputs {
    fn unchanged_from(&self, previous: &Self) -> bool {
        self.format == previous.format
            && self.pending == previous.pending
            && self.command_active == previous.command_active
            && self.graph_pending == previous.graph_pending
            && self.graph_drafts == previous.graph_drafts
            && self.collecting == previous.collecting
            && self.geometry_epoch == previous.geometry_epoch
            && self.reset_band_focus == previous.reset_band_focus
            && (Arc::ptr_eq(&self.config, &previous.config)
                || (self.config.equalizer == previous.config.equalizer
                    && self.config.processing_graph.revision
                        == previous.config.processing_graph.revision
                    && self.config.ui_scale == previous.config.ui_scale))
    }
}

struct EqualizerField {
    input: Entity<Input>,
    // Semantic field identity survives native-widget retirement, but not band remapping.
    identity: EntityId,
    native_epoch: u64,
    committed: String,
    dirty: bool,
    generation: u64,
    draft_revision: Option<u64>,
    slider_bounds: MeasuredGraph,
    _subscription: Subscription,
}

pub(super) struct SubmittedField {
    editor: WeakEntity<EqualizerEditor>,
    key: EqField,
    identity: EntityId,
    generation: u64,
    text: String,
}

pub(super) struct NodeEqualizerSubmission {
    pub(super) config: EqualizerConfig,
    // While this explicit field submit is in flight, accept newer text but
    // no further graph commands until the prior revision is acknowledged.
    pub(super) field_submit: bool,
    pub(super) revision: u64,
    node: u64,
    fields: Vec<SubmittedField>,
}

struct NodeDrag {
    panel: u64,
    band: usize,
    members: [usize; MAX_BANDS],
    member_count: usize,
    press_position: Point<Pixels>,
    moved: bool,
    original_frequency: f64,
    original_gain: f64,
    bounds: Bounds<Pixels>,
    geometry: WindowGeometry,
    geometry_epoch: u64,
    frequency: f64,
    gain: f64,
    _subscriptions: [Subscription; 2],
}

struct FieldDrag {
    key: EqField,
    input: Entity<Input>,
    bounds: Bounds<Pixels>,
    geometry: WindowGeometry,
    geometry_epoch: u64,
    revision: u64,
    value: Option<f64>,
    _subscriptions: [Subscription; 2],
}

struct BandFocus {
    handle: FocusHandle,
    _subscriptions: [Subscription; 2],
}

struct EqualizerEditor {
    owner: WeakEntity<AudioProcessing>,
    context: FeatureContext,
    target: EqTarget,
    panel: u64,
    state: Rc<GuiSnapshot>,
    format: EqFormat,
    config: EqualizerConfig,
    revision: u64,
    blocked: bool,
    collecting_revision: Option<u64>,
    pending: usize,
    active: bool,
    response_dirty: bool,
    band: usize,
    input_band: usize,
    graph_epoch: u64,
    input_epoch: u64,
    native_epoch: u64,
    band_focus: [Option<BandFocus>; MAX_BANDS],
    response_channel: usize,
    fields: HashMap<EqField, EqualizerField>,
    channel_choices: rivu_ui::ChoiceState<usize>,
    channel_anchor: Rc<Cell<Option<Bounds<Pixels>>>>,
    response: Arc<Vec<f64>>,
    response_buffer: Arc<Vec<f64>>,
    /// Realized FIR magnitude on `frequencies`; empty for IIR targets.
    realized: Arc<Vec<f64>>,
    /// Whether `realized` belongs to the currently displayed target.
    realized_current: bool,
    realized_generation: u64,
    realized_task: Option<Task<()>>,
    preview_config: EqualizerConfig,
    response_error: Option<String>,
    error: Option<String>,
    maximum_frequency: f64,
    frequencies: [f64; 257],
    frequency_limit: f64,
    drag: Option<NodeDrag>,
    released_band: Option<usize>,
    drag_epoch: u64,
    response_frame_pending: bool,
    #[cfg(test)]
    response_computations: usize,
    field_drag: Option<FieldDrag>,
    pointer_count: Rc<Cell<usize>>,
    bounds: MeasuredGraph,
    removing_band: Option<(EqualizerConfig, usize)>,
    pending_added_band: Option<(usize, usize)>,
    geometry_epoch: u64,
}

impl EqualizerEditor {
    fn new(
        owner: WeakEntity<AudioProcessing>,
        target: EqTarget,
        panel: u64,
        parent: &AudioProcessing,
        config: &EqualizerConfig,
        cx: &mut Context<Self>,
    ) -> Self {
        let context = parent.context.clone();
        let state = Rc::clone(&parent.state);
        let config = config.clone();
        let revision = state.system.config.processing_graph.revision;
        let blocked = parent.equalizer_blocked(target);
        let collecting_revision = parent
            .collecting_equalizer_target()
            .filter(|(pending_target, _)| *pending_target == target)
            .map(|(_, revision)| revision);
        let pending = parent.pending;
        let geometry_epoch = parent.geometry_epoch;
        let format = state.equalizer_format();
        let mut editor = Self {
            owner,
            context,
            target,
            panel,
            state,
            format,
            config,
            revision,
            blocked,
            collecting_revision,
            pending,
            active: true,
            response_dirty: true,
            band: 0,
            input_band: 0,
            input_epoch: 0,
            native_epoch: 0,
            graph_epoch: 0,
            band_focus: std::array::from_fn(|_| None),
            response_channel: 0,
            fields: HashMap::new(),
            channel_choices: rivu_ui::ChoiceState::default(),
            channel_anchor: Rc::new(Cell::new(None)),
            response: Arc::new(Vec::new()),
            response_buffer: Arc::new(Vec::new()),
            realized: Arc::new(Vec::new()),
            realized_current: false,
            realized_generation: 0,
            realized_task: None,
            preview_config: EqualizerConfig::default(),
            response_error: None,
            error: None,
            maximum_frequency: 20_000.,
            drag: None,
            released_band: None,
            drag_epoch: 0,
            response_frame_pending: false,
            #[cfg(test)]
            response_computations: 0,
            field_drag: None,
            bounds: Rc::new(Cell::new(None)),
            frequencies: [0.; 257],
            frequency_limit: 0.,
            removing_band: None,
            pending_added_band: None,
            geometry_epoch,
            pointer_count: Rc::clone(&parent.equalizer_editors.pointer_count),
        };
        editor.sync_fields(cx);
        editor.update_response(cx);
        editor
    }

    fn tr(&self, id: &str) -> SharedString {
        self.context.tr(id)
    }

    fn reconcile(
        &mut self,
        state: Rc<GuiSnapshot>,
        config: &EqualizerConfig,
        availability: EqAvailability,
        geometry_epoch: u64,
        cx: &mut Context<Self>,
    ) {
        let EqAvailability {
            blocked,
            pending,
            format,
            collecting_revision,
        } = availability;
        let had_preview = self.drag.is_some() || self.field_drag.is_some();
        let format_changed = self.format != format;
        let dimensions = |format: EqualizerFormat| (format.sample_rate, format.channels);
        let taps_changed =
            target_fir_taps(&self.state, self.target) != target_fir_taps(&state, self.target);
        let response_format_changed = self.format.map(dimensions) != format.map(dimensions);
        let config_changed = &self.config != config;
        let revision = state.system.config.processing_graph.revision;
        let fence_changed = matches!(self.target, EqTarget::Node(_)) && self.revision != revision;
        let release = self
            .released_band
            .filter(|_| config_changed || format_changed || fence_changed);
        let preview_matches = release.is_some() && &self.preview_config == config;
        if config_changed
            || format_changed
            || self.revision != revision
            || self.geometry_epoch != geometry_epoch
            || self.blocked != blocked
        {
            self.input_epoch = self.input_epoch.wrapping_add(1);
            self.graph_epoch = self.graph_epoch.wrapping_add(1);
        }
        let geometry_changed = self.geometry_epoch != geometry_epoch;
        let retired_preview = had_preview
            && (config_changed
                || format_changed
                || self.revision != revision
                || geometry_changed
                || blocked
                || pending > 0);
        if retired_preview {
            self.retire_pointer(cx);
        }
        // Ordinary same-length parameter updates preserve native keyboard focus.
        if self.config.bands.len() != config.bands.len() {
            // Without persistent band IDs, duplicate rows cannot be disambiguated
            // after an arbitrary structural snapshot. Retire native identities.
            self.remap_band_fields(&config.bands, cx);
            self.retire_band_focus();
        }
        let changed = config_changed
            || retired_preview
            || format_changed
            || fence_changed
            || geometry_changed
            || self.blocked != blocked
            || self.collecting_revision != collecting_revision
            || !Arc::ptr_eq(&self.state.system.config, &state.system.config)
                && self.state.system.config.ui_scale != state.system.config.ui_scale
            || self.pending != pending
            || taps_changed;
        self.state = state;
        self.format = format;
        if config_changed {
            if let Some((start, count)) = self.pending_added_band.take()
                && start == self.config.bands.len()
                && config.bands.len() == start + count
                && config.bands.starts_with(&self.config.bands)
            {
                self.band = start;
            }
            self.config.clone_from(config);
        } else if pending == 0 {
            self.pending_added_band = None;
        }
        self.revision = revision;
        self.blocked = blocked;
        self.collecting_revision = collecting_revision;
        self.pending = pending;
        self.geometry_epoch = geometry_epoch;
        if !changed {
            return;
        }
        for (key, field) in &self.fields {
            field.input.update(cx, |input, cx| {
                input.set_enabled(
                    self.field_input_enabled(key)
                        && (key != &EqField::BandCount || !self.config_locked() && pending == 0),
                    cx,
                )
            });
        }
        if format_changed || geometry_changed || blocked {
            self.channel_choices.close();
        }
        if geometry_changed {
            self.bounds.set(None);
        }
        self.band = self.band.min(self.config.bands.len().saturating_sub(1));
        if config_changed {
            self.sync_fields(cx);
            self.removing_band = None;
        }
        let retired_release = release.is_some() && self.retire_node_preview(cx);
        if (config_changed || retired_release) && !preview_matches
            || response_format_changed
            || retired_preview
        {
            self.response_dirty = true;
            if self.active {
                self.update_response(cx);
            }
        } else if taps_changed && self.active {
            self.request_realized(cx);
        }
        cx.notify();
    }

    fn deactivate(&mut self, cx: &mut Context<Self>) {
        self.native_epoch = self.native_epoch.wrapping_add(1);
        self.active = false;
        // Dropping the task cancels publication; a hidden editor recomputes on reveal.
        self.realized_task = None;
        self.realized_current = false;
        self.response_dirty = true;
        self.bounds.set(None);
        self.channel_anchor.set(None);
        self.channel_choices.close();
        for field in self.fields.values() {
            field.slider_bounds.set(None);
            field
                .input
                .update(cx, |input, cx| input.set_enabled(false, cx));
        }
        self.retire_band_focus();
        self.retire_node_preview(cx);
        self.cancel_pointer(cx);
        self.prune_inputs();
    }

    fn retire_band_focus(&mut self) {
        self.input_epoch = self.input_epoch.wrapping_add(1);
        self.graph_epoch = self.graph_epoch.wrapping_add(1);
        self.band_focus.iter_mut().for_each(|focus| *focus = None);
        self.take_field_drag();
    }

    fn band_focus_handle(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> FocusHandle {
        if let Some(focus) = &self.band_focus[index] {
            return focus.handle.clone();
        }
        let handle = cx.focus_handle();
        let weak = handle.downgrade();
        let subscription = cx.on_focus(&handle, window, move |this, window, cx| {
            let Some(handle) = weak.upgrade() else {
                return;
            };
            // Resolve the live native identity, never a stale band index.
            let index = this
                .band_focus
                .iter()
                .position(|focus| focus.as_ref().is_some_and(|focus| focus.handle == handle));
            if this.active
                && handle.is_focused(window)
                && let Some(index) = index
            {
                if this.band != index {
                    this.cancel_pointer(cx);
                    this.band = index;
                    this.sync_fields(cx);
                }
                cx.notify();
            }
        });
        let blur = cx.on_blur(&handle, window, |_, _, cx| cx.notify());
        self.band_focus[index] = Some(BandFocus {
            handle: handle.clone(),
            _subscriptions: [subscription, blur],
        });
        handle
    }

    fn focus_graph_band(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.band = index;
        self.sync_fields(cx);
        self.band_focus_handle(index, window, cx).focus(window, cx);
    }

    fn cancel_pointer(&mut self, cx: &mut Context<Self>) {
        if self.retire_pointer(cx) {
            self.response_dirty = true;
            if self.active {
                self.update_response(cx);
            }
            cx.notify();
        }
    }

    fn retire_pointer(&mut self, cx: &mut Context<Self>) -> bool {
        let node = self.take_drag();
        let field = self.take_field_drag();
        if let Some(drag) = &node {
            self.restore_position_fields(drag.band, cx);
        }
        if let Some(drag) = &field {
            self.restore_preview_field(&drag.key, cx);
        }
        node.is_some() || field.is_some()
    }

    fn take_field_drag(&mut self) -> Option<FieldDrag> {
        let drag = self.field_drag.take();
        if drag.is_some() {
            self.drag_epoch = self.drag_epoch.wrapping_add(1);
            self.response_frame_pending = false;
            self.pointer_count
                .set(self.pointer_count.get().saturating_sub(1));
        }
        drag
    }

    fn take_drag(&mut self) -> Option<NodeDrag> {
        let drag = self.drag.take();
        if drag.is_some() {
            self.drag_epoch = self.drag_epoch.wrapping_add(1);
            self.response_frame_pending = false;
            self.pointer_count
                .set(self.pointer_count.get().saturating_sub(1));
        }
        drag
    }

    fn retire_node_preview(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(band) = self.released_band.take() else {
            return false;
        };
        self.restore_position_fields(band, cx);
        true
    }

    fn node_geometry_valid(&mut self, window: &Window, cx: &mut Context<Self>) -> bool {
        let snapshot = self.context.snapshot();
        let config = target_config(&snapshot, self.target);
        let format_valid = self.owner.upgrade().is_some()
            && snapshot.equalizer_format() == self.state.equalizer_format();
        let valid = self.active
            && !self.pointer_locked(cx)
            && self
                .band_numeric_range(&EqField::Band(0, BandField::FrequencyHz))
                .is_some()
            && format_valid
            && config == Some(&self.config)
            && snapshot.system.config.processing_graph.revision == self.revision
            && self.drag.as_ref().is_some_and(|drag| {
                drag.geometry == WindowGeometry::current(window, snapshot.system.config.ui_scale)
                    && drag.geometry_epoch == self.geometry_epoch
            });
        if !valid {
            self.cancel_pointer(cx);
        }
        valid
    }

    fn pointer_locked(&self, cx: &App) -> bool {
        self.config_locked()
            || self.pending > 0
            || self.owner.upgrade().is_some_and(|owner| {
                let owner = owner.read(cx);
                owner.pending > 0 || owner.equalizer_blocked(self.target)
            })
    }

    fn config_locked(&self) -> bool {
        self.blocked || !self.config.enabled
    }

    fn input_locked(&self) -> bool {
        !self.config.enabled || self.blocked && self.collecting_revision.is_none()
    }

    fn reset_locked(&self, cx: &App) -> bool {
        !self.config.enabled
            || self.pending > 0
            || matches!(self.target, EqTarget::Node(_))
                && self
                    .owner
                    .upgrade()
                    .is_none_or(|owner| owner.read(cx).graph_busy())
    }

    fn eq(&mut self, edit: EqualizerEdit, cx: &mut Context<Self>) {
        self.submit_edit(edit, None, cx);
    }

    fn submit_edit(
        &mut self,
        edit: EqualizerEdit,
        submitted: Option<SubmittedField>,
        cx: &mut Context<Self>,
    ) {
        if self.blocked || (!self.config.enabled && !matches!(edit, EqualizerEdit::Enabled(_))) {
            return;
        }
        let target = self.target;
        let revision = self.revision;
        let expected = &self.config;
        let _ = self.owner.update(cx, |owner, cx| {
            owner.submit_target_equalizer(
                (target, revision, expected, self.format),
                edit,
                submitted,
                cx,
            )
        });
    }

    fn band_edit(&mut self, index: usize, field: EqBandEdit, cx: &mut Context<Self>) {
        self.eq(EqualizerEdit::BandField { index, field }, cx);
        cx.notify();
    }

    fn new_field_input(
        &self,
        key: &EqField,
        label: &str,
        value: String,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> Entity<Input> {
        let placeholder = if matches!(key, EqField::Band(_, BandField::Channels)) {
            self.tr("processing-channels-placeholder")
        } else {
            label.to_owned().into()
        };
        let input = cx.new(|cx| Input::new(label.to_owned(), value, placeholder, cx));
        input.update(cx, |input, cx| {
            input.set_enabled(enabled, cx);
            let unit = match key {
                EqField::Band(_, BandField::FrequencyHz) => Some("Hz"),
                EqField::PreampDb | EqField::OutputGainDb | EqField::Band(_, BandField::GainDb) => {
                    Some("dB")
                }
                _ => None,
            };
            if let Some(unit) = unit {
                input.set_trailing_text(unit.into(), cx);
            }
        });
        input
    }

    fn field(&mut self, key: &EqField, label: &str, value: String, cx: &mut Context<Self>) {
        let enabled = self.field_input_enabled(key)
            && (key != &EqField::BandCount || !self.config_locked() && self.pending == 0);
        if self.active
            && key.band_index().is_none_or(|index| index == self.band)
            && self
                .fields
                .get(key)
                .is_some_and(|field| field.native_epoch != self.native_epoch)
        {
            let text = self.fields[key].input.read(cx).text().to_owned();
            let input = self.new_field_input(key, label, text, enabled, cx);
            let subscription = self.subscribe_field(&input, *key, cx);
            let field = self.fields.get_mut(key).expect("existing equalizer field");
            field.input = input;
            field.native_epoch = self.native_epoch;
            field._subscription = subscription;
        }
        if let Some(field) = self.fields.get_mut(key) {
            field
                .input
                .update(cx, |input, cx| input.set_enabled(enabled, cx));
            if field.committed != value {
                field.committed.clone_from(&value);
                if !field.dirty {
                    field
                        .input
                        .update(cx, |input, cx| input.set_text(value, cx));
                }
            }
            return;
        }
        if !self.active || key.band_index().is_some_and(|index| index != self.band) {
            return;
        }
        let input = self.new_field_input(key, label, value.clone(), enabled, cx);
        let subscription = self.subscribe_field(&input, *key, cx);
        self.fields.insert(
            *key,
            EqualizerField {
                identity: input.entity_id(),
                native_epoch: self.native_epoch,
                input,
                committed: value,
                dirty: false,
                generation: 0,
                draft_revision: None,
                slider_bounds: Rc::new(Cell::new(None)),
                _subscription: subscription,
            },
        );
    }

    fn subscribe_field(
        &self,
        input: &Entity<Input>,
        name: EqField,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe(input, move |this, input, event, cx| {
            if !this.active
                || !this.field_input_enabled(&name)
                || name.band_index().is_some_and(|index| index != this.band)
                || !this
                    .fields
                    .get(&name)
                    .is_some_and(|field| field.input.entity_id() == input.entity_id())
            {
                return;
            }
            match event {
                InputEvent::Changed { user: true, .. } => {
                    if let Some(field) = this.fields.get_mut(&name) {
                        if !field.dirty {
                            field.draft_revision = Some(this.revision);
                        }
                        field.dirty = true;
                        field.generation = field.generation.wrapping_add(1);
                    }
                    this.cancel_pointer(cx);
                    cx.notify();
                    let _ = this.owner.update(cx, |_, cx| cx.notify());
                }
                InputEvent::Submit => this.submit_field(&name, cx),
                _ => {}
            }
        })
    }

    fn band_numeric_range(&self, key: &EqField) -> Option<(f64, f64, bool)> {
        match key {
            EqField::Band(_, BandField::FrequencyHz) => {
                let maximum = self
                    .format
                    .and_then(EqualizerFormat::known_sample_rate)
                    .map_or(96_000., |sample_rate| {
                        (f64::from(sample_rate) * MAX_FREQUENCY_RATE_RATIO).min(96_000.)
                    });
                (maximum > 1.).then_some((1., maximum, true))
            }
            EqField::Band(_, BandField::GainDb) => Some((-24., 24., false)),
            EqField::Band(_, BandField::Q) => Some((0.1, 20., true)),
            EqField::Band(_, BandField::ShelfSlope) => Some((0.1, 1., false)),
            EqField::Band(_, BandField::Stages) => Some((1., 4., false)),
            _ => None,
        }
    }

    fn field_input_enabled(&self, key: &EqField) -> bool {
        self.active
            && !self.input_locked()
            && key.band_index().is_none_or(|index| index == self.band)
            && (!matches!(key, EqField::Band(_, BandField::FrequencyHz))
                || self.band_numeric_range(key).is_some())
    }

    fn field_control(&self, key: EqField) -> FieldControl {
        FieldControl {
            key,
            input: self.fields[&key].input.clone(),
            epoch: self.input_epoch,
            revision: self.revision,
            format: self.format,
            geometry_epoch: self.geometry_epoch,
        }
    }

    fn field_control_current(&self, control: &FieldControl, cx: &App) -> bool {
        self.active
            && self.field_input_enabled(&control.key)
            && control.epoch == self.input_epoch
            && control.revision == self.revision
            && control.format == self.format
            && control.geometry_epoch == self.geometry_epoch
            && control
                .key
                .band_index()
                .is_none_or(|index| index == self.band)
            && self
                .fields
                .get(&control.key)
                .is_some_and(|field| field.input == control.input)
            && self.controls_current(cx)
    }

    fn field_control_valid(&self, control: &FieldControl, window: &Window, cx: &App) -> bool {
        self.field_control_current(control, cx)
            && self.fields.get(&control.key).is_some_and(|field| {
                field.slider_bounds.get().is_none_or(|(_, geometry)| {
                    geometry
                        == WindowGeometry::current(
                            window,
                            self.context.snapshot().system.config.ui_scale,
                        )
                })
            })
    }

    fn controls_current(&self, cx: &App) -> bool {
        self.owner.upgrade().is_none_or(|owner| {
            let snapshot = self.context.snapshot();
            let owner = owner.read(cx);
            snapshot.system.config.processing_graph.revision == self.revision
                && target_config(&snapshot, self.target) == Some(&self.config)
                && snapshot.equalizer_format() == self.state.equalizer_format()
                && (!owner.equalizer_blocked(self.target)
                    || owner.collecting_equalizer_target() == Some((self.target, self.revision)))
        })
    }

    fn remap_band_fields(&mut self, next: &[EqBand], cx: &mut Context<Self>) {
        let previous = &self.config.bands;
        let mut mapping = [None; MAX_BANDS];
        let removed = self
            .removing_band
            .as_ref()
            .filter(|(state, index)| {
                state.bands == *previous
                    && *index < previous.len()
                    && previous.len() == next.len() + 1
                    && previous
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| i != index)
                        .map(|(_, band)| band)
                        .eq(next.iter())
            })
            .map(|(_, index)| *index);
        if let Some(removed) = removed {
            for (index, target) in mapping.iter_mut().enumerate().take(previous.len()) {
                if index != removed {
                    *target = Some(if index < removed { index } else { index - 1 });
                }
            }
        } else {
            // Persistent bands have no IDs. Match unchanged bands in order;
            // appending retains every old band, while removed/replaced rows retire.
            let mut lengths = [[0u8; MAX_BANDS + 1]; MAX_BANDS + 1];
            for old in (0..previous.len()).rev() {
                for new in (0..next.len()).rev() {
                    lengths[old][new] = if previous[old] == next[new] {
                        1 + lengths[old + 1][new + 1]
                    } else {
                        lengths[old + 1][new].max(lengths[old][new + 1])
                    };
                }
            }
            let (mut old, mut new) = (0, 0);
            while old < previous.len() && new < next.len() {
                if previous[old] == next[new] {
                    mapping[old] = Some(new);
                    old += 1;
                    new += 1;
                } else if lengths[old + 1][new] >= lengths[old][new + 1] {
                    old += 1;
                } else {
                    new += 1;
                }
            }
        }
        self.band = mapping
            .get(self.band)
            .copied()
            .flatten()
            .unwrap_or_else(|| self.band.min(next.len().saturating_sub(1)));
        for (key, mut field) in std::mem::take(&mut self.fields) {
            let EqField::Band(index, name) = key else {
                self.fields.insert(key, field);
                continue;
            };
            let Some(new) = mapping.get(index).copied().flatten() else {
                continue;
            };
            let new_key = EqField::Band(new, name);
            if new_key != key {
                let label = self.tr(match name {
                    BandField::FrequencyHz => "processing-frequency",
                    BandField::GainDb => "processing-gain",
                    BandField::Q => "processing-q",
                    BandField::ShelfSlope => "processing-shelf-slope",
                    BandField::Stages => "processing-stages",
                    BandField::Channels => "processing-channels",
                });
                let text = field.input.read(cx).text().to_owned();
                field.input = self.new_field_input(&new_key, &label, text, true, cx);
                field.identity = field.input.entity_id();
                field.native_epoch = self.native_epoch;
                field.slider_bounds = Rc::new(Cell::new(None));
                field._subscription = self.subscribe_field(&field.input, new_key, cx);
            }
            self.fields.insert(new_key, field);
        }
    }
    fn sync_fields(&mut self, cx: &mut Context<Self>) {
        if self.input_band != self.band {
            self.input_band = self.band;
            self.input_epoch = self.input_epoch.wrapping_add(1);
            self.cancel_pointer(cx);
        }
        let state = Rc::clone(&self.state);
        let Some(eq) = target_config(&state, self.target) else {
            return;
        };
        self.field(
            &EqField::PreampDb,
            &self.tr("processing-preamp"),
            equalizer_number_text(&EqField::PreampDb, eq.preamp_db),
            cx,
        );
        self.field(
            &EqField::OutputGainDb,
            &self.tr("processing-output-gain"),
            equalizer_number_text(&EqField::OutputGainDb, eq.output_gain_db),
            cx,
        );
        self.field(
            &EqField::BandCount,
            &self.tr("processing-band-count-label"),
            eq.bands.len().to_string(),
            cx,
        );
        for (i, b) in eq.bands.iter().enumerate() {
            for (name, label) in [
                (BandField::FrequencyHz, "processing-frequency"),
                (BandField::GainDb, "processing-gain"),
                (BandField::Q, "processing-q"),
                (BandField::ShelfSlope, "processing-shelf-slope"),
                (BandField::Stages, "processing-stages"),
                (BandField::Channels, "processing-channels"),
            ] {
                let key = EqField::Band(i, name);
                if !(self.active && i == self.band)
                    && !self.fields.get(&key).is_some_and(|field| field.dirty)
                {
                    continue;
                }
                let value = match name {
                    BandField::FrequencyHz => equalizer_number_text(&key, b.frequency_hz),
                    BandField::GainDb => equalizer_number_text(&key, b.gain_db),
                    BandField::Q => equalizer_number_text(&key, b.q),
                    BandField::ShelfSlope => equalizer_number_text(&key, b.shelf_slope),
                    BandField::Stages => b.stages.to_string(),
                    BandField::Channels => match &b.channels {
                        ChannelTarget::All => "all".to_owned(),
                        ChannelTarget::Selected(channels) => channels
                            .iter()
                            .map(|c| (c + 1).to_string())
                            .collect::<Vec<_>>()
                            .join(","),
                    },
                };
                self.field(&key, &self.tr(label), value, cx);
            }
        }
        self.prune_inputs();
    }

    fn prune_inputs(&mut self) {
        self.fields.retain(|key, field| {
            let index = key.band_index();
            index.is_none_or(|index| index < self.config.bands.len())
                && (field.dirty || self.active && index.is_none_or(|index| index == self.band))
        });
    }

    fn submit_field(&mut self, key: &EqField, cx: &mut Context<Self>) {
        let Some(field) = self.fields.get(key) else {
            return;
        };
        let input = field.input.read(cx);
        if self.config_locked()
            || key == &EqField::BandCount && self.pending > 0
            || !field.dirty
            || input.is_composing()
        {
            return;
        }
        if matches!(self.target, EqTarget::Node(_)) && field.draft_revision != Some(self.revision) {
            self.error = Some(self.context.i18n.text("processing-editor-stale").to_owned());
            cx.notify();
            return;
        }
        self.error = None;
        let value = input.text().trim().to_owned();
        match self.field_edit(key, &value) {
            Ok(edit) => {
                let submitted = SubmittedField {
                    editor: cx.weak_entity(),
                    key: *key,
                    identity: field.identity,
                    generation: field.generation,
                    text: value,
                };
                self.cancel_pointer(cx);
                if let EqualizerEdit::ResizeBands { count } = &edit {
                    self.pending_added_band = (*count > self.config.bands.len()).then_some((
                        self.config.bands.len(),
                        count.saturating_sub(self.config.bands.len()),
                    ));
                }
                self.submit_edit(edit, Some(submitted), cx);
            }
            Err(error) => {
                self.error = Some(error);
                cx.notify();
            }
        }
    }

    fn field_edit(&self, key: &EqField, value: &str) -> Result<EqualizerEdit, String> {
        let number = || {
            value
                .parse::<f64>()
                .ok()
                .filter(|n| {
                    n.is_finite()
                        && self
                            .band_numeric_range(key)
                            .is_none_or(|(minimum, maximum, _)| (minimum..=maximum).contains(n))
                })
                .ok_or_else(|| {
                    self.context
                        .i18n
                        .text("processing-invalid-number")
                        .to_owned()
                })
        };
        let edit = match *key {
            EqField::BandCount => {
                let count = value
                    .parse::<usize>()
                    .ok()
                    .filter(|count| *count <= MAX_BANDS)
                    .ok_or_else(|| self.tr("processing-band-count-invalid").to_string())?;
                // Automatic placement has a 20 Hz floor in the protocol. A
                // known unusable stream is not the offline/unknown fallback.
                if count > self.config.bands.len()
                    && self
                        .format
                        .and_then(EqualizerFormat::known_sample_rate)
                        .is_some_and(|sample_rate| {
                            f64::from(sample_rate) * MAX_FREQUENCY_RATE_RATIO < 20.
                        })
                {
                    return Err(self.tr("processing-response-unknown").to_string());
                }
                EqualizerEdit::ResizeBands { count }
            }
            EqField::PreampDb | EqField::OutputGainDb => {
                let gain = number()?;
                if !(-24.0..=24.0).contains(&gain) {
                    return Err(self.tr("processing-invalid-number").to_string());
                }
                if *key == EqField::PreampDb {
                    EqualizerEdit::PreampDb(gain)
                } else {
                    EqualizerEdit::OutputGainDb(gain)
                }
            }
            EqField::Band(index, name) => {
                let field = match name {
                    BandField::Stages => {
                        let stages = value.parse::<u8>().map_err(|error| error.to_string())?;
                        if !(1..=4).contains(&stages) {
                            return Err(self.tr("processing-invalid-number").to_string());
                        }
                        EqBandEdit::Stages(stages)
                    }
                    BandField::Channels => {
                        EqBandEdit::Channels(if value.eq_ignore_ascii_case("all") {
                            ChannelTarget::All
                        } else {
                            let channels = value
                                .split(',')
                                .map(|channel| {
                                    channel
                                        .trim()
                                        .parse::<usize>()
                                        .ok()
                                        .and_then(|number| number.checked_sub(1))
                                        .ok_or_else(|| {
                                            self.context
                                                .i18n
                                                .text("processing-channel-help")
                                                .to_owned()
                                        })
                                })
                                .collect::<Result<Vec<_>, _>>()?;
                            ChannelTarget::Selected(channels)
                        })
                    }
                    _ => name.numeric_edit(number()?),
                };
                EqualizerEdit::BandField { index, field }
            }
        };
        Ok(edit)
    }
    fn step_field(&mut self, key: &EqField, step: f64, cx: &mut Context<Self>) {
        if self.pointer_locked(cx) || !self.field_input_enabled(key) {
            return;
        }
        self.cancel_pointer(cx);
        let value = self
            .fields
            .get(key)
            .and_then(|field| field.input.read(cx).text().trim().parse::<f64>().ok());
        match value {
            Some(value) if value.is_finite() => {
                let next = self
                    .band_numeric_range(key)
                    .map_or(value + step, |(min, max, _)| (value + step).clamp(min, max));
                self.submit_numeric_step(key, next, cx);
            }
            _ => self.error = Some(self.tr("processing-invalid-number").to_string()),
        }
        cx.notify();
    }

    fn submit_numeric_step(&mut self, key: &EqField, value: f64, cx: &mut Context<Self>) {
        let text = self.preview_number_text(key, value);
        let Some(field) = self.fields.get_mut(key) else {
            return;
        };
        if field.input.read(cx).is_composing() || field.input.read(cx).text() == text {
            return;
        }
        field.dirty = true;
        field.draft_revision = Some(self.revision);
        field.generation = field.generation.wrapping_add(1);
        field.input.update(cx, |input, cx| input.set_text(text, cx));
        self.submit_field(key, cx);
        cx.notify();
        let _ = self.owner.update(cx, |_, cx| cx.notify());
    }

    fn slider_geometry_valid(&mut self, window: &Window, cx: &mut Context<Self>) -> bool {
        let Some(drag) = &self.field_drag else {
            return false;
        };
        let valid = !self.pointer_locked(cx)
            && self.field_input_enabled(&drag.key)
            && self.controls_current(cx)
            && drag.geometry_epoch == self.geometry_epoch
            && drag.revision == self.revision
            && drag.geometry == WindowGeometry::current(window, self.state.system.config.ui_scale)
            && self.fields.get(&drag.key).is_some_and(|field| {
                field.input == drag.input
                    && field.slider_bounds.get() == Some((drag.bounds, drag.geometry))
            });
        if !valid {
            self.cancel_pointer(cx);
        }
        valid
    }

    fn move_field_slider(
        &mut self,
        position: Point<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if !self.slider_geometry_valid(window, cx) {
            return;
        }
        let drag = self.field_drag.as_ref().unwrap();
        let Some(fraction) = Slider::value_at(drag.bounds, position) else {
            self.cancel_pointer(cx);
            return;
        };
        let key = drag.key;
        let Some(range) = self.band_numeric_range(&key) else {
            self.cancel_pointer(cx);
            return;
        };
        let value = slider_number(
            fraction,
            range,
            matches!(key, EqField::Band(_, BandField::Stages)),
        );
        let drag = self.field_drag.as_mut().unwrap();
        if drag.value != Some(value) {
            drag.value = Some(value);
            self.response_dirty = true;
        }
    }

    fn copy_preview_config(&mut self) {
        self.preview_config.enabled = self.config.enabled;
        self.preview_config.preamp_db = self.config.preamp_db;
        self.preview_config.output_gain_db = self.config.output_gain_db;
        self.preview_config.bands.clone_from(&self.config.bands);
    }

    fn finish_field_slider(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.take_field_drag() else {
            return;
        };
        self.restore_preview_field(&drag.key, cx);
        self.update_response(cx);
        if let (EqField::Band(index, field), Some(value)) = (drag.key, drag.value) {
            let edit = EqualizerEdit::BandField {
                index,
                field: field.numeric_edit(value),
            };
            if !equalizer_field_matches_config(&edit, &self.config) {
                self.eq(edit, cx);
            }
        }
        cx.notify();
    }

    fn slider_action(
        identity: FieldControl,
        bounds: MeasuredGraph,
    ) -> impl Fn(&mut Self, SliderInput, &mut Window, &mut Context<Self>) {
        move |this, input, window, cx| {
            if this.pointer_locked(cx) || !this.field_control_valid(&identity, window, cx) {
                return;
            }
            this.cancel_pointer(cx);
            let name = identity.key;
            match input {
                SliderInput::PointerDown { position, .. } => {
                    let Some((bounds, geometry)) = bounds.get() else {
                        return;
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
                    this.copy_preview_config();
                    this.field_drag = Some(FieldDrag {
                        key: name,
                        input: identity.input.clone(),
                        bounds,
                        geometry,
                        geometry_epoch: this.geometry_epoch,
                        revision: this.revision,
                        value: None,
                        _subscriptions: subscriptions,
                    });
                    this.pointer_count.set(this.pointer_count.get() + 1);
                    this.move_field_slider(position, window, cx);
                    this.schedule_drag_response(window, cx);
                }
                SliderInput::Key(key) => {
                    let Some(range) = this.band_numeric_range(&name) else {
                        return;
                    };
                    let number = identity
                        .input
                        .read(cx)
                        .text()
                        .trim()
                        .parse::<f64>()
                        .unwrap_or(range.0);
                    let fraction = slider_fraction(number, range);
                    let value = match key {
                        SliderKey::Start => range.0,
                        SliderKey::End => range.1,
                        SliderKey::Decrease | SliderKey::Increase => {
                            let direction = if key == SliderKey::Decrease { -1. } else { 1. };
                            if matches!(name, EqField::Band(_, BandField::Stages)) {
                                (number + direction).clamp(range.0, range.1)
                            } else {
                                slider_number(fraction + direction as f32 * 0.01, range, false)
                            }
                        }
                    };
                    this.submit_numeric_step(&name, value, cx);
                }
            }
        }
    }

    fn step_action(
        identity: FieldControl,
        delta: f64,
    ) -> impl Fn(&mut Self, &mut Window, &mut Context<Self>) {
        move |this, window, cx| {
            if this.field_control_valid(&identity, window, cx) {
                this.step_field(&identity.key, delta, cx);
            }
        }
    }

    fn band_slider(
        &self,
        key: &EqField,
        label: SharedString,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let field = &self.fields[key];
        let range = self.band_numeric_range(key).unwrap();
        let number = self.pointer_preview_value(key).unwrap_or_else(|| {
            field
                .input
                .read(cx)
                .text()
                .trim()
                .parse::<f64>()
                .unwrap_or(range.0)
        });
        let fraction = slider_fraction(number, range);
        let bounds = field.slider_bounds.clone();
        let measured = bounds.clone();
        let identity = self.field_control(*key);
        let ui_scale = self.state.system.config.ui_scale;
        Slider::new(
            SharedString::from(format!("eq-slider-{key}")),
            fraction,
            label,
        )
        .disabled(!enabled)
        .dragging(
            self.field_drag
                .as_ref()
                .is_some_and(|drag| drag.key == *key),
        )
        .render(cx, Self::slider_action(identity, bounds))
        .flex_1()
        .relative()
        .debug_selector({
            let name = *key;
            move || format!("eq-slider-{name}")
        })
        .child(
            canvas(
                move |bounds, window, _| {
                    measured.set(Some((bounds, WindowGeometry::current(window, ui_scale))));
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
    }

    fn pointer_preview_value(&self, key: &EqField) -> Option<f64> {
        if let Some(drag) = &self.field_drag
            && drag.key == *key
        {
            return drag.value;
        }
        let position = self
            .drag
            .as_ref()
            .map(|drag| (drag.band, drag.frequency, drag.gain))
            .or_else(|| {
                let index = self.released_band?;
                let band = &self.preview_config.bands[index];
                Some((index, band.frequency_hz, band.gain_db))
            });
        match (key, position) {
            (EqField::Band(index, BandField::FrequencyHz), Some((band, hz, _)))
                if *index == band =>
            {
                Some(hz)
            }
            (EqField::Band(index, BandField::GainDb), Some((band, _, db))) if *index == band => {
                Some(db)
            }
            _ => None,
        }
    }

    fn preview_position(&self, index: usize, band: &EqBand) -> (f64, f64) {
        (
            self.pointer_preview_value(&EqField::Band(index, BandField::FrequencyHz))
                .unwrap_or(band.frequency_hz),
            self.pointer_preview_value(&EqField::Band(index, BandField::GainDb))
                .unwrap_or(band.gain_db),
        )
    }

    fn preview_number_text(&self, key: &EqField, value: f64) -> String {
        let text = equalizer_number_text(key, value);
        if self.band_numeric_range(key).is_some_and(|(min, max, _)| {
            text.parse::<f64>()
                .is_ok_and(|rounded| !(min..=max).contains(&rounded))
        }) {
            value.to_string()
        } else {
            text
        }
    }

    fn update_preview_field(&self, key: &EqField, value: f64, cx: &mut Context<Self>) {
        if let Some(field) = self.fields.get(key).filter(|field| !field.dirty) {
            let text = self.preview_number_text(key, value);
            if field.input.read(cx).text() != text {
                field.input.update(cx, |input, cx| input.set_text(text, cx));
            }
        }
    }

    fn restore_preview_field(&self, key: &EqField, cx: &mut Context<Self>) {
        if let Some(field) = self.fields.get(key).filter(|field| !field.dirty)
            && field.input.read(cx).text() != field.committed
        {
            field
                .input
                .update(cx, |input, cx| input.set_text(field.committed.clone(), cx));
        }
    }

    fn restore_position_fields(&self, band: usize, cx: &mut Context<Self>) {
        for name in [BandField::FrequencyHz, BandField::GainDb] {
            self.restore_preview_field(&EqField::Band(band, name), cx);
        }
    }

    fn field_view(
        &self,
        key: &EqField,
        label: impl Into<SharedString>,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let field = &self.fields[key];
        let label: SharedString = label.into();
        let compact = matches!(key, EqField::PreampDb | EqField::OutputGainDb);
        let enabled = enabled && self.field_input_enabled(key);
        let step = match key {
            EqField::Band(_, BandField::FrequencyHz | BandField::Stages) => Some(1.),
            EqField::Band(_, BandField::GainDb) => Some(0.5),
            EqField::Band(_, BandField::Q | BandField::ShelfSlope) => Some(0.1),
            _ => None,
        };
        let help = match key {
            EqField::PreampDb => Some("processing-preamp-input-help"),
            EqField::OutputGainDb => Some("processing-output-gain-input-help"),
            EqField::Band(_, BandField::FrequencyHz) => Some("processing-frequency-input-help"),
            EqField::Band(_, BandField::GainDb) => Some("processing-gain-input-help"),
            EqField::Band(_, BandField::ShelfSlope) => Some("processing-shelf-slope-input-help"),
            EqField::Band(_, BandField::Channels) => Some("processing-channel-help"),
            EqField::Band(_, BandField::Stages) => Some("processing-stage-help"),
            _ => None,
        };
        let mut numeric = rivu_ui::NumericInput::new(
            div()
                .min_w_0()
                .w(numeric_parameter_width(cx))
                .flex_shrink_0()
                .id(SharedString::from(format!("eq-input-help-{key}")))
                .when_some(help, |view, help| {
                    view.tooltip(rivu_ui::text_tooltip(self.tr(help)))
                })
                .child(field.input.clone()),
        );
        let mut control = row()
            .gap_1()
            .when(compact, |view| view.flex_shrink_0())
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(theme.secondary))
                    .when(!compact, |view| view.w(rems(7.)).min_w_0())
                    .when(compact, |view| view.flex_shrink_0())
                    .truncate()
                    .child(rivu_ui::truncated_label(
                        SharedString::from(format!("eq-label-{key}")),
                        label.clone(),
                    )),
            )
            .child(
                div()
                    .w(rems(0.4))
                    .flex_shrink_0()
                    .text_color(rgb(theme.accent))
                    .id(SharedString::from(format!("eq-draft-{key}")))
                    .tooltip(rivu_ui::text_tooltip(self.tr("processing-draft")))
                    .when(field.dirty, |view| view.child("·")),
            );
        if let Some(step) = step {
            for (suffix, glyph, hint, delta) in [
                ("less", "−", "processing-field-decrease", -step),
                ("more", "+", "processing-field-increase", step),
            ] {
                let key_owned = *key;
                let identity = self.field_control(key_owned);
                numeric = numeric.step(
                    rivu_ui::IconButton::new(
                        SharedString::from(format!("eq-step-{key}-{suffix}")),
                        glyph,
                        self.tr(hint),
                    )
                    .surface(crate::gui::appearance::material::surface(cx))
                    .enabled(enabled)
                    .build(
                        rivu_ui::IconButtonStyle::Quiet,
                        rivu_ui::IconButtonDensity::Compact,
                        cx,
                        Self::step_action(identity, delta),
                    )
                    .debug_selector(move || format!("eq-step-{key_owned}-{suffix}")),
                );
            }
        }
        control = control.child(numeric.into_div().flex_shrink_0());
        if !compact && matches!(key, EqField::Band(_, BandField::Channels)) {
            let key_owned = *key;
            let identity = self.field_control(key_owned);
            control = control.child(
                rivu_ui::IconButton::new(
                    SharedString::from(format!("apply-{key}")),
                    "",
                    self.tr("processing-apply"),
                )
                .surface(crate::gui::appearance::material::surface(cx))
                .enabled(enabled)
                .build(
                    rivu_ui::IconButtonStyle::Quiet,
                    rivu_ui::IconButtonDensity::Compact,
                    cx,
                    move |this, window, cx| {
                        if this.field_control_valid(&identity, window, cx) {
                            this.submit_field(&key_owned, cx);
                        }
                    },
                ),
            );
        }
        if self.band_numeric_range(key).is_some() {
            control = control.child(self.band_slider(key, label, enabled, cx));
        }
        control.opacity(if enabled { 1. } else { 0.5 })
    }
    // Numeric controls and curve nodes share the same committed target. A graph
    // drag previews its position locally; the existing release path commits it.
    fn update_response(&mut self, cx: &mut Context<Self>) {
        self.response_dirty = false;
        // The previous scene can still hold the displayed buffer. Reuse the spare
        // once released; never copy old samples just to overwrite all of them.
        if Arc::get_mut(&mut self.response_buffer).is_none() {
            self.response_buffer = Arc::new(Vec::with_capacity(self.frequencies.len()));
        }
        let Some((sample_rate, channel_count)) = self.format.and_then(|format| {
            format
                .known_sample_rate()
                .map(|sample_rate| (sample_rate, format.channels))
        }) else {
            self.maximum_frequency = 20_000.;
            Arc::get_mut(&mut self.response_buffer)
                .expect("unique response buffer")
                .clear();
            self.response_error = None;
            std::mem::swap(&mut self.response, &mut self.response_buffer);
            self.request_realized(cx);
            return;
        };
        let Some((_, maximum_frequency, _)) = self
            .band_numeric_range(&EqField::Band(0, BandField::FrequencyHz))
            .filter(|_| channel_count != 0)
        else {
            Arc::get_mut(&mut self.response_buffer)
                .expect("unique response buffer")
                .clear();
            self.response_error = Some(
                self.context
                    .i18n
                    .text("processing-response-unknown")
                    .to_owned(),
            );
            std::mem::swap(&mut self.response, &mut self.response_buffer);
            self.request_realized(cx);
            return;
        };
        self.maximum_frequency = maximum_frequency;
        self.response_channel = self.response_channel.min(channel_count - 1);
        if self.frequency_limit != self.maximum_frequency {
            self.frequency_limit = self.maximum_frequency;
            for (index, frequency) in self.frequencies.iter_mut().enumerate() {
                let minimum = graph_minimum_frequency(self.maximum_frequency);
                *frequency = minimum * (self.maximum_frequency / minimum).powf(index as f64 / 256.);
            }
        }
        if let Some(drag) = &self.drag {
            if let Some(band) = self.preview_config.bands.get_mut(drag.band) {
                band.frequency_hz = drag.frequency;
                band.gain_db = drag.gain;
            }
        } else if self.released_band.is_none()
            && let Some(FieldDrag {
                key: EqField::Band(index, field),
                value: Some(value),
                ..
            }) = &self.field_drag
        {
            let _ = EqualizerEdit::BandField {
                index: *index,
                field: field.numeric_edit(*value),
            }
            .apply_config(&mut self.preview_config, Some(sample_rate))
            .expect("slider preview band still exists");
        }
        let config = if self.uses_response_preview() {
            &self.preview_config
        } else {
            &self.config
        };
        let response = Arc::get_mut(&mut self.response_buffer).expect("unique response buffer");
        response.resize(self.frequencies.len(), 0.);
        #[cfg(test)]
        {
            self.response_computations += 1;
        }
        match config.sample_response_db(
            sample_rate,
            channel_count,
            self.response_channel,
            &self.frequencies,
            response,
        ) {
            Ok(()) => self.response_error = None,
            Err(error) => {
                response.clear();
                self.response_error = Some(error.to_string());
            }
        }
        std::mem::swap(&mut self.response, &mut self.response_buffer);
        self.request_realized(cx);
    }

    /// Graph FIR nodes plot the finite kernel next to its parametric target.
    /// Kernel design costs up to tens of milliseconds, so it runs off the UI
    /// thread: one job at a time, and only the latest request publishes. A
    /// superseded result keeps the previous curve but hides deviation marks.
    fn request_realized(&mut self, cx: &mut Context<Self>) {
        self.realized_generation = self.realized_generation.wrapping_add(1);
        self.realized_current = false;
        let Some(request) = self.realized_inputs() else {
            self.realized_task = None;
            if !self.realized.is_empty() {
                self.realized = Arc::new(Vec::new());
            }
            return;
        };
        if self.realized_task.is_some() {
            // The running job notices the newer generation and restarts.
            return;
        }
        let (config, sample_rate, channels, channel, taps) = request;
        let config = config.clone();
        let generation = self.realized_generation;
        let frequencies = self.frequencies;
        let job = cx.background_executor().spawn(async move {
            let mut realized = vec![0.; frequencies.len()];
            rivu_dsp::equalizer::fir::FirEqualizer::sample_response_db(
                &config,
                sample_rate,
                channels,
                channel,
                taps,
                &frequencies,
                &mut realized,
            )
            .map(|()| realized)
        });
        self.realized_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |this, cx| {
                this.realized_task = None;
                if this.realized_generation != generation {
                    this.request_realized(cx);
                    return;
                }
                // A failed design hides only the realized curve, never the target.
                this.realized = Arc::new(result.unwrap_or_default());
                this.realized_current = true;
                cx.notify();
            });
        }));
    }

    fn uses_response_preview(&self) -> bool {
        self.drag.is_some()
            // A released node keeps its final response until the snapshot takes over.
            || self.released_band.is_some()
            || matches!(
                self.field_drag,
                Some(FieldDrag {
                    key: EqField::Band(..),
                    value: Some(_),
                    ..
                })
            )
    }

    fn realized_inputs(&self) -> Option<(&EqualizerConfig, u32, usize, usize, usize)> {
        let taps = target_fir_taps(&self.state, self.target)?;
        if !self.active || self.response_error.is_some() || self.response.is_empty() {
            return None;
        }
        let format = self.format?;
        let config = if self.uses_response_preview() {
            &self.preview_config
        } else {
            &self.config
        };
        Some((
            config,
            format.known_sample_rate()?,
            format.channels,
            self.response_channel,
            taps,
        ))
    }

    fn schedule_drag_response(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.response_frame_pending || !self.response_dirty {
            return;
        }
        self.response_frame_pending = true;
        let epoch = self.drag_epoch;
        let entity = cx.weak_entity();
        window.on_next_frame(move |window, cx| {
            let _ = entity.update(cx, |this, cx| {
                // Ending, cancelling, hiding or replacing a gesture retires its
                // callback even if another drag starts before this frame arrives.
                if this.drag_epoch != epoch || !this.response_frame_pending {
                    return;
                }
                this.response_frame_pending = false;
                let valid = if this.field_drag.is_some() {
                    this.slider_geometry_valid(window, cx)
                } else {
                    this.node_geometry_valid(window, cx)
                };
                if valid {
                    if let Some(drag) = &this.field_drag
                        && let Some(value) = drag.value
                    {
                        this.update_preview_field(&drag.key, value, cx);
                    } else if let Some(drag) = &this.drag {
                        for (field, value) in [
                            (BandField::FrequencyHz, drag.frequency),
                            (BandField::GainDb, drag.gain),
                        ] {
                            this.update_preview_field(&EqField::Band(drag.band, field), value, cx);
                        }
                    }
                    if this.response_dirty {
                        this.update_response(cx);
                    }
                    cx.notify();
                }
            });
        });
    }

    fn frequency_x(&self, frequency: f64) -> f32 {
        let minimum = graph_minimum_frequency(self.maximum_frequency);
        slider_fraction(frequency, (minimum, self.maximum_frequency, true))
    }

    fn graph_has_focus(&self, window: &Window) -> bool {
        self.band_focus
            .iter()
            .flatten()
            .any(|focus| focus.handle.is_focused(window))
    }

    fn register_pointer_handlers(
        weak: WeakEntity<Self>,
        panel: u64,
        window: &mut Window,
        _cx: &mut App,
    ) {
        let entity = weak.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase == DispatchPhase::Capture {
                let _ = entity.update(cx, |this, cx| {
                    if this.field_drag.is_some() {
                        if !event.dragging() {
                            this.cancel_pointer(cx);
                        } else {
                            this.move_field_slider(event.position, window, cx);
                            this.schedule_drag_response(window, cx);
                        }
                        window.prevent_default();
                    }
                    if this.drag.as_ref().is_some_and(|d| d.panel == panel) {
                        if !event.dragging() {
                            this.cancel_pointer(cx);
                        } else if this.node_geometry_valid(window, cx) {
                            this.move_node(event.position);
                            this.schedule_drag_response(window, cx);
                        }
                        window.prevent_default();
                    }
                });
            }
        });
        let entity = weak.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
            if phase == DispatchPhase::Capture && event.button == MouseButton::Left {
                let _ = entity.update(cx, |this, cx| {
                    if this.field_drag.is_some() {
                        // Sample the release itself; the last scheduled preview
                        // frame may not have painted yet.
                        this.move_field_slider(event.position, window, cx);
                        this.finish_field_slider(cx);
                    }
                    if this.drag.as_ref().is_some_and(|d| d.panel == panel)
                        && this.node_geometry_valid(window, cx)
                    {
                        this.move_node(event.position);
                        if let Some(drag) = this.take_drag() {
                            if drag.moved {
                                this.release_node_position(drag, cx);
                            } else if drag.member_count > 1 {
                                let current = drag.members[..drag.member_count]
                                    .iter()
                                    .position(|index| *index == drag.band)
                                    .unwrap_or(0);
                                let index = drag.members[(current + 1) % drag.member_count];
                                this.focus_graph_band(index, window, cx);
                            }
                            cx.notify();
                        }
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

    fn release_node_position(&mut self, drag: NodeDrag, cx: &mut Context<Self>) {
        let band = &self.config.bands[drag.band];
        let changed = band.frequency_hz != drag.frequency || band.gain_db != drag.gain;
        let admitted = changed
            && self
                .owner
                .update(cx, |owner, owner_cx| {
                    owner.submit_target_equalizer(
                        (self.target, self.revision, &self.config, self.format),
                        EqualizerEdit::BandField {
                            index: drag.band,
                            field: EqBandEdit::Position {
                                frequency_hz: drag.frequency,
                                gain_db: drag.gain,
                            },
                        },
                        None,
                        owner_cx,
                    )
                })
                .unwrap_or(false);
        if admitted {
            self.released_band = Some(drag.band);
            let band = &mut self.preview_config.bands[drag.band];
            band.frequency_hz = drag.frequency;
            band.gain_db = drag.gain;
            for (field, value) in [
                (BandField::FrequencyHz, drag.frequency),
                (BandField::GainDb, drag.gain),
            ] {
                self.update_preview_field(&EqField::Band(drag.band, field), value, cx);
            }
        } else {
            self.restore_position_fields(drag.band, cx);
        }
        if self.response_dirty || !admitted {
            self.update_response(cx);
        }
    }

    fn graph(&mut self, panel: u64, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = rivu_ui::theme::palette(cx);
        let measured = Rc::clone(&self.bounds);
        let curve = Arc::clone(&self.response);
        let realized_current = self.realized_current;
        let realized = Arc::clone(&self.realized);
        let nodes = self
            .config
            .bands
            .iter()
            .take(MAX_BANDS)
            .enumerate()
            .filter(|_| {
                self.band_numeric_range(&EqField::Band(0, BandField::FrequencyHz))
                    .is_some()
            })
            .map(|(i, b)| {
                let (frequency, gain) = self.preview_position(i, b);
                (i, self.frequency_x(frequency), (0.5 - gain / 48.) as f32)
            })
            .collect::<Vec<_>>();
        let selected = self.band;
        let bounds_copy = measured.clone();
        let weak = cx.entity().downgrade();
        let geometry_owner = weak.clone();
        let geometry_epoch = self.geometry_epoch;
        let ui_scale = self.state.system.config.ui_scale;
        let graph_epoch = self.graph_epoch;
        let revision = self.revision;
        let graph_focused = self.graph_has_focus(window);
        let mut controls = Vec::with_capacity(nodes.len());
        for &(index, x, y) in nodes
            .iter()
            .filter(|(index, _, _)| *index != selected)
            .chain(nodes.iter().filter(|(index, _, _)| *index == selected))
        {
            let focus = self.band_focus_handle(index, window, cx);
            let band = &self.config.bands[index];
            let (frequency, gain) = self.preview_position(index, band);
            let boundary = graph_frequency_boundary(frequency, self.maximum_frequency);
            let boundary_hint = boundary.map(|key| self.tr(key));
            let node_value = format!("{frequency:.1} Hz, {gain:.2} dB");
            let node_hint = match &boundary_hint {
                Some(hint) => format!("{node_value} · {hint}"),
                None => node_value.clone(),
            };
            let mut stack_count = 0usize;
            let mut representative = index;
            for &(other, other_x, other_y) in &nodes {
                let overlaps = if let Some((bounds, _)) = measured.get() {
                    let dx = f32::from(bounds.size.width) * (x - other_x);
                    let dy = f32::from(bounds.size.height) * (y - other_y);
                    dx * dx + dy * dy < 225.
                } else {
                    x == other_x && y == other_y
                };
                if overlaps {
                    stack_count += 1;
                    if other == selected || (representative != selected && other < representative) {
                        representative = other;
                    }
                }
            }
            controls.push(
                div()
                    .id(SharedString::from(format!("eq-node-{panel}-{index}")))
                    .debug_selector(move || format!("eq-node-{panel}-{index}"))
                    .absolute()
                    .left(relative(x))
                    .top(relative(y))
                    .ml(-px(11.))
                    .mt(-px(11.))
                    .size(px(22.))
                    .tab_index(0)
                    .tab_stop(!self.blocked)
                    .track_focus(&focus)
                    .role(Role::Slider)
                    .aria_label(SharedString::from(format!(
                        "{} {}: {}",
                        self.context.i18n.text("processing-node"),
                        index + 1,
                        boundary_hint
                            .clone()
                            .unwrap_or_else(|| self.tr("processing-node-help"))
                    )))
                    .aria_value(SharedString::from(node_value))
                    .tooltip(rivu_ui::text_tooltip(node_hint))
                    .border_2()
                    .rounded_full()
                    .border_color(rgb(if index == selected {
                        theme.accent
                    } else {
                        theme.muted
                    }))
                    .bg(rgb(if band.enabled {
                        if index == selected {
                            theme.accent
                        } else {
                            theme.secondary
                        }
                    } else {
                        theme.bg
                    }))
                    .text_color(rgb(if band.enabled {
                        theme.bg
                    } else if index == selected {
                        theme.accent
                    } else {
                        theme.muted
                    }))
                    .text_xs()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(match boundary {
                        Some("processing-frequency-below-graph") => format!("‹{}", index + 1),
                        Some(_) => format!("{}›", index + 1),
                        None => (index + 1).to_string(),
                    })
                    .opacity(
                        if boundary.is_some() || graph_focused || self.drag.is_some() {
                            1.
                        } else {
                            0.
                        },
                    )
                    .group_hover("eq-curve", |style| style.opacity(1.))
                    .focus(|style| style.opacity(1.).border_color(rgb(theme.accent)))
                    .when(stack_count > 1 && representative == index, |node| {
                        node.child(
                            div()
                                .absolute()
                                .left(px(14.))
                                .top(-px(14.))
                                .px_1()
                                .rounded_sm()
                                .bg(rgb(theme.panel))
                                .text_color(rgb(if index == selected {
                                    theme.accent
                                } else {
                                    theme.text
                                }))
                                .text_xs()
                                .whitespace_nowrap()
                                .child(self.context.i18n.format(
                                    "processing-overlap-count",
                                    &[("count", stack_count.into()), ("band", (index + 1).into())],
                                ))
                                .id(SharedString::from(format!("eq-stack-{panel}-{index}")))
                                .tooltip(rivu_ui::text_tooltip(self.tr("processing-overlap-help"))),
                        )
                    })
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        if this.band_focus[index]
                            .as_ref()
                            .is_some_and(|current| current.handle == focus)
                            && focus.is_focused(window)
                            && this.active
                            && graph_epoch == this.graph_epoch
                            && revision == this.revision
                            && geometry_epoch == this.geometry_epoch
                            && this.controls_current(cx)
                            && this.bounds.get().is_some_and(|(_, geometry)| {
                                geometry
                                    == WindowGeometry::current(
                                        window,
                                        this.context.snapshot().system.config.ui_scale,
                                    )
                            })
                        {
                            this.node_key(index, event, cx);
                        }
                    }))
                    .into_any_element(),
            );
        }
        div()
            .id(("eq-graph", panel))
            .debug_selector(move || format!("eq-graph-{panel}"))
            .group("eq-curve")
            .relative()
            .w_full()
            .h(rems(12.))
            .flex_shrink_0()
            .bg(rgb(theme.bg))
            .border_1()
            .border_color(rgb(theme.border))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    if !this.active
                        || this.blocked
                        || graph_epoch != this.graph_epoch
                        || revision != this.revision
                        || !this.controls_current(cx)
                    {
                        return;
                    }
                    let Some((bounds, geometry)) = measured.get() else {
                        return;
                    };
                    if geometry_epoch != this.geometry_epoch
                        || geometry
                            != WindowGeometry::current(
                                window,
                                this.context.snapshot().system.config.ui_scale,
                            )
                    {
                        return;
                    }
                    let mut members = [0; MAX_BANDS];
                    let mut member_count = 0;
                    let mut nearest = None;
                    let mut nearest_distance = f32::INFINITY;
                    for (index, band) in this.config.bands.iter().take(MAX_BANDS).enumerate() {
                        let x = bounds.origin.x
                            + bounds.size.width * this.frequency_x(band.frequency_hz);
                        let y = bounds.origin.y
                            + bounds.size.height * ((0.5 - band.gain_db / 48.) as f32);
                        let dx = f32::from(x - event.position.x);
                        let dy = f32::from(y - event.position.y);
                        let distance = dx * dx + dy * dy;
                        if distance < 225. {
                            members[member_count] = index;
                            member_count += 1;
                            if distance < nearest_distance {
                                nearest = Some(index);
                                nearest_distance = distance;
                            }
                        }
                    }
                    let target = if members[..member_count].contains(&this.band) {
                        Some(this.band)
                    } else {
                        nearest
                    };
                    if let Some(index) = target {
                        let Some(b) = this.config.bands.get(index) else {
                            return;
                        };
                        let frequency = b.frequency_hz;
                        let gain = b.gain_db;
                        this.focus_graph_band(index, window, cx);
                        if this.pointer_locked(cx) {
                            cx.notify();
                            return;
                        }
                        let subscriptions = [
                            cx.observe_window_activation(window, |this, window, cx| {
                                if !window.is_window_active() {
                                    this.cancel_pointer(cx)
                                }
                            }),
                            cx.observe_window_visibility(window, |this, visibility, _, cx| {
                                if !matches!(visibility, WindowVisibility::Visible) {
                                    this.cancel_pointer(cx)
                                }
                            }),
                        ];
                        this.cancel_pointer(cx);
                        this.copy_preview_config();
                        this.pointer_count.set(this.pointer_count.get() + 1);
                        this.drag = Some(NodeDrag {
                            panel,
                            band: index,
                            members,
                            member_count,
                            press_position: event.position,
                            moved: false,
                            original_frequency: frequency,
                            original_gain: gain,
                            bounds,
                            geometry,
                            geometry_epoch,
                            frequency,
                            gain,
                            _subscriptions: subscriptions,
                        });
                        cx.stop_propagation();
                        cx.notify();
                    }
                }),
            )
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let geometry = WindowGeometry::current(window, ui_scale);
                        let _ = geometry_owner.update(cx, |this, cx| {
                            if geometry_epoch != this.geometry_epoch {
                                return;
                            }
                            if bounds_copy.get() != Some((bounds, geometry)) {
                                cx.notify();
                            }
                            bounds_copy.set(Some((bounds, geometry)));
                            if this.drag.as_ref().is_some_and(|d| {
                                d.panel == panel && (d.bounds != bounds || d.geometry != geometry)
                            }) {
                                this.cancel_pointer(cx);
                            }
                        });
                        bounds
                    },
                    move |bounds, _, window, cx| {
                        Self::register_pointer_handlers(weak.clone(), panel, window, cx);
                        let mut grid = PathBuilder::stroke(px(1.));
                        for fraction in [0., 0.25, 0.5, 0.75, 1.] {
                            let y = bounds.origin.y + bounds.size.height * fraction;
                            grid.move_to(point(bounds.origin.x, y));
                            grid.line_to(point(bounds.right(), y));
                        }
                        for fraction in [0., 0.25, 0.5, 0.75, 1.] {
                            let x = bounds.origin.x + bounds.size.width * fraction;
                            grid.move_to(point(x, bounds.origin.y));
                            grid.line_to(point(x, bounds.bottom()));
                        }
                        if let Ok(path) = grid.build() {
                            window.paint_path(path, rgb(theme.border));
                        }
                        let x = |i: usize| bounds.origin.x + bounds.size.width * (i as f32 / 256.);
                        let y = |db: f64| {
                            bounds.origin.y
                                + bounds.size.height * (0.5 - db.clamp(-24., 24.) as f32 / 48.)
                        };
                        let line = |values: &[f64], builder: PathBuilder| {
                            let mut path = builder;
                            for (i, &db) in values.iter().enumerate() {
                                if i == 0 {
                                    path.move_to(point(x(i), y(db)));
                                } else {
                                    path.line_to(point(x(i), y(db)));
                                }
                            }
                            path.build().ok()
                        };
                        if realized.len() == curve.len() && !curve.is_empty() {
                            // Shade only visible gaps, and only when both curves
                            // describe the same edit; one quad per sample keeps
                            // the path bounded by the 257-point frequency grid.
                            let mut gap = PathBuilder::fill();
                            let mut any = false;
                            for i in (1..curve.len()).filter(|_| realized_current) {
                                let deviates =
                                    |j: usize| (realized[j] - curve[j]).abs() >= FIR_DEVIATION_DB;
                                if !deviates(i - 1) && !deviates(i) {
                                    continue;
                                }
                                any = true;
                                gap.move_to(point(x(i - 1), y(curve[i - 1])));
                                gap.line_to(point(x(i), y(curve[i])));
                                gap.line_to(point(x(i), y(realized[i])));
                                gap.line_to(point(x(i - 1), y(realized[i - 1])));
                                gap.close();
                            }
                            if any && let Ok(path) = gap.build() {
                                window.paint_path(path, rgb(theme.warning).alpha(0.35));
                            }
                            let dash = PathBuilder::stroke(px(1.5)).dash_array(&[px(5.), px(4.)]);
                            if let Some(path) = line(&curve, dash) {
                                window.paint_path(path, rgb(theme.muted));
                            }
                            if let Some(path) = line(&realized, PathBuilder::stroke(px(2.))) {
                                window.paint_path(path, rgb(theme.accent));
                            }
                        } else if !curve.is_empty()
                            && let Some(path) = line(&curve, PathBuilder::stroke(px(2.)))
                        {
                            window.paint_path(path, rgb(theme.accent));
                        }
                    },
                )
                .size_full(),
            )
            .children(controls)
            .when_some(self.fir_deviation_caption(), |graph, text| {
                // Painted text only: no id or listeners, so presses fall through
                // to the graph's node hit-testing underneath.
                graph.child(
                    div()
                        .absolute()
                        .bottom(px(2.))
                        .right(px(4.))
                        .px_1()
                        .rounded_sm()
                        .bg(rgb(theme.bg).alpha(0.75))
                        .text_xs()
                        .text_color(rgb(theme.muted))
                        .debug_selector(move || format!("eq-fir-caption-{panel}"))
                        .child(text),
                )
            })
            .into_any_element()
    }

    /// Compact in-plot legend and largest realized-vs-target FIR deviation.
    fn fir_deviation_caption(&self) -> Option<String> {
        if !self.realized_current
            || self.realized.len() != self.response.len()
            || self.response.is_empty()
        {
            return None;
        }
        let (index, deviation) = self
            .realized
            .iter()
            .zip(self.response.iter())
            .map(|(realized, target)| realized - target)
            .enumerate()
            .filter(|(_, deviation)| deviation.is_finite())
            .fold((0, 0.0_f64), |best, (index, deviation)| {
                if deviation.abs() > best.1.abs() {
                    (index, deviation)
                } else {
                    best
                }
            });
        Some(self.context.i18n.format(
            "processing-fir-deviation",
            &[
                ("deviation", format!("{deviation:+.1}").into()),
                (
                    "frequency",
                    format!("{:.0}", self.frequencies[index]).into(),
                ),
                ("threshold", format!("{FIR_DEVIATION_DB}").into()),
            ],
        ))
    }

    fn node_key(&mut self, index: usize, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if !self.active || self.blocked || event.keystroke.modifiers != Modifiers::default() {
            return;
        }
        if event.keystroke.key == "escape" {
            self.cancel_pointer(cx);
            cx.stop_propagation();
            return;
        }
        let Some(band) = self.config.bands.get(index) else {
            return;
        };
        if event.keystroke.key == "delete" {
            if self.config_locked() {
                return;
            }
            self.cancel_pointer(cx);
            self.band = index;
            self.sync_fields(cx);
            self.eq(EqualizerEdit::RemoveBand { index }, cx);
            cx.stop_propagation();
            return;
        }
        // These are explicit control actions, not numeric input events.
        let (field, value) = match event.keystroke.key.as_str() {
            "left" | "right" => {
                let key = EqField::Band(index, BandField::FrequencyHz);
                let Some((minimum, maximum, _)) = self.band_numeric_range(&key) else {
                    return;
                };
                let steps = if event.keystroke.key == "left" {
                    -1.
                } else {
                    1.
                };
                let minimum = minimum.max(graph_minimum_frequency(self.maximum_frequency));
                let maximum = maximum.min(self.maximum_frequency).max(minimum);
                let current = self.live_number(&key, cx).unwrap_or(band.frequency_hz);
                (
                    key,
                    (current * 2.0_f64.powf(steps / 12.)).clamp(minimum, maximum),
                )
            }
            "up" | "down"
                if matches!(
                    band.kind,
                    FilterKind::Peak | FilterKind::LowShelf | FilterKind::HighShelf
                ) =>
            {
                let key = EqField::Band(index, BandField::GainDb);
                let step = if event.keystroke.key == "up" {
                    0.5
                } else {
                    -0.5
                };
                let current = self.live_number(&key, cx).unwrap_or(band.gain_db);
                (key, (current + step).clamp(-24., 24.))
            }
            "enter" | "space" => {
                self.band = index;
                self.sync_fields(cx);
                cx.notify();
                cx.stop_propagation();
                return;
            }
            _ => return,
        };
        if self.pointer_locked(cx) {
            return;
        }
        self.cancel_pointer(cx);
        self.band = index;
        self.sync_fields(cx);
        self.submit_numeric_step(&field, value, cx);
        cx.stop_propagation();
    }

    /// The current draft, not the last render, is the base for repeated steps.
    fn live_number(&self, key: &EqField, cx: &App) -> Option<f64> {
        let field = self.fields.get(key).filter(|field| field.dirty)?;
        let value = field.input.read(cx).text().trim().parse::<f64>().ok()?;
        value.is_finite().then_some(value)
    }

    fn move_node(&mut self, position: Point<Pixels>) {
        let frequency_range = self.band_numeric_range(&EqField::Band(0, BandField::FrequencyHz));
        if let Some(drag) = self.drag.as_mut() {
            if drag.bounds.size.width <= px(0.) || drag.bounds.size.height <= px(0.) {
                return;
            }
            let delta = position - drag.press_position;
            let dx = f32::from(delta.x);
            let dy = f32::from(delta.y);
            if !drag.moved && dx * dx + dy * dy < 9. {
                return;
            }
            let previous = (drag.frequency, drag.gain);
            drag.moved = true;
            drag.frequency = if delta.x == px(0.) || frequency_range.is_none() {
                drag.original_frequency
            } else {
                let minimum = graph_minimum_frequency(self.maximum_frequency);
                let original_x = (drag.original_frequency / minimum).ln()
                    / (self.maximum_frequency / minimum).ln();
                let x = (original_x + (delta.x / drag.bounds.size.width) as f64).clamp(0., 1.);
                minimum * (self.maximum_frequency / minimum).powf(x)
            };
            let kind = self.config.bands[drag.band].kind;
            if matches!(
                kind,
                FilterKind::Peak | FilterKind::LowShelf | FilterKind::HighShelf
            ) {
                drag.gain = (drag.original_gain - (delta.y / drag.bounds.size.height) as f64 * 48.)
                    .clamp(-24., 24.);
            }
            if previous != (drag.frequency, drag.gain) {
                self.response_dirty = true;
            }
        }
    }

    fn choose_response_channel(&mut self, index: usize, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(channel) = self.channel_choices.item(index).map(|item| item.value) {
            self.response_channel = channel;
            self.update_response(cx);
        }
        self.channel_choices.close();
        cx.notify();
    }

    fn dismiss_response_channels(
        &mut self,
        _: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.channel_choices.close();
        cx.notify();
        true
    }

    fn equalizer_view(&mut self, panel: u64, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let graph = self.graph(panel, window, cx);
        let count_control = row()
            .gap_1()
            .flex_shrink_0()
            .child(caption(theme, self.tr("processing-band-count-label")))
            .child(
                div()
                    .w(rems(3.))
                    .id(("eq-band-count", panel))
                    .debug_selector(move || format!("eq-band-count-{panel}"))
                    .tooltip(rivu_ui::text_tooltip(self.tr("processing-band-count-help")))
                    .child(self.fields[&EqField::BandCount].input.clone()),
            );
        let gains = row()
            .min_w_0()
            .gap_2()
            .child(self.field_view(
                &EqField::PreampDb,
                self.tr("processing-input-gain-short"),
                !self.input_locked(),
                cx,
            ))
            .flex_wrap()
            .child(self.field_view(
                &EqField::OutputGainDb,
                self.tr("processing-output-gain-short"),
                !self.input_locked(),
                cx,
            ));
        let mut controls = row()
            .min_w_0()
            .flex_wrap()
            .gap_2()
            .child(
                count_control
                    .pr_2()
                    .border_r_1()
                    .border_color(rgb(theme.border)),
            )
            .child(gains.flex_1().flex_basis(rems(17.)));
        let mut curve = column().gap_1().min_w_0();
        if let Some(EqualizerFormat {
            channels: channel_count,
            ..
        }) = self.format
            && channel_count > 1
        {
            let anchor = Rc::clone(&self.channel_anchor);
            let measure = canvas(
                move |bounds, _, _| {
                    anchor.set(Some(bounds));
                },
                |_, _, _, _| {},
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full();
            let response_label = self.tr("processing-response-channel");
            let trigger = rivu_ui::ChoiceSelect::new(
                ("eq-response-channel", panel),
                format!(
                    "{} {}",
                    self.tr("processing-response-channel"),
                    self.response_channel + 1
                ),
            )
            .surface(crate::gui::appearance::material::surface(cx))
            .build(
                self,
                |this| Some(&mut this.channel_choices),
                cx,
                move |this, _, cx| {
                    if this.channel_choices.is_open() {
                        this.channel_choices.close();
                    } else {
                        this.channel_choices.open(
                            (0..channel_count).map(|channel| {
                                rivu_ui::ChoiceItem::new(channel, (channel + 1).to_string())
                            }),
                            this.response_channel,
                        );
                    }
                    cx.notify();
                },
                Self::choose_response_channel,
                |this, _, cx| {
                    this.channel_choices.close();
                    cx.notify();
                },
            )
            .w_auto()
            .flex_shrink_0()
            .relative()
            .aria_label(response_label)
            .debug_selector(move || format!("eq-response-channel-{panel}"))
            .child(measure);
            controls = controls.child(trigger);
        }
        curve = curve.child(graph);
        if !self.response.is_empty() {
            let mut labels = row().justify_between().gap_1();
            for fraction in [0., 0.25, 0.5, 0.75, 1.] {
                let minimum = graph_minimum_frequency(self.maximum_frequency);
                let frequency = minimum * (self.maximum_frequency / minimum).powf(fraction);
                labels = labels.child(caption(
                    theme,
                    if frequency >= 1000. {
                        format!("{:.1}k", frequency / 1000.)
                    } else {
                        format!("{frequency:.0}")
                    },
                ));
            }
            curve = curve.child(labels);
        }
        if let Some(error) = &self.response_error {
            curve = curve.child(caption(theme, error.clone()).text_color(rgb(theme.error)));
        } else if self.format.is_none() {
            curve = curve.child(caption(theme, self.tr("processing-response-unknown")));
        }
        let mut precision = column().gap_1().min_w_0();
        let presets = row()
            .min_w_0()
            .items_start()
            .gap_2()
            .child(
                rivu_ui::IconButton::new(
                    ("eq-enable", panel),
                    "󰓃",
                    self.tr(if self.config.enabled {
                        "processing-eq-disable"
                    } else {
                        "processing-eq-enable"
                    }),
                )
                .surface(crate::gui::appearance::material::surface(cx))
                .toggled(self.config.enabled)
                .enabled(!self.blocked && self.pending == 0)
                .build(
                    rivu_ui::IconButtonStyle::Quiet,
                    rivu_ui::IconButtonDensity::Compact,
                    cx,
                    |this, _, cx| this.eq(EqualizerEdit::Enabled(!this.config.enabled), cx),
                )
                .debug_selector(move || format!("eq-enable-{panel}")),
            )
            .when(self.target == EqTarget::Monitor, |view| {
                let preset = self
                    .owner
                    .update(cx, |owner, cx| owner.preset_view(panel, window, cx))
                    .ok();
                view.child(div().flex_1().max_w_full().min_w_0().children(preset))
            });
        let restores_preset = self.target == EqTarget::Monitor
            && self
                .owner
                .upgrade()
                .is_some_and(|owner| owner.read(cx).equalizer_editors.loaded_preset.is_some());
        let reset_controls = row()
            .gap_1()
            .flex_shrink_0()
            .child(
                rivu_ui::IconButton::new(
                    ("eq-reset", panel),
                    "󰑓",
                    self.tr(if matches!(self.target, EqTarget::Node(_)) {
                        "processing-eq-reset-node"
                    } else if restores_preset {
                        "processing-eq-restore-preset"
                    } else {
                        "processing-eq-clear-parameters"
                    }),
                )
                .surface(crate::gui::appearance::material::surface(cx))
                .enabled(!self.reset_locked(cx))
                .build(
                    rivu_ui::IconButtonStyle::Quiet,
                    rivu_ui::IconButtonDensity::Compact,
                    cx,
                    |this, _, cx| {
                        let owner = this.owner.clone();
                        let target = this.target;
                        let revision = this.revision;
                        let format = this.format;
                        // Release the initiating editor before retiring all
                        // unsubmitted parameter drafts across this target's panels.
                        cx.defer(move |cx| {
                            let _ = owner.update(cx, |owner, cx| {
                                owner.reset_target_equalizer(target, revision, format, cx);
                            });
                        });
                    },
                )
                .debug_selector(move || format!("eq-reset-{panel}")),
            )
            .child(
                rivu_ui::icon("", cx)
                    .text_color(rgb(theme.secondary))
                    .id(("eq-help", panel))
                    .tooltip(rivu_ui::text_tooltip(self.tr("processing-graph-help"))),
            );
        let tools = row()
            .min_w_0()
            .flex_wrap()
            .gap_2()
            .child(presets.flex_shrink_0())
            .child(
                row()
                    .min_w_0()
                    .flex_1()
                    .flex_wrap()
                    .gap_1()
                    .justify_end()
                    .id(("eq-bank-tools", panel))
                    .child(reset_controls),
            );
        let eq = &self.config;
        if let Some(band) = eq.bands.get(self.band) {
            let index = self.band;
            let enabled_control = self.field_control(EqField::Band(index, BandField::Channels));
            let solo_control = enabled_control.clone();
            let remove_control = enabled_control.clone();
            let frequency = self
                .drag
                .as_ref()
                .filter(|drag| drag.band == index)
                .map_or(band.frequency_hz, |drag| drag.frequency);
            if let Some(boundary) = graph_frequency_boundary(frequency, self.maximum_frequency) {
                precision = precision.child(
                    caption(
                        theme,
                        self.context.i18n.format(
                            "processing-frequency-boundary",
                            &[
                                ("band", (index + 1).into()),
                                ("frequency", format!("{frequency:.1}").into()),
                                ("boundary", self.tr(boundary).to_string().into()),
                            ],
                        ),
                    )
                    .id(("eq-frequency-boundary", panel))
                    .debug_selector(move || format!("eq-frequency-boundary-{panel}"))
                    .text_color(rgb(theme.accent)),
                );
            }
            let kind_key = match band.kind {
                FilterKind::Peak => "processing-peak",
                FilterKind::LowShelf => "processing-low-shelf",
                FilterKind::HighShelf => "processing-high-shelf",
                FilterKind::LowPass => "processing-low-pass",
                FilterKind::HighPass => "processing-high-pass",
                FilterKind::BandPass => "processing-band-pass",
                FilterKind::Notch => "processing-notch",
            };
            let title = self.context.i18n.format(
                "processing-band-properties",
                &[
                    ("band", (index + 1).into()),
                    ("kind", self.tr(kind_key).to_string().into()),
                ],
            );
            let title = if band.enabled {
                title.to_string()
            } else {
                format!("{title} · {}", self.tr("processing-band-disabled"))
            };
            precision = precision.child(
                row()
                    .min_w_0()
                    .gap_2()
                    .id(("eq-band-properties", panel))
                    .debug_selector(move || format!("eq-band-properties-{panel}"))
                    .pb_1()
                    .border_b_1()
                    .border_color(rgb(theme.border))
                    .child(
                        div()
                            .text_xs()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_color(rgb(theme.accent))
                            .id(("eq-band-title", panel))
                            .child(rivu_ui::truncated_label("eq-band-title-text", title)),
                    )
                    .child(
                        row()
                            .gap_1()
                            .flex_shrink_0()
                            .whitespace_nowrap()
                            .child(checkbox(
                                ("band-enabled", panel),
                                self.tr("processing-enabled"),
                                band.enabled,
                                !self.config_locked(),
                                cx,
                                move |this, value, cx| {
                                    if this.field_control_current(&enabled_control, cx) {
                                        this.band_edit(index, EqBandEdit::Enabled(value), cx);
                                    }
                                },
                            ))
                            .child(checkbox(
                                ("band-solo", panel),
                                self.tr("processing-solo"),
                                band.solo,
                                !self.config_locked(),
                                cx,
                                move |this, value, cx| {
                                    if this.field_control_current(&solo_control, cx) {
                                        this.band_edit(index, EqBandEdit::Solo(value), cx);
                                    }
                                },
                            ))
                            .child(
                                rivu_ui::IconButton::new(
                                    ("remove-band", panel),
                                    "󰆴",
                                    self.tr("processing-remove-band"),
                                )
                                .surface(crate::gui::appearance::material::surface(cx))
                                .enabled(!self.config_locked())
                                .build(
                                    rivu_ui::IconButtonStyle::Quiet,
                                    rivu_ui::IconButtonDensity::Compact,
                                    cx,
                                    move |this, window, cx| {
                                        if this.field_control_valid(&remove_control, window, cx) {
                                            this.eq(EqualizerEdit::RemoveBand { index }, cx);
                                        }
                                    },
                                ),
                            ),
                    ),
            );
            let mut types = row().flex_wrap().gap_1();
            for (kind, key) in [
                (FilterKind::Peak, "processing-peak"),
                (FilterKind::LowShelf, "processing-low-shelf"),
                (FilterKind::HighShelf, "processing-high-shelf"),
                (FilterKind::LowPass, "processing-low-pass"),
                (FilterKind::HighPass, "processing-high-pass"),
                (FilterKind::BandPass, "processing-band-pass"),
                (FilterKind::Notch, "processing-notch"),
            ] {
                let identity = self.field_control(EqField::Band(index, BandField::Channels));
                types = types.child(disabled_control(
                    theme,
                    crate::gui::appearance::material::button(
                        SharedString::from(format!("band-kind-{kind:?}")),
                        self.tr(key),
                        cx,
                        move |this, window, cx| {
                            if this.field_control_valid(&identity, window, cx) {
                                this.band_edit(index, EqBandEdit::Kind(kind), cx);
                            }
                        },
                    )
                    .map(|view| {
                        rivu_ui::active_control(
                            theme,
                            view,
                            kind == band.kind,
                            crate::gui::appearance::material::surface(cx),
                        )
                    })
                    .when(kind == band.kind, |view| {
                        view.border_color(rgb(theme.accent))
                    }),
                    self.config_locked(),
                ));
            }
            precision = precision.child(types);
            for (name, key, applicable) in [
                (BandField::FrequencyHz, "processing-frequency", true),
                (
                    BandField::GainDb,
                    "processing-gain",
                    matches!(
                        band.kind,
                        FilterKind::Peak | FilterKind::LowShelf | FilterKind::HighShelf
                    ),
                ),
                (
                    BandField::Q,
                    "processing-q",
                    !matches!(band.kind, FilterKind::LowShelf | FilterKind::HighShelf),
                ),
                (
                    BandField::ShelfSlope,
                    "processing-shelf-slope",
                    matches!(band.kind, FilterKind::LowShelf | FilterKind::HighShelf),
                ),
                (
                    BandField::Stages,
                    "processing-stages",
                    !matches!(
                        band.kind,
                        FilterKind::Peak | FilterKind::LowShelf | FilterKind::HighShelf
                    ),
                ),
                (BandField::Channels, "processing-channels", true),
            ] {
                if applicable {
                    precision = precision.child(self.field_view(
                        &EqField::Band(index, name),
                        self.tr(key),
                        !self.input_locked(),
                        cx,
                    ));
                }
            }
        }
        let body = column()
            .min_w_0()
            .gap_2()
            .child(curve)
            .when(!eq.bands.is_empty(), |view| view.child(precision));
        column()
            .min_w_0()
            .gap_2()
            .child(tools)
            .child(controls)
            .child(body)
            .when(self.channel_choices.is_open(), |view| {
                if let Some(anchor) = self.channel_anchor.get() {
                    view.child(
                        rivu_ui::ChoicePopup::new(
                            ("eq-channel-choice", panel),
                            ("eq-channel-scroll", panel),
                            anchor,
                        )
                        .render(
                            self,
                            |this| &this.channel_choices,
                            Self::choose_response_channel,
                            Self::dismiss_response_channels,
                            window,
                            cx,
                        ),
                    )
                } else {
                    view
                }
            })
    }
}

impl Drop for EqualizerEditor {
    fn drop(&mut self) {
        self.take_drag();
        self.take_field_drag();
    }
}

impl Render for EqualizerEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.active {
            return div();
        }
        let panel = self.panel;
        self.equalizer_view(panel, window, cx)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape"
                    && (this.drag.is_some() || this.field_drag.is_some())
                {
                    this.cancel_pointer(cx);
                    window.prevent_default();
                    cx.stop_propagation();
                }
            }))
            .when(self.blocked, |view| view.opacity(0.5))
            .when_some(self.error.clone(), |view, error| {
                view.child(
                    caption(rivu_ui::theme::palette(cx), error)
                        .text_color(rgb(rivu_ui::theme::palette(cx).error)),
                )
            })
    }
}

fn target_config(state: &GuiSnapshot, target: EqTarget) -> Option<&EqualizerConfig> {
    match target {
        EqTarget::Monitor => Some(&state.system.config.equalizer),
        EqTarget::Node(id) => state
            .system
            .config
            .processing_graph
            .nodes
            .iter()
            .find_map(|node| {
                if node.id != id {
                    return None;
                }
                match &node.kind {
                    GraphNodeKind::Equalizer { config, .. } => Some(config),
                    _ => None,
                }
            }),
    }
}

fn target_fir_taps(state: &GuiSnapshot, target: EqTarget) -> Option<usize> {
    let EqTarget::Node(id) = target else {
        return None;
    };
    state
        .system
        .config
        .processing_graph
        .nodes
        .iter()
        .find_map(|node| match &node.kind {
            GraphNodeKind::Equalizer {
                implementation: EqImplementation::LinearPhase { taps },
                ..
            } if node.id == id => Some(*taps),
            _ => None,
        })
}

impl AudioProcessing {
    fn reset_target_equalizer(
        &mut self,
        target: EqTarget,
        revision: u64,
        format: EqFormat,
        cx: &mut Context<Self>,
    ) {
        // Replace is bank-wide: never construct it ahead of an in-flight edit.
        if self.pending > 0 || matches!(target, EqTarget::Node(_)) && self.graph_busy() {
            return;
        }
        let state = self.context.snapshot();
        let Some(config) = target_config(&state, target).filter(|config| config.enabled) else {
            return;
        };
        if state.equalizer_format() != format
            || matches!(target, EqTarget::Node(_))
                && (state.system.config.processing_graph.revision != revision
                    || self.state.system.config.processing_graph.revision != revision)
        {
            self.error = Some(self.tr("processing-editor-stale").to_string());
            cx.notify();
            return;
        }
        let config = config.clone();
        if let EqTarget::Node(id) = target {
            self.discard_graph_node(id, cx);
        }
        if let Some(editor) = self.equalizer_editors.editors.get(&target) {
            editor.update(cx, |editor, cx| {
                editor.cancel_pointer(cx);
                for field in editor.fields.values_mut() {
                    field.dirty = false;
                    field.draft_revision = None;
                    field.generation = field.generation.wrapping_add(1);
                    let text = field.committed.clone();
                    field.input.update(cx, |input, cx| input.set_text(text, cx));
                }
                editor.prune_inputs();
                cx.notify();
            });
        }
        let next = if target == EqTarget::Monitor {
            self.equalizer_editors
                .loaded_preset
                .clone()
                .map(|mut preset| {
                    preset.enabled = config.enabled;
                    preset
                })
        } else {
            None
        }
        .unwrap_or_else(|| reset_equalizer_config(&config));
        match target {
            EqTarget::Monitor => {
                self.submit_target_equalizer(
                    (target, revision, &config, format),
                    EqualizerEdit::Replace { config: next },
                    None,
                    cx,
                );
            }
            EqTarget::Node(id) => {
                let mut submission = NodeEqualizerSubmission {
                    config: next,
                    field_submit: false,
                    revision,
                    node: id,
                    fields: Vec::new(),
                };
                let config = std::mem::take(&mut submission.config);
                self.equalizer_editors.pending_node_focus_reset = Some((id, revision));
                self.graph_edit_equalizer(
                    GraphEdit::Replace {
                        id,
                        kind: GraphNodeKind::Equalizer {
                            config,
                            implementation: Default::default(),
                        },
                    },
                    Some(submission),
                    cx,
                );
            }
        }
    }

    pub(super) fn eq(&mut self, edit: EqualizerEdit, cx: &mut Context<Self>) {
        if !self.context.snapshot().system.config.equalizer.enabled
            && !matches!(edit, EqualizerEdit::Enabled(_))
        {
            return;
        }
        self.send(
            Command::EqualizerEdit {
                expected_revision: self.context.snapshot().revisions.configuration,
                edit,
            },
            cx,
        );
    }

    fn equalizer_blocked(&self, target: EqTarget) -> bool {
        matches!(target, EqTarget::Node(_)) && (self.graph_busy() || self.graph_has_drafts())
    }

    fn collecting_equalizer_target(&self) -> Option<(EqTarget, u64)> {
        if self.graph_has_drafts() {
            return None;
        }
        self.graph_collecting_equalizer_input()
            .map(|(id, revision)| (EqTarget::Node(id), revision))
    }

    fn submit_target_equalizer(
        &mut self,
        source: (EqTarget, u64, &EqualizerConfig, EqFormat),
        edit: EqualizerEdit,
        submitted: Option<SubmittedField>,
        cx: &mut Context<Self>,
    ) -> bool {
        let releases_node = matches!(
            edit,
            EqualizerEdit::BandField {
                field: EqBandEdit::Position { .. },
                ..
            }
        );
        if releases_node && self.pending != 0 {
            return false;
        }
        let (target, revision, expected, expected_format) = source;
        let snapshot = self.context.snapshot();
        if target_config(&snapshot, target) != Some(expected)
            || snapshot.equalizer_format() != expected_format
            || matches!(target, EqTarget::Node(_))
                && (self.equalizer_blocked(target)
                    || snapshot.system.config.processing_graph.revision != revision
                    || self.state.system.config.processing_graph.revision != revision)
        {
            self.error = Some(self.context.i18n.text("processing-editor-stale").to_owned());
            cx.notify();
            return false;
        }
        if matches!(target, EqTarget::Node(_))
            && let EqualizerEdit::RemoveBand { index } = &edit
        {
            self.equalizer_editors
                .removing
                .insert(target, (expected.clone(), *index));
        }
        match target {
            EqTarget::Monitor => self.enqueue_submission(
                Submission {
                    command: Command::EqualizerEdit {
                        expected_revision: snapshot.revisions.configuration,
                        edit,
                    },
                    field: None,
                    equalizer_field: submitted,
                },
                cx,
            ),
            EqTarget::Node(id) => {
                let Some(node) = snapshot
                    .system
                    .config
                    .processing_graph
                    .nodes
                    .iter()
                    .find(|node| node.id == id)
                else {
                    return false;
                };
                let GraphNodeKind::Equalizer {
                    config,
                    implementation,
                } = &node.kind
                else {
                    return false;
                };
                let mut next = config.clone();
                let sample_rate = expected_format.map(|format| format.sample_rate);
                let field_submit = submitted.is_some();
                let replaces_band_identity = replaces_band_identity(&edit);
                match edit.apply_config(&mut next, sample_rate) {
                    Ok(None) => {
                        let mut submission = NodeEqualizerSubmission {
                            config: next,
                            field_submit: field_submit && !replaces_band_identity,
                            revision,
                            node: id,
                            fields: submitted.into_iter().collect(),
                        };
                        let config = std::mem::take(&mut submission.config);
                        if replaces_band_identity {
                            self.equalizer_editors.pending_node_focus_reset = Some((id, revision));
                        }
                        self.graph_edit_equalizer(
                            GraphEdit::Replace {
                                id,
                                kind: GraphNodeKind::Equalizer {
                                    config,
                                    implementation: *implementation,
                                },
                            },
                            Some(submission),
                            cx,
                        );
                    }
                    Ok(Some(_)) => {
                        self.error = Some(
                            self.context
                                .i18n
                                .text("processing-editor-monitor-only")
                                .to_owned(),
                        );
                        cx.notify();
                        return false;
                    }
                    Err(error) => {
                        self.error = Some(error.to_string());
                        cx.notify();
                        return false;
                    }
                }
            }
        }
        if releases_node {
            // Releases require an empty queue: the very next completion owns this preview.
            self.equalizer_editors.released_target = Some(target);
        }
        true
    }

    pub(super) fn finish_released_equalizer_position(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.equalizer_editors.released_target.take() else {
            return;
        };
        if let Some(editor) = self.equalizer_editors.editors.get(&target) {
            editor.update(cx, |editor, cx| {
                if editor.retire_node_preview(cx) {
                    editor.response_dirty = true;
                    if editor.active {
                        editor.update_response(cx);
                    }
                    cx.notify();
                }
            });
        }
    }

    fn equalizer_editor(
        &mut self,
        panel: u64,
        target: EqTarget,
        config: &EqualizerConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<EqualizerEditor> {
        self.bind_panel(panel, cx);
        if self.equalizer_editors.active != Some((panel, target)) {
            if let Some((previous, _)) = self.equalizer_editors.active {
                self.deactivate_equalizer_panel(previous, window, cx);
            }
            self.equalizer_editors.active = Some((panel, target));
        }
        if let Some(editor) = self.equalizer_editors.editors.get(&target) {
            let editor = editor.clone();
            let availability = EqAvailability {
                blocked: self.equalizer_blocked(target),
                pending: self.pending,
                format: self.state.equalizer_format(),
                collecting_revision: self
                    .collecting_equalizer_target()
                    .filter(|(pending_target, _)| *pending_target == target)
                    .map(|(_, revision)| revision),
            };
            editor.update(cx, |editor, cx| {
                editor.panel = panel;
                editor.reconcile(
                    Rc::clone(&self.state),
                    config,
                    availability,
                    self.geometry_epoch,
                    cx,
                );
                if !editor.active {
                    editor.active = true;
                    editor.sync_fields(cx);
                    if editor.response_dirty {
                        editor.update_response(cx);
                    }
                }
            });
            return editor;
        }
        let owner = cx.entity().downgrade();
        let editor = cx.new(|cx| EqualizerEditor::new(owner, target, panel, self, config, cx));
        self.equalizer_editors
            .editors
            .insert(target, editor.clone());
        editor
    }

    pub(super) fn equalizer_view(
        &mut self,
        panel: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let state = Rc::clone(&self.state);
        let editor = self.equalizer_editor(
            panel,
            EqTarget::Monitor,
            &state.system.config.equalizer,
            window,
            cx,
        );
        column().child(editor)
    }

    pub(super) fn node_equalizer_view(
        &mut self,
        panel: u64,
        node_id: u64,
        config: &EqualizerConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let target = EqTarget::Node(node_id);
        let editor = self.equalizer_editor(panel, target, config, window, cx);
        let theme = rivu_ui::theme::palette(cx);
        column()
            .gap_3()
            .child(caption(theme, self.tr("processing-editor-node")))
            .when(
                self.equalizer_blocked(target)
                    && !self
                        .collecting_equalizer_target()
                        .is_some_and(|(pending_target, _)| pending_target == target),
                |view| view.child(caption(theme, self.tr("processing-editor-drafts-block"))),
            )
            .child(editor)
    }

    pub(super) fn notify_preset_panel(&self, panel: u64, cx: &mut Context<Self>) {
        if self.equalizer_editors.active == Some((panel, EqTarget::Monitor))
            && let Some(editor) = self.equalizer_editors.editors.get(&EqTarget::Monitor)
        {
            editor.update(cx, |_, cx| cx.notify());
        }
    }

    pub(super) fn notify_preset_labels(&self, cx: &mut Context<Self>) {
        if let Some((panel, EqTarget::Monitor)) = self.equalizer_editors.active {
            self.notify_preset_panel(panel, cx);
        }
    }

    pub(super) fn reconcile_equalizers(&mut self, cx: &mut Context<Self>) {
        let (graph_pending, graph_drafts) = self.graph_editor.equalizer_dependencies();
        let inputs = EqualizerReconcileInputs {
            config: Arc::clone(&self.state.system.config),
            format: self.state.equalizer_format(),
            pending: self.pending,
            command_active: self.command_active,
            graph_pending,
            graph_drafts,
            collecting: self.graph_collecting_equalizer_input(),
            geometry_epoch: self.geometry_epoch,
            reset_band_focus: self.equalizer_editors.reset_band_focus,
        };
        let unchanged = self
            .equalizer_editors
            .reconciled
            .as_ref()
            .is_some_and(|previous| inputs.unchanged_from(previous));
        let format = inputs.format;
        self.equalizer_editors.reconciled = Some(inputs);
        if unchanged {
            return;
        }
        let state = Rc::clone(&self.state);
        let blocked = self.equalizer_blocked(EqTarget::Node(0));
        let collecting = self.collecting_equalizer_target();
        let removals = &self.equalizer_editors.removing;
        let reset_band_focus = self.equalizer_editors.reset_band_focus;
        let command_active = self.command_active;
        self.equalizer_editors.editors.retain(|target, editor| {
            let Some(config) = target_config(&state, *target) else {
                editor.update(cx, |editor, cx| editor.deactivate(cx));
                return false;
            };
            editor.update(cx, |editor, cx| {
                if reset_band_focus == Some(*target)
                    && (!command_active || editor.config.bands != config.bands)
                {
                    editor.retire_band_focus();
                    cx.notify();
                }
                if editor.config.bands.len() != config.bands.len()
                    && let Some((previous, index)) = removals.get(target)
                {
                    editor.removing_band = Some((previous.clone(), *index));
                }
                editor.reconcile(
                    Rc::clone(&state),
                    config,
                    EqAvailability {
                        blocked: matches!(target, EqTarget::Node(_)) && blocked,
                        pending: self.pending,
                        format,
                        collecting_revision: collecting
                            .filter(|(pending_target, _)| pending_target == target)
                            .map(|(_, revision)| revision),
                    },
                    self.geometry_epoch,
                    cx,
                );
            });
            true
        });
        if self
            .equalizer_editors
            .active
            .is_some_and(|(_, target)| target_config(&state, target).is_none())
        {
            self.equalizer_editors.active = None;
        }
        self.equalizer_editors
            .removing
            .retain(|target, (previous, _)| {
                target_config(&state, *target).is_some_and(|config| config.bands == previous.bands)
            });
        if !command_active {
            self.equalizer_editors.reset_band_focus = None;
        }
    }

    pub(super) fn cancel_equalizer_pointer(&mut self, cx: &mut Context<Self>) -> bool {
        let mut cancelled = false;
        for editor in self.equalizer_editors.editors.values() {
            editor.update(cx, |editor, cx| {
                cancelled |= editor.drag.is_some() || editor.field_drag.is_some();
                editor.cancel_pointer(cx);
            });
        }
        cancelled
    }

    pub(super) fn invalidate_equalizer_geometry(&mut self, cx: &mut Context<Self>) {
        for editor in self.equalizer_editors.editors.values() {
            editor.update(cx, |editor, cx| {
                editor.geometry_epoch = self.geometry_epoch;
                editor.bounds.set(None);
                editor.cancel_pointer(cx);
            });
        }
    }

    pub(super) fn equalizer_pointer_active(&self) -> bool {
        self.equalizer_editors.pointer_count.get() > 0
    }

    pub(super) fn equalizer_input_focused(&self, window: &Window, cx: &App) -> bool {
        self.equalizer_editors.active.is_some_and(|(_, target)| {
            self.equalizer_editors
                .editors
                .get(&target)
                .is_some_and(|editor| {
                    let editor = editor.read(cx);
                    editor.active
                        && editor
                            .fields
                            .values()
                            .any(|field| field.input.read(cx).is_focused(window))
                })
        })
    }

    pub(super) fn retain_equalizer_panels(
        &mut self,
        layout: &super::super::workspace::WorkspacePanels,
        visible: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some((panel, target)) = self.equalizer_editors.active
            && (!visible || !layout.is_panel_active(panel))
        {
            self.equalizer_editors.active = None;
            if let Some(editor) = self.equalizer_editors.editors.get(&target) {
                editor.update(cx, |editor, cx| editor.deactivate(cx));
            }
        }
    }
}

fn apply_equalizer_draft_edits(
    config: &mut EqualizerConfig,
    edits: impl IntoIterator<Item = EqualizerEdit>,
    sample_rate: Option<u32>,
) -> Result<(), String> {
    let mut resize = None;
    for edit in edits {
        if let EqualizerEdit::ResizeBands { count } = edit {
            resize = Some(count);
            continue;
        }
        let _ = edit
            .apply_config(config, sample_rate)
            .map_err(|error| error.to_string())?;
    }
    if let Some(count) = resize {
        EqualizerEdit::ResizeBands { count }
            .apply_config(config, sample_rate)
            .map_err(|error| error.to_string())?;
    }
    config.validate().map_err(|error| error.to_string())
}

impl AudioProcessing {
    pub(super) fn node_equalizer_dirty_count(&self, cx: &App) -> usize {
        self.equalizer_editors
            .editors
            .iter()
            .filter(|(target, _)| matches!(target, EqTarget::Node(_)))
            .map(|(_, editor)| {
                editor
                    .read(cx)
                    .fields
                    .values()
                    .filter(|field| field.dirty)
                    .count()
            })
            .sum()
    }

    pub(super) fn node_equalizer_dirty_count_for(&self, id: u64, cx: &App) -> usize {
        self.equalizer_editors
            .editors
            .get(&EqTarget::Node(id))
            .map_or(0, |editor| {
                editor
                    .read(cx)
                    .fields
                    .values()
                    .filter(|field| field.dirty)
                    .count()
            })
    }

    pub(super) fn prepare_node_equalizer_drafts(
        &self,
        id: u64,
        base: &EqualizerConfig,
        cx: &App,
    ) -> Result<Option<NodeEqualizerSubmission>, String> {
        let revision = self.state.system.config.processing_graph.revision;
        let Some(entity) = self.equalizer_editors.editors.get(&EqTarget::Node(id)) else {
            return Ok(None);
        };
        let editor = entity.read(cx);
        let mut edits = Vec::new();
        let mut fields = Vec::new();
        for (key, field) in &editor.fields {
            if !field.dirty {
                continue;
            }
            if field.draft_revision != Some(revision)
                || editor.revision != revision
                || &editor.config != base
            {
                return Err(self.context.i18n.text("processing-editor-stale").to_owned());
            }
            let text = field.input.read(cx).text().trim().to_owned();
            edits.push(editor.field_edit(key, &text)?);
            fields.push(SubmittedField {
                editor: entity.downgrade(),
                key: *key,
                identity: field.identity,
                generation: field.generation,
                text,
            });
        }
        if fields.is_empty() {
            return Ok(None);
        }
        let mut config = base.clone();
        let sample_rate = self
            .state
            .equalizer_format()
            .map(|format| format.sample_rate);
        apply_equalizer_draft_edits(&mut config, edits, sample_rate)?;
        Ok(Some(NodeEqualizerSubmission {
            config,
            field_submit: false,
            revision,
            node: id,
            fields,
        }))
    }

    pub(super) fn finish_node_equalizer_submission(
        &mut self,
        submitted: NodeEqualizerSubmission,
        success: bool,
        cx: &mut Context<Self>,
    ) {
        let revision = self.state.system.config.processing_graph.revision;
        if !success || submitted.revision.checked_add(1) != Some(revision) {
            if let Some(editor) = self
                .equalizer_editors
                .editors
                .get(&EqTarget::Node(submitted.node))
            {
                editor.update(cx, |editor, cx| editor.cancel_pointer(cx));
            }
            return;
        }
        self.rebase_node_equalizer_drafts(submitted.node, submitted.revision, revision, cx);
        for field in submitted.fields {
            self.finish_equalizer_field(field, true, cx);
        }
        cx.notify();
    }

    pub(super) fn discard_node_equalizer_drafts(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(editor) = self.equalizer_editors.editors.get(&EqTarget::Node(id)) {
            editor.update(cx, |editor, cx| {
                editor.cancel_pointer(cx);
                for field in editor.fields.values_mut().filter(|field| field.dirty) {
                    field.dirty = false;
                    field.draft_revision = None;
                    field.generation = field.generation.wrapping_add(1);
                    let text = field.committed.clone();
                    field.input.update(cx, |input, cx| input.set_text(text, cx));
                }
                editor.error = None;
                editor.prune_inputs();
                cx.notify();
            });
        }
        cx.notify();
    }
}

impl AudioProcessing {
    pub(super) fn finish_equalizer_field(
        &mut self,
        submitted: SubmittedField,
        success: bool,
        cx: &mut Context<Self>,
    ) {
        if !success {
            return;
        }
        let state = Rc::clone(&self.state);
        let pending = self.pending;
        let epoch = self.geometry_epoch;
        let format = state.equalizer_format();
        let collecting = self.collecting_equalizer_target();
        let _ = submitted.editor.update(cx, |editor, cx| {
            let Some(config) = target_config(&state, editor.target) else {
                return;
            };
            editor.reconcile(
                Rc::clone(&state),
                config,
                EqAvailability {
                    blocked: editor.blocked,
                    pending,
                    format,
                    collecting_revision: collecting
                        .filter(|(pending_target, _)| *pending_target == editor.target)
                        .map(|(_, revision)| revision),
                },
                epoch,
                cx,
            );
            let still_authoritative = editor
                .field_edit(&submitted.key, &submitted.text)
                .ok()
                .is_some_and(|edit| equalizer_field_matches_config(&edit, config));
            if let Some(field) = editor.fields.get_mut(&submitted.key)
                && field.identity == submitted.identity
                && field.generation == submitted.generation
                && field.input.read(cx).text().trim() == submitted.text
            {
                field.dirty = false;
                field.draft_revision = None;
                if !still_authoritative {
                    let text = field.committed.clone();
                    field.input.update(cx, |input, cx| input.set_text(text, cx));
                }
                cx.notify();
            }
            editor.prune_inputs();
        });
    }

    pub(super) fn deactivate_equalizer_panel(
        &mut self,
        panel: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some((active_panel, target)) = self.equalizer_editors.active
            && active_panel == panel
            && let Some(editor) = self.equalizer_editors.editors.get(&target)
        {
            self.equalizer_editors.active = None;
            let focused = editor
                .read(cx)
                .fields
                .values()
                .any(|field| field.input.read(cx).is_focused(window));
            if focused {
                window.blur(cx);
            }
            editor.update(cx, |editor, cx| {
                editor.deactivate(cx);
            });
        }
    }
}

fn equalizer_field_matches_config(edit: &EqualizerEdit, config: &EqualizerConfig) -> bool {
    match edit {
        EqualizerEdit::PreampDb(value) => *value == config.preamp_db,
        EqualizerEdit::OutputGainDb(value) => *value == config.output_gain_db,
        EqualizerEdit::ResizeBands { count } => *count == config.bands.len(),
        EqualizerEdit::BandField { index, field } => {
            config.bands.get(*index).is_some_and(|band| match field {
                EqBandEdit::FrequencyHz(value) => *value == band.frequency_hz,
                EqBandEdit::GainDb(value) => *value == band.gain_db,
                EqBandEdit::Q(value) => *value == band.q,
                EqBandEdit::ShelfSlope(value) => *value == band.shelf_slope,
                EqBandEdit::Stages(value) => *value == band.stages,
                EqBandEdit::Channels(value) => value == &band.channels,
                _ => false,
            })
        }
        _ => false,
    }
}
fn equalizer_number_text(key: &EqField, value: f64) -> String {
    let precision = match key {
        EqField::Band(_, BandField::FrequencyHz) => 1,
        EqField::Band(_, BandField::Stages) => 0,
        EqField::Band(_, BandField::Q | BandField::ShelfSlope) => 3,
        _ => 2,
    };
    let mut text = format!("{value:.precision$}");
    if precision > 0 {
        text.truncate(text.trim_end_matches('0').trim_end_matches('.').len());
    }
    if text == "-0" {
        text.remove(0);
    }
    text
}

fn slider_fraction(value: f64, (minimum, maximum, logarithmic): (f64, f64, bool)) -> f32 {
    let value = value.clamp(minimum, maximum);
    if logarithmic {
        ((value / minimum).ln() / (maximum / minimum).ln()) as f32
    } else {
        ((value - minimum) / (maximum - minimum)) as f32
    }
}

fn slider_number(
    fraction: f32,
    (minimum, maximum, logarithmic): (f64, f64, bool),
    integer: bool,
) -> f64 {
    let fraction = f64::from(fraction.clamp(0., 1.));
    let value = if logarithmic {
        minimum * (maximum / minimum).powf(fraction)
    } else {
        minimum + (maximum - minimum) * fraction
    };
    if integer {
        value.round()
    } else {
        value.clamp(minimum, maximum)
    }
}

fn graph_minimum_frequency(maximum: f64) -> f64 {
    if maximum > 20. { 20. } else { 1. }
}

fn graph_frequency_boundary(frequency: f64, maximum: f64) -> Option<&'static str> {
    if frequency < graph_minimum_frequency(maximum) {
        Some("processing-frequency-below-graph")
    } else if frequency > maximum {
        Some("processing-frequency-above-graph")
    } else {
        None
    }
}

fn reset_equalizer_config(config: &EqualizerConfig) -> EqualizerConfig {
    EqualizerConfig {
        enabled: config.enabled,
        ..Default::default()
    }
}

fn rebase_draft_origin(origin: &mut Option<u64>, from: u64, to: u64, current: u64) {
    if from.checked_add(1) == Some(to) && to == current && *origin == Some(from) {
        *origin = Some(to);
    }
}

impl AudioProcessing {
    pub(super) fn rebase_node_equalizer_drafts(
        &mut self,
        id: u64,
        from: u64,
        to: u64,
        cx: &mut Context<Self>,
    ) {
        let current = self.state.system.config.processing_graph.revision;
        if from.checked_add(1) != Some(to) || to != current {
            return;
        }
        if let Some(editor) = self.equalizer_editors.editors.get(&EqTarget::Node(id)) {
            editor.update(cx, |editor, _| {
                for field in editor.fields.values_mut().filter(|field| field.dirty) {
                    rebase_draft_origin(&mut field.draft_revision, from, to, current);
                }
            });
        }
    }
}

fn replaces_band_identity(edit: &EqualizerEdit) -> bool {
    matches!(
        edit,
        EqualizerEdit::Band { .. }
            | EqualizerEdit::Replace { .. }
            | EqualizerEdit::LoadPreset { .. }
    )
}

impl AudioProcessing {
    pub(super) fn begin_equalizer_command(&mut self, command: &Command) {
        self.equalizer_editors.loading_preset = match command {
            Command::EqualizerEdit {
                edit: EqualizerEdit::LoadPreset { name },
                ..
            } => self
                .context
                .snapshot()
                .system
                .config
                .equalizer_presets
                .iter()
                .find(|preset| preset.name == *name)
                .map(|preset| preset.config.clone()),
            _ => None,
        };
        self.equalizer_editors.reset_band_focus = match command {
            Command::EqualizerEdit { edit, .. } if replaces_band_identity(edit) => {
                Some(EqTarget::Monitor)
            }
            Command::ProcessingGraphEdit {
                expected_revision,
                edit,
            } if matches!(
                edit.as_ref(),
                GraphEdit::Replace { id, .. }
                    if self.equalizer_editors.pending_node_focus_reset == Some((*id, *expected_revision))
            ) =>
            {
                self.equalizer_editors
                    .pending_node_focus_reset
                    .take()
                    .map(|(id, _)| EqTarget::Node(id))
            }
            _ => None,
        };
        if let Command::EqualizerEdit {
            edit: EqualizerEdit::RemoveBand { index },
            ..
        } = command
        {
            self.equalizer_editors.removing.insert(
                EqTarget::Monitor,
                (self.state.system.config.equalizer.clone(), *index),
            );
        }
    }

    pub(super) fn finish_equalizer_command(&mut self, success: bool) {
        if let Some(preset) = self.equalizer_editors.loading_preset.take()
            && success
        {
            self.equalizer_editors.loaded_preset = Some(preset);
        }
        if !success {
            self.equalizer_editors.removing.clear();
            self.equalizer_editors.reset_band_focus = None;
        }
    }
}
#[cfg(test)]
mod tests {
    use super::test_support::{click, draw};
    use super::*;
    use crate::test_support::CoreRuntime;
    use core::prelude::v1::test;

    const PANEL: u64 = 7;

    fn test_format(sample_rate: u32, channels: usize) -> EqualizerFormat {
        EqualizerFormat {
            track_id: 99,
            generation: 0,
            sample_rate,
            channels,
        }
    }

    #[gpui::test]
    fn offline_midi_snapshot_allows_eq_edits_without_fabricating_response(cx: &mut TestAppContext) {
        let (_directory, _runtime, owner, editor, _handle) = fixture_with_owner(cx);
        let mut track = rivu_protocol::test_support::artwork_track("source.mid".into());
        track.sample_rate = 0;
        track.channels = 0;
        let mut client = rivu_protocol::model::ClientSnapshot {
            current_track: Some(Arc::new(track)),
            source_generation: 42,
            ..Default::default()
        };
        client.configuration.config =
            owner.read_with(cx, |owner, _| Arc::clone(&owner.state.system.config));
        let snapshot = GuiSnapshot::from_client(&client, Default::default());
        let format = snapshot.equalizer_format().unwrap();
        assert_eq!(format.track_id, client.current_track.as_ref().unwrap().id);
        assert_eq!(format.generation, 42);
        assert_eq!(format.known_sample_rate(), None);
        owner.update(cx, |owner, cx| owner.reconcile(Rc::new(snapshot), cx));
        let epoch = editor.read_with(cx, |editor, _| {
            let key = EqField::Band(0, BandField::FrequencyHz);
            assert!(editor.field_input_enabled(&key));
            let mut config = editor.config.clone();
            let count = config.bands.len() + 1;
            editor
                .field_edit(&EqField::BandCount, &count.to_string())
                .unwrap()
                .apply_config(&mut config, Some(0))
                .unwrap();
            assert_eq!(config.bands.len(), count);
            assert!((20.0..=20_000.0).contains(&config.bands.last().unwrap().frequency_hz));
            editor
                .field_edit(&key, "1200")
                .unwrap()
                .apply_config(&mut config, Some(0))
                .unwrap();
            assert_eq!(config.bands[0].frequency_hz, 1200.);
            assert!(editor.response.is_empty());
            assert_eq!(editor.response_computations, 0);
            assert!(editor.response_error.is_none());
            editor.input_epoch
        });
        client.source_generation += 1;
        owner.update(cx, |owner, cx| {
            owner.reconcile(
                Rc::new(GuiSnapshot::from_client(&client, Default::default())),
                cx,
            );
        });
        let epoch = editor.read_with(cx, |editor, _| {
            assert_ne!(editor.input_epoch, epoch);
            assert!(editor.response.is_empty());
            editor.input_epoch
        });
        Arc::make_mut(client.current_track.as_mut().unwrap()).id += 1;
        owner.update(cx, |owner, cx| {
            owner.reconcile(
                Rc::new(GuiSnapshot::from_client(&client, Default::default())),
                cx,
            );
        });
        editor.read_with(cx, |editor, _| {
            assert_ne!(editor.input_epoch, epoch);
            assert!(editor.response.is_empty());
            assert_eq!(editor.response_computations, 0);
        });
    }

    #[gpui::test]
    fn midi_eq_uses_current_decoded_rate_and_channels(cx: &mut TestAppContext) {
        use rivu_protocol::model::{
            AudioOutputState, AudioSourceFormat, AudioStreamFormat, PlaybackStatus,
            ReplayGainStatus,
        };

        let (_directory, _runtime, owner, editor, handle) = fixture_with_owner(cx);
        let mut state = owner.read_with(cx, |owner, _| (*owner.state).clone());
        let mut track = rivu_protocol::test_support::artwork_track("source.mid".into());
        track.sample_rate = 0;
        track.channels = 0;
        let track = Arc::new(track);
        state.current_track = Some(Arc::clone(&track));
        state.source_generation = 42;
        state.playback.status = PlaybackStatus::Paused;
        state.playback.audio_output = Some(Arc::new(AudioOutputState {
            source_generation: 41,
            segment: 1,
            active: true,
            source: Arc::new(AudioSourceFormat {
                codec: "MIDI".into(),
                sample_rate: 48_000,
                bits_per_sample: None,
                channels: vec!["FL".into(), "FR".into()],
                processed: false,
            }),
            stream: Arc::new(AudioStreamFormat {
                sample_rate: 96_000,
                sample_format: "F32".into(),
                channels: vec!["Mono".into()],
            }),
            graph_processing: false,
            monitor_eq: true,
            master_gain_db: None,
            source_loudness_lufs: None,
            replaygain: ReplayGainStatus {
                enabled: false,
                peak_policy: rivu_format::config::ReplayGainPeakPolicy::Off,
                requested_gain_db: None,
                applied_gain_db: 0.0,
                source_peak: None,
                processed_source: false,
            },
            sink: None,
            remix_allowed: false,
            stream_resample_quality: None,
            graph_rate: None,
            quantum_frames: None,
            device_rate: None,
            device_format: None,
            metrics: Default::default(),
            session_underruns: 0,
            session_missing_seconds: 0.0,
        }));
        owner.update(cx, |owner, cx| owner.reconcile(Rc::new(state.clone()), cx));
        assert!(editor.read_with(cx, |editor, _| editor.response.is_empty()));

        Arc::make_mut(state.playback.audio_output.as_mut().unwrap()).source_generation = 42;
        owner.update(cx, |owner, cx| owner.reconcile(Rc::new(state.clone()), cx));
        let computations = editor.read_with(cx, |editor, _| {
            assert_eq!(editor.maximum_frequency, 48_000. * 0.499);
            assert!(!editor.response.is_empty());
            assert!(Arc::ptr_eq(
                editor.state.current_track.as_ref().unwrap(),
                &track
            ));
            editor.response_computations
        });
        draw(cx, handle);
        assert!(
            VisualTestContext::from_window(handle, cx)
                .debug_bounds("eq-response-channel-7")
                .is_some()
        );

        Arc::make_mut(state.playback.audio_output.as_mut().unwrap()).segment += 1;
        owner.update(cx, |owner, cx| owner.reconcile(Rc::new(state.clone()), cx));
        assert_eq!(
            editor.read_with(cx, |editor, _| editor.response_computations),
            computations
        );

        state.playback.status = PlaybackStatus::Stopped;
        owner.update(cx, |owner, cx| owner.reconcile(Rc::new(state), cx));
        assert!(editor.read_with(cx, |editor, _| editor.response.is_empty()));
        draw(cx, handle);
        assert!(
            VisualTestContext::from_window(handle, cx)
                .debug_bounds("eq-response-channel-7")
                .is_none()
        );
    }

    #[gpui::test]
    fn eq_switch_locks_only_eq_edits_while_master_bypass_preserves_editability(
        cx: &mut TestAppContext,
    ) {
        let (_directory, runtime, owner, editor, handle) = fixture_with_owner(cx);
        let original = runtime.handle.config_snapshot().equalizer.clone();
        assert!(
            futures::executor::block_on(
                runtime
                    .handle
                    .request_command(Command::ProcessingBypass { bypassed: true })
            )
            .is_success()
        );
        owner.update(cx, |owner, cx| {
            owner.reconcile(owner.context.snapshot(), cx);
            owner.command_active = true;
        });
        draw(cx, handle);
        editor.read_with(cx, |editor, cx| {
            assert_eq!(editor.config, original);
            assert!(!editor.input_locked());
            assert!(!editor.reset_locked(cx));
        });
        click(cx, handle, "eq-enable-7");
        owner.read_with(cx, |owner, _| {
            assert!(matches!(
                owner.commands.front().unwrap().command,
                Command::EqualizerEdit {
                    expected_revision: _,
                    edit: EqualizerEdit::Enabled(false)
                }
            ));
        });
        acknowledge_front(cx, &runtime, &owner, true);
        draw(cx, handle);
        editor.update(cx, |editor, cx| {
            assert!(editor.input_locked());
            assert!(editor.reset_locked(cx));
            editor.eq(EqualizerEdit::PreampDb(3.5), cx);
        });
        assert!(owner.read_with(cx, |owner, _| owner.commands.is_empty()));
        let mut expected = original;
        expected.enabled = false;
        assert_eq!(runtime.handle.config_snapshot().equalizer, expected);
        assert!(runtime.handle.config_snapshot().processing_bypassed);

        click(cx, handle, "eq-enable-7");
        acknowledge_front(cx, &runtime, &owner, true);
        draw(cx, handle);
        editor.update(cx, |editor, cx| editor.eq(EqualizerEdit::PreampDb(3.5), cx));
        acknowledge_front(cx, &runtime, &owner, true);
        assert_eq!(runtime.handle.config_snapshot().equalizer.preamp_db, 3.5);
        assert!(runtime.handle.config_snapshot().processing_bypassed);
    }

    #[gpui::test]
    fn off_curve_bands_keep_real_frequencies_and_boundary_overlap_cycling(cx: &mut TestAppContext) {
        let (_directory, runtime, owner, editor, handle) = fixture_with_owner(cx);
        let config = EqualizerConfig {
            enabled: true,
            bands: [1., 10., 96_000.]
                .into_iter()
                .map(|frequency_hz| EqBand {
                    frequency_hz,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        assert!(
            futures::executor::block_on(runtime.handle.request_command(Command::EqualizerEdit {
                expected_revision: runtime.handle.gui_snapshot().revisions.configuration,
                edit: EqualizerEdit::Replace {
                    config: config.clone()
                },
            }))
            .is_success()
        );
        owner.update(cx, |owner, cx| {
            owner.command_active = true;
            owner.reconcile(owner.context.snapshot(), cx);
        });
        draw(cx, handle);
        assert!(
            VisualTestContext::from_window(handle, cx)
                .debug_bounds("eq-frequency-boundary-7")
                .is_some()
        );
        editor.read_with(cx, |editor, _| {
            assert_eq!(editor.frequency_x(1.), 0.);
            assert_eq!(editor.frequency_x(10.), 0.);
            assert_eq!(editor.frequency_x(96_000.), 1.);
        });
        click(cx, handle, "eq-node-7-0");
        assert_eq!(editor.read_with(cx, |editor, _| editor.band), 1);
        click(cx, handle, "eq-node-7-1");
        assert_eq!(editor.read_with(cx, |editor, _| editor.band), 0);
        click(cx, handle, "eq-node-7-2");
        editor.read_with(cx, |editor, cx| {
            assert_eq!(editor.band, 2);
            assert_eq!(
                editor.fields[&EqField::Band(2, BandField::FrequencyHz)]
                    .input
                    .read(cx)
                    .text(),
                "96000"
            );
        });
        assert_eq!(
            runtime.handle.gui_snapshot().system.config.equalizer,
            config
        );
        assert!(owner.read_with(cx, |owner, _| owner.commands.is_empty()));
    }

    #[test]
    fn band_slider_mapping_round_trips_and_clamps_supported_parameters() {
        for range in [
            (1., 1.497, true),
            (1., 96_000., true),
            (1., 23_952., true),
            (-24., 24., false),
            (0.1, 20., true),
            (0.1, 1., false),
        ] {
            for fraction in [0., 0.25, 0.5, 0.75, 1.] {
                let value = slider_number(fraction, range, false);
                assert!((slider_fraction(value, range) - fraction).abs() < 1e-6);
                assert!((range.0..=range.1).contains(&value));
            }
            assert_eq!(slider_number(-1., range, false), range.0);
            assert!((slider_number(2., range, false) - range.1).abs() < 1e-8);
        }
        assert_eq!(slider_number(0.5, (1., 4., false), true), 3.);
    }

    #[gpui::test]
    fn frequency_controls_distinguish_unknown_and_low_stream_rates(cx: &mut TestAppContext) {
        for rate in [None, Some(0), Some(1), Some(2), Some(3), Some(48_001)] {
            let (_directory, _runtime, owner, editor, handle) = fixture_with_owner(cx);
            editor.update(cx, |editor, cx| {
                editor.format = rate.map(|sample_rate| EqualizerFormat {
                    track_id: 99,
                    generation: 0,
                    sample_rate,
                    channels: 2,
                });
                editor.sync_fields(cx);
                editor.update_response(cx);
                cx.notify();
            });
            draw(cx, handle);
            let input = editor.read_with(cx, |editor, _| {
                editor.fields[&EqField::Band(0, BandField::FrequencyHz)]
                    .input
                    .clone()
            });
            if matches!(rate, Some(1..=2)) {
                assert!(
                    VisualTestContext::from_window(handle, cx)
                        .debug_bounds("eq-slider-band.0.frequency_hz")
                        .is_none()
                );
                assert!(
                    VisualTestContext::from_window(handle, cx)
                        .debug_bounds("eq-node-7-0")
                        .is_none()
                );
                enter_number(cx, handle, &input, "1");
                press(cx, handle, "enter");
                click(cx, handle, "eq-step-band.0.frequency_hz-more");
                assert_eq!(
                    input.read_with(cx, |input, _| input.text().to_owned()),
                    "80"
                );
                editor.read_with(cx, |editor, _| {
                    assert_eq!(
                        editor.response_error.as_deref(),
                        Some(editor.context.i18n.text("processing-response-unknown"))
                    );
                    assert!(editor.field_edit(&EqField::BandCount, "4").is_err());
                    assert!(editor.field_edit(&EqField::BandCount, "2").is_ok());
                });
            } else {
                let maximum = rate
                    .filter(|rate| *rate > 0)
                    .map_or(96_000., |rate| f64::from(rate) * 0.499);
                editor.read_with(cx, |editor, _| {
                    let key = EqField::Band(0, BandField::FrequencyHz);
                    assert_eq!(editor.band_numeric_range(&key), Some((1., maximum, true)));
                    let endpoint = editor.preview_number_text(&key, maximum);
                    assert_eq!(endpoint.parse::<f64>().unwrap(), maximum);
                    assert!(editor.field_edit(&key, &endpoint).is_ok());
                });
                if rate == Some(3) {
                    editor.read_with(cx, |editor, _| {
                        assert!(editor.field_edit(&EqField::BandCount, "4").is_err());
                        assert!(editor.field_edit(&EqField::BandCount, "2").is_ok());
                    });
                }
                if rate.is_some_and(|rate| rate > 0) {
                    let node = VisualTestContext::from_window(handle, cx)
                        .debug_bounds("eq-node-7-0")
                        .unwrap();
                    assert!(f32::from(node.origin.x).is_finite());
                    assert!(f32::from(node.origin.y).is_finite());
                } else {
                    assert!(editor.read_with(cx, |editor, _| editor.response.is_empty()));
                }
            }
            assert!(owner.read_with(cx, |owner, _| owner.commands.is_empty()));
        }
    }

    #[gpui::test]
    fn low_rate_curve_pointer_keeps_preview_in_the_supported_frequency_interval(
        cx: &mut TestAppContext,
    ) {
        let (_directory, _runtime, owner, editor, handle) = fixture_with_owner(cx);
        owner.update(cx, |owner, _| {
            owner.command_active = true;
        });
        editor.update(cx, |editor, cx| {
            editor.format = Some(test_format(3, 2));
            editor.sync_fields(cx);
            editor.update_response(cx);
            cx.notify();
        });
        draw(cx, handle);
        let position = VisualTestContext::from_window(handle, cx)
            .debug_bounds("eq-node-7-0")
            .unwrap()
            .center();
        dispatch_pointer_event(
            cx,
            handle,
            MouseDownEvent {
                button: MouseButton::Left,
                position,
                ..Default::default()
            },
        );
        dispatch_pointer_event(
            cx,
            handle,
            MouseMoveEvent {
                position: position + point(px(-100.), px(0.)),
                pressed_button: Some(MouseButton::Left),
                ..Default::default()
            },
        );
        cx.update_window(handle, |_, window, cx| window.simulate_next_frame(cx))
            .unwrap();
        editor.read_with(cx, |editor, cx| {
            let frequency = editor.fields[&EqField::Band(0, BandField::FrequencyHz)]
                .input
                .read(cx)
                .text()
                .parse::<f64>()
                .unwrap();
            assert!((1.0..=1.497).contains(&frequency));
            assert!(editor.frequency_x(frequency).is_finite());
        });
        press(cx, handle, "escape");
        assert!(owner.read_with(cx, |owner, _| owner.commands.is_empty()));
    }

    #[gpui::test]
    fn retired_slider_events_cannot_edit_a_replacement_or_hidden_band(cx: &mut TestAppContext) {
        for transition in [
            "remove", "remap", "select", "hide", "layout", "reopen", "block", "revision",
            "geometry", "kind",
        ] {
            let (_directory, runtime, owner, editor, handle) = fixture_with_owner(cx);
            owner.update(cx, |owner, _| {
                owner.command_active = true;
            });
            if transition == "remap" {
                click(cx, handle, "eq-node-7-2");
            }
            let selector = if transition == "remap" {
                "eq-slider-band.2.gain_db"
            } else {
                "eq-slider-band.0.gain_db"
            };
            let position = VisualTestContext::from_window(handle, cx)
                .debug_bounds(selector)
                .unwrap()
                .center();
            let (retired_slider, retired_step) = editor.read_with(cx, |editor, _| {
                let key = EqField::Band(editor.band, BandField::GainDb);
                (
                    EqualizerEditor::slider_action(
                        editor.field_control(key),
                        editor.fields[&key].slider_bounds.clone(),
                    ),
                    EqualizerEditor::step_action(editor.field_control(key), 0.5),
                )
            });
            // Invoke the exact production callbacks with their painted identity.
            // Native key dispatch draws a dirty window synchronously, replacing
            // listeners before delivery; those would be fresh, not retired events.
            cx.update_window(handle, |_, window, cx| {
                match transition {
                    "remove" | "remap" | "kind" => {
                        let edit = if transition == "kind" {
                            EqualizerEdit::BandField {
                                index: 0,
                                field: EqBandEdit::Kind(FilterKind::LowPass),
                            }
                        } else {
                            EqualizerEdit::RemoveBand { index: 0 }
                        };
                        assert!(
                            futures::executor::block_on(runtime.handle.request_command(
                                Command::EqualizerEdit {
                                    expected_revision:
                                        runtime.handle.gui_snapshot().revisions.configuration,
                                    edit
                                }
                            ))
                            .is_success()
                        );
                        owner.update(cx, |owner, cx| {
                            owner.reconcile(owner.context.snapshot(), cx)
                        });
                    }
                    "select" => {
                        editor.update(cx, |editor, cx| editor.focus_graph_band(1, window, cx))
                    }
                    "hide" => owner.update(cx, |owner, cx| {
                        owner.deactivate_equalizer_panel(PANEL, window, cx)
                    }),
                    "layout" | "reopen" => owner.update(cx, |owner, cx| {
                        if transition == "reopen" {
                            owner.sync_panels(&Default::default(), false, cx);
                        }
                        let state = Rc::clone(&owner.state);
                        let rebound = owner.equalizer_editor(
                            PANEL + 1,
                            EqTarget::Monitor,
                            &state.system.config.equalizer,
                            window,
                            cx,
                        );
                        assert_eq!(rebound.entity_id(), editor.entity_id());
                        assert_eq!(owner.equalizer_editors.editors.len(), 1);
                    }),
                    "block" | "revision" => editor.update(cx, |editor, cx| {
                        let mut state = Rc::clone(&editor.state);
                        if transition == "revision" {
                            Arc::make_mut(&mut Rc::make_mut(&mut state).system.config)
                                .processing_graph
                                .revision += 1;
                        }
                        let config = editor.config.clone();
                        editor.reconcile(
                            state,
                            &config,
                            EqAvailability {
                                blocked: transition == "block",
                                pending: 0,
                                format: editor.format,
                                collecting_revision: None,
                            },
                            editor.geometry_epoch,
                            cx,
                        );
                    }),
                    "geometry" => editor.update(cx, |editor, _| editor.geometry_epoch += 1),
                    _ => unreachable!(),
                }
                let before = editor
                    .read(cx)
                    .fields
                    .iter()
                    .map(|(key, field)| (*key, field.input.read(cx).text().to_owned()))
                    .collect::<HashMap<_, _>>();
                editor.update(cx, |editor, cx| {
                    for key in [SliderKey::Increase, SliderKey::End] {
                        retired_slider(editor, SliderInput::Key(key), window, cx);
                    }
                    retired_slider(
                        editor,
                        SliderInput::PointerDown {
                            position,
                            alt: false,
                        },
                        window,
                        cx,
                    );
                    retired_step(editor, window, cx);
                    assert!(editor.field_drag.is_none());
                    assert_eq!(editor.pointer_count.get(), 0);
                });
                let after = editor
                    .read(cx)
                    .fields
                    .iter()
                    .map(|(key, field)| (*key, field.input.read(cx).text().to_owned()))
                    .collect::<HashMap<_, _>>();
                assert_eq!(before, after, "retired slider after {transition}");
            })
            .unwrap();
            cx.run_until_parked();
            assert!(
                owner.read_with(cx, |owner, _| owner.commands.is_empty()),
                "retired slider after {transition}"
            );
            // Reopen/reconcile the current projection, then prove that newly
            // painted native slider and step events still edit and submit.
            cx.update_window(handle, |_, window, cx| {
                owner.update(cx, |owner, cx| {
                    owner.pending = 0;
                    owner.reconcile(owner.context.snapshot(), cx);
                    let config = owner.state.system.config.equalizer.clone();
                    assert_eq!(
                        owner.equalizer_editor(PANEL, EqTarget::Monitor, &config, window, cx),
                        editor
                    );
                });
                editor.update(cx, |editor, cx| editor.focus_graph_band(0, window, cx));
            })
            .unwrap();
            draw(cx, handle);
            click(cx, handle, "eq-slider-band.0.frequency_hz");
            if !matches!(transition, "remap" | "reopen") {
                assert_eq!(
                    owner.read_with(cx, |owner, _| owner.commands.len()),
                    1,
                    "fresh pointer submission after {transition}"
                );
                continue;
            }
            acknowledge_front(cx, &runtime, &owner, true);
            draw(cx, handle);
            press(cx, handle, "end");
            editor.read_with(cx, |editor, cx| {
                assert_eq!(
                    editor.fields[&EqField::Band(0, BandField::FrequencyHz)]
                        .input
                        .read(cx)
                        .text(),
                    "96000",
                    "fresh slider after {transition}"
                );
            });
            acknowledge_front(cx, &runtime, &owner, true);
            draw(cx, handle);
            click(cx, handle, "eq-step-band.0.frequency_hz-less");
            editor.read_with(cx, |editor, cx| {
                assert_eq!(
                    editor.fields[&EqField::Band(0, BandField::FrequencyHz)]
                        .input
                        .read(cx)
                        .text(),
                    "95999",
                    "fresh step after {transition}"
                );
            });
            acknowledge_front(cx, &runtime, &owner, true);
            assert!(owner.read_with(cx, |owner, _| owner.commands.is_empty()));
            assert_eq!(
                runtime.handle.gui_snapshot().system.config.equalizer.bands[0].frequency_hz,
                95_999.,
                "fresh frequency must reach Core after {transition}"
            );
        }
    }

    #[gpui::test]
    fn numeric_display_does_not_quantize_untouched_configuration_or_user_text(
        cx: &mut TestAppContext,
    ) {
        let (_directory, runtime, owner, editor, handle) = fixture_with_owner(cx);
        owner.update(cx, |owner, _| owner.command_active = true);
        let input = editor.read_with(cx, |editor, _| {
            assert_eq!(editor.config.bands[0].q, std::f64::consts::FRAC_1_SQRT_2);
            editor.fields[&EqField::Band(0, BandField::Q)].input.clone()
        });
        editor.update(cx, |editor, cx| {
            editor.submit_field(&EqField::Band(0, BandField::Q), cx)
        });
        draw(cx, handle);
        assert!(owner.read_with(cx, |owner, _| owner.commands.is_empty()));
        let text = "0.8123456789";
        enter_number(cx, handle, &input, text);
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            text
        );
        assert!(owner.read_with(cx, |owner, _| owner.commands.is_empty()));
        press(cx, handle, "enter");
        owner.read_with(cx, |owner, _| {
            assert!(matches!(&owner.commands.front().unwrap().command,
                Command::EqualizerEdit { edit: EqualizerEdit::BandField { index: 0, field: EqBandEdit::Q(value) }, .. } if *value == 0.8123456789));
        });
        acknowledge_front(cx, &runtime, &owner, true);
        editor.read_with(cx, |editor, cx| {
            assert!(!editor.fields[&EqField::Band(0, BandField::Q)].dirty);
            assert_eq!(
                editor.fields[&EqField::Band(0, BandField::Q)]
                    .input
                    .read(cx)
                    .text(),
                text
            );
            assert_eq!(editor.config.bands[0].q, 0.8123456789);
        });
    }

    #[gpui::test]
    fn band_numbers_validate_and_submit_only_on_enter(cx: &mut TestAppContext) {
        let (_directory, _runtime, owner, editor, handle) = fixture_with_owner(cx);
        owner.update(cx, |owner, _| owner.command_active = true);
        let input = editor.read_with(cx, |editor, _| {
            editor.fields[&EqField::Band(0, BandField::Q)].input.clone()
        });
        for (text, expected) in [("-", 0), ("21", 0), ("1e", 0), ("2.5", 1)] {
            enter_number(cx, handle, &input, text);
            assert!(owner.read_with(cx, |owner, _| owner.commands.is_empty()));
            press(cx, handle, "enter");
            assert_eq!(
                owner.read_with(cx, |owner, _| owner.commands.len()),
                expected
            );
        }
        owner.read_with(cx, |owner, _| {
            assert!(matches!(&owner.commands.front().unwrap().command,
                Command::EqualizerEdit { edit: EqualizerEdit::BandField { index: 0, field: EqBandEdit::Q(value) }, .. } if *value == 2.5));
        });
    }

    #[gpui::test]
    fn graph_drag_links_numeric_preview_and_cancellation_restores_committed_text(
        cx: &mut TestAppContext,
    ) {
        use gpui::InputEvent as _;
        let (_directory, _runtime, owner, editor, handle) = fixture_with_owner(cx);
        use crate::gui::layout::{Layout, Node, Panel};
        let mut panels = crate::gui::workspace::WorkspacePanels::default();
        let layout = Layout {
            root: Some(Node::Tabs {
                id: 1,
                panels: vec![Panel {
                    id: PANEL,
                    kind: "audio_processing".into(),
                    show_tab_bar: true,
                    audio_visualizer: None,
                    artwork: None,
                }],
                active: PANEL,
            }),
            next_id: PANEL + 1,
            locked: false,
        };
        panels.reconcile(&layout);
        owner.update(cx, |owner, cx| owner.sync_panels(&panels, true, cx));
        owner.update(cx, |owner, _| owner.command_active = true);
        let position = VisualTestContext::from_window(handle, cx)
            .debug_bounds("eq-node-7-0")
            .unwrap()
            .center();
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
            window.dispatch_event(
                MouseMoveEvent {
                    position: position + point(px(40.), px(-18.)),
                    pressed_button: Some(MouseButton::Left),
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
            window.simulate_next_frame(cx);
            editor.update(cx, |editor, cx| {
                let drag = editor.drag.as_ref().unwrap();
                assert!(drag.moved);
                let frequency = editor.fields[&EqField::Band(0, BandField::FrequencyHz)]
                    .input
                    .read(cx)
                    .text()
                    .parse::<f64>()
                    .unwrap();
                let gain = editor.fields[&EqField::Band(0, BandField::GainDb)]
                    .input
                    .read(cx)
                    .text()
                    .parse::<f64>()
                    .unwrap();
                assert!((frequency - drag.frequency).abs() <= 0.05);
                assert!((gain - drag.gain).abs() <= 0.005);
            });
            // Unchanged demand preserves the gesture; hiding the same layout
            // still cancels it even though the structural revision is unchanged.
            owner.update(cx, |owner, cx| owner.sync_panels(&panels, true, cx));
            assert!(editor.read_with(cx, |editor, _| editor.drag.is_some()));
            owner.update(cx, |owner, cx| owner.sync_panels(&panels, false, cx));
            editor.read_with(cx, |editor, _| {
                assert!(!editor.active);
                assert!(editor.drag.is_none());
                assert_eq!(editor.pointer_count.get(), 0);
            });
            // Clean inputs retire while hidden and are rebuilt from committed
            // values when the panel is rendered again.
            owner.update(cx, |owner, cx| {
                owner.sync_panels(&panels, true, cx);
                let _ = owner.equalizer_view(PANEL, window, cx);
            });
            editor.update(cx, |editor, cx| {
                assert!(editor.drag.is_none());
                for key in [
                    &EqField::Band(0, BandField::FrequencyHz),
                    &EqField::Band(0, BandField::GainDb),
                ] {
                    assert_eq!(
                        editor.fields[key].input.read(cx).text(),
                        editor.fields[key].committed
                    );
                }
                assert_eq!(editor.pointer_count.get(), 0);
            });
            // Named layouts may reuse numeric IDs: the published layout revision
            // still retires native identities without replacing the target draft owner.
            let previous_epoch = editor.read(cx).native_epoch;
            panels.reconcile(&layout);
            owner.update(cx, |owner, cx| {
                owner.sync_panels(&panels, true, cx);
                assert!(!editor.read(cx).active);
                assert_ne!(editor.read(cx).native_epoch, previous_epoch);
                let state = Rc::clone(&owner.state);
                let current = owner.equalizer_editor(
                    PANEL,
                    EqTarget::Monitor,
                    &state.system.config.equalizer,
                    window,
                    cx,
                );
                assert_eq!(current.entity_id(), editor.entity_id());
                assert_eq!(owner.equalizer_editors.editors.len(), 1);
            });
        })
        .unwrap();
        assert!(owner.read_with(cx, |owner, _| owner.commands.is_empty()));
    }

    #[gpui::test]
    fn slider_geometry_changes_cancel_capture_without_committing(cx: &mut TestAppContext) {
        use gpui::InputEvent as _;
        let (_directory, _runtime, owner, editor, handle) = fixture_with_owner(cx);
        owner.update(cx, |owner, _| owner.command_active = true);
        let position = VisualTestContext::from_window(handle, cx)
            .debug_bounds("eq-slider-band.0.gain_db")
            .unwrap()
            .center();
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
            editor.update(cx, |editor, cx| {
                assert!(editor.field_drag.is_some());
                editor.geometry_epoch = editor.geometry_epoch.wrapping_add(1);
                editor.move_field_slider(position + point(px(40.), px(0.)), window, cx);
                assert!(editor.field_drag.is_none());
                assert_eq!(editor.pointer_count.get(), 0);
                let field = &editor.fields[&EqField::Band(0, BandField::GainDb)];
                assert_eq!(field.input.read(cx).text(), field.committed);
            });
        })
        .unwrap();
        draw(cx, handle);
        assert!(owner.read_with(cx, |owner, _| owner.commands.is_empty()));
    }

    #[gpui::test]
    fn slider_escape_discards_preview_and_late_release(cx: &mut TestAppContext) {
        let (_directory, _runtime, owner, editor, handle) = fixture_with_owner(cx);
        owner.update(cx, |owner, _| owner.command_active = true);
        let bounds = VisualTestContext::from_window(handle, cx)
            .debug_bounds("eq-slider-band.0.gain_db")
            .unwrap();
        let position = slider_point(bounds, 0.75);
        dispatch_pointer_event(
            cx,
            handle,
            MouseDownEvent {
                button: MouseButton::Left,
                position,
                ..Default::default()
            },
        );
        cx.update_window(handle, |_, window, cx| window.simulate_next_frame(cx))
            .unwrap();
        let key = EqField::Band(0, BandField::GainDb);
        editor.read_with(cx, |editor, cx| {
            assert_eq!(editor.fields[&key].input.read(cx).text(), "12");
        });
        press(cx, handle, "escape");
        dispatch_pointer_event(
            cx,
            handle,
            MouseUpEvent {
                button: MouseButton::Left,
                position,
                ..Default::default()
            },
        );
        editor.read_with(cx, |editor, cx| {
            assert!(editor.field_drag.is_none());
            assert_eq!(editor.fields[&key].input.read(cx).text(), "-8");
        });
        assert!(owner.read_with(cx, |owner, _| owner.commands.is_empty()));
    }

    // Keep dispatch paused, execute the actual queued command in Core, then
    // deliver its real ack through the production reconciliation/identity guards.
    fn acknowledge_front(
        cx: &mut TestAppContext,
        runtime: &CoreRuntime,
        owner: &Entity<AudioProcessing>,
        success: bool,
    ) {
        let submission = owner.update(cx, |owner, _| owner.commands.pop_front().unwrap());
        let graph_command = matches!(&submission.command, Command::ProcessingGraphEdit { .. });
        let ack = futures::executor::block_on(runtime.handle.request_command(submission.command));
        assert_eq!(ack.is_success(), success, "{:?}", ack.error());
        owner.update(cx, |owner, cx| {
            owner.pending = owner.pending.saturating_sub(1);
            owner.finish_equalizer_command(success);
            owner.reconcile(owner.context.snapshot(), cx);
            if graph_command {
                owner.graph_ack(ack.status(), true, cx);
            }
            if let Some(field) = submission.equalizer_field {
                owner.finish_equalizer_field(field, success, cx);
            }
            owner.finish_released_equalizer_position(cx);
            owner.reconcile(owner.context.snapshot(), cx);
        });
        cx.run_until_parked();
    }

    fn enter_number(
        cx: &mut TestAppContext,
        handle: AnyWindowHandle,
        input: &Entity<Input>,
        text: &str,
    ) {
        use gpui::EntityInputHandler as _;
        cx.update_window(handle, |_, window, cx| {
            input.update(cx, |input, cx| {
                input.focus_and_select_all(window, cx);
                input.replace_text_in_range(None, text, window, cx);
            });
        })
        .unwrap();
        draw(cx, handle);
    }

    #[gpui::test]
    fn stale_monitor_edit_rejection_keeps_numeric_draft(cx: &mut TestAppContext) {
        let (_directory, runtime, owner, editor, handle) = fixture_with_owner(cx);
        owner.update(cx, |owner, _| owner.command_active = true);
        let input = editor.read_with(cx, |editor, _| {
            editor.fields[&EqField::Band(0, BandField::Q)].input.clone()
        });
        enter_number(cx, handle, &input, "2.5");
        press(cx, handle, "enter");
        assert_eq!(owner.read_with(cx, |owner, _| owner.commands.len()), 1);
        let ack =
            futures::executor::block_on(runtime.handle.request_command(Command::EqualizerEdit {
                expected_revision: runtime.handle.gui_snapshot().revisions.configuration,
                edit: EqualizerEdit::BandField {
                    index: 0,
                    field: EqBandEdit::GainDb(4.0),
                },
            }));
        assert!(ack.is_success());
        acknowledge_front(cx, &runtime, &owner, false);
        editor.read_with(cx, |editor, cx| {
            let field = &editor.fields[&EqField::Band(0, BandField::Q)];
            assert!(field.dirty);
            assert_eq!(field.input.read(cx).text(), "2.5");
        });
        assert_eq!(
            runtime.handle.config_snapshot().equalizer.bands[0].gain_db,
            4.0
        );
        assert!(owner.read_with(cx, |owner, _| owner.commands.is_empty()));
    }

    #[gpui::test]
    fn successful_old_ack_does_not_overwrite_or_submit_new_numeric_text(cx: &mut TestAppContext) {
        let (_directory, runtime, owner, editor, handle) = fixture_with_owner(cx);
        owner.update(cx, |owner, _| owner.command_active = true);
        let key = EqField::Band(0, BandField::Q);
        let input = editor.read_with(cx, |editor, _| editor.fields[&key].input.clone());
        enter_number(cx, handle, &input, "1.1");
        press(cx, handle, "enter");
        enter_number(cx, handle, &input, "3.3");
        acknowledge_front(cx, &runtime, &owner, true);
        assert!(owner.read_with(cx, |owner, _| owner.commands.is_empty()));
        editor.read_with(cx, |editor, cx| {
            assert_eq!(editor.config.bands[0].q, 1.1);
            assert_eq!(input.read(cx).text(), "3.3");
            assert!(editor.fields[&key].dirty);
        });
        press(cx, handle, "enter");
        acknowledge_front(cx, &runtime, &owner, true);
        editor.read_with(cx, |editor, _| {
            assert_eq!(editor.config.bands[0].q, 3.3);
            assert!(!editor.fields[&key].dirty);
        });
    }

    #[gpui::test]
    fn late_numeric_ack_displays_latest_core_value_but_preserves_matching_precision(
        cx: &mut TestAppContext,
    ) {
        let (_directory, runtime, owner, editor, handle) = fixture_with_owner(cx);
        owner.update(cx, |owner, _| owner.command_active = true);
        let input = editor.read_with(cx, |editor, _| {
            editor.fields[&EqField::Band(0, BandField::Q)].input.clone()
        });
        for (text, external, authoritative) in [
            ("0.8123456789", EqBandEdit::Q(0.923456789), 0.923456789),
            ("0.8345678901", EqBandEdit::GainDb(1.25), 0.8345678901),
            ("0.845678901200", EqBandEdit::Q(0.8456789012), 0.8456789012),
            ("0.923456789", EqBandEdit::Q(0.923499999), 0.923499999),
        ] {
            enter_number(cx, handle, &input, text);
            press(cx, handle, "enter");
            let issued = owner.update(cx, |owner, _| owner.commands.pop_front().unwrap());
            assert!(
                futures::executor::block_on(runtime.handle.request_command(issued.command))
                    .is_success()
            );
            // Core accepts the local number, then another client updates Core
            // before this editor receives its successful local acknowledgement.
            assert!(
                futures::executor::block_on(runtime.handle.request_command(
                    Command::EqualizerEdit {
                        expected_revision: runtime.handle.gui_snapshot().revisions.configuration,
                        edit: EqualizerEdit::BandField {
                            index: 0,
                            field: external
                        },
                    }
                ))
                .is_success()
            );
            owner.update(cx, |owner, cx| {
                owner.reconcile(owner.context.snapshot(), cx);
            });
            cx.run_until_parked();
            draw(cx, handle);
            editor.read_with(cx, |editor, cx| {
                assert!(editor.fields[&EqField::Band(0, BandField::Q)].dirty);
                assert_eq!(input.read(cx).text(), text);
                assert_eq!(editor.config.bands[0].q, authoritative);
            });
            owner.update(cx, |owner, cx| {
                owner.pending = owner.pending.saturating_sub(1);
                owner.finish_equalizer_command(true);
                owner.finish_equalizer_field(issued.equalizer_field.unwrap(), true, cx);
                owner.reconcile(owner.context.snapshot(), cx);
            });
            cx.run_until_parked();
            draw(cx, handle);
            editor.read_with(cx, |editor, cx| {
                assert!(!editor.fields[&EqField::Band(0, BandField::Q)].dirty);
                if text.parse::<f64>().unwrap() == authoritative {
                    assert_eq!(input.read(cx).text(), text);
                } else {
                    assert_ne!(input.read(cx).text(), text);
                }
                assert_eq!(editor.config.bands[0].q, authoritative);
            });
            assert_eq!(
                runtime.handle.gui_snapshot().system.config.equalizer.bands[0].q,
                authoritative
            );
            assert!(owner.read_with(cx, |owner, _| owner.commands.is_empty()));
        }
    }

    fn select_node_target(
        cx: &mut TestAppContext,
        runtime: &CoreRuntime,
        owner: &Entity<AudioProcessing>,
        editor: &Entity<EqualizerEditor>,
    ) -> u64 {
        let snapshot = runtime.handle.gui_snapshot();
        let id = snapshot.system.config.processing_graph.next_id;
        assert!(
            futures::executor::block_on(runtime.handle.request_command(
                Command::ProcessingGraphEdit {
                    expected_revision: snapshot.system.config.processing_graph.revision,
                    edit: Box::new(GraphEdit::Add {
                        kind: GraphNodeKind::Equalizer {
                            config: snapshot.system.config.equalizer.clone(),
                            implementation: Default::default(),
                        },
                        position: [120., 120.],
                    }),
                }
            ))
            .is_success()
        );
        owner.update(cx, |owner, cx| {
            owner.reconcile(owner.context.snapshot(), cx);
            let state = Rc::clone(&owner.state);
            let target = EqTarget::Node(id);
            let config = target_config(&state, target).unwrap();
            editor.update(cx, |editor, cx| {
                editor.target = target;
                editor.reconcile(
                    Rc::clone(&state),
                    config,
                    EqAvailability {
                        blocked: false,
                        pending: 0,
                        format: state.equalizer_format(),
                        collecting_revision: None,
                    },
                    owner.geometry_epoch,
                    cx,
                );
            });
            owner.equalizer_editors.editors.remove(&EqTarget::Monitor);
            owner
                .equalizer_editors
                .editors
                .insert(target, editor.clone());
            owner.equalizer_editors.active = Some((PANEL, target));
            owner.command_active = true;
        });
        id
    }

    #[gpui::test]
    fn node_reset_restores_default_implementation_and_retires_inspector_drafts(
        cx: &mut TestAppContext,
    ) {
        use super::super::graph_editor::{GraphField, NodeField};
        use rivu_format::config::processing_graph::EqImplementation;
        let (_directory, runtime, owner, editor, handle) = fixture_with_owner(cx);
        let id = select_node_target(cx, &runtime, &owner, &editor);
        let snapshot = runtime.handle.gui_snapshot();
        assert!(
            futures::executor::block_on(runtime.handle.request_command(
                Command::ProcessingGraphEdit {
                    expected_revision: snapshot.system.config.processing_graph.revision,
                    edit: Box::new(GraphEdit::Replace {
                        id,
                        kind: GraphNodeKind::Equalizer {
                            config: snapshot.system.config.equalizer.clone(),
                            implementation: EqImplementation::LinearPhase { taps: 4097 },
                        },
                    }),
                }
            ))
            .is_success()
        );
        let before = runtime
            .handle
            .gui_snapshot()
            .system
            .config
            .processing_graph
            .clone();
        let node = before.nodes.iter().find(|node| node.id == id).unwrap();
        let implementation = FieldKey::Graph(NodeField::new(node, GraphField::Implementation));
        let taps = FieldKey::Graph(NodeField::new(node, GraphField::Taps));
        owner.update(cx, |owner, cx| {
            owner.reconcile(owner.context.snapshot(), cx);
            owner.field(implementation, "Implementation", "linear_phase".into(), cx);
            owner.field(taps, "Taps", "4097".into(), cx);
            owner.stage_graph_text(implementation, "iir".into(), cx);
            owner.stage_graph_text(taps, "8193".into(), cx);
            owner.reconcile(owner.context.snapshot(), cx);
        });
        draw(cx, handle);
        editor.read_with(cx, |editor, cx| {
            assert!(editor.blocked);
            assert!(!editor.reset_locked(cx));
        });
        click(cx, handle, "eq-reset-7");
        owner.read_with(cx, |owner, _| {
            assert!(!owner.fields[&implementation].dirty);
            assert!(!owner.fields[&taps].dirty);
            assert!(matches!(&owner.commands.front().unwrap().command,
                Command::ProcessingGraphEdit { edit, .. }
                    if matches!(edit.as_ref(), GraphEdit::Replace {
                        id: target,
                        kind: GraphNodeKind::Equalizer {
                            implementation: EqImplementation::Iir, ..
                        },
                    } if *target == id)));
        });
        acknowledge_front(cx, &runtime, &owner, true);
        let after = runtime.handle.gui_snapshot();
        let mut expected = before;
        expected.revision += 1;
        expected
            .nodes
            .iter_mut()
            .find(|node| node.id == id)
            .unwrap()
            .kind = GraphNodeKind::Equalizer {
            config: EqualizerConfig {
                enabled: true,
                ..Default::default()
            },
            implementation: EqImplementation::Iir,
        };
        assert_eq!(after.system.config.processing_graph, expected);
        assert_eq!(
            after.system.config.equalizer,
            snapshot.system.config.equalizer
        );
        assert!(owner.read_with(cx, |owner, _| owner.commands.is_empty()));
    }

    #[gpui::test]
    fn fir_node_plots_realized_kernel_and_refreshes_when_only_taps_change(cx: &mut TestAppContext) {
        let (_directory, runtime, owner, editor, handle) = fixture_with_owner(cx);
        let id = select_node_target(cx, &runtime, &owner, &editor);
        let notch = EqualizerConfig {
            enabled: true,
            bands: vec![EqBand {
                kind: FilterKind::Notch,
                frequency_hz: 1_000.,
                q: 8.,
                ..Default::default()
            }],
            ..Default::default()
        };
        let replace = |implementation, cx: &mut TestAppContext| {
            let revision = runtime
                .handle
                .gui_snapshot()
                .system
                .config
                .processing_graph
                .revision;
            assert!(
                futures::executor::block_on(runtime.handle.request_command(
                    Command::ProcessingGraphEdit {
                        expected_revision: revision,
                        edit: Box::new(GraphEdit::Replace {
                            id,
                            kind: GraphNodeKind::Equalizer {
                                config: notch.clone(),
                                implementation,
                            },
                        }),
                    }
                ))
                .is_success()
            );
            // The headless fixture has no stream; pin the plotting format and
            // let the editor's own reconcile decide whether to recompute.
            let state = runtime.handle.gui_snapshot();
            editor.update(cx, |editor, cx| {
                editor.format = Some(test_format(48_000, 1));
                let config = target_config(&state, editor.target).unwrap().clone();
                editor.reconcile(
                    Rc::new(state),
                    &config,
                    EqAvailability {
                        blocked: false,
                        pending: 0,
                        format: editor.format,
                        collecting_revision: None,
                    },
                    editor.geometry_epoch,
                    cx,
                );
            });
        };
        let assert_realized = |taps, cx: &mut TestAppContext| {
            cx.run_until_parked();
            editor.read_with(cx, |editor, _| {
                assert!(editor.realized_current && editor.realized_task.is_none());
                let mut expected = vec![0.; editor.frequencies.len()];
                rivu_dsp::equalizer::fir::FirEqualizer::sample_response_db(
                    &notch,
                    48_000,
                    1,
                    0,
                    taps,
                    &editor.frequencies,
                    &mut expected,
                )
                .unwrap();
                assert_eq!(editor.realized.as_slice(), expected);
            });
        };
        replace(EqImplementation::LinearPhase { taps: 257 }, cx);
        editor.update(cx, |editor, cx| editor.update_response(cx));
        assert_realized(257, cx);
        draw(cx, handle);
        let (target_samples, target_computations, short_samples) =
            editor.read_with(cx, |editor, _| {
                (
                    Arc::clone(&editor.response),
                    editor.response_computations,
                    Arc::clone(&editor.realized),
                )
            });
        // Supersede a tap-only request before its result can publish on the UI thread.
        replace(EqImplementation::LinearPhase { taps: 513 }, cx);
        replace(EqImplementation::LinearPhase { taps: 8_193 }, cx);
        editor.read_with(cx, |editor, _| {
            assert!(editor.realized_task.is_some() && !editor.realized_current);
            assert!(editor.fir_deviation_caption().is_none());
            assert!(Arc::ptr_eq(&short_samples, &editor.realized));
            assert!(Arc::ptr_eq(&target_samples, &editor.response));
            assert_eq!(editor.response_computations, target_computations);
        });
        assert_realized(8_193, cx);
        draw(cx, handle);
        let mut window = VisualTestContext::from_window(handle, cx);
        let graph = window.debug_bounds("eq-graph-7").unwrap();
        let caption = window.debug_bounds("eq-fir-caption-7").unwrap();
        assert!(graph.contains(&caption.origin) && graph.contains(&caption.bottom_right()));
        assert!(
            caption.bottom() + px(4.) >= graph.bottom(),
            "{caption:?} in {graph:?}"
        );
        // IIR clears the displayed FIR curve and cancels any queued publication.
        replace(EqImplementation::LinearPhase { taps: 257 }, cx);
        replace(EqImplementation::Iir, cx);
        cx.run_until_parked();
        editor.read_with(cx, |editor, _| {
            assert!(editor.realized.is_empty() && editor.realized_task.is_none());
            assert!(!editor.realized_current);
            assert!(Arc::ptr_eq(&target_samples, &editor.response));
            assert_eq!(editor.response_computations, target_computations);
        });
    }

    #[gpui::test]
    fn accepted_node_revision_rebases_latest_draft_but_external_revision_never_replays_it(
        cx: &mut TestAppContext,
    ) {
        let (_directory, runtime, owner, editor, handle) = fixture_with_owner(cx);
        let id = select_node_target(cx, &runtime, &owner, &editor);
        draw(cx, handle);
        let input = editor.read_with(cx, |editor, _| {
            editor.fields[&EqField::Band(0, BandField::Q)].input.clone()
        });
        enter_number(cx, handle, &input, "1.2");
        press(cx, handle, "enter");
        // The real parent reconciles while the graph command is still in flight.
        // Its serialization lock must not disable collection of newer native text.
        owner.update(cx, |owner, cx| {
            owner.reconcile(owner.context.snapshot(), cx)
        });
        draw(cx, handle);
        enter_number(cx, handle, &input, "2.3");
        acknowledge_front(cx, &runtime, &owner, true);
        assert!(owner.read_with(cx, |owner, _| owner.commands.is_empty()));
        editor.read_with(cx, |editor, _| {
            assert_eq!(
                editor.fields[&EqField::Band(0, BandField::Q)].draft_revision,
                Some(2)
            );
        });
        enter_number(cx, handle, &input, "3.4");
        assert!(
            futures::executor::block_on(runtime.handle.request_command(
                Command::ProcessingGraphEdit {
                    expected_revision: 2,
                    edit: Box::new(GraphEdit::Move {
                        id,
                        position: [200., 180.]
                    }),
                }
            ))
            .is_success()
        );
        owner.update(cx, |owner, cx| {
            owner.reconcile(owner.context.snapshot(), cx)
        });
        press(cx, handle, "enter");
        draw(cx, handle);
        assert!(owner.read_with(cx, |owner, _| owner.commands.is_empty()));
        editor.read_with(cx, |editor, cx| {
            assert!(editor.fields[&EqField::Band(0, BandField::Q)].dirty);
            assert_eq!(
                editor.fields[&EqField::Band(0, BandField::Q)]
                    .input
                    .read(cx)
                    .text(),
                "3.4"
            );
            assert_eq!(
                editor.fields[&EqField::Band(0, BandField::Q)].draft_revision,
                Some(2)
            );
            assert_eq!(editor.revision, 3);
            assert!(editor.error.is_some());
            assert_eq!(editor.config.bands[0].q, 1.2);
        });
        owner.update(cx, |owner, cx| owner.discard_node_equalizer_drafts(id, cx));
        editor.read_with(cx, |editor, cx| {
            assert!(!editor.fields[&EqField::Band(0, BandField::Q)].dirty);
            assert_eq!(
                editor.fields[&EqField::Band(0, BandField::Q)]
                    .input
                    .read(cx)
                    .text(),
                "1.2"
            );
            assert!(editor.error.is_none());
        });
        assert!(owner.read_with(cx, |owner, _| owner.commands.is_empty()));
        enter_number(cx, handle, &input, "4.5");
        press(cx, handle, "enter");
        acknowledge_front(cx, &runtime, &owner, true);
        editor.read_with(cx, |editor, _| assert_eq!(editor.config.bands[0].q, 4.5));
    }

    fn dispatch_pointer_event(
        cx: &mut TestAppContext,
        handle: AnyWindowHandle,
        event: impl gpui::InputEvent,
    ) {
        cx.update_window(handle, |_, window, cx| {
            window.dispatch_event(event.to_platform_input(), cx);
        })
        .unwrap();
    }

    fn slider_point(bounds: Bounds<Pixels>, fraction: f32) -> Point<Pixels> {
        point(
            bounds.origin.x + bounds.size.width * fraction,
            bounds.center().y,
        )
    }

    #[gpui::test]
    fn node_revision_invalidates_slider_release_before_editor_reconcile(cx: &mut TestAppContext) {
        let (_directory, runtime, owner, editor, handle) = fixture_with_owner(cx);
        let id = select_node_target(cx, &runtime, &owner, &editor);
        draw(cx, handle);
        let bounds = VisualTestContext::from_window(handle, cx)
            .debug_bounds("eq-slider-band.0.gain_db")
            .unwrap();
        dispatch_pointer_event(
            cx,
            handle,
            MouseDownEvent {
                button: MouseButton::Left,
                position: slider_point(bounds, 0.25),
                ..Default::default()
            },
        );
        draw(cx, handle);
        assert!(
            futures::executor::block_on(runtime.handle.request_command(
                Command::ProcessingGraphEdit {
                    expected_revision: 1,
                    edit: Box::new(GraphEdit::Move {
                        id,
                        position: [200., 180.]
                    }),
                }
            ))
            .is_success()
        );
        // Release can see the authoritative snapshot before the view reconciles.
        dispatch_pointer_event(
            cx,
            handle,
            MouseUpEvent {
                button: MouseButton::Left,
                position: slider_point(bounds, 0.9),
                ..Default::default()
            },
        );
        draw(cx, handle);
        assert!(owner.read_with(cx, |owner, _| owner.commands.is_empty()));
        editor.read_with(cx, |editor, cx| {
            let field = &editor.fields[&EqField::Band(0, BandField::GainDb)];
            assert!(!field.dirty);
            assert_eq!(field.input.read(cx).text(), field.committed);
            assert!(editor.field_drag.is_none());
            assert_eq!(editor.pointer_count.get(), 0);
        });
    }

    struct ParentInspectorWindow {
        owner: Entity<AudioProcessing>,
        _subscription: Subscription,
    }

    impl Render for ParentInspectorWindow {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.owner
                .update(cx, |owner, cx| owner.render_panel(PANEL, window, cx))
        }
    }

    #[gpui::test]
    fn parent_inspector_slider_previews_then_commits_only_the_release_value(
        cx: &mut TestAppContext,
    ) {
        let (_directory, runtime, owner, editor) = editor_fixture(cx);
        let id = select_node_target(cx, &runtime, &owner, &editor);
        let window = cx.open_window(size(px(1200.), px(1200.)), {
            let owner = owner.clone();
            move |_, cx| ParentInspectorWindow {
                _subscription: cx.observe(&owner, |_, _, cx| cx.notify()),
                owner,
            }
        });
        let handle = window.into();
        cx.update_window(handle, |_, window, _| window.activate_window())
            .unwrap();
        draw(cx, handle);
        // The production panel measures the workbench before showing its inspector.
        draw(cx, handle);
        let node_position = owner.read_with(cx, |owner, _| {
            let bounds = owner.graph_editor.bounds.get().unwrap().0;
            bounds.origin + point(px(24. + 120. + 200.), px(24. + 120. + 16.))
        });
        dispatch_pointer_event(
            cx,
            handle,
            MouseDownEvent {
                button: MouseButton::Left,
                position: node_position,
                ..Default::default()
            },
        );
        dispatch_pointer_event(
            cx,
            handle,
            MouseUpEvent {
                button: MouseButton::Left,
                position: node_position,
                ..Default::default()
            },
        );
        draw(cx, handle);
        let bounds = VisualTestContext::from_window(handle, cx)
            .debug_bounds("eq-slider-band.0.gain_db")
            .unwrap();
        dispatch_pointer_event(
            cx,
            handle,
            MouseDownEvent {
                button: MouseButton::Left,
                position: slider_point(bounds, 0.25),
                ..Default::default()
            },
        );
        draw(cx, handle);
        assert_eq!(
            VisualTestContext::from_window(handle, cx)
                .debug_bounds("eq-slider-band.0.gain_db")
                .unwrap(),
            bounds
        );
        for fraction in [0.5, 0.6, 0.75] {
            dispatch_pointer_event(
                cx,
                handle,
                MouseMoveEvent {
                    position: slider_point(bounds, fraction),
                    pressed_button: Some(MouseButton::Left),
                    ..Default::default()
                },
            );
        }
        assert!(editor.read_with(cx, |editor, _| editor.response_frame_pending));
        cx.update_window(handle, |_, window, cx| window.simulate_next_frame(cx))
            .unwrap();
        editor.read_with(cx, |editor, cx| {
            assert!(editor.field_drag.is_some());
            assert_eq!(
                editor.fields[&EqField::Band(0, BandField::GainDb)]
                    .input
                    .read(cx)
                    .text(),
                "12"
            );
        });
        assert!(owner.read_with(cx, |owner, _| owner.commands.is_empty()));
        let state = runtime.handle.gui_snapshot();
        assert_eq!(
            target_config(&state, EqTarget::Node(id)).unwrap().bands[0].gain_db,
            -8.
        );
        dispatch_pointer_event(
            cx,
            handle,
            MouseMoveEvent {
                position: slider_point(bounds, 0.8),
                pressed_button: Some(MouseButton::Left),
                ..Default::default()
            },
        );
        assert!(editor.read_with(cx, |editor, _| editor.response_frame_pending));
        dispatch_pointer_event(
            cx,
            handle,
            MouseUpEvent {
                button: MouseButton::Left,
                position: slider_point(bounds, 0.9),
                ..Default::default()
            },
        );
        draw(cx, handle);
        assert_eq!(owner.read_with(cx, |owner, _| owner.commands.len()), 1);
        acknowledge_front(cx, &runtime, &owner, true);
        assert!(editor.read_with(cx, |editor, _| editor.field_drag.is_none()));
        let state = runtime.handle.gui_snapshot();
        assert!(
            (target_config(&state, EqTarget::Node(id)).unwrap().bands[0].gain_db - 19.2).abs()
                < 0.01
        );
        assert!(owner.read_with(cx, |owner, _| owner.commands.is_empty()));
    }

    #[gpui::test]
    fn graph_response_frames_coalesce_and_retire_with_their_drag(cx: &mut TestAppContext) {
        for transition in ["cancel", "hide", "replace", "restart"] {
            let (_directory, _runtime, owner, editor, handle) = fixture_with_owner(cx);
            owner.update(cx, |owner, _| owner.command_active = true);
            editor.update(cx, |editor, cx| {
                // Supply the headless fixture's plotting format; pointer fences
                // still use its real, unchanged no-stream snapshot.
                editor.format = Some(test_format(48_000, 2));
                editor.update_response(cx);
                editor.update_response(cx);
                cx.notify();
            });
            draw(cx, handle);
            let position = VisualTestContext::from_window(handle, cx)
                .debug_bounds("eq-node-7-0")
                .unwrap()
                .center();
            dispatch_pointer_event(
                cx,
                handle,
                MouseDownEvent {
                    button: MouseButton::Left,
                    position,
                    ..Default::default()
                },
            );
            let computations = editor.read_with(cx, |editor, _| editor.response_computations);
            for step in 1..=8 {
                dispatch_pointer_event(
                    cx,
                    handle,
                    MouseMoveEvent {
                        position: position + point(px(step as f32 * 5.), px(-18.)),
                        pressed_button: Some(MouseButton::Left),
                        ..Default::default()
                    },
                );
            }
            editor.read_with(cx, |editor, _| {
                assert!(editor.response_frame_pending);
                assert_eq!(editor.response_computations, computations);
            });
            cx.update_window(handle, |_, window, cx| window.simulate_next_frame(cx))
                .unwrap();
            editor.read_with(cx, |editor, _| {
                assert_eq!(editor.response_computations, computations + 1);
                assert!(!editor.response_frame_pending);
                let drag = editor.drag.as_ref().unwrap();
                let mut preview = editor.config.clone();
                preview.bands[drag.band].frequency_hz = drag.frequency;
                preview.bands[drag.band].gain_db = drag.gain;
                let mut expected = vec![0.; editor.frequencies.len()];
                preview
                    .sample_response_db(48_000, 2, 0, &editor.frequencies, &mut expected)
                    .unwrap();
                assert_eq!(*editor.response, expected);
            });
            draw(cx, handle);
            dispatch_pointer_event(
                cx,
                handle,
                MouseMoveEvent {
                    position: position + point(px(55.), px(-25.)),
                    pressed_button: Some(MouseButton::Left),
                    ..Default::default()
                },
            );
            assert!(editor.read_with(cx, |editor, _| editor.response_frame_pending));
            match transition {
                "hide" => editor.update(cx, |editor, cx| editor.deactivate(cx)),
                "replace" => {
                    let mut config = editor.read_with(cx, |editor, _| editor.config.clone());
                    config.preamp_db = 2.;
                    reconcile_config(cx, &editor, &config, 0);
                }
                _ => editor.update(cx, |editor, cx| editor.cancel_pointer(cx)),
            }
            let computations = editor.read_with(cx, |editor, _| editor.response_computations);
            if transition == "restart" {
                dispatch_pointer_event(
                    cx,
                    handle,
                    MouseDownEvent {
                        button: MouseButton::Left,
                        position,
                        ..Default::default()
                    },
                );
                dispatch_pointer_event(
                    cx,
                    handle,
                    MouseMoveEvent {
                        position: position + point(px(15.), px(-8.)),
                        pressed_button: Some(MouseButton::Left),
                        ..Default::default()
                    },
                );
                assert!(editor.read_with(cx, |editor, _| editor.response_frame_pending));
            }
            let response = editor.read_with(cx, |editor, _| Arc::clone(&editor.response));
            cx.update_window(handle, |_, window, cx| window.simulate_next_frame(cx))
                .unwrap();
            editor.read_with(cx, |editor, _| {
                assert!(!editor.response_frame_pending);
                if transition == "restart" {
                    assert_eq!(editor.response_computations, computations + 1);
                    assert!(editor.drag.is_some());
                } else {
                    assert_eq!(editor.response_computations, computations);
                    assert!(Arc::ptr_eq(&response, &editor.response));
                    assert!(editor.drag.is_none());
                    assert_eq!(editor.pointer_count.get(), 0);
                }
            });
            assert!(owner.read_with(cx, |owner, _| owner.commands.is_empty()));
        }
    }

    #[gpui::test]
    fn graph_release_commits_once_without_overwriting_dirty_numeric_text(cx: &mut TestAppContext) {
        use gpui::InputEvent as _;
        let (_directory, runtime, owner, editor, handle) = fixture_with_owner(cx);
        owner.update(cx, |owner, _| owner.command_active = true);
        let input = editor.read_with(cx, |editor, _| {
            editor.fields[&EqField::Band(0, BandField::GainDb)]
                .input
                .clone()
        });
        enter_number(cx, handle, &input, "-");
        let position = VisualTestContext::from_window(handle, cx)
            .debug_bounds("eq-node-7-0")
            .unwrap()
            .center();
        let end = position + point(px(40.), px(-18.));
        let release = end + point(px(0.), px(-7.));
        let mut expected_gain = 0.;
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
            window.dispatch_event(
                MouseMoveEvent {
                    position: end,
                    pressed_button: Some(MouseButton::Left),
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
            assert_eq!(
                editor.read(cx).fields[&EqField::Band(0, BandField::GainDb)]
                    .input
                    .read(cx)
                    .text(),
                "-"
            );
            assert!(owner.read(cx).commands.is_empty());
            expected_gain = editor.read(cx).config.bands[0].gain_db
                - ((release.y - position.y)
                    / editor.read(cx).drag.as_ref().unwrap().bounds.size.height)
                    as f64
                    * 48.;
            window.dispatch_event(
                MouseUpEvent {
                    button: MouseButton::Left,
                    position: release,
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
            window.dispatch_event(
                MouseUpEvent {
                    button: MouseButton::Left,
                    position: release,
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
            let response = Arc::clone(&editor.read(cx).response);
            window.simulate_next_frame(cx);
            assert!(Arc::ptr_eq(&response, &editor.read(cx).response));
        })
        .unwrap();
        draw(cx, handle);
        assert_eq!(owner.read_with(cx, |owner, _| owner.commands.len()), 1);
        let released_center = VisualTestContext::from_window(handle, cx)
            .debug_bounds("eq-node-7-0")
            .unwrap()
            .center();
        assert!((released_center.y - release.y).abs() < px(1.));
        assert_eq!(
            editor.read_with(cx, |editor, _| editor.config.bands[0].gain_db),
            -8.
        );
        acknowledge_front(cx, &runtime, &owner, true);
        draw(cx, handle);
        assert_eq!(
            VisualTestContext::from_window(handle, cx)
                .debug_bounds("eq-node-7-0")
                .unwrap()
                .center(),
            released_center
        );
        editor.read_with(cx, |editor, cx| {
            assert!(editor.drag.is_none());
            assert!(editor.released_band.is_none());
            assert_eq!(editor.pointer_count.get(), 0);
            assert_eq!(
                editor.fields[&EqField::Band(0, BandField::GainDb)]
                    .input
                    .read(cx)
                    .text(),
                "-"
            );
            assert!(editor.fields[&EqField::Band(0, BandField::GainDb)].dirty);
            assert_ne!(editor.config.bands[0].frequency_hz, 80.);
            assert_eq!(editor.config.bands[0].gain_db, expected_gain);
        });
        assert!(owner.read_with(cx, |owner, _| owner.commands.is_empty()));
    }

    #[gpui::test]
    fn reset_restores_successfully_loaded_preset_after_parameter_edits(cx: &mut TestAppContext) {
        let (_directory, runtime, owner, _editor, _handle) = fixture_with_owner(cx);
        let preset = owner.read_with(cx, |owner, _| {
            owner.context.snapshot().system.config.equalizer_presets[1].clone()
        });
        let load = Command::EqualizerEdit {
            expected_revision: owner.read_with(cx, |owner, _| {
                owner.context.snapshot().revisions.configuration
            }),
            edit: EqualizerEdit::LoadPreset { name: preset.name },
        };
        owner.update(cx, |owner, _| owner.begin_equalizer_command(&load));
        assert!(futures::executor::block_on(runtime.handle.request_command(load)).is_success());
        owner.update(cx, |owner, _| owner.finish_equalizer_command(true));
        let edit = Command::EqualizerEdit {
            expected_revision: owner.read_with(cx, |owner, _| {
                owner.context.snapshot().revisions.configuration
            }),
            edit: EqualizerEdit::PreampDb(3.),
        };
        assert!(futures::executor::block_on(runtime.handle.request_command(edit)).is_success());
        owner.update(cx, |owner, cx| {
            owner.reconcile(owner.context.snapshot(), cx);
            owner.command_active = true;
            let state = owner.context.snapshot();
            assert!(state.system.config.equalizer.enabled);
            owner.reset_target_equalizer(EqTarget::Monitor, 0, state.equalizer_format(), cx);
            let command = owner.commands.pop_front().unwrap().command;
            assert!(matches!(command, Command::EqualizerEdit {
                edit: EqualizerEdit::Replace { config }, ..
            } if config == EqualizerConfig { enabled: true, ..preset.config }));
        });
    }

    #[gpui::test]
    fn reset_waits_for_issued_command_and_clears_parameter_intents_before_late_ack(
        cx: &mut TestAppContext,
    ) {
        let (_directory, runtime, owner, editor, handle) = fixture_with_owner(cx);
        owner.update(cx, |parent, _| parent.command_active = true);
        let gain = editor.read_with(cx, |editor, _| {
            editor.fields[&EqField::Band(0, BandField::GainDb)]
                .input
                .clone()
        });
        let q = editor.read_with(cx, |editor, _| {
            editor.fields[&EqField::Band(0, BandField::Q)].input.clone()
        });
        enter_number(cx, handle, &gain, "1");
        press(cx, handle, "enter");
        enter_number(cx, handle, &gain, "2");
        enter_number(cx, handle, &q, "-");
        owner.update(cx, |owner, cx| {
            owner.reset_target_equalizer(EqTarget::Monitor, 0, None, cx);
            assert_eq!(
                owner.commands.len(),
                1,
                "the issued command cannot be replaced by Reset"
            );
        });
        editor.read_with(cx, |editor, _| {
            assert!(editor.fields[&EqField::Band(0, BandField::GainDb)].dirty)
        });
        let issued = owner.update(cx, |owner, _| owner.commands.pop_front().unwrap());
        assert!(
            futures::executor::block_on(runtime.handle.request_command(issued.command))
                .is_success()
        );
        owner.update(cx, |owner, cx| {
            owner.pending = 0;
            owner.reconcile(owner.context.snapshot(), cx);
            // Accept idle Reset before the old field ack is delivered. Already
            // issued Core work is preserved; all unsent parameter intent is retired.
            owner.reset_target_equalizer(EqTarget::Monitor, 0, None, cx);
            owner.finish_equalizer_field(issued.equalizer_field.unwrap(), true, cx);
        });
        draw(cx, handle);
        owner.read_with(cx, |owner, _| {
            assert_eq!(owner.commands.len(), 1);
            assert!(matches!(
                &owner.commands.front().unwrap().command,
                Command::EqualizerEdit { edit: EqualizerEdit::Replace { config }, .. }
                    if config == &(EqualizerConfig { enabled: true, ..Default::default() })
            ));
        });
        editor.read_with(cx, |editor, _| {
            assert!(!editor.fields[&EqField::Band(0, BandField::GainDb)].dirty);
            assert!(!editor.fields[&EqField::Band(0, BandField::Q)].dirty);
        });
        acknowledge_front(cx, &runtime, &owner, true);
        editor.read_with(cx, |editor, _| {
            assert_eq!(
                editor.config,
                EqualizerConfig {
                    enabled: true,
                    ..Default::default()
                }
            );
            assert!(editor.fields.values().all(|field| !field.dirty));
            assert!(editor.fields.keys().all(|key| key.band_index().is_none()));
        });
        assert!(owner.read_with(cx, |owner, _| owner.commands.is_empty()));
    }

    #[gpui::test]
    fn vertical_curve_drag_preserves_exact_frequency_and_ignored_axis_release_is_noop(
        cx: &mut TestAppContext,
    ) {
        use gpui::InputEvent as _;
        let (_directory, runtime, owner, editor, handle) = fixture_with_owner(cx);
        owner.update(cx, |owner, _| owner.command_active = true);
        for kind in [FilterKind::Peak, FilterKind::LowPass] {
            let mut band = editor.read_with(cx, |editor, _| editor.config.bands[0].clone());
            band.frequency_hz = 333.333_333_333;
            band.kind = kind;
            assert!(
                futures::executor::block_on(runtime.handle.request_command(
                    Command::EqualizerEdit {
                        expected_revision: runtime.handle.gui_snapshot().revisions.configuration,
                        edit: EqualizerEdit::Band { index: 0, band },
                    }
                ))
                .is_success()
            );
            owner.update(cx, |owner, cx| {
                owner.reconcile(owner.context.snapshot(), cx)
            });
            draw(cx, handle);
            let position = VisualTestContext::from_window(handle, cx)
                .debug_bounds("eq-node-7-0")
                .unwrap()
                .center();
            let end = position + point(px(0.), px(-18.));
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
                window.dispatch_event(
                    MouseMoveEvent {
                        position: end,
                        pressed_button: Some(MouseButton::Left),
                        ..Default::default()
                    }
                    .to_platform_input(),
                    cx,
                );
                assert_eq!(
                    editor.read(cx).drag.as_ref().unwrap().frequency,
                    333.333_333_333
                );
                window.dispatch_event(
                    MouseUpEvent {
                        button: MouseButton::Left,
                        position: end,
                        ..Default::default()
                    }
                    .to_platform_input(),
                    cx,
                );
            })
            .unwrap();
            draw(cx, handle);
            if kind == FilterKind::Peak {
                assert_eq!(owner.read_with(cx, |owner, _| owner.commands.len()), 1);
                acknowledge_front(cx, &runtime, &owner, true);
            } else {
                assert!(owner.read_with(cx, |owner, _| owner.commands.is_empty()));
            }
            assert_eq!(
                runtime.handle.gui_snapshot().system.config.equalizer.bands[0].frequency_hz,
                333.333_333_333
            );
        }
    }

    struct EditorWindow(Entity<EqualizerEditor>);

    impl Render for EditorWindow {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.0.clone())
        }
    }

    fn editor_fixture(
        cx: &mut TestAppContext,
    ) -> (
        tempfile::TempDir,
        CoreRuntime,
        Entity<AudioProcessing>,
        Entity<EqualizerEditor>,
    ) {
        cx.update(Input::init);
        let mut config = crate::gui::audio_processing::test_support::config();
        config.equalizer.enabled = true;
        config.equalizer.bands = [(80., -8.), (800., 0.), (8_000., 8.)]
            .into_iter()
            .enumerate()
            .map(|(index, (frequency_hz, gain_db))| EqBand {
                frequency_hz,
                gain_db,
                enabled: index != 1,
                ..Default::default()
            })
            .collect();
        let (directory, runtime, processing) =
            crate::gui::audio_processing::test_support::owner(cx, config);
        let owner = processing.downgrade();
        let editor = processing.update(cx, |parent, cx| {
            let editor = cx.new(|cx| {
                EqualizerEditor::new(
                    owner,
                    EqTarget::Monitor,
                    PANEL,
                    parent,
                    &parent.state.system.config.equalizer,
                    cx,
                )
            });
            parent
                .equalizer_editors
                .editors
                .insert(EqTarget::Monitor, editor.clone());
            parent.equalizer_editors.active = Some((PANEL, EqTarget::Monitor));
            editor
        });
        assert!(editor.read_with(cx, |editor, _| editor.format.is_none()));
        (directory, runtime, processing, editor)
    }

    fn fixture_with_owner(
        cx: &mut TestAppContext,
    ) -> (
        tempfile::TempDir,
        CoreRuntime,
        Entity<AudioProcessing>,
        Entity<EqualizerEditor>,
        AnyWindowHandle,
    ) {
        let (directory, runtime, processing, editor) = editor_fixture(cx);
        let window = cx.open_window(size(px(1200.), px(900.)), {
            let editor = editor.clone();
            move |_, _| EditorWindow(editor)
        });
        let handle = window.into();
        cx.update_window(handle, |_, window, _| window.activate_window())
            .unwrap();
        draw(cx, handle);
        (directory, runtime, processing, editor, handle)
    }

    fn fixture(
        cx: &mut TestAppContext,
    ) -> (
        tempfile::TempDir,
        CoreRuntime,
        Entity<EqualizerEditor>,
        AnyWindowHandle,
    ) {
        let (directory, runtime, processing, editor, handle) = fixture_with_owner(cx);
        // Existing control tests reconcile snapshots explicitly instead of
        // starting native command workers.
        drop(processing);
        assert!(editor.read_with(cx, |editor, _| editor.owner.upgrade().is_none()));
        (directory, runtime, editor, handle)
    }

    struct PresetWindow {
        owner: Entity<AudioProcessing>,
        scope: rivu_ui::FocusScope,
        outside_clicks: Rc<Cell<usize>>,
        panel: Rc<Cell<u64>>,
        _subscription: Subscription,
    }

    impl Render for PresetWindow {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let panel = self.owner.update(cx, |owner, cx| {
                owner.preset_view(self.panel.get(), window, cx)
            });
            self.scope
                .surface(div().id("preset-test").size_full())
                .child(
                    panel
                        .id("preset-controls")
                        .debug_selector(|| "preset-controls".to_owned()),
                )
                .child(
                    rivu_ui::button("preset-outside", "Outside action", cx, |this, _, _| {
                        this.outside_clicks.set(this.outside_clicks.get() + 1);
                    })
                    .debug_selector(|| "preset-outside".to_owned())
                    .mt_32(),
                )
        }
    }

    fn preset_window(
        cx: &mut TestAppContext,
        owner: &Entity<AudioProcessing>,
    ) -> (
        AnyWindowHandle,
        rivu_ui::FocusScope,
        Rc<Cell<usize>>,
        Rc<Cell<u64>>,
    ) {
        let scope = cx.update(rivu_ui::FocusScope::new);
        let outside_clicks = Rc::new(Cell::new(0));
        let panel = Rc::new(Cell::new(7));
        let window = cx.open_window(size(px(800.), px(600.)), {
            let owner = owner.clone();
            let scope = scope.clone();
            let outside_clicks = outside_clicks.clone();
            let panel = panel.clone();
            move |_, cx| PresetWindow {
                _subscription: cx.observe(&owner, |_, _, cx| cx.notify()),
                owner,
                scope,
                outside_clicks,
                panel,
            }
        });
        (window.into(), scope, outside_clicks, panel)
    }

    #[gpui::test]
    fn preset_choices_follow_names_across_candidate_reordering(cx: &mut TestAppContext) {
        let (_directory, _runtime, owner, _editor, handle) = fixture_with_owner(cx);
        cx.update_window(handle, |_, window, cx| {
            owner.update(cx, |owner, cx| {
                owner.command_active = true;
                let mut state = (*owner.state).clone();
                Arc::make_mut(&mut state.system.config).equalizer_presets = ["First", "Second"]
                    .into_iter()
                    .map(|name| rivu_format::config::EqualizerPreset { name: name.into(), config: Default::default() })
                    .collect();
                owner.reconcile(Rc::new(state), cx);
                owner.toggle_preset(7, false, window, cx);
                owner.preset_choices.select_index(1);
                let mut state = (*owner.state).clone();
                Arc::make_mut(&mut state.system.config).equalizer_presets.reverse();
                owner.reconcile(Rc::new(state), cx);
                let selected = owner.preset_choices.selected_index().unwrap();
                assert_eq!(selected, 0);
                assert_eq!(owner.preset_choices.item(selected).unwrap().value, "Second");
                owner.choose_preset(selected, window, cx);
                assert!(owner.preset_popup.is_none());
                assert!(!owner.preset_choices.is_open());
                assert!(matches!(&owner.commands.back().unwrap().command,
                    Command::EqualizerEdit { edit: EqualizerEdit::LoadPreset { name }, .. } if name == "Second"));
            });
        }).unwrap();
    }

    #[gpui::test]
    fn preset_popup_keeps_controls_and_following_content_in_place(cx: &mut TestAppContext) {
        let (_directory, _runtime, owner, _editor) = editor_fixture(cx);
        let (handle, scope, _, _) = preset_window(cx, &owner);
        for names in [&[][..], &["First", "Second"][..]] {
            owner.update(cx, |owner, cx| {
                let mut state = (*owner.state).clone();
                Arc::make_mut(&mut state.system.config).equalizer_presets = names
                    .iter()
                    .map(|name| rivu_format::config::EqualizerPreset {
                        name: (*name).into(),
                        config: Default::default(),
                    })
                    .collect();
                owner.reconcile(Rc::new(state), cx);
            });
            draw(cx, handle);
            let bounds = |cx: &mut TestAppContext| {
                let mut visual = VisualTestContext::from_window(handle, cx);
                (
                    visual.debug_bounds("preset-controls").unwrap(),
                    visual.debug_bounds("preset-outside").unwrap(),
                )
            };
            let closed = bounds(cx);
            cx.update_window(handle, |_, window, cx| {
                window.activate_window();
                scope.handle().focus(window, cx);
                scope.advance(false, window, cx);
            })
            .unwrap();
            press(cx, handle, "enter");
            assert!(owner.read_with(cx, |owner, _| owner.preset_popup.is_some()));
            assert_eq!(
                bounds(cx),
                closed,
                "opening presets must not reflow content"
            );
            press(cx, handle, "escape");
            assert!(owner.read_with(cx, |owner, _| owner.preset_popup.is_none()));
            assert_eq!(
                bounds(cx),
                closed,
                "closing presets must not reflow content"
            );
        }
    }

    #[gpui::test]
    fn preset_keyboard_management_restores_origin_and_disabled_trigger_is_inert(
        cx: &mut TestAppContext,
    ) {
        let (_directory, _runtime, owner, _editor) = editor_fixture(cx);
        owner.update(cx, |owner, cx| {
            owner.command_active = true;
            owner.page = Page::Equalizer;
            let mut state = (*owner.state).clone();
            Arc::make_mut(&mut state.system.config).equalizer_presets = ["First", "Second"]
                .into_iter()
                .map(|name| rivu_format::config::EqualizerPreset {
                    name: name.into(),
                    config: Default::default(),
                })
                .collect();
            owner.reconcile(Rc::new(state), cx);
        });
        let (handle, scope, outside_clicks, panel) = preset_window(cx, &owner);
        draw(cx, handle);
        cx.update_window(handle, |_, window, cx| {
            window.activate_window();
            scope.handle().focus(window, cx);
            scope.advance(false, window, cx);
        })
        .unwrap();
        draw(cx, handle);
        let origin = cx
            .update_window(handle, |_, window, cx| window.focused(cx))
            .unwrap();
        press(cx, handle, "enter");
        cx.update_window(handle, |_, window, cx| {
            owner.read_with(cx, |owner, cx| {
                let popup = owner.preset_popup.as_ref().expect("keyboard opens presets");
                assert_eq!(popup.panel, 7);
                assert!(popup.scope.handle().contains_focused(window, cx));
            });
        })
        .unwrap();
        press(cx, handle, "escape");
        assert!(owner.read_with(cx, |owner, _| owner.preset_popup.is_none()));
        assert_eq!(
            cx.update_window(handle, |_, window, cx| window.focused(cx))
                .unwrap(),
            origin
        );
        press(cx, handle, "enter");
        press(cx, handle, "down");
        press(cx, handle, "enter");
        owner.read_with(cx, |owner, cx| {
            assert!(owner.preset_popup.is_none());
            assert_eq!(owner.fields[&FieldKey::PresetName].input.read(cx).text(), "Second");
            assert!(matches!(&owner.commands.back().unwrap().command,
                Command::EqualizerEdit { edit: EqualizerEdit::LoadPreset { name }, .. } if name == "Second"));
        });
        // Tab reaches Manage; its first tab stop is the native name input.
        press(cx, handle, "tab");
        press(cx, handle, "enter");
        // An unrelated playback publication must preserve the focused input.
        owner.update(cx, |owner, cx| {
            let mut state = (*owner.state).clone();
            state.playback.position += 1.;
            owner.reconcile(Rc::new(state), cx);
        });
        draw(cx, handle);
        cx.update_window(handle, |_, window, cx| {
            owner.update(cx, |owner, cx| {
                owner.deactivate_graph_inputs(window, cx);
            });
            owner.read_with(cx, |owner, cx| {
                assert!(owner.preset_popup.as_ref().unwrap().manage);
                assert!(
                    owner.fields[&FieldKey::PresetName]
                        .input
                        .read(cx)
                        .is_focused(window)
                );
            });
        })
        .unwrap();
        cx.update_window(handle, |_, window, cx| {
            use gpui::EntityInputHandler as _;
            let input = owner.read_with(cx, |owner, _| {
                owner.fields[&FieldKey::PresetName].input.clone()
            });
            input.update(cx, |input, cx| {
                input.replace_and_mark_text_in_range(None, "预设", None, window, cx)
            });
        })
        .unwrap();
        press(cx, handle, "escape");
        assert!(owner.read_with(cx, |owner, _| owner.preset_popup.is_some()));
        press(cx, handle, "escape");
        assert!(owner.read_with(cx, |owner, _| owner.preset_popup.is_none()));
        // A different named layout hosts the same processing business owner.
        panel.set(8);
        owner.update(cx, |owner, cx| {
            owner.invalidate_geometry(cx);
            cx.notify();
        });
        draw(cx, handle);
        cx.update_window(handle, |_, window, cx| {
            scope.handle().focus(window, cx);
            scope.advance(false, window, cx);
        })
        .unwrap();
        draw(cx, handle);
        press(cx, handle, "enter");
        assert_eq!(
            owner.read_with(cx, |owner, _| owner.preset_popup.as_ref().unwrap().panel),
            8
        );
        owner.update(cx, |owner, cx| {
            owner.sync_panels(
                &crate::gui::workspace::WorkspacePanels::default(),
                false,
                cx,
            );
            cx.notify();
        });
        draw(cx, handle);
        assert!(owner.read_with(cx, |owner, _| owner.preset_popup.is_none()));
        cx.update_window(handle, |_, window, cx| {
            scope.handle().focus(window, cx);
            scope.advance(false, window, cx);
        })
        .unwrap();
        draw(cx, handle);
        press(cx, handle, "enter");
        click(cx, handle, "preset-outside");
        assert!(owner.read_with(cx, |owner, _| owner.preset_popup.is_none()));
        assert_eq!(
            outside_clicks.get(),
            0,
            "dismissal must not activate the underlying action"
        );
        click(cx, handle, "preset-outside");
        assert_eq!(
            outside_clicks.get(),
            1,
            "the next ordinary click remains live"
        );
        owner.update(cx, |owner, cx| {
            let mut state = (*owner.state).clone();
            Arc::make_mut(&mut state.system.config).equalizer.enabled = false;
            owner.reconcile(Rc::new(state), cx);
        });
        draw(cx, handle);
        cx.update_window(handle, |_, window, cx| {
            scope.handle().focus(window, cx);
            scope.advance(false, window, cx);
        })
        .unwrap();
        draw(cx, handle);
        press(cx, handle, "enter");
        assert_eq!(
            outside_clicks.get(),
            2,
            "Tab skips every disabled preset control"
        );
        cx.update_window(handle, |_, window, cx| {
            origin.as_ref().unwrap().focus(window, cx);
        })
        .unwrap();
        draw(cx, handle);
        press(cx, handle, "enter");
        assert!(owner.read_with(cx, |owner, _| owner.preset_popup.is_none()));
    }

    #[gpui::test]
    fn gain_ime_commit_and_blur_leave_a_draft_until_enter(cx: &mut TestAppContext) {
        let (_directory, runtime, owner, editor, handle) = fixture_with_owner(cx);
        owner.update(cx, |owner, _| owner.command_active = true);
        for (key, text, unmark) in [
            (EqField::PreampDb, "6", true),
            (EqField::OutputGainDb, "-3", false),
        ] {
            let input = editor.read_with(cx, |editor, _| editor.fields[&key].input.clone());
            cx.update_window(handle, |_, window, cx| {
                input.update(cx, |input, cx| {
                    input.focus_and_select_all(window, cx);
                    let end = input.text().encode_utf16().count();
                    input.replace_and_mark_text_in_range(Some(0..end), text, None, window, cx);
                });
            })
            .unwrap();
            draw(cx, handle);
            assert!(owner.read_with(cx, |owner, _| owner.commands.is_empty()));
            cx.update_window(handle, |_, window, cx| {
                input.update(cx, |input, cx| {
                    if unmark {
                        input.unmark_text(window, cx);
                    } else {
                        input.replace_text_in_range(None, text, window, cx);
                    }
                });
            })
            .unwrap();
            click(cx, handle, "eq-node-7-0");
            assert!(owner.read_with(cx, |owner, _| owner.commands.is_empty()));
            cx.update_window(handle, |_, window, cx| {
                input.update(cx, |input, cx| input.focus_and_select_all(window, cx));
            })
            .unwrap();
            press(cx, handle, "enter");
            assert_eq!(owner.read_with(cx, |owner, _| owner.commands.len()), 1);
            acknowledge_front(cx, &runtime, &owner, true);
            editor.read_with(cx, |editor, cx| {
                assert!(!editor.fields[&key].dirty);
                assert_eq!(input.read(cx).text(), text);
                let value = if key == EqField::PreampDb {
                    editor.config.preamp_db
                } else {
                    editor.config.output_gain_db
                };
                assert_eq!(value, text.parse::<f64>().unwrap());
            });
        }
    }

    fn press(cx: &mut TestAppContext, handle: AnyWindowHandle, key: &str) {
        use gpui::InputEvent as _;
        let keystroke = gpui::Keystroke::parse(key).unwrap();
        cx.update_window(handle, |_, window, cx| {
            // GPUI commits native keyboard Click on key-up with matching focus.
            // simulate_keystrokes dispatches only key-down, never that activation.
            window.dispatch_event(
                KeyDownEvent {
                    keystroke: keystroke.clone(),
                    is_held: false,
                    prefer_character_input: false,
                }
                .to_platform_input(),
                cx,
            );
            window.dispatch_event(gpui::KeyUpEvent { keystroke }.to_platform_input(), cx);
        })
        .unwrap();
        draw(cx, handle);
        // Opening a FocusScope advances from its sentinel after the new surface
        // is painted; TestAppContext has no platform frame loop to deliver it.
        let callbacks = cx
            .update_window(handle, |_, window, cx| window.simulate_next_frame(cx))
            .unwrap();
        if callbacks > 0 {
            draw(cx, handle);
        }
    }

    #[gpui::test]
    fn delete_removes_only_the_focused_curve_node(cx: &mut TestAppContext) {
        let (_directory, _runtime, owner, editor, handle) = fixture_with_owner(cx);
        owner.update(cx, |owner, _| owner.command_active = true);
        click(cx, handle, "eq-node-7-2");
        press(cx, handle, "delete");
        owner.read_with(cx, |owner, _| {
            assert_eq!(owner.commands.len(), 1);
            assert!(matches!(
                &owner.commands.front().unwrap().command,
                Command::EqualizerEdit {
                    expected_revision: _,
                    edit: EqualizerEdit::RemoveBand { index: 2 }
                }
            ));
        });

        // Focusing an input must keep Delete within its native editing path,
        // even though the curve node remains selected.
        let input = editor.read_with(cx, |editor, _| {
            editor.fields[&EqField::Band(2, BandField::FrequencyHz)]
                .input
                .clone()
        });
        cx.update_window(handle, |_, window, cx| {
            input.update(cx, |input, cx| input.focus_and_select_all(window, cx));
        })
        .unwrap();
        draw(cx, handle);
        press(cx, handle, "delete");
        assert_eq!(owner.read_with(cx, |owner, _| owner.commands.len()), 1);

        for (enabled, blocked) in [(false, false), (true, true)] {
            editor.update(cx, |editor, cx| {
                editor.config.enabled = enabled;
                editor.blocked = blocked;
                cx.notify();
            });
            draw(cx, handle);
            cx.update_window(handle, |_, window, cx| {
                let focus = editor.read_with(cx, |editor, _| {
                    editor.band_focus[2].as_ref().unwrap().handle.clone()
                });
                focus.focus(window, cx);
            })
            .unwrap();
            draw(cx, handle);
            press(cx, handle, "delete");
            assert_eq!(owner.read_with(cx, |owner, _| owner.commands.len()), 1);
        }
    }

    #[gpui::test]
    fn viewing_another_channel_changes_the_curve_without_changing_eq(cx: &mut TestAppContext) {
        let (_directory, _runtime, editor, handle) = fixture(cx);
        let original = editor.update(cx, |editor, cx| {
            editor.format = Some(test_format(48_000, 2));
            for band in &mut editor.config.bands {
                band.channels = ChannelTarget::Selected(vec![1]);
            }
            editor.update_response(cx);
            assert!(editor.response.iter().all(|db| db.abs() < 1e-10));
            editor.config.clone()
        });
        draw(cx, handle);
        click(cx, handle, "eq-response-channel-7");
        press(cx, handle, "down");
        press(cx, handle, "enter");
        editor.read_with(cx, |editor, _| {
            assert_eq!(editor.response_channel, 1);
            assert!(editor.response.iter().any(|db| db.abs() > 1.));
            assert_eq!(editor.config, original);
        });
        editor.update(cx, |editor, cx| {
            editor.format = Some(test_format(48_000, 1));
            editor.update_response(cx);
        });
        draw(cx, handle);
        assert!(
            VisualTestContext::from_window(handle, cx)
                .debug_bounds("eq-response-channel-7")
                .is_none()
        );
        editor.read_with(cx, |editor, _| assert_eq!(editor.response_channel, 0));
    }

    fn assert_selected(
        cx: &mut TestAppContext,
        editor: &Entity<EqualizerEditor>,
        handle: AnyWindowHandle,
        index: usize,
    ) {
        assert!(
            VisualTestContext::from_window(handle, cx)
                .debug_bounds("eq-band-properties-7")
                .is_some(),
            "the selected band's property header is painted"
        );
        editor.read_with(cx, |editor, _| {
            assert_eq!(editor.band, index);
            for name in [
                BandField::FrequencyHz,
                BandField::GainDb,
                BandField::Q,
                BandField::Channels,
            ] {
                assert!(editor.fields.contains_key(&EqField::Band(index, name)));
            }
            assert!(editor.fields.iter().all(|(key, field)| {
                key.band_index().is_none_or(|band| band == index) || field.dirty
            }));
        });
    }

    fn reconcile_config(
        cx: &mut TestAppContext,
        editor: &Entity<EqualizerEditor>,
        config: &EqualizerConfig,
        pending: usize,
    ) {
        editor.update(cx, |editor, cx| {
            let mut state = Rc::clone(&editor.state);
            Arc::make_mut(&mut Rc::make_mut(&mut state).system.config)
                .equalizer
                .clone_from(config);
            editor.reconcile(
                state,
                config,
                EqAvailability {
                    blocked: false,
                    pending,
                    format: editor.format,
                    collecting_revision: None,
                },
                editor.geometry_epoch,
                cx,
            );
        });
    }

    #[gpui::test]
    fn disabling_equalizer_preserves_drafts_and_locks_native_inputs_and_field_actions(
        cx: &mut TestAppContext,
    ) {
        use gpui::EntityInputHandler as _;

        let (_directory, _runtime, editor, handle) = fixture(cx);
        let inputs = editor.read_with(cx, |editor, _| {
            [
                &EqField::PreampDb,
                &EqField::OutputGainDb,
                &EqField::Band(0, BandField::FrequencyHz),
                &EqField::Band(0, BandField::GainDb),
                &EqField::Band(0, BandField::Q),
                &EqField::Band(0, BandField::Channels),
            ]
            .map(|key| (key, editor.fields[key].input.clone()))
        });
        cx.update_window(handle, |_, window, cx| {
            for (key, input) in &inputs {
                input.update(cx, |input, cx| {
                    input.focus_and_select_all(window, cx);
                    input.replace_text_in_range(
                        None,
                        if *key == &EqField::Band(0, BandField::FrequencyHz) {
                            "invalid"
                        } else {
                            "4.75"
                        },
                        window,
                        cx,
                    );
                });
            }
        })
        .unwrap();
        draw(cx, handle);
        let drafts = editor.read_with(cx, |editor, cx| {
            inputs.each_ref().map(|(key, input)| {
                assert!(editor.fields[*key].dirty);
                (
                    input.read(cx).text().to_owned(),
                    editor.fields[*key].generation,
                )
            })
        });
        let mut config = editor.read_with(cx, |editor, _| editor.config.clone());
        config.enabled = false;
        reconcile_config(cx, &editor, &config, 0);
        draw(cx, handle);
        click(cx, handle, "eq-node-7-2");
        assert_selected(cx, &editor, handle, 2);
        click(cx, handle, "eq-node-7-0");
        assert_selected(cx, &editor, handle, 0);
        // Reconcile the authoritative snapshot rather than imitating the power
        // command: this fixture deliberately has no live command owner.
        cx.update_window(handle, |_, window, cx| {
            for (_, input) in &inputs {
                input.update(cx, |input, cx| {
                    input.focus_and_select_all(window, cx);
                    input.replace_text_in_range(None, "999", window, cx);
                });
            }
        })
        .unwrap();
        editor.update(cx, |editor, cx| {
            assert!(editor.config_locked());
            assert!(editor.error.is_none());
            editor.submit_field(&EqField::Band(0, BandField::FrequencyHz), cx);
            editor.step_field(&EqField::Band(0, BandField::GainDb), 0.5, cx);
            assert!(
                editor.error.is_none(),
                "locked fields must not even validate drafts"
            );
        });
        draw(cx, handle);
        editor.read_with(cx, |editor, cx| {
            assert_eq!(editor.config, config);
            for ((key, input), (text, generation)) in inputs.iter().zip(&drafts) {
                let field = &editor.fields[*key];
                assert_eq!(&field.input, input);
                assert!(field.dirty);
                assert_eq!(field.input.read(cx).text(), text);
                assert_eq!(field.generation, *generation);
            }
        });
        config.enabled = true;
        reconcile_config(cx, &editor, &config, 0);
        draw(cx, handle);
        editor.update(cx, |editor, cx| {
            assert!(!editor.config_locked());
            editor.submit_field(&EqField::Band(0, BandField::FrequencyHz), cx);
            assert!(
                editor.error.is_some(),
                "reenabled fields validate preserved drafts"
            );
            editor.step_field(&EqField::Band(0, BandField::GainDb), 0.5, cx);
            assert_eq!(
                editor.fields[&EqField::Band(0, BandField::GainDb)]
                    .input
                    .read(cx)
                    .text(),
                "5.25"
            );
        });
        cx.update_window(handle, |_, window, cx| {
            let input = &inputs[0].1;
            input.update(cx, |input, cx| {
                input.focus_and_select_all(window, cx);
                input.replace_text_in_range(None, "6.25", window, cx);
            });
        })
        .unwrap();
        assert_eq!(
            inputs[0]
                .1
                .read_with(cx, |input, _| input.text().to_owned()),
            "6.25"
        );
    }

    #[gpui::test]
    fn disabled_equalizer_allows_graph_selection_but_cannot_start_or_move_drag(
        cx: &mut TestAppContext,
    ) {
        use gpui::InputEvent as _;

        let (_directory, _runtime, editor, handle) = fixture(cx);
        let mut config = editor.read_with(cx, |editor, _| editor.config.clone());
        for enabled in [false, true] {
            config.enabled = enabled;
            reconcile_config(cx, &editor, &config, 0);
            draw(cx, handle);
            click(cx, handle, "eq-node-7-2");
            assert_selected(cx, &editor, handle, 2);
            let position = VisualTestContext::from_window(handle, cx)
                .debug_bounds("eq-node-7-2")
                .unwrap()
                .center();
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
                editor.read_with(cx, |editor, _| {
                    assert_eq!(editor.drag.is_some(), enabled);
                });
                if !enabled {
                    window.dispatch_event(
                        MouseMoveEvent {
                            position: position + point(px(24.), px(-12.)),
                            pressed_button: Some(MouseButton::Left),
                            ..Default::default()
                        }
                        .to_platform_input(),
                        cx,
                    );
                    editor.read_with(cx, |editor, _| {
                        assert_eq!(editor.config, config);
                        assert!(editor.drag.is_none());
                    });
                } else {
                    // Motion validates a live owner; this fixture only proves
                    // that reenabling restores the real pointer-down entry.
                    editor.update(cx, |editor, cx| editor.cancel_pointer(cx));
                }
                window.dispatch_event(
                    MouseUpEvent {
                        button: MouseButton::Left,
                        position: position + point(px(24.), px(-12.)),
                        ..Default::default()
                    }
                    .to_platform_input(),
                    cx,
                );
            })
            .unwrap();
            draw(cx, handle);
            assert!(editor.read_with(cx, |editor, _| editor.drag.is_none()));
        }
    }

    #[gpui::test]
    fn graph_click_selects_properties_and_preserves_other_band_draft_keys(cx: &mut TestAppContext) {
        let (_directory, _runtime, editor, handle) = fixture(cx);
        click(cx, handle, "eq-node-7-2");
        assert_selected(cx, &editor, handle, 2);
        let input = editor.read_with(cx, |editor, _| {
            editor.fields[&EqField::Band(2, BandField::GainDb)]
                .input
                .clone()
        });
        cx.update_window(handle, |_, window, cx| {
            input.update(cx, |input, cx| input.focus_and_select_all(window, cx));
        })
        .unwrap();
        draw(cx, handle);
        cx.simulate_keystrokes(handle, "4 . 7 5");
        draw(cx, handle);
        editor.read_with(cx, |editor, cx| {
            let field = &editor.fields[&EqField::Band(2, BandField::GainDb)];
            assert!(field.dirty);
            assert_eq!(field.input.read(cx).text(), "4.75");
        });
        click(cx, handle, "eq-node-7-0");
        assert_selected(cx, &editor, handle, 0);
        editor.read_with(cx, |editor, cx| {
            let draft = &editor.fields[&EqField::Band(2, BandField::GainDb)];
            assert_eq!(draft.input, input);
            assert!(draft.dirty);
            assert_eq!(draft.input.read(cx).text(), "4.75");
            assert_eq!(editor.config.bands[2].gain_db, 8.);
            assert!(editor.drag.is_none());
        });
        click(cx, handle, "eq-node-7-2");
        assert_selected(cx, &editor, handle, 2);
        assert_eq!(
            editor.read_with(cx, |editor, _| editor.fields
                [&EqField::Band(2, BandField::GainDb)]
                .input
                .clone()),
            input
        );
    }

    #[gpui::test]
    fn disabled_band_can_be_selected_and_enable_changes_preserve_native_identity(
        cx: &mut TestAppContext,
    ) {
        let (_directory, _runtime, editor, handle) = fixture(cx);
        click(cx, handle, "eq-node-7-1");
        assert_selected(cx, &editor, handle, 1);
        let (focus, input, mut config) = editor.read_with(cx, |editor, _| {
            assert!(!editor.config.bands[1].enabled);
            (
                editor.band_focus[1].as_ref().unwrap().handle.clone(),
                editor.fields[&EqField::Band(1, BandField::FrequencyHz)]
                    .input
                    .clone(),
                editor.config.clone(),
            )
        });
        for enabled in [true, false] {
            config.bands[1].enabled = enabled;
            reconcile_config(cx, &editor, &config, 0);
            draw(cx, handle);
            assert_selected(cx, &editor, handle, 1);
            editor.read_with(cx, |editor, _| {
                assert_eq!(editor.band_focus[1].as_ref().unwrap().handle, focus);
                assert_eq!(
                    editor.fields[&EqField::Band(1, BandField::FrequencyHz)].input,
                    input
                );
            });
            cx.update_window(handle, |_, window, _| assert!(focus.is_focused(window)))
                .unwrap();
        }
    }

    #[gpui::test]
    fn native_graph_focus_changes_selected_properties_without_pointer_input(
        cx: &mut TestAppContext,
    ) {
        let (_directory, _runtime, editor, handle) = fixture(cx);
        for index in [2, 0, 1] {
            let focus = editor.read_with(cx, |editor, _| {
                editor.band_focus[index].as_ref().unwrap().handle.clone()
            });
            cx.update_window(handle, |_, window, cx| focus.focus(window, cx))
                .unwrap();
            draw(cx, handle);
            assert_selected(cx, &editor, handle, index);
            assert!(editor.read_with(cx, |editor, _| editor.drag.is_none()));
        }
    }

    fn enter_band_count(
        cx: &mut TestAppContext,
        editor: &Entity<EqualizerEditor>,
        handle: AnyWindowHandle,
        text: &str,
    ) {
        use gpui::EntityInputHandler as _;
        let input = editor.read_with(cx, |editor, _| {
            editor.fields[&EqField::BandCount].input.clone()
        });
        cx.update_window(handle, |_, window, cx| {
            input.update(cx, |input, cx| {
                input.focus_and_select_all(window, cx);
                input.replace_text_in_range(None, text, window, cx);
            });
        })
        .unwrap();
        draw(cx, handle);
        press(cx, handle, "enter");
    }

    #[gpui::test]
    fn total_count_intent_selects_first_new_band_when_reconciled(cx: &mut TestAppContext) {
        let (_directory, _runtime, editor, handle) = fixture(cx);
        enter_band_count(cx, &editor, handle, "5");
        let mut config = editor.read_with(cx, |editor, _| {
            assert_eq!(editor.pending_added_band, Some((3, 2)));
            editor.config.clone()
        });
        reconcile_config(cx, &editor, &config, 1);
        draw(cx, handle);
        assert_selected(cx, &editor, handle, 0);
        EqualizerEdit::ResizeBands { count: 5 }
            .apply_config(&mut config, None)
            .unwrap();
        reconcile_config(cx, &editor, &config, 0);
        draw(cx, handle);
        assert_selected(cx, &editor, handle, 3);
        assert!(editor.read_with(cx, |editor, _| editor.pending_added_band.is_none()));
    }

    #[gpui::test]
    fn append_selection_rejects_wrong_count_changed_prefix_and_completed_failure(
        cx: &mut TestAppContext,
    ) {
        for mismatch in 0..3 {
            let (_directory, _runtime, editor, handle) = fixture(cx);
            enter_band_count(cx, &editor, handle, "4");
            let mut config = editor.read_with(cx, |editor, _| editor.config.clone());
            if mismatch == 2 {
                // A failed/no-op completion must not select a later external append.
                reconcile_config(cx, &editor, &config, 0);
            }
            EqualizerEdit::AddBands {
                count: if mismatch == 0 { 2 } else { 1 },
            }
            .apply_config(&mut config, None)
            .unwrap();
            if mismatch == 1 {
                config.bands[0].gain_db += 1.;
            }
            reconcile_config(cx, &editor, &config, 0);
            draw(cx, handle);
            assert_selected(cx, &editor, handle, 0);
            assert!(editor.read_with(cx, |editor, _| editor.pending_added_band.is_none()));
        }
    }

    #[gpui::test]
    fn total_count_submits_atomically_only_on_enter_and_respects_guards(cx: &mut TestAppContext) {
        let (_directory, _runtime, owner, editor, handle) = fixture_with_owner(cx);
        owner.update(cx, |owner, _| owner.command_active = true);
        for text in ["-1", "33", "1.5", "invalid"] {
            enter_band_count(cx, &editor, handle, text);
            assert!(owner.read_with(cx, |owner, _| owner.commands.is_empty()));
        }
        let input = editor.read_with(cx, |editor, _| {
            editor.fields[&EqField::BandCount].input.clone()
        });
        cx.update_window(handle, |_, window, cx| {
            use gpui::EntityInputHandler as _;
            input.update(cx, |input, cx| {
                input.focus_and_select_all(window, cx);
                input.replace_text_in_range(None, "1", window, cx);
            });
        })
        .unwrap();
        draw(cx, handle);
        assert!(owner.read_with(cx, |owner, _| owner.commands.is_empty()));
        press(cx, handle, "enter");
        owner.read_with(cx, |owner, _| {
            assert_eq!(owner.commands.len(), 1);
            assert!(matches!(
                &owner.commands.front().unwrap().command,
                Command::EqualizerEdit {
                    expected_revision: _,
                    edit: EqualizerEdit::ResizeBands { count: 1 }
                }
            ));
        });
        for (enabled, blocked, pending) in [(false, false, 0), (true, true, 0), (true, false, 1)] {
            editor.update(cx, |editor, cx| {
                editor.config.enabled = enabled;
                editor.blocked = blocked;
                editor.pending = pending;
                cx.notify();
            });
            enter_band_count(cx, &editor, handle, "0");
            assert_eq!(owner.read_with(cx, |owner, _| owner.commands.len()), 1);
        }
    }

    #[gpui::test]
    fn reducing_total_count_preserves_prefix_drafts_and_clamps_selection(cx: &mut TestAppContext) {
        let (_directory, _runtime, editor, handle) = fixture(cx);
        let input = editor.read_with(cx, |editor, _| {
            editor.fields[&EqField::Band(0, BandField::FrequencyHz)]
                .input
                .clone()
        });
        cx.update_window(handle, |_, window, cx| {
            use gpui::EntityInputHandler as _;
            input.update(cx, |input, cx| {
                input.focus_and_select_all(window, cx);
                input.replace_text_in_range(None, "123", window, cx);
            });
        })
        .unwrap();
        draw(cx, handle);
        click(cx, handle, "eq-node-7-2");
        enter_band_count(cx, &editor, handle, "1");
        let mut config = editor.read_with(cx, |editor, _| editor.config.clone());
        EqualizerEdit::ResizeBands { count: 1 }
            .apply_config(&mut config, None)
            .unwrap();
        reconcile_config(cx, &editor, &config, 0);
        draw(cx, handle);
        assert_selected(cx, &editor, handle, 0);
        editor.read_with(cx, |editor, cx| {
            assert_eq!(
                editor.fields[&EqField::Band(0, BandField::FrequencyHz)].input,
                input
            );
            assert_eq!(input.read(cx).text(), "123");
            assert!(editor.fields[&EqField::Band(0, BandField::FrequencyHz)].dirty);
            assert!(
                !editor
                    .fields
                    .contains_key(&EqField::Band(2, BandField::FrequencyHz))
            );
        });
    }

    #[test]
    fn batch_drafts_apply_band_parameters_before_resizing_regardless_of_field_order() {
        let base = EqualizerConfig {
            enabled: true,
            preamp_db: 5.,
            output_gain_db: -3.,
            bands: vec![EqBand::default(), EqBand::default()],
        };
        let edits = [
            EqualizerEdit::ResizeBands { count: 1 },
            EqualizerEdit::BandField {
                index: 1,
                field: EqBandEdit::Channels(ChannelTarget::Selected(vec![1])),
            },
            EqualizerEdit::BandField {
                index: 0,
                field: EqBandEdit::GainDb(3.25),
            },
            EqualizerEdit::PreampDb(-2.),
        ];
        for edits in [edits.to_vec(), edits.into_iter().rev().collect()] {
            let mut config = base.clone();
            apply_equalizer_draft_edits(&mut config, edits, Some(48_000)).unwrap();
            assert_eq!(config.bands.len(), 1);
            assert_eq!(config.bands[0].gain_db, 3.25);
            assert_eq!(config.bands[0].channels, base.bands[0].channels);
            assert_eq!(config.preamp_db, -2.);
            assert_eq!(config.output_gain_db, -3.);
        }
    }

    #[test]
    fn accepted_edit_rebases_new_inflight_drafts_but_not_external_or_skipped_revisions() {
        // No submitted EQ snapshots are needed: text first entered while a
        // typed implementation edit was in flight still belongs to its node.
        let mut origin = Some(7);
        rebase_draft_origin(&mut origin, 7, 8, 8);
        assert_eq!(origin, Some(8));
        // Repeating the completion cannot rebase a later generation again.
        rebase_draft_origin(&mut origin, 7, 8, 8);
        assert_eq!(origin, Some(8));
        let mut externally_stale = Some(6);
        rebase_draft_origin(&mut externally_stale, 7, 8, 8);
        assert_eq!(externally_stale, Some(6));
        let mut skipped = Some(7);
        rebase_draft_origin(&mut skipped, 7, 9, 9);
        assert_eq!(skipped, Some(7));
        rebase_draft_origin(&mut skipped, 7, 8, 9);
        assert_eq!(skipped, Some(7));
        let mut clean = None;
        rebase_draft_origin(&mut clean, 7, 8, 8);
        assert_eq!(clean, None);
    }
}
