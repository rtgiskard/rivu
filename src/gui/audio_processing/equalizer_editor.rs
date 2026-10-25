//! One explicit-target EQ editor per dock panel. Core owns committed parameters;
//! these entities own only drafts, response samples and cancellable pointer intent.
//! Gain and response-channel controls share a wrapping header above the editor body.
use super::*;
use rivu_dsp::equalizer::config::{ChannelTarget, EqBand, EqualizerConfig, FilterKind, MAX_BANDS};
use rivu_format::config::processing_graph::{GraphEdit, GraphNodeKind};
use rivu_protocol::model::EqBandEdit;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum EqTarget {
    Monitor,
    Node(u64),
}

type EqFormat = Option<(i64, u32, u16)>;

#[derive(Default)]
pub(super) struct EqualizerUi {
    editors: HashMap<(u64, EqTarget), Entity<EqualizerEditor>>,
    active: HashMap<u64, EqTarget>,
    pointer_count: Rc<Cell<usize>>,
    removing: HashMap<EqTarget, (EqualizerConfig, usize)>,
    // Replacement intent follows serialized command start/ack, not refreshes.
    reset_band_focus: Option<EqTarget>,
    pending_node_focus_reset: Option<(u64, u64)>,
}

struct EqualizerField {
    input: Entity<Input>,
    committed: String,
    dirty: bool,
    auto_pending: bool,
    generation: u64,
    draft_revision: Option<u64>,
    _subscription: Subscription,
}

pub(super) struct SubmittedField {
    editor: WeakEntity<EqualizerEditor>,
    key: String,
    input: Entity<Input>,
    generation: u64,
    text: String,
}

pub(super) struct NodeEqualizerSubmission {
    pub(super) config: EqualizerConfig,
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
    geometry: DockGeometry,
    geometry_epoch: u64,
    frequency: f64,
    gain: f64,
    _subscriptions: [Subscription; 2],
}

struct BandFocus {
    handle: FocusHandle,
    _subscription: Subscription,
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
    pending: usize,
    active: bool,
    response_dirty: bool,
    band: usize,
    band_focus: [Option<BandFocus>; MAX_BANDS],
    response_channel: usize,
    fields: HashMap<String, EqualizerField>,
    channel_choices: rivu_ui::ChoiceState<usize>,
    channel_anchor: Rc<Cell<Option<Bounds<Pixels>>>>,
    gain_submission_scheduled: bool,
    response: Arc<Vec<f64>>,
    response_error: Option<String>,
    error: Option<String>,
    maximum_frequency: f64,
    frequencies: [f64; 257],
    frequency_limit: f64,
    drag: Option<NodeDrag>,
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
        let pending = parent.pending;
        let geometry_epoch = parent.geometry_epoch;
        let format = equalizer_format(&state);
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
            pending,
            active: true,
            response_dirty: true,
            band: 0,
            band_focus: std::array::from_fn(|_| None),
            response_channel: 0,
            fields: HashMap::new(),
            channel_choices: rivu_ui::ChoiceState::default(),
            channel_anchor: Rc::new(Cell::new(None)),
            gain_submission_scheduled: false,
            response: Arc::new(Vec::new()),
            response_error: None,
            error: None,
            maximum_frequency: 20_000.,
            drag: None,
            bounds: Rc::new(Cell::new(None)),
            frequencies: [0.; 257],
            frequency_limit: 0.,
            removing_band: None,
            pending_added_band: None,
            geometry_epoch,
            pointer_count: Rc::clone(&parent.equalizer_ui.pointer_count),
        };
        editor.sync_fields(cx);
        editor.update_response();
        editor
    }

    fn tr(&self, id: &str) -> SharedString {
        self.context.tr(id)
    }

    fn reconcile(
        &mut self,
        state: Rc<GuiSnapshot>,
        config: &EqualizerConfig,
        availability: (bool, usize, EqFormat),
        geometry_epoch: u64,
        cx: &mut Context<Self>,
    ) {
        let (blocked, pending, format) = availability;
        let had_preview = self.drag.is_some();
        let format_changed = self.format != format;
        let response_format_changed = self.format.map(|(_, rate, channels)| (rate, channels))
            != format.map(|(_, rate, channels)| (rate, channels));
        let config_changed = &self.config != config;
        let revision = state.system.config.processing_graph.revision;
        let fence_changed = matches!(self.target, EqTarget::Node(_)) && self.revision != revision;
        let geometry_changed = self.geometry_epoch != geometry_epoch;
        // Ordinary same-length parameter updates preserve native keyboard focus.
        if self.config.bands.len() != config.bands.len() {
            // Without persistent band IDs, duplicate rows cannot be disambiguated
            // after an arbitrary structural snapshot. Retire native identities.
            self.remap_band_fields(&config.bands, cx);
            self.retire_band_focus();
        }
        let changed = config_changed
            || format_changed
            || fence_changed
            || geometry_changed
            || self.blocked != blocked
            || self.pending != pending;
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
        self.pending = pending;
        self.geometry_epoch = geometry_epoch;
        if changed {
            let enabled = !self.config_locked();
            for (key, field) in &self.fields {
                field.input.update(cx, |input, cx| {
                    input.set_enabled(enabled && (key != "band_count" || pending == 0), cx)
                });
            }
        }
        if format_changed || geometry_changed || blocked {
            self.channel_choices.close();
        }
        if config_changed || format_changed || fence_changed || geometry_changed || blocked {
            self.take_drag();
        }
        if geometry_changed {
            self.bounds.set(None);
        }
        self.band = self.band.min(self.config.bands.len().saturating_sub(1));
        if config_changed {
            self.sync_fields(cx);
            self.removing_band = None;
        }
        if config_changed
            || response_format_changed
            || had_preview && (format_changed || fence_changed || geometry_changed || blocked)
        {
            self.response_dirty = true;
            if self.active {
                self.update_response();
            }
        }
        self.schedule_gain_submission(cx);
        if changed {
            cx.notify();
        }
    }

    fn retire_band_focus(&mut self) {
        self.band_focus.iter_mut().for_each(|focus| *focus = None);
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
                && this.band != index
            {
                this.cancel_pointer(cx);
                this.band = index;
                this.sync_fields(cx);
                cx.notify();
            }
        });
        self.band_focus[index] = Some(BandFocus {
            handle: handle.clone(),
            _subscription: subscription,
        });
        handle
    }

    fn focus_graph_band(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.band = index;
        self.sync_fields(cx);
        self.band_focus_handle(index, window, cx).focus(window, cx);
    }

    fn cancel_pointer(&mut self, cx: &mut Context<Self>) {
        if self.take_drag().is_some() {
            self.response_dirty = true;
            if self.active {
                self.update_response();
            }
            cx.notify();
        }
    }

    fn take_drag(&mut self) -> Option<NodeDrag> {
        let drag = self.drag.take();
        if drag.is_some() {
            self.pointer_count
                .set(self.pointer_count.get().saturating_sub(1));
        }
        drag
    }

    fn node_geometry_valid(&mut self, window: &Window, cx: &mut Context<Self>) -> bool {
        let snapshot = self.context.snapshot();
        let config = target_config(&snapshot, self.target);
        let format_valid =
            self.owner.upgrade().is_some() && equalizer_format(&snapshot) == self.format;
        let valid = !self.config_locked()
            && format_valid
            && config == Some(&self.config)
            && (self.target == EqTarget::Monitor
                || snapshot.system.config.processing_graph.revision == self.revision)
            && self.drag.as_ref().is_some_and(|drag| {
                drag.geometry == DockGeometry::current(window, snapshot.system.config.ui_scale)
                    && drag.geometry_epoch == self.geometry_epoch
            });
        if !valid {
            self.cancel_pointer(cx);
        }
        valid
    }

    fn config_locked(&self) -> bool {
        self.blocked || !self.config.enabled
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
    }

    fn field(&mut self, key: &str, label: &str, value: String, cx: &mut Context<Self>) {
        if let Some(field) = self.fields.get_mut(key) {
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
        if !self.active
            || key
                .strip_prefix("band.")
                .and_then(|path| path.split('.').next())
                .and_then(|index| index.parse::<usize>().ok())
                .is_some_and(|index| index != self.band)
        {
            return;
        }
        let placeholder = if key.ends_with(".channels") {
            self.tr("processing-channels-placeholder")
        } else {
            label.to_owned().into()
        };
        let input = cx.new(|cx| Input::new(label.to_owned(), value.clone(), placeholder, cx));
        let enabled = !self.config_locked() && (key != "band_count" || self.pending == 0);
        input.update(cx, |input, cx| input.set_enabled(enabled, cx));
        let subscription = self.subscribe_field(&input, key.to_owned(), cx);
        self.fields.insert(
            key.to_owned(),
            EqualizerField {
                input,
                committed: value,
                dirty: false,
                auto_pending: false,
                generation: 0,
                draft_revision: None,
                _subscription: subscription,
            },
        );
    }

    fn subscribe_field(
        &self,
        input: &Entity<Input>,
        name: String,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe(input, move |this, _, event, cx| match event {
            InputEvent::Changed {
                user: true,
                composing,
            } => {
                if let Some(field) = this.fields.get_mut(&name) {
                    if !field.dirty {
                        field.draft_revision = Some(this.revision);
                    }
                    field.dirty = true;
                    field.generation = field.generation.wrapping_add(1);
                    if matches!(name.as_str(), "preamp_db" | "output_gain_db") {
                        field.auto_pending =
                            !composing && valid_gain_draft(field.input.read(cx).text());
                    }
                }
                this.schedule_gain_submission(cx);
                cx.notify();
                let _ = this.owner.update(cx, |_, cx| cx.notify());
            }
            InputEvent::Submit => this.apply_field(&name, cx),
            _ => {}
        })
    }

    fn schedule_gain_submission(&mut self, cx: &mut Context<Self>) {
        if self.gain_submission_scheduled
            || self.config_locked()
            || !self.fields.values().any(|field| field.auto_pending)
        {
            return;
        }
        // Defer out of the input callback and reconcile. A node edit owns one
        // graph revision; its next gain waits for acknowledgement and rebasing.
        self.gain_submission_scheduled = true;
        let editor = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = editor.update(cx, |this, cx| {
                this.gain_submission_scheduled = false;
                for key in ["preamp_db", "output_gain_db"] {
                    let node_busy = matches!(this.target, EqTarget::Node(_))
                        && this
                            .owner
                            .upgrade()
                            .is_some_and(|owner| owner.read(cx).equalizer_blocked(this.target));
                    if this.config_locked() || node_busy {
                        break;
                    }
                    let Some(field) = this.fields.get_mut(key) else {
                        continue;
                    };
                    if !field.auto_pending || field.input.read(cx).is_composing() {
                        continue;
                    }
                    field.auto_pending = false;
                    this.apply_field(key, cx);
                }
            });
        });
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
            let Some((index, name)) = key
                .strip_prefix("band.")
                .and_then(|path| path.split_once('.'))
            else {
                self.fields.insert(key, field);
                continue;
            };
            let Some(new) = index
                .parse::<usize>()
                .ok()
                .and_then(|index| mapping.get(index).copied().flatten())
            else {
                continue;
            };
            let new_key = format!("band.{new}.{name}");
            if new_key != key {
                field._subscription = self.subscribe_field(&field.input, new_key.clone(), cx);
            }
            self.fields.insert(new_key, field);
        }
    }
    fn sync_fields(&mut self, cx: &mut Context<Self>) {
        let state = Rc::clone(&self.state);
        let Some(eq) = target_config(&state, self.target) else {
            return;
        };
        self.field(
            "preamp_db",
            &self.tr("processing-preamp"),
            eq.preamp_db.to_string(),
            cx,
        );
        self.field(
            "output_gain_db",
            &self.tr("processing-output-gain"),
            eq.output_gain_db.to_string(),
            cx,
        );
        self.field(
            "band_count",
            &self.tr("processing-band-count-label"),
            eq.bands.len().to_string(),
            cx,
        );
        for (i, b) in eq.bands.iter().enumerate() {
            for (name, label, value) in [
                (
                    "frequency_hz",
                    "processing-frequency",
                    b.frequency_hz.to_string(),
                ),
                ("gain_db", "processing-gain", b.gain_db.to_string()),
                ("q", "processing-q", b.q.to_string()),
                (
                    "shelf_slope",
                    "processing-shelf-slope",
                    b.shelf_slope.to_string(),
                ),
                ("stages", "processing-stages", b.stages.to_string()),
                (
                    "channels",
                    "processing-channels",
                    match &b.channels {
                        ChannelTarget::All => "all".to_owned(),
                        ChannelTarget::Selected(channels) => channels
                            .iter()
                            .map(|c| (c + 1).to_string())
                            .collect::<Vec<_>>()
                            .join(","),
                    },
                ),
            ] {
                self.field(&format!("band.{i}.{name}"), &self.tr(label), value, cx);
            }
        }
        self.prune_inputs();
    }

    fn prune_inputs(&mut self) {
        self.fields.retain(|key, field| {
            let index = key
                .strip_prefix("band.")
                .and_then(|path| path.split('.').next())
                .and_then(|index| index.parse::<usize>().ok());
            index.is_none_or(|index| index < self.config.bands.len())
                && (field.dirty || self.active && index.is_none_or(|index| index == self.band))
        });
    }
    fn apply_field(&mut self, key: &str, cx: &mut Context<Self>) {
        if self.config_locked() || key == "band_count" && self.pending > 0 {
            return;
        }
        if matches!(self.target, EqTarget::Node(_))
            && self
                .fields
                .get(key)
                .is_some_and(|field| field.dirty && field.draft_revision != Some(self.revision))
        {
            self.error = Some(self.context.i18n.text("processing-editor-stale").to_owned());
            cx.notify();
            return;
        }
        self.error = None;
        if let Some(field) = self.fields.get_mut(key) {
            field.auto_pending = false;
        }
        let value = self
            .fields
            .get(key)
            .map(|f| f.input.read(cx).text().trim().to_owned())
            .unwrap_or_default();
        match self.field_edit(key, &value) {
            Ok(Some(edit)) => {
                if let EqualizerEdit::ResizeBands { count } = &edit {
                    self.cancel_pointer(cx);
                    self.pending_added_band = (*count > self.config.bands.len()).then_some((
                        self.config.bands.len(),
                        count.saturating_sub(self.config.bands.len()),
                    ));
                }
                let submitted = self.fields.get(key).map(|field| SubmittedField {
                    editor: cx.entity().downgrade(),
                    key: key.to_owned(),
                    input: field.input.clone(),
                    generation: field.generation,
                    text: value,
                });
                self.submit_edit(edit, submitted, cx);
            }
            Ok(None) => {}
            Err(error) => {
                self.error = Some(error);
                cx.notify();
            }
        }
    }

    fn field_edit(&self, key: &str, value: &str) -> Result<Option<EqualizerEdit>, String> {
        let number = || {
            value
                .parse::<f64>()
                .ok()
                .filter(|n| n.is_finite())
                .ok_or_else(|| {
                    self.context
                        .i18n
                        .text("processing-invalid-number")
                        .to_owned()
                })
        };
        let edit = if key == "band_count" {
            let count = value
                .parse::<usize>()
                .ok()
                .filter(|count| *count <= MAX_BANDS)
                .ok_or_else(|| self.tr("processing-band-count-invalid").to_string())?;
            Some(EqualizerEdit::ResizeBands { count })
        } else if matches!(key, "preamp_db" | "output_gain_db") {
            let gain = number()?;
            if !(-24.0..=24.0).contains(&gain) {
                return Err(self.tr("processing-invalid-number").to_string());
            }
            Some(if key == "preamp_db" {
                EqualizerEdit::PreampDb(gain)
            } else {
                EqualizerEdit::OutputGainDb(gain)
            })
        } else if let Some(path) = key.strip_prefix("band.") {
            let (index, name) = path.split_once('.').ok_or("Invalid band field")?;
            let index = index.parse::<usize>().map_err(|e| e.to_string())?;
            let field = match name {
                "frequency_hz" => EqBandEdit::FrequencyHz(number()?),
                "gain_db" => EqBandEdit::GainDb(number()?),
                "q" => EqBandEdit::Q(number()?),
                "shelf_slope" => EqBandEdit::ShelfSlope(number()?),
                "stages" => EqBandEdit::Stages(value.parse::<u8>().map_err(|e| e.to_string())?),
                "channels" => EqBandEdit::Channels(if value.eq_ignore_ascii_case("all") {
                    ChannelTarget::All
                } else {
                    let channels = value
                        .split(',')
                        .map(|c| {
                            c.trim()
                                .parse::<usize>()
                                .ok()
                                .and_then(|n| n.checked_sub(1))
                                .ok_or_else(|| {
                                    self.context.i18n.text("processing-channel-help").to_owned()
                                })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    ChannelTarget::Selected(channels)
                }),
                _ => return Ok(None),
            };
            Some(EqualizerEdit::BandField { index, field })
        } else {
            None
        };
        Ok(edit)
    }
    fn step_field(&mut self, key: &str, step: f64, cx: &mut Context<Self>) {
        if self.config_locked() {
            return;
        }
        let Some(field) = self.fields.get_mut(key) else {
            return;
        };
        let value = field.input.read(cx).text().trim().parse::<f64>();
        match value {
            Ok(value) if value.is_finite() && (value + step).is_finite() => {
                if !field.dirty {
                    field.draft_revision = Some(self.revision);
                }
                field.dirty = true;
                field.generation = field.generation.wrapping_add(1);
                field.input.update(cx, |input, cx| {
                    input.set_text((value + step).to_string(), cx);
                });
                self.error = None;
                let _ = self.owner.update(cx, |_, cx| cx.notify());
            }
            _ => self.error = Some(self.tr("processing-invalid-number").to_string()),
        }
        cx.notify();
    }

    fn field_view(
        &self,
        key: &str,
        label: impl Into<SharedString>,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let field = &self.fields[key];
        let name = key.rsplit('.').next().unwrap_or(key);
        let compact = matches!(name, "preamp_db" | "output_gain_db");
        let (unit, step) = match name {
            "frequency_hz" => ("Hz", Some(1.)),
            "preamp_db" | "output_gain_db" => ("dB", None),
            "gain_db" => ("dB", Some(0.5)),
            "q" | "shelf_slope" => ("", Some(0.1)),
            "stages" => ("", Some(1.)),
            _ => ("", None),
        };
        let help = match name {
            "channels" => Some("processing-channel-help"),
            "stages" => Some("processing-stage-help"),
            _ => None,
        };
        let mut control = row()
            .gap_1()
            .when(compact, |view| view.flex_shrink_0())
            .child(
                caption(theme, label)
                    .when(!compact, |view| view.w(rems(7.)))
                    .flex_shrink_0()
                    .truncate()
                    .id(SharedString::from(format!("eq-label-{key}")))
                    .when_some(help, |view, help| {
                        view.tooltip(rivu_ui::text_tooltip(self.tr(help)))
                    }),
            )
            .child(
                div()
                    .w(rems(0.4))
                    .flex_shrink_0()
                    .text_color(rgb(theme.accent))
                    .id(SharedString::from(format!("eq-draft-{key}")))
                    .tooltip(rivu_ui::text_tooltip(self.tr("processing-draft")))
                    .when(field.dirty, |view| view.child("·")),
            )
            .child(
                div()
                    .min_w_0()
                    .when(compact, |view| view.w(rems(3.5)).flex_shrink_0())
                    .when(!compact, |view| view.flex_1())
                    .child(field.input.clone()),
            )
            .child(
                caption(theme, unit)
                    .when(!compact, |view| view.w(rems(1.5)))
                    .flex_shrink_0(),
            );
        if let Some(step) = step {
            for (suffix, glyph, hint, delta) in [
                ("less", "−", "processing-field-decrease", -step),
                ("more", "+", "processing-field-increase", step),
            ] {
                let key_owned = key.to_owned();
                control = control.child(
                    rivu_ui::IconButton::new(
                        SharedString::from(format!("eq-step-{key}-{suffix}")),
                        glyph,
                        self.tr(hint),
                    )
                    .enabled(enabled)
                    .build(
                        rivu_ui::IconButtonStyle::Quiet,
                        rivu_ui::IconButtonDensity::Compact,
                        cx,
                        move |this, _, cx| this.step_field(&key_owned, delta, cx),
                    ),
                );
            }
        }
        if !compact {
            let key_owned = key.to_owned();
            control = control.child(
                rivu_ui::IconButton::new(
                    SharedString::from(format!("apply-{key}")),
                    "",
                    self.tr("processing-apply"),
                )
                .enabled(enabled)
                .build(
                    rivu_ui::IconButtonStyle::Quiet,
                    rivu_ui::IconButtonDensity::Compact,
                    cx,
                    move |this, _, cx| this.apply_field(&key_owned, cx),
                ),
            );
        }
        control.opacity(if enabled { 1. } else { 0.5 })
    }
    fn update_response(&mut self) {
        self.response_dirty = false;
        let Some((_, sample_rate, channel_count)) = self.format else {
            self.response = Arc::new(Vec::new());
            self.response_error = Some(
                self.context
                    .i18n
                    .text("processing-response-unknown")
                    .to_owned(),
            );
            return;
        };
        if sample_rate == 0 || channel_count == 0 {
            self.response = Arc::new(Vec::new());
            self.response_error = Some(
                self.context
                    .i18n
                    .text("processing-response-unknown")
                    .to_owned(),
            );
            return;
        }
        self.maximum_frequency = (sample_rate as f64 * 0.499).min(96_000.);
        self.response_channel = self.response_channel.min(channel_count as usize - 1);
        if self.frequency_limit != self.maximum_frequency {
            self.frequency_limit = self.maximum_frequency;
            for (index, frequency) in self.frequencies.iter_mut().enumerate() {
                *frequency = 20. * (self.maximum_frequency / 20.).powf(index as f64 / 256.);
            }
        }
        let mut response = vec![0.; self.frequencies.len()];
        let preview = self.drag.as_ref().map(|drag| {
            let mut config = self.config.clone();
            if let Some(band) = config.bands.get_mut(drag.band) {
                band.frequency_hz = drag.frequency;
                band.gain_db = drag.gain;
            }
            config
        });
        let config = preview.as_ref().unwrap_or(&self.config);
        match config.sample_response_db(
            sample_rate,
            channel_count as usize,
            self.response_channel,
            &self.frequencies,
            &mut response,
        ) {
            Ok(()) => {
                self.response = Arc::new(response);
                self.response_error = None;
            }
            Err(error) => {
                self.response = Arc::new(Vec::new());
                self.response_error = Some(error.to_string());
            }
        }
    }

    fn frequency_x(&self, frequency: f64) -> f32 {
        ((frequency / 20.).ln() / (self.maximum_frequency / 20.).ln()).clamp(0., 1.) as f32
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
                    if this.drag.as_ref().is_some_and(|d| d.panel == panel) {
                        if !event.dragging() {
                            this.cancel_pointer(cx);
                        } else if this.node_geometry_valid(window, cx) {
                            this.move_node(event.position, cx);
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
                    if this.drag.as_ref().is_some_and(|d| d.panel == panel)
                        && this.node_geometry_valid(window, cx)
                    {
                        this.move_node(event.position, cx);
                        if let Some(drag) = this.take_drag() {
                            if drag.moved {
                                this.update_response();
                                let band = &this.config.bands[drag.band];
                                if band.frequency_hz != drag.frequency || band.gain_db != drag.gain
                                {
                                    this.band_edit(
                                        drag.band,
                                        EqBandEdit::Position {
                                            frequency_hz: drag.frequency,
                                            gain_db: drag.gain,
                                        },
                                        cx,
                                    );
                                }
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

    fn graph(&mut self, panel: u64, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = rivu_ui::theme::palette(cx);
        let measured = Rc::clone(&self.bounds);
        let curve = Arc::clone(&self.response);
        let nodes = self
            .config
            .bands
            .iter()
            .take(MAX_BANDS)
            .enumerate()
            .map(|(i, b)| {
                let (frequency, gain) = self
                    .drag
                    .as_ref()
                    .filter(|d| d.band == i)
                    .map(|d| (d.frequency, d.gain))
                    .unwrap_or((b.frequency_hz, b.gain_db));
                (i, self.frequency_x(frequency), (0.5 - gain / 48.) as f32)
            })
            .collect::<Vec<_>>();
        let selected = self.band;
        let bounds_copy = measured.clone();
        let weak = cx.entity().downgrade();
        let geometry_owner = weak.clone();
        let geometry_epoch = self.geometry_epoch;
        let ui_scale = self.state.system.config.ui_scale;
        let mut controls = Vec::with_capacity(nodes.len());
        for &(index, x, y) in nodes
            .iter()
            .filter(|(index, _, _)| *index != selected)
            .chain(nodes.iter().filter(|(index, _, _)| *index == selected))
        {
            let focus = self.band_focus_handle(index, window, cx);
            let band = &self.config.bands[index];
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
                        self.context.i18n.text("processing-node-help")
                    )))
                    .aria_value(SharedString::from(format!(
                        "{:.1} Hz, {:.2} dB",
                        band.frequency_hz, band.gain_db
                    )))
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
                    .child((index + 1).to_string())
                    .focus(|style| style.border_color(rgb(theme.accent)))
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
                        {
                            this.node_key(index, event, cx);
                        }
                    }))
                    .into_any_element(),
            );
        }
        div()
            .id(("eq-graph", panel))
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
                    if this.blocked {
                        return;
                    }
                    let Some((bounds, geometry)) = measured.get() else {
                        return;
                    };
                    if geometry_epoch != this.geometry_epoch
                        || geometry
                            != DockGeometry::current(
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
                        if this.config_locked() {
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
                        this.take_drag();
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
                        let geometry = DockGeometry::current(window, ui_scale);
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
                        let mut path = PathBuilder::stroke(px(2.));
                        for (i, db) in curve.iter().enumerate() {
                            let p = point(
                                bounds.origin.x + bounds.size.width * (i as f32 / 256.),
                                bounds.origin.y
                                    + bounds.size.height
                                        * (0.5 - (*db).clamp(-24., 24.) as f32 / 48.),
                            );
                            if i == 0 {
                                path.move_to(p);
                            } else {
                                path.line_to(p);
                            }
                        }
                        if !curve.is_empty()
                            && let Ok(path) = path.build()
                        {
                            window.paint_path(path, rgb(theme.accent));
                        }
                    },
                )
                .size_full(),
            )
            .children(controls)
            .into_any_element()
    }

    fn node_key(&mut self, index: usize, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if self.blocked || event.keystroke.modifiers != Modifiers::default() {
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
        let field = match event.keystroke.key.as_str() {
            "left" => EqBandEdit::FrequencySteps {
                steps: -1,
                maximum_hz: self.maximum_frequency,
            },
            "right" => EqBandEdit::FrequencySteps {
                steps: 1,
                maximum_hz: self.maximum_frequency,
            },
            "up" | "down"
                if matches!(
                    band.kind,
                    FilterKind::Peak | FilterKind::LowShelf | FilterKind::HighShelf
                ) =>
            {
                EqBandEdit::GainSteps(if event.keystroke.key == "up" { 1 } else { -1 })
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
        if self.config_locked() {
            return;
        }
        self.cancel_pointer(cx);
        self.band = index;
        self.sync_fields(cx);
        self.band_edit(index, field, cx);
        cx.stop_propagation();
    }

    fn move_node(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
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
            let original_x =
                (drag.original_frequency / 20.).ln() / (self.maximum_frequency / 20.).ln();
            let x = (original_x + (delta.x / drag.bounds.size.width) as f64).clamp(0., 1.);
            drag.frequency = 20. * (self.maximum_frequency / 20.).powf(x);
            let kind = self.config.bands[drag.band].kind;
            if matches!(
                kind,
                FilterKind::Peak | FilterKind::LowShelf | FilterKind::HighShelf
            ) {
                drag.gain = (drag.original_gain - (delta.y / drag.bounds.size.height) as f64 * 48.)
                    .clamp(-24., 24.);
            }
            if previous != (drag.frequency, drag.gain) {
                cx.notify();
                self.update_response();
            }
        }
    }

    fn choose_response_channel(&mut self, index: usize, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(channel) = self.channel_choices.item(index).map(|item| item.value) {
            self.response_channel = channel;
            self.update_response();
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
        let eq = &self.config;
        let gains = row()
            .min_w_0()
            .flex_wrap()
            .gap_2()
            .child(self.field_view(
                "preamp_db",
                self.tr("processing-input-gain-short"),
                !self.config_locked(),
                cx,
            ))
            .child(self.field_view(
                "output_gain_db",
                self.tr("processing-output-gain-short"),
                !self.config_locked(),
                cx,
            ));
        let mut controls = row().min_w_0().flex_wrap().gap_3().child(gains);
        let mut curve = column().gap_1().min_w_0();
        if let Some((_, _, channel_count)) = self.format
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
            let trigger = rivu_ui::dropdown_trigger(
                theme,
                ("eq-response-channel", panel),
                format!(
                    "{} {}",
                    self.tr("processing-response-channel"),
                    self.response_channel + 1
                ),
                self.channel_choices.is_open(),
                cx,
            )
            .w_auto()
            .flex_shrink_0()
            .relative()
            .role(Role::ComboBox)
            .aria_label(self.tr("processing-response-channel"))
            .debug_selector(move || format!("eq-response-channel-{panel}"))
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                match this
                    .channel_choices
                    .key(event, rivu_ui::ChoiceMode::Select, false, window)
                    .apply(window, cx)
                {
                    rivu_ui::ChoiceAction::Commit(index) => {
                        this.choose_response_channel(index, window, cx)
                    }
                    rivu_ui::ChoiceAction::Dismiss => {
                        this.channel_choices.close();
                        cx.notify();
                    }
                    rivu_ui::ChoiceAction::Navigated => cx.notify(),
                    _ => {}
                }
            }))
            .child(measure);
            controls = controls.child(rivu_ui::activate_control(
                trigger,
                cx,
                move |this, _, cx| {
                    if this.channel_choices.is_open() {
                        this.channel_choices.close();
                    } else {
                        this.channel_choices.open(
                            (0..channel_count as usize).map(|channel| {
                                rivu_ui::DropdownItem::new(channel, (channel + 1).to_string())
                            }),
                            this.response_channel,
                        );
                    }
                    cx.notify();
                },
            ));
        }
        curve = curve.child(graph);
        if self.response_error.is_none() {
            let mut labels = row().justify_between().gap_1();
            for fraction in [0., 0.25, 0.5, 0.75, 1.] {
                let frequency = 20. * (self.maximum_frequency / 20.).powf(fraction);
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
        }
        if self.format.is_some() {
            curve = curve.child(caption(
                theme,
                format!("20–{:.0} Hz · ±24 dB", self.maximum_frequency),
            ));
        }
        let mut precision = column().gap_1().min_w_0();
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
                    .child(self.fields["band_count"].input.clone()),
            );
        let presets =
            row()
                .min_w_0()
                .items_start()
                .gap_2()
                .when(self.target == EqTarget::Monitor, |view| {
                    let preset = self
                        .owner
                        .update(cx, |owner, cx| owner.preset_view(panel, cx))
                        .ok();
                    view.child(div().flex_1().max_w_full().min_w_0().children(preset))
                });
        let reset_controls = row()
            .gap_1()
            .flex_shrink_0()
            .child(
                rivu_ui::IconButton::new(("eq-flat", panel), "󰑐", self.tr("processing-flat"))
                    .enabled(!self.config_locked())
                    .build(
                        rivu_ui::IconButtonStyle::Quiet,
                        rivu_ui::IconButtonDensity::Compact,
                        cx,
                        |this, _, cx| {
                            let mut config = EqualizerConfig::default();
                            config.enabled = this.config.enabled;
                            this.eq(EqualizerEdit::Replace { config }, cx);
                        },
                    ),
            )
            .when(self.target == EqTarget::Monitor, |r| {
                r.child(
                    rivu_ui::IconButton::new(("eq-ab", panel), "󰓡", self.tr("processing-ab"))
                        .enabled(!self.config_locked())
                        .build(
                            rivu_ui::IconButtonStyle::Quiet,
                            rivu_ui::IconButtonDensity::Compact,
                            cx,
                            |this, _, cx| this.eq(EqualizerEdit::SwapAb, cx),
                        ),
                )
            })
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
            .when(self.target == EqTarget::Monitor, |view| {
                view.child(presets.flex_shrink_0())
            })
            .child(
                row()
                    .min_w_0()
                    .flex_1()
                    .flex_basis(rems(24.))
                    .gap_1()
                    .id(("eq-bank-tools", panel))
                    .overflow_x_scroll()
                    .child(count_control)
                    .child(reset_controls),
            );
        if let Some(band) = eq.bands.get(self.band) {
            let index = self.band;
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
                        caption(theme, title.clone())
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_color(rgb(theme.accent))
                            .id(("eq-band-title", panel))
                            .tooltip(rivu_ui::text_tooltip(title)),
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
                                    this.band_edit(index, EqBandEdit::Enabled(value), cx)
                                },
                            ))
                            .child(checkbox(
                                ("band-solo", panel),
                                self.tr("processing-solo"),
                                band.solo,
                                !self.config_locked(),
                                cx,
                                move |this, value, cx| {
                                    this.band_edit(index, EqBandEdit::Solo(value), cx)
                                },
                            ))
                            .child(
                                rivu_ui::IconButton::new(
                                    ("remove-band", panel),
                                    "󰆴",
                                    self.tr("processing-remove-band"),
                                )
                                .enabled(!self.config_locked())
                                .build(
                                    rivu_ui::IconButtonStyle::Quiet,
                                    rivu_ui::IconButtonDensity::Compact,
                                    cx,
                                    move |this, _, cx| {
                                        this.eq(EqualizerEdit::RemoveBand { index }, cx)
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
                types = types.child(disabled_control(
                    theme,
                    button(
                        SharedString::from(format!("band-kind-{kind:?}")),
                        self.tr(key),
                        cx,
                        move |this, _, cx| this.band_edit(index, EqBandEdit::Kind(kind), cx),
                    )
                    .when(kind == band.kind, |v| v.bg(rgb(theme.selected))),
                    self.config_locked(),
                ));
            }
            precision = precision.child(types);
            for (name, key, applicable) in [
                ("frequency_hz", "processing-frequency", true),
                (
                    "gain_db",
                    "processing-gain",
                    matches!(
                        band.kind,
                        FilterKind::Peak | FilterKind::LowShelf | FilterKind::HighShelf
                    ),
                ),
                (
                    "q",
                    "processing-q",
                    !matches!(band.kind, FilterKind::LowShelf | FilterKind::HighShelf),
                ),
                (
                    "shelf_slope",
                    "processing-shelf-slope",
                    matches!(band.kind, FilterKind::LowShelf | FilterKind::HighShelf),
                ),
                (
                    "stages",
                    "processing-stages",
                    !matches!(
                        band.kind,
                        FilterKind::Peak | FilterKind::LowShelf | FilterKind::HighShelf
                    ),
                ),
                ("channels", "processing-channels", true),
            ] {
                if applicable {
                    precision = precision.child(self.field_view(
                        &format!("band.{index}.{name}"),
                        self.tr(key),
                        !self.config_locked(),
                        cx,
                    ));
                }
            }
        } else {
            precision = precision.child(caption(theme, self.tr("processing-band-empty")));
        }
        let body = if eq.bands.is_empty() {
            column().min_w_0().gap_1().child(precision).child(curve)
        } else {
            row()
                .min_w_0()
                .items_start()
                .flex_wrap()
                .gap_3()
                .child(curve.flex_1().flex_basis(rems(28.)))
                .child(precision.flex_1().flex_basis(rems(24.)))
        };
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
    }
}

impl Render for EqualizerEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let panel = self.panel;
        self.equalizer_view(panel, window, cx)
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

impl AudioProcessing {
    pub(super) fn eq(&mut self, edit: EqualizerEdit, cx: &mut Context<Self>) {
        if !self.context.snapshot().system.config.equalizer.enabled
            && !matches!(edit, EqualizerEdit::Enabled(_))
        {
            return;
        }
        self.send(Command::EqualizerEdit { edit }, cx);
    }

    fn equalizer_blocked(&self, target: EqTarget) -> bool {
        matches!(target, EqTarget::Node(_)) && (self.graph_busy() || self.graph_has_drafts())
    }

    fn submit_target_equalizer(
        &mut self,
        source: (EqTarget, u64, &EqualizerConfig, EqFormat),
        edit: EqualizerEdit,
        submitted: Option<SubmittedField>,
        cx: &mut Context<Self>,
    ) {
        let (target, revision, expected, expected_format) = source;
        let snapshot = self.context.snapshot();
        if target_config(&snapshot, target) != Some(expected)
            || equalizer_format(&snapshot) != expected_format
            || matches!(target, EqTarget::Node(_))
                && (self.equalizer_blocked(target)
                    || snapshot.system.config.processing_graph.revision != revision
                    || self.state.system.config.processing_graph.revision != revision)
        {
            self.error = Some(self.context.i18n.text("processing-editor-stale").to_owned());
            cx.notify();
            return;
        }
        if matches!(target, EqTarget::Node(_))
            && let EqualizerEdit::RemoveBand { index } = &edit
        {
            self.equalizer_ui
                .removing
                .insert(target, (expected.clone(), *index));
        }
        match target {
            EqTarget::Monitor => self.enqueue_submission(
                Submission {
                    command: Command::EqualizerEdit { edit },
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
                    return;
                };
                let GraphNodeKind::Equalizer {
                    config,
                    implementation,
                } = &node.kind
                else {
                    return;
                };
                let mut next = config.clone();
                let sample_rate = equalizer_format(&snapshot).map(|(_, rate, _)| rate);
                let replaces_band_identity = replaces_band_identity(&edit);
                match edit.apply_config(&mut next, sample_rate) {
                    Ok(None) => {
                        let mut submission = NodeEqualizerSubmission {
                            config: next,
                            revision,
                            node: id,
                            fields: submitted.into_iter().collect(),
                        };
                        let config = std::mem::take(&mut submission.config);
                        if replaces_band_identity {
                            self.equalizer_ui.pending_node_focus_reset = Some((id, revision));
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
                    }
                    Err(error) => {
                        self.error = Some(error.to_string());
                        cx.notify();
                    }
                }
            }
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
        if self.equalizer_ui.active.get(&panel) != Some(&target) {
            self.deactivate_equalizer_panel(panel, window, cx);
            self.equalizer_ui.active.insert(panel, target);
        }
        let key = (panel, target);
        if let Some(editor) = self.equalizer_ui.editors.get(&key) {
            let editor = editor.clone();
            let availability = (
                self.equalizer_blocked(target),
                self.pending,
                equalizer_format(&self.state),
            );
            editor.update(cx, |editor, cx| {
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
                        editor.update_response();
                    }
                }
            });
            return editor;
        }
        let owner = cx.entity().downgrade();
        let editor = cx.new(|cx| EqualizerEditor::new(owner, target, panel, self, config, cx));
        self.equalizer_ui.editors.insert(key, editor.clone());
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
        column()
            .gap_3()
            .child(caption(
                rivu_ui::theme::palette(cx),
                self.tr("processing-editor-monitor"),
            ))
            .child(editor)
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
            .when(self.equalizer_blocked(target), |view| {
                view.child(caption(theme, self.tr("processing-editor-drafts-block")))
            })
            .child(editor)
    }

    pub(super) fn notify_preset_panel(&self, panel: u64, cx: &mut Context<Self>) {
        if let Some(editor) = self.equalizer_ui.editors.get(&(panel, EqTarget::Monitor)) {
            editor.update(cx, |_, cx| cx.notify());
        }
    }

    pub(super) fn notify_preset_labels(&self, cx: &mut Context<Self>) {
        for ((panel, target), editor) in &self.equalizer_ui.editors {
            if *target == EqTarget::Monitor && self.equalizer_ui.active.get(panel) == Some(target) {
                editor.update(cx, |editor, cx| {
                    if editor.active {
                        cx.notify();
                    }
                });
            }
        }
    }

    pub(super) fn reconcile_equalizers(&mut self, cx: &mut Context<Self>) {
        let state = Rc::clone(&self.state);
        let blocked = self.equalizer_blocked(EqTarget::Node(0));
        let format = equalizer_format(&state);
        let removals = &self.equalizer_ui.removing;
        let reset_band_focus = self.equalizer_ui.reset_band_focus;
        let command_active = self.command_active;
        self.equalizer_ui.editors.retain(|(_, target), editor| {
            let Some(config) = target_config(&state, *target) else {
                editor.update(cx, |editor, cx| editor.cancel_pointer(cx));
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
                    (
                        matches!(target, EqTarget::Node(_)) && blocked,
                        self.pending,
                        format,
                    ),
                    self.geometry_epoch,
                    cx,
                );
            });
            true
        });
        self.equalizer_ui
            .active
            .retain(|_, target| target_config(&state, *target).is_some());
        self.equalizer_ui.removing.retain(|target, (previous, _)| {
            target_config(&state, *target).is_some_and(|config| config.bands == previous.bands)
        });
        if !command_active {
            self.equalizer_ui.reset_band_focus = None;
        }
    }

    pub(super) fn cancel_equalizer_pointer(&mut self, cx: &mut Context<Self>) -> bool {
        let mut cancelled = false;
        for editor in self.equalizer_ui.editors.values() {
            editor.update(cx, |editor, cx| {
                cancelled |= editor.drag.is_some();
                editor.cancel_pointer(cx);
            });
        }
        cancelled
    }

    pub(super) fn invalidate_equalizer_geometry(&mut self, cx: &mut Context<Self>) {
        for editor in self.equalizer_ui.editors.values() {
            editor.update(cx, |editor, cx| {
                editor.geometry_epoch = self.geometry_epoch;
                editor.bounds.set(None);
                editor.cancel_pointer(cx);
            });
        }
    }

    pub(super) fn equalizer_pointer_active(&self) -> bool {
        self.equalizer_ui.pointer_count.get() > 0
    }

    pub(super) fn equalizer_input_focused(&self, window: &Window, cx: &App) -> bool {
        self.equalizer_ui.active.iter().any(|(panel, target)| {
            self.equalizer_ui
                .editors
                .get(&(*panel, *target))
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
        layout: &super::super::layout::Layout,
        visible: bool,
        cx: &mut Context<Self>,
    ) {
        self.equalizer_ui.editors.retain(|(panel, _), editor| {
            let exists = layout.contains_panel(*panel);
            if !visible || !layout.is_panel_active(*panel) {
                editor.update(cx, |editor, cx| {
                    editor.active = false;
                    editor.cancel_pointer(cx);
                    editor.prune_inputs();
                });
            }
            exists
        });
        self.equalizer_ui
            .active
            .retain(|panel, _| layout.contains_panel(*panel));
    }
}

fn merge_draft_edit(
    edits: &mut HashMap<String, EqualizerEdit>,
    key: &str,
    edit: EqualizerEdit,
) -> bool {
    if let Some(previous) = edits.get(key) {
        return previous == &edit;
    }
    edits.insert(key.to_owned(), edit);
    true
}

impl AudioProcessing {
    pub(super) fn node_equalizer_dirty_count(&self, cx: &App) -> usize {
        self.equalizer_ui
            .editors
            .iter()
            .filter(|((_, target), _)| matches!(target, EqTarget::Node(_)))
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
        self.equalizer_ui
            .editors
            .iter()
            .filter(|((_, target), _)| *target == EqTarget::Node(id))
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

    pub(super) fn prepare_node_equalizer_drafts(
        &self,
        id: u64,
        base: &EqualizerConfig,
        cx: &App,
    ) -> Result<Option<NodeEqualizerSubmission>, String> {
        let revision = self.state.system.config.processing_graph.revision;
        let mut edits = HashMap::new();
        let mut fields = Vec::new();
        for ((_, target), entity) in &self.equalizer_ui.editors {
            if *target != EqTarget::Node(id) {
                continue;
            }
            let editor = entity.read(cx);
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
                let edit = editor
                    .field_edit(key, &text)?
                    .ok_or_else(|| self.context.i18n.text("processing-editor-stale").to_owned())?;
                if !merge_draft_edit(&mut edits, key, edit) {
                    return Err(self
                        .context
                        .i18n
                        .format(
                            "processing-editor-conflict",
                            &[("field", key.clone().into())],
                        )
                        .to_string());
                }
                fields.push(SubmittedField {
                    editor: entity.downgrade(),
                    key: key.clone(),
                    input: field.input.clone(),
                    generation: field.generation,
                    text,
                });
            }
        }
        if fields.is_empty() {
            return Ok(None);
        }
        let mut config = base.clone();
        let sample_rate = self.input_track().map(|track| track.sample_rate);
        for edit in edits.into_values() {
            let _ = edit
                .apply_config(&mut config, sample_rate)
                .map_err(|error| error.to_string())?;
        }
        config.validate().map_err(|error| error.to_string())?;
        Ok(Some(NodeEqualizerSubmission {
            config,
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
        if !success {
            return;
        }
        let revision = self.state.system.config.processing_graph.revision;
        if submitted.revision.checked_add(1) != Some(revision) {
            return;
        }
        self.rebase_node_equalizer_drafts(submitted.node, submitted.revision, revision, cx);
        for field in submitted.fields {
            self.finish_equalizer_field(field, true, cx);
        }
        cx.notify();
    }

    pub(super) fn discard_node_equalizer_drafts(&mut self, id: u64, cx: &mut Context<Self>) {
        for ((_, target), editor) in &self.equalizer_ui.editors {
            if *target != EqTarget::Node(id) {
                continue;
            }
            editor.update(cx, |editor, cx| {
                editor.cancel_pointer(cx);
                for field in editor.fields.values_mut().filter(|field| field.dirty) {
                    field.dirty = false;
                    field.auto_pending = false;
                    field.draft_revision = None;
                    field.generation = field.generation.wrapping_add(1);
                    let text = field.committed.clone();
                    field.input.update(cx, |input, cx| input.set_text(text, cx));
                }
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
        let format = equalizer_format(&state);
        let _ = submitted.editor.update(cx, |editor, cx| {
            let Some(config) = target_config(&state, editor.target) else {
                return;
            };
            editor.reconcile(
                Rc::clone(&state),
                config,
                (editor.blocked, pending, format),
                epoch,
                cx,
            );
            if let Some(field) = editor.fields.get_mut(&submitted.key)
                && field.input.entity_id() == submitted.input.entity_id()
                && field.generation == submitted.generation
                && field.input.read(cx).text().trim() == submitted.text
            {
                field.dirty = false;
                field.auto_pending = false;
                field.draft_revision = None;
                let committed = field.committed.clone();
                field
                    .input
                    .update(cx, |input, cx| input.set_text(committed, cx));
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
        if let Some(target) = self.equalizer_ui.active.remove(&panel)
            && let Some(editor) = self.equalizer_ui.editors.get(&(panel, target))
        {
            let focused = editor
                .read(cx)
                .fields
                .values()
                .any(|field| field.input.read(cx).is_focused(window));
            if focused {
                window.blur(cx);
            }
            editor.update(cx, |editor, cx| {
                editor.active = false;
                editor.retire_band_focus();
                editor.cancel_pointer(cx);
                editor.fields.retain(|_, field| field.dirty);
            });
        }
    }
}

fn valid_gain_draft(value: &str) -> bool {
    value
        .trim()
        .parse::<f64>()
        .is_ok_and(|gain| gain.is_finite() && (-24.0..=24.0).contains(&gain))
}

fn equalizer_format(state: &GuiSnapshot) -> EqFormat {
    state
        .current_track()
        .map(|track| (track.id, track.sample_rate, track.channels))
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
        for ((_, target), editor) in &self.equalizer_ui.editors {
            if *target != EqTarget::Node(id) {
                continue;
            }
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
            | EqualizerEdit::SwapAb
    )
}

impl AudioProcessing {
    pub(super) fn begin_equalizer_command(&mut self, command: &Command) {
        self.equalizer_ui.reset_band_focus = match command {
            Command::EqualizerEdit { edit } if replaces_band_identity(edit) => {
                Some(EqTarget::Monitor)
            }
            Command::ProcessingGraphEdit { revision, edit }
                if matches!(
                    edit.as_ref(),
                    GraphEdit::Replace { id, .. }
                        if self.equalizer_ui.pending_node_focus_reset == Some((*id, *revision))
                ) =>
            {
                self.equalizer_ui
                    .pending_node_focus_reset
                    .take()
                    .map(|(id, _)| EqTarget::Node(id))
            }
            _ => None,
        };
        if let Command::EqualizerEdit {
            edit: EqualizerEdit::RemoveBand { index },
        } = command
        {
            self.equalizer_ui.removing.insert(
                EqTarget::Monitor,
                (self.state.system.config.equalizer.clone(), *index),
            );
        }
    }

    pub(super) fn finish_equalizer_command(&mut self, success: bool) {
        if !success {
            self.equalizer_ui.removing.clear();
            self.equalizer_ui.reset_band_focus = None;
        }
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

    #[test]
    fn live_gain_drafts_require_finite_complete_numbers_in_the_supported_range() {
        for value in ["0", "-0", "-24", "24", " 4.75 ", "+2", "1e1"] {
            assert!(valid_gain_draft(value), "valid gain draft: {value:?}");
        }
        for value in [
            "", " ", "-", "+", ".", "-.", "1e", "1e-", "NaN", "inf", "-inf", "1e309", "24.01",
            "-24.01", "invalid", "2 dB",
        ] {
            assert!(!valid_gain_draft(value), "invalid gain draft: {value:?}");
        }
    }

    struct EditorWindow(Entity<EqualizerEditor>);

    impl Render for EditorWindow {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.0.clone())
        }
    }

    fn fixture_with_owner(
        cx: &mut TestAppContext,
    ) -> (
        tempfile::TempDir,
        Runtime,
        Entity<AudioProcessing>,
        Entity<EqualizerEditor>,
        AnyWindowHandle,
    ) {
        cx.update(|cx| {
            Input::init(cx);
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
        let owner = processing.downgrade();
        let editor = processing.update(cx, |parent, cx| {
            cx.new(|cx| {
                EqualizerEditor::new(
                    owner,
                    EqTarget::Monitor,
                    PANEL,
                    parent,
                    &parent.state.system.config.equalizer,
                    cx,
                )
            })
        });
        let window = cx.open_window(size(px(1200.), px(900.)), {
            let editor = editor.clone();
            move |_, _| EditorWindow(editor)
        });
        let handle = window.into();
        cx.update_window(handle, |_, window, _| window.activate_window())
            .unwrap();
        draw(cx, handle);
        assert!(editor.read_with(cx, |editor, _| editor.format.is_none()));
        (directory, runtime, processing, editor, handle)
    }

    fn fixture(
        cx: &mut TestAppContext,
    ) -> (
        tempfile::TempDir,
        Runtime,
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
        _subscription: Subscription,
    }

    impl Render for PresetWindow {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let panels = self.owner.update(cx, |owner, cx| {
                (owner.preset_view(7, cx), owner.preset_view(8, cx))
            });
            self.scope
                .surface(div().id("preset-test").size_full())
                .child(panels.0)
                .child(panels.1)
                .child(
                    button("preset-outside", "Outside action", cx, |this, _, _| {
                        this.outside_clicks.set(this.outside_clicks.get() + 1);
                    })
                    .debug_selector(|| "preset-outside".to_owned())
                    .mt_32(),
                )
        }
    }

    #[gpui::test]
    fn preset_keyboard_management_restores_origin_and_disabled_trigger_is_inert(
        cx: &mut TestAppContext,
    ) {
        let (_directory, _runtime, owner, _editor, _handle) = fixture_with_owner(cx);
        owner.update(cx, |owner, cx| {
            owner.command_active = true;
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
        let scope = cx.update(rivu_ui::FocusScope::new);
        let outside_clicks = Rc::new(Cell::new(0));
        let window = cx.open_window(size(px(800.), px(600.)), {
            let owner = owner.clone();
            let scope = scope.clone();
            let outside_clicks = outside_clicks.clone();
            move |_, cx| {
                let subscription = cx.observe(&owner, |_, _, cx| cx.notify());
                PresetWindow {
                    owner,
                    scope,
                    outside_clicks,
                    _subscription: subscription,
                }
            }
        });
        let handle = window.into();
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
            assert_eq!(owner.fields["preset_name"].input.read(cx).text(), "Second");
            assert!(matches!(&owner.commands.back().unwrap().command,
                Command::EqualizerEdit { edit: EqualizerEdit::LoadPreset { name } } if name == "Second"));
        });
        // Tab reaches Manage; its first tab stop is the native name input.
        press(cx, handle, "tab");
        press(cx, handle, "enter");
        cx.update_window(handle, |_, window, cx| {
            owner.read_with(cx, |owner, cx| {
                assert!(owner.preset_popup.as_ref().unwrap().manage);
                assert!(
                    owner.fields["preset_name"]
                        .input
                        .read(cx)
                        .is_focused(window)
                );
            });
        })
        .unwrap();
        cx.update_window(handle, |_, window, cx| {
            use gpui::EntityInputHandler as _;
            let input = owner.read_with(cx, |owner, _| owner.fields["preset_name"].input.clone());
            input.update(cx, |input, cx| {
                input.replace_and_mark_text_in_range(None, "预设", None, window, cx)
            });
        })
        .unwrap();
        press(cx, handle, "escape");
        assert!(owner.read_with(cx, |owner, _| owner.preset_popup.is_some()));
        press(cx, handle, "escape");
        assert!(owner.read_with(cx, |owner, _| owner.preset_popup.is_none()));
        // The next trigger belongs to the second panel, never both panels.
        press(cx, handle, "tab");
        press(cx, handle, "enter");
        assert_eq!(
            owner.read_with(cx, |owner, _| owner.preset_popup.as_ref().unwrap().panel),
            8
        );
        owner.update(cx, |owner, cx| {
            owner.sync_panels(&crate::gui::layout::Layout::default(), false, cx);
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
    fn gain_preedit_submits_once_only_after_unchanged_unmark_or_commit(cx: &mut TestAppContext) {
        let (_directory, _runtime, owner, editor, handle) = fixture_with_owner(cx);
        // Hold the production queue at its dispatch boundary, retaining actual
        // submissions without sending native commands or replacing the handler.
        owner.update(cx, |owner, _| owner.command_active = true);
        for (key, text, value, unmark) in [
            ("preamp_db", "6", 6., true),
            ("output_gain_db", "-3", -3., false),
        ] {
            let input = editor.read_with(cx, |editor, _| editor.fields[key].input.clone());
            let before = owner.read_with(cx, |owner, _| owner.commands.len());
            cx.update_window(handle, |_, window, cx| {
                input.update(cx, |input, cx| {
                    let end = input.text().encode_utf16().count();
                    input.replace_and_mark_text_in_range(Some(0..end), text, None, window, cx);
                });
            })
            .unwrap();
            cx.run_until_parked();
            assert_eq!(owner.read_with(cx, |owner, _| owner.commands.len()), before);
            editor.read_with(cx, |editor, _| {
                assert!(!editor.fields[key].auto_pending);
                assert!(!editor.gain_submission_scheduled);
            });
            cx.update_window(handle, |_, window, cx| {
                input.update(cx, |input, cx| {
                    if unmark {
                        input.unmark_text(window, cx);
                    } else {
                        input.replace_text_in_range(None, text, window, cx);
                    }
                    assert_eq!(input.text(), text);
                });
            })
            .unwrap();
            cx.run_until_parked();
            owner.read_with(cx, |owner, _| {
                assert_eq!(owner.commands.len(), before + 1);
                let submission = owner.commands.back().unwrap();
                match &submission.command {
                    Command::EqualizerEdit {
                        edit: EqualizerEdit::PreampDb(gain),
                    } if unmark => {
                        assert_eq!(*gain, value);
                    }
                    Command::EqualizerEdit {
                        edit: EqualizerEdit::OutputGainDb(gain),
                    } if !unmark => {
                        assert_eq!(*gain, value);
                    }
                    _ => panic!("unexpected gain command"),
                }
                let field = submission.equalizer_field.as_ref().unwrap();
                assert_eq!(field.key, key);
                assert_eq!(field.text, text);
            });
            // Repeated unmark, redraw and scheduling do not duplicate the commit.
            cx.update_window(handle, |_, window, cx| {
                input.update(cx, |input, cx| input.unmark_text(window, cx));
                editor.update(cx, |editor, cx| editor.schedule_gain_submission(cx));
            })
            .unwrap();
            draw(cx, handle);
            assert_eq!(
                owner.read_with(cx, |owner, _| owner.commands.len()),
                before + 1
            );
        }
    }

    #[gpui::test]
    fn preedit_interrupts_a_queued_gain_submission_until_unchanged_commit(cx: &mut TestAppContext) {
        let (_directory, _runtime, owner, editor, handle) = fixture_with_owner(cx);
        owner.update(cx, |owner, _| owner.command_active = true);
        let input = editor.read_with(cx, |editor, _| editor.fields["preamp_db"].input.clone());
        cx.update_window(handle, |_, window, cx| {
            input.update(cx, |input, cx| {
                let end = input.text().encode_utf16().count();
                input.replace_text_in_range(Some(0..end), "4", window, cx);
            });
            let input = input.clone();
            let editor = editor.clone();
            // Changed queues the gain defer behind this interruption. The
            // preedit notification itself has not run when that gain defer runs.
            cx.defer(move |cx| {
                assert!(editor.read(cx).gain_submission_scheduled);
                assert!(editor.read(cx).fields["preamp_db"].auto_pending);
                handle
                    .update(cx, |_, window, cx| {
                        input.update(cx, |input, cx| {
                            input.replace_and_mark_text_in_range(
                                Some(0..1),
                                "-7",
                                None,
                                window,
                                cx,
                            );
                        });
                    })
                    .unwrap();
            });
        })
        .unwrap();
        cx.run_until_parked();
        assert!(owner.read_with(cx, |owner, _| owner.commands.is_empty()));
        assert!(input.read_with(cx, |input, _| input.is_composing()));
        cx.update_window(handle, |_, window, cx| {
            input.update(cx, |input, cx| {
                input.replace_text_in_range(None, "-7", window, cx);
            });
        })
        .unwrap();
        cx.run_until_parked();
        owner.read_with(cx, |owner, _| {
            assert_eq!(owner.commands.len(), 1);
            let submission = owner.commands.front().unwrap();
            assert!(matches!(
                &submission.command,
                Command::EqualizerEdit { edit: EqualizerEdit::PreampDb(gain) } if *gain == -7.
            ));
            assert_eq!(submission.equalizer_field.as_ref().unwrap().text, "-7");
        });
    }

    fn draw(cx: &mut TestAppContext, handle: AnyWindowHandle) {
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
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

    fn click(cx: &mut TestAppContext, handle: AnyWindowHandle, selector: &'static str) {
        use gpui::InputEvent as _;
        let position = VisualTestContext::from_window(handle, cx)
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("missing painted control {selector}"))
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
                    edit: EqualizerEdit::RemoveBand { index: 2 }
                }
            ));
        });

        // Focusing an input must keep Delete within its native editing path,
        // even though the curve node remains selected.
        let input = editor.read_with(cx, |editor, _| {
            editor.fields["band.2.frequency_hz"].input.clone()
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
        let original = editor.update(cx, |editor, _| {
            editor.format = Some((99, 48_000, 2));
            for band in &mut editor.config.bands {
                band.channels = ChannelTarget::Selected(vec![1]);
            }
            editor.update_response();
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
        editor.update(cx, |editor, _| {
            editor.format = Some((99, 48_000, 1));
            editor.update_response();
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
            for name in ["frequency_hz", "gain_db", "q", "channels"] {
                assert!(editor.fields.contains_key(&format!("band.{index}.{name}")));
            }
            assert!(editor.fields.iter().all(|(key, field)| {
                !key.starts_with("band.")
                    || key.starts_with(&format!("band.{index}."))
                    || field.dirty
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
                (false, pending, editor.format),
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
                "preamp_db",
                "output_gain_db",
                "band.0.frequency_hz",
                "band.0.gain_db",
                "band.0.q",
                "band.0.channels",
            ]
            .map(|key| (key, editor.fields[key].input.clone()))
        });
        cx.update_window(handle, |_, window, cx| {
            for (key, input) in &inputs {
                input.update(cx, |input, cx| {
                    input.focus_and_select_all(window, cx);
                    input.replace_text_in_range(
                        None,
                        if *key == "band.0.frequency_hz" {
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
            editor.apply_field("band.0.frequency_hz", cx);
            editor.step_field("band.0.gain_db", 0.5, cx);
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
            editor.apply_field("band.0.frequency_hz", cx);
            assert!(
                editor.error.is_some(),
                "reenabled fields validate preserved drafts"
            );
            editor.step_field("band.0.gain_db", 0.5, cx);
            assert_eq!(
                editor.fields["band.0.gain_db"].input.read(cx).text(),
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
            editor.fields["band.2.gain_db"].input.clone()
        });
        cx.update_window(handle, |_, window, cx| {
            input.update(cx, |input, cx| input.focus_and_select_all(window, cx));
        })
        .unwrap();
        draw(cx, handle);
        cx.simulate_keystrokes(handle, "4 . 7 5");
        draw(cx, handle);
        editor.read_with(cx, |editor, cx| {
            let field = &editor.fields["band.2.gain_db"];
            assert!(field.dirty);
            assert_eq!(field.input.read(cx).text(), "4.75");
        });
        click(cx, handle, "eq-node-7-0");
        assert_selected(cx, &editor, handle, 0);
        editor.read_with(cx, |editor, cx| {
            let draft = &editor.fields["band.2.gain_db"];
            assert_eq!(draft.input, input);
            assert!(draft.dirty);
            assert_eq!(draft.input.read(cx).text(), "4.75");
            assert_eq!(editor.config.bands[2].gain_db, 8.);
            assert!(editor.drag.is_none());
        });
        click(cx, handle, "eq-node-7-2");
        assert_selected(cx, &editor, handle, 2);
        assert_eq!(
            editor.read_with(cx, |editor, _| editor.fields["band.2.gain_db"]
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
                editor.fields["band.1.frequency_hz"].input.clone(),
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
                assert_eq!(editor.fields["band.1.frequency_hz"].input, input);
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
        let input = editor.read_with(cx, |editor, _| editor.fields["band_count"].input.clone());
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
        let input = editor.read_with(cx, |editor, _| editor.fields["band_count"].input.clone());
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
            editor.fields["band.0.frequency_hz"].input.clone()
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
            assert_eq!(editor.fields["band.0.frequency_hz"].input, input);
            assert_eq!(input.read(cx).text(), "123");
            assert!(editor.fields["band.0.frequency_hz"].dirty);
            assert!(!editor.fields.contains_key("band.2.frequency_hz"));
        });
    }

    #[test]
    fn panel_drafts_merge_equal_values_and_reject_conflicts_without_replacement() {
        let mut edits = HashMap::new();
        assert!(merge_draft_edit(
            &mut edits,
            "band.0.gain_db",
            EqualizerEdit::BandField {
                index: 0,
                field: EqBandEdit::GainDb(3.25),
            }
        ));
        assert!(merge_draft_edit(
            &mut edits,
            "band.0.gain_db",
            EqualizerEdit::BandField {
                index: 0,
                field: EqBandEdit::GainDb(3.25),
            }
        ));
        assert!(!merge_draft_edit(
            &mut edits,
            "band.0.gain_db",
            EqualizerEdit::BandField {
                index: 0,
                field: EqBandEdit::GainDb(-2.),
            }
        ));
        let mut config = EqualizerConfig {
            bands: vec![EqBand::default()],
            ..Default::default()
        };
        for edit in edits.into_values() {
            let _ = edit.apply_config(&mut config, Some(48_000)).unwrap();
        }
        assert_eq!(config.bands[0].gain_db, 3.25);
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
