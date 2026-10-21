//! One explicit-target EQ editor per dock panel. Core owns committed parameters;
//! these entities own only drafts, response samples and cancellable pointer intent.
use super::*;
use crate::model::EqBandEdit;
use rivu_dsp::equalizer::{ChannelTarget, EqBand, EqualizerConfig, FilterKind, MAX_BANDS};
use rivu_format::config::processing_graph::{GraphEdit, GraphNodeKind};

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
}

struct EqualizerField {
    input: Entity<Input>,
    committed: String,
    dirty: bool,
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
    add_band_count: usize,
    response_channel: usize,
    fields: HashMap<String, EqualizerField>,
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
        let format = parent.equalizer_format_from(&state, target);
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
            add_band_count: 1,
            response_channel: 0,
            fields: HashMap::new(),
            response: Arc::new(Vec::new()),
            response_error: None,
            error: None,
            maximum_frequency: 20_000.,
            drag: None,
            bounds: Rc::new(Cell::new(None)),
            frequencies: [0.; 257],
            frequency_limit: 0.,
            removing_band: None,
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
        if self.config.bands.len() != config.bands.len() {
            self.remap_band_fields(&config.bands, cx);
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
            self.config.clone_from(config);
        }
        self.revision = revision;
        self.blocked = blocked;
        self.pending = pending;
        self.geometry_epoch = geometry_epoch;
        if config_changed || format_changed || fence_changed || geometry_changed || blocked {
            self.take_drag();
        }
        if geometry_changed {
            self.bounds.set(None);
        }
        self.band = self.band.min(self.config.bands.len().saturating_sub(1));
        self.add_band_count = self
            .add_band_count
            .clamp(1, MAX_BANDS.saturating_sub(self.config.bands.len()).max(1));
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
        if changed {
            cx.notify();
        }
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
        let format_valid = self
            .owner
            .update(cx, |owner, _| {
                owner.equalizer_format_from(&snapshot, self.target)
            })
            .is_ok_and(|format| format == self.format);
        let valid = !self.blocked
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

    fn eq(&mut self, edit: EqualizerEdit, cx: &mut Context<Self>) {
        self.submit_edit(edit, None, cx);
    }

    fn submit_edit(
        &mut self,
        edit: EqualizerEdit,
        submitted: Option<SubmittedField>,
        cx: &mut Context<Self>,
    ) {
        if self.blocked {
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
        let input = cx.new(|cx| Input::new(label.to_owned(), value.clone(), label.to_owned(), cx));
        let subscription = self.subscribe_field(&input, key.to_owned(), cx);
        self.fields.insert(
            key.to_owned(),
            EqualizerField {
                input,
                committed: value,
                dirty: false,
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
            InputEvent::Changed { user: true } => {
                if let Some(field) = this.fields.get_mut(&name) {
                    if !field.dirty {
                        field.draft_revision = Some(this.revision);
                    }
                    field.dirty = true;
                    field.generation = field.generation.wrapping_add(1);
                }
                cx.notify();
                let _ = this.owner.update(cx, |_, cx| cx.notify());
            }
            InputEvent::Submit => this.apply_field(&name, cx),
            _ => {}
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
        if self.blocked {
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
        let value = self
            .fields
            .get(key)
            .map(|f| f.input.read(cx).text().trim().to_owned())
            .unwrap_or_default();
        match self.field_edit(key, &value) {
            Ok(Some(edit)) => {
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
        let edit = if key == "preamp_db" {
            Some(EqualizerEdit::PreampDb(number()?))
        } else if key == "output_gain_db" {
            Some(EqualizerEdit::OutputGainDb(number()?))
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
    fn field_view(
        &self,
        key: &str,
        label: impl Into<SharedString>,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let field = &self.fields[key];
        let label: SharedString = label.into();
        let label: SharedString = if field.dirty {
            format!("{label} · {}", self.context.i18n.text("processing-draft")).into()
        } else {
            label
        };
        let key_owned = key.to_owned();
        let control = row()
            .min_w_0()
            .gap_1()
            .child(div().flex_1().min_w_0().child(field.input.clone()))
            .child(disabled_control(
                theme,
                button(
                    SharedString::from(format!("apply-{key}")),
                    self.tr("processing-apply"),
                    cx,
                    move |this, _, cx| this.apply_field(&key_owned, cx),
                ),
                !enabled,
            ));
        column()
            .min_w_0()
            .gap_1()
            .child(caption(theme, label))
            .child(control)
            .opacity(if enabled { 1. } else { 0.5 })
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
                                this.band = drag.members[(current + 1) % drag.member_count];
                                this.sync_fields(cx);
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

    fn graph(&mut self, panel: u64, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = rivu_ui::theme::palette(cx);
        let measured = Rc::clone(&self.bounds);
        let curve = Arc::clone(&self.response);
        let nodes = self
            .config
            .bands
            .iter()
            .take(MAX_BANDS)
            .enumerate()
            .filter(|(_, b)| b.enabled)
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
        for &(index, x, y) in &nodes {
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
                    .absolute()
                    .left(relative(x))
                    .top(relative(y))
                    .ml(-px(8.))
                    .mt(-px(8.))
                    .size(px(16.))
                    .tab_index(0)
                    .tab_stop(!self.response.is_empty())
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
                    .border_1()
                    .border_color(transparent_black())
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
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        this.node_key(index, event, cx)
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
                    if this.blocked || this.response.is_empty() {
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
                    for (index, band) in this
                        .config
                        .bands
                        .iter()
                        .take(MAX_BANDS)
                        .enumerate()
                        .filter(|(_, band)| band.enabled)
                    {
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
                        this.band = index;
                        let Some(b) = this.config.bands.get(index) else {
                            return;
                        };
                        let frequency = b.frequency_hz;
                        let gain = b.gain_db;
                        this.sync_fields(cx);
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
                        for (index, x, y) in nodes
                            .iter()
                            .filter(|(index, _, _)| *index != selected)
                            .chain(nodes.iter().filter(|(index, _, _)| *index == selected))
                        {
                            let center = point(
                                bounds.origin.x + bounds.size.width * *x,
                                bounds.origin.y + bounds.size.height * *y,
                            );
                            window.paint_quad(fill(
                                Bounds::new(center - point(px(4.), px(4.)), size(px(8.), px(8.))),
                                rgb(if *index == selected {
                                    theme.accent
                                } else {
                                    theme.text
                                }),
                            ));
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

    fn equalizer_view(&mut self, panel: u64, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let graph = self.graph(panel, window, cx);
        let eq = &self.config;
        let mut content = column()
            .gap_3()
            .child(
                row()
                    .flex_wrap()
                    .gap_2()
                    .child(checkbox(
                        ("eq-enabled", panel),
                        self.tr("processing-eq-enabled"),
                        eq.enabled,
                        !self.blocked,
                        cx,
                        |this, value, cx| this.eq(EqualizerEdit::Enabled(value), cx),
                    ))
                    .child(disabled_control(
                        theme,
                        button(
                            ("eq-flat", panel),
                            self.tr("processing-flat"),
                            cx,
                            |this, _, cx| {
                                let mut config = rivu_dsp::equalizer::EqualizerConfig::default();
                                config.enabled = this.config.enabled;
                                this.eq(EqualizerEdit::Replace { config }, cx);
                            },
                        ),
                        self.blocked,
                    ))
                    .when(self.target == EqTarget::Monitor, |r| {
                        r.child(button(
                            ("eq-ab", panel),
                            self.tr("processing-ab"),
                            cx,
                            |this, _, cx| this.eq(EqualizerEdit::SwapAb, cx),
                        ))
                    }),
            )
            .child(graph)
            .child(caption(theme, self.tr("processing-graph-help")));
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
            content = content.child(labels);
        }
        if let Some(error) = &self.response_error {
            content = content.child(caption(theme, error.clone()).text_color(rgb(theme.error)));
        }
        if let Some((_, sample_rate, channel_count)) = self.format {
            content = content.child(caption(
                theme,
                format!(
                    "{} Hz · {} {} · 20–{:.0} Hz · ±24 dB",
                    sample_rate,
                    channel_count,
                    self.context.i18n.text("processing-channel-count"),
                    self.maximum_frequency
                ),
            ));
            let mut channels = row().flex_wrap().gap_1();
            for channel in 0..channel_count as usize {
                channels = channels.child(
                    button(
                        SharedString::from(format!("response-channel-{channel}")),
                        format!(
                            "{} {}",
                            self.context.i18n.text("processing-response-channel"),
                            channel + 1
                        ),
                        cx,
                        move |this, _, cx| {
                            if this.response_channel != channel {
                                this.response_channel = channel;
                                this.update_response();
                                cx.notify();
                            }
                        },
                    )
                    .when(channel == self.response_channel, |v| {
                        v.bg(rgb(theme.selected))
                    }),
                );
            }
            content = content.child(channels);
        }
        content = content
            .child(self.field_view("preamp_db", self.tr("processing-preamp"), !self.blocked, cx))
            .child(self.field_view(
                "output_gain_db",
                self.tr("processing-output-gain"),
                !self.blocked,
                cx,
            ));
        let mut bands = row().flex_wrap().gap_1();
        for (index, band) in eq.bands.iter().enumerate() {
            bands = bands.child(
                button(
                    SharedString::from(format!("select-band-{index}")),
                    format!(
                        "{} {}{}",
                        index + 1,
                        if band.enabled { "" } else { "○ " },
                        if band.solo { "S" } else { "" }
                    ),
                    cx,
                    move |this, _, cx| {
                        this.cancel_pointer(cx);
                        this.band = index;
                        this.sync_fields(cx);
                        cx.notify();
                    },
                )
                .when(index == self.band, |v| v.bg(rgb(theme.selected))),
            );
        }
        let remaining = MAX_BANDS.saturating_sub(eq.bands.len());
        let count = self.add_band_count.min(remaining);
        let add_disabled = self.blocked || self.pending > 0 || remaining == 0;
        let add_controls = row()
            .flex_wrap()
            .gap_2()
            .child(disabled_control(
                theme,
                button(("eq-add-less", panel), "−", cx, |this, _, cx| {
                    if this.pending == 0 && this.add_band_count > 1 {
                        this.add_band_count -= 1;
                        cx.notify();
                    }
                })
                .aria_label(self.tr("processing-add-fewer")),
                add_disabled || count <= 1,
            ))
            .child(caption(
                theme,
                self.context.i18n.format(
                    "processing-add-count",
                    &[("count", count.into()), ("remaining", remaining.into())],
                ),
            ))
            .child(disabled_control(
                theme,
                button(("eq-add-more", panel), "+", cx, |this, _, cx| {
                    let remaining = MAX_BANDS.saturating_sub(this.config.bands.len());
                    if this.pending == 0 && this.add_band_count < remaining {
                        this.add_band_count += 1;
                        cx.notify();
                    }
                })
                .aria_label(self.tr("processing-add-more")),
                add_disabled || count >= remaining,
            ))
            .child(disabled_control(
                theme,
                button(
                    ("add-band", panel),
                    self.tr("processing-add-bands"),
                    cx,
                    |this, _, cx| {
                        let remaining = MAX_BANDS.saturating_sub(this.config.bands.len());
                        let count = this.add_band_count.min(remaining);
                        if this.pending == 0 && count > 0 {
                            this.cancel_pointer(cx);
                            this.eq(EqualizerEdit::AddBands { count }, cx);
                        }
                    },
                )
                .tooltip(rivu_ui::text_tooltip(self.tr("processing-add-bands-help"))),
                add_disabled,
            ));
        content = content.child(bands).child(add_controls);
        if let Some(band) = eq.bands.get(self.band) {
            let index = self.band;
            content = content.child(
                row()
                    .flex_wrap()
                    .gap_2()
                    .child(checkbox(
                        ("band-enabled", panel),
                        self.tr("processing-enabled"),
                        band.enabled,
                        !self.blocked,
                        cx,
                        move |this, value, cx| {
                            this.band_edit(index, EqBandEdit::Enabled(value), cx)
                        },
                    ))
                    .child(checkbox(
                        ("band-solo", panel),
                        self.tr("processing-solo"),
                        band.solo,
                        !self.blocked,
                        cx,
                        move |this, value, cx| this.band_edit(index, EqBandEdit::Solo(value), cx),
                    ))
                    .child(disabled_control(
                        theme,
                        button(
                            ("remove-band", panel),
                            self.tr("processing-remove-band"),
                            cx,
                            move |this, _, cx| this.eq(EqualizerEdit::RemoveBand { index }, cx),
                        ),
                        self.blocked,
                    )),
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
                    self.blocked,
                ));
            }
            content = content.child(types);
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
                    content = content.child(self.field_view(
                        &format!("band.{index}.{name}"),
                        self.tr(key),
                        !self.blocked,
                        cx,
                    ));
                }
            }
            content = content
                .child(caption(theme, self.tr("processing-channel-help")))
                .child(caption(theme, self.tr("processing-stage-help")));
        }
        content
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
            || self.equalizer_format_from(&snapshot, target) != expected_format
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
                let sample_rate = self
                    .equalizer_format_from(&snapshot, target)
                    .map(|(_, rate, _)| rate);
                match edit.apply_config(&mut next, sample_rate) {
                    Ok(None) => {
                        let mut submission = NodeEqualizerSubmission {
                            config: next,
                            revision,
                            node: id,
                            fields: submitted.into_iter().collect(),
                        };
                        let config = std::mem::take(&mut submission.config);
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
                self.equalizer_format_from(&self.state, target),
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
            .child(self.preset_view(panel, cx))
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

    pub(super) fn reconcile_equalizers(&mut self, cx: &mut Context<Self>) {
        let state = Rc::clone(&self.state);
        let blocked = self.equalizer_blocked(EqTarget::Node(0));
        let monitor_format = self.equalizer_format_from(&state, EqTarget::Monitor);
        let node_format = self.equalizer_format_from(&state, EqTarget::Node(0));
        let removals = &self.equalizer_ui.removing;
        self.equalizer_ui.editors.retain(|(_, target), editor| {
            let Some(config) = target_config(&state, *target) else {
                editor.update(cx, |editor, cx| editor.cancel_pointer(cx));
                return false;
            };
            editor.update(cx, |editor, cx| {
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
                        if *target == EqTarget::Monitor {
                            monitor_format
                        } else {
                            node_format
                        },
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
        let monitor_format = self.equalizer_format_from(&state, EqTarget::Monitor);
        let node_format = self.equalizer_format_from(&state, EqTarget::Node(0));
        let _ = submitted.editor.update(cx, |editor, cx| {
            let Some(config) = target_config(&state, editor.target) else {
                return;
            };
            let format = if editor.target == EqTarget::Monitor {
                monitor_format
            } else {
                node_format
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
                editor.cancel_pointer(cx);
                editor.fields.retain(|_, field| field.dirty);
            });
        }
    }
}

impl AudioProcessing {
    fn equalizer_format_from(&self, state: &GuiSnapshot, target: EqTarget) -> EqFormat {
        let track = if matches!(target, EqTarget::Node(_)) && self.use_selected {
            self.selected.as_deref()
        } else {
            state.current_track()
        };
        track.map(|track| (track.id, track.sample_rate, track.channels))
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

impl AudioProcessing {
    pub(super) fn begin_equalizer_command(&mut self, command: &Command) {
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
        }
    }
}
#[cfg(test)]
mod tests {
    use super::{
        EqBand, EqBandEdit, EqualizerConfig, EqualizerEdit, merge_draft_edit, rebase_draft_origin,
    };
    use std::collections::HashMap;

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
