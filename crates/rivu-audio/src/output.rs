//! Native device output; callback state and ring buffers stay private.
//! Native PipeWire streams keep a fixed target and explicit speaker positions.
//! The selected sink and its strict speaker routes are never silently replaced.

use super::{Channel, PlaybackMetadata, channels};
use crate::analysis::AnalysisWorker;
use ::pipewire::{self as pw, properties::properties, spa};
use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, bounded};
use parking_lot::Mutex;
use ringbuf::{
    HeapCons, HeapProd, HeapRb,
    traits::{Consumer, Observer, Producer, Split},
};
use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::{BTreeMap, VecDeque},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

// Preserve the quarter-second queue (at least 2048 frames) without letting
// decoded metadata request unbounded memory. Callback scratch is at most 1/8
// of this budget because it holds only 256 frames.
const MAX_PCM_BUFFER_BYTES: usize = 16 * 1024 * 1024;

fn output_capacity(rate: u32, channels: usize) -> Result<usize> {
    if rate == 0 || channels == 0 {
        bail!("Invalid decoded sample rate or speaker layout");
    }
    let frames = usize::try_from((rate / 4).max(2048))
        .context("Audio output frame capacity exceeds addressable memory")?;
    let samples = frames
        .checked_mul(channels)
        .context("Audio output sample capacity overflow")?;
    let bytes = samples
        .checked_mul(size_of::<f32>())
        .context("Audio output PCM buffer size overflow")?;
    if bytes > MAX_PCM_BUFFER_BYTES {
        bail!("Audio output PCM buffer exceeds 16 MiB limit ({rate} Hz, {channels} channels)");
    }
    Ok(samples)
}

pub fn devices() -> Result<Vec<String>> {
    let connection = Connection::new()?;
    let mut names: Vec<_> = connection
        .inventory
        .borrow()
        .sinks
        .values()
        .map(|sink| format!("{PREFIX}{}", sink.name))
        .collect();
    names.sort();
    names.dedup();
    Ok(names)
}

fn target_name(name: Option<&str>) -> Result<Option<&str>> {
    name.map(|name| {
        name.strip_prefix(PREFIX)
            .ok_or_else(|| anyhow::anyhow!("Output device not found: {name}"))
    })
    .transpose()
}

pub(super) fn validate_device(name: Option<&str>) -> Result<()> {
    let target = target_name(name)?;
    Connection::new()?.target(target).map(|_| ())
}

/// Nanoseconds in PipeWire's monotonic clock domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct StreamInstant(u64);

impl StreamInstant {
    fn from_nanos(nanos: u64) -> Self {
        Self(nanos)
    }

    #[cfg(test)]
    fn from_millis(millis: u64) -> Self {
        Self(
            millis
                .checked_mul(1_000_000)
                .expect("test timestamp overflow"),
        )
    }

    fn checked_add(self, duration: Duration) -> Option<Self> {
        self.0
            .checked_add(u64::try_from(duration.as_nanos()).ok()?)
            .map(Self)
    }

    fn duration_since(self, earlier: Self) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(earlier.0))
    }
}

#[derive(Clone, Copy)]
struct OutputTimestamp {
    callback: StreamInstant,
    playback: StreamInstant,
}

#[derive(Debug)]
pub(super) struct OutputError(String);

impl std::fmt::Display for OutputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for OutputError {}

struct OutputShared {
    paused: AtomicBool,
    pause_settled: AtomicBool,
    wakeup: crossbeam_channel::Sender<()>,
    notifications: Receiver<()>,
    refill_armed: AtomicBool,
    // Audible source frames also determine position and output drain completion.
    heard: AtomicU64,
    timing_error: AtomicU32,
    // Linear amplitude gain; conversion happens outside the realtime callback.
    volume: AtomicU32,
}
impl OutputShared {
    fn new(volume: f32, paused: bool) -> Self {
        let (wakeup, notifications) = bounded(1);
        Self {
            paused: AtomicBool::new(paused),
            pause_settled: AtomicBool::new(paused),
            wakeup,
            notifications,
            refill_armed: AtomicBool::new(false),
            heard: AtomicU64::new(0),
            timing_error: AtomicU32::new(0),
            volume: AtomicU32::new(crate::volume::gain(volume).to_bits()),
        }
    }

    fn fail_timing(&self, code: u32) {
        if self
            .timing_error
            .compare_exchange(0, code, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let _ = self.wakeup.try_send(());
        }
    }
}

struct ScheduledFrames {
    start: StreamInstant,
    frames: u64,
    heard: u64,
}

// Kept entirely in the PCM callback. Allocation happens before starting the
// stream; a broken callback clock / excessive backend queue is reported, never
// guessed. Backend playback predictions may move backwards when latency changes;
// queued spans are kept in submission order below instead of treating that
// backend estimate as a clock failure.
struct PlaybackClock {
    pending: VecDeque<ScheduledFrames>,
    last_callback: Option<StreamInstant>,
    heard: u64,
}
impl PlaybackClock {
    fn new() -> Self {
        Self {
            pending: VecDeque::with_capacity(1024),
            last_callback: None,
            heard: 0,
        }
    }

    fn schedule_start(&self, predicted: StreamInstant, rate: u32) -> Option<StreamInstant> {
        let Some(previous) = self.pending.back() else {
            return Some(predicted);
        };
        let rate = u128::from(rate.max(1));
        let nanos =
            u64::try_from((u128::from(previous.frames) * 1_000_000_000).div_ceil(rate)).ok()?;
        let end = previous.start.checked_add(Duration::from_nanos(nanos))?;
        Some(if end > predicted { end } else { predicted })
    }

    fn advance(&mut self, timestamp: OutputTimestamp, rate: u32, shared: &OutputShared) {
        // PipeWire's playback prediction includes queue and device latency;
        // either can shrink without the callback clock moving backwards.
        if self
            .last_callback
            .is_some_and(|last| timestamp.callback < last)
        {
            shared.fail_timing(1);
            return;
        }
        self.last_callback = Some(timestamp.callback);
        while let Some(span) = self.pending.front_mut() {
            let elapsed = timestamp.callback.duration_since(span.start);
            let heard = (elapsed.as_nanos() * u128::from(rate) / 1_000_000_000)
                .min(u128::from(span.frames)) as u64;
            let newly_heard = heard - span.heard;
            self.heard += newly_heard;
            span.heard = heard;
            if heard != span.frames {
                break;
            }
            self.pending.pop_front();
        }
        shared.heard.store(self.heard, Ordering::Release);
    }
}

pub(super) struct OutputOptions<'a> {
    pub(super) device: Option<&'a str>,
    pub(super) volume: f32,
    pub(super) paused: bool,
    pub(super) auto_mix: bool,
    pub(super) metadata: Arc<PlaybackMetadata>,
}

pub(super) struct Output {
    stream: Option<NativeStream>,
    producer: HeapProd<f32>,
    shared: Arc<OutputShared>,
    rate: u32,
    source_channels: usize,
    errors: Receiver<OutputError>,
    submitted: u64,
}
impl Output {
    pub(super) fn new(
        source_rate: u32,
        source_layout: &[Channel],
        options: OutputOptions<'_>,
        analyzer: &AnalysisWorker,
    ) -> Result<Self> {
        let target = target_name(options.device)?;
        let rate = source_rate;
        let source_channels = source_layout.len();
        let capacity = output_capacity(rate, source_channels)?;
        let (producer, consumer) = HeapRb::try_new(capacity)
            .context("Cannot allocate audio output PCM buffer")?
            .split();
        let scratch_samples = source_channels
            .checked_mul(OutputCallback::BLOCK_FRAMES)
            .context("Audio output callback buffer size overflow")?;
        let mut scratch = Vec::new();
        scratch
            .try_reserve_exact(scratch_samples)
            .context("Cannot allocate audio output callback buffer")?;
        scratch.resize(scratch_samples, 0.0);
        let shared = Arc::new(OutputShared::new(options.volume, options.paused));
        let (error_send, errors) = bounded(4);
        analyzer.reset(rate);
        let callback = OutputCallback {
            consumer,
            shared: shared.clone(),
            channels: source_channels,
            scratch,
            mapping: Vec::new(),
            source_channels,
            rate,
            clock: PlaybackClock::new(),
        };
        let stream = NativeStream::open(
            target,
            source_layout,
            options.auto_mix,
            options.metadata,
            callback,
            error_send,
        )?;
        Ok(Self {
            stream: Some(stream),
            producer,
            shared,
            rate,
            source_channels,
            errors,
            submitted: 0,
        })
    }

    pub(super) fn errors(&self) -> &Receiver<OutputError> {
        &self.errors
    }

    pub(super) fn set_volume(&self, volume: f32) {
        self.shared
            .volume
            .store(crate::volume::gain(volume).to_bits(), Ordering::Release);
    }

    pub(super) fn set_metadata(&self, metadata: Arc<PlaybackMetadata>) -> Result<()> {
        if let Some(stream) = &self.stream {
            stream.set_metadata(metadata)?;
        }
        Ok(())
    }

    pub(super) fn vacant_frames(&self) -> usize {
        self.producer.vacant_len() / self.source_channels
    }

    pub(super) fn refill_delay(&self, eof: bool) -> Duration {
        let frames = if eof {
            self.submitted
                .saturating_sub(self.shared.heard.load(Ordering::Acquire))
        } else {
            (self.producer.occupied_len() / self.source_channels) as u64 / 2
        };
        // Keep half the buffered PCM as decode/scheduling headroom. End-of-track
        // tails retain a 10ms clock fallback, not an empty-ring refill busy loop.
        Duration::from_secs_f64(frames as f64 / self.rate as f64).clamp(
            Duration::from_millis(if eof { 10 } else { 1 }),
            Duration::from_millis(100),
        )
    }

    // Return complete frames consumed; never publish a partial interleaved frame.
    pub(super) fn push(&mut self, samples: &[f32]) -> usize {
        let frames = (samples.len() / self.source_channels)
            .min(self.producer.vacant_len() / self.source_channels);
        let count = self
            .producer
            .push_slice(&samples[..frames * self.source_channels]);
        let frames = count / self.source_channels;
        self.submitted += frames as u64;
        if self.producer.occupied_len() > self.producer.capacity().get() / 4 {
            self.shared.refill_armed.store(true, Ordering::Release);
        }
        frames
    }

    pub(super) fn drained(&self) -> bool {
        self.shared.heard.load(Ordering::Acquire) == self.submitted
    }

    pub(super) fn heard(&self) -> f64 {
        self.shared.heard.load(Ordering::Acquire) as f64 / self.rate as f64
    }
    pub(super) fn position(&self) -> f64 {
        // The source cursor advances only as scheduled PCM becomes audible.
        self.heard()
    }
    pub(super) fn pause(&self, paused: bool) {
        if self.shared.paused.load(Ordering::Acquire) == paused {
            return;
        }
        // Publish an unsettled transition before the callback can observe the
        // new pause request. Duplicate pauses must leave an inactive stream settled.
        self.shared.pause_settled.store(false, Ordering::Release);
        self.shared.paused.store(paused, Ordering::Release);
        if let Some(stream) = &self.stream
            && stream.wake_owner().is_err()
        {
            self.shared.fail_timing(4);
        }
    }

    pub(super) fn notifications(&self) -> &Receiver<()> {
        &self.shared.notifications
    }

    pub(super) fn check_timing(&self) -> Result<()> {
        match self.shared.timing_error.load(Ordering::Acquire) {
            0 => Ok(()),
            1 => bail!("Audio output timestamp moved backwards"),
            2 => bail!("Audio output timing queue exhausted before playback advanced"),
            4 => bail!("Cannot wake PipeWire output owner"),
            _ => bail!("PipeWire output returned invalid buffers or unavailable timing"),
        }
    }
    pub(super) fn close(&mut self) -> f64 {
        self.shared.paused.store(true, Ordering::Release);
        self.stream.take();
        self.heard()
    }
}

struct OutputCallback {
    consumer: HeapCons<f32>,
    shared: Arc<OutputShared>,
    channels: usize,
    scratch: Vec<f32>,
    mapping: Vec<Option<usize>>,
    source_channels: usize,
    rate: u32,
    clock: PlaybackClock,
}
impl OutputCallback {
    // Fixed storage bounds callback memory independently of device buffer size.
    const BLOCK_FRAMES: usize = 256;

    fn render_native(&mut self, data: &mut [f32], timestamp: OutputTimestamp, routed: bool) {
        if routed {
            self.render(data, timestamp);
        } else {
            // Keep unconfirmed PipeWire routes silent without consuming PCM.
            data.fill(0.0);
        }
    }

    fn render(&mut self, data: &mut [f32], timestamp: OutputTimestamp) {
        self.clock.advance(timestamp, self.rate, &self.shared);
        let paused = self.shared.paused.load(Ordering::Acquire);
        if paused
            && self.clock.pending.is_empty()
            && !self.shared.pause_settled.swap(true, Ordering::AcqRel)
        {
            let _ = self.shared.wakeup.try_send(());
        }
        let volume = f32::from_bits(self.shared.volume.load(Ordering::Relaxed));
        let source_channels = self.source_channels;
        // Snapshot availability: valid frames form one prefix, even if the
        // producer refills concurrently after an underflow.
        let mut count = if paused || self.shared.timing_error.load(Ordering::Acquire) != 0 {
            0
        } else {
            (self.consumer.occupied_len() / source_channels).min(data.len() / self.channels)
        };
        if count > 0 && self.clock.pending.len() == self.clock.pending.capacity() {
            self.shared.fail_timing(2);
            count = 0;
        }
        let start = if count > 0 {
            match self.clock.schedule_start(timestamp.playback, self.rate) {
                Some(start) => Some(start),
                None => {
                    self.shared.fail_timing(3);
                    count = 0;
                    None
                }
            }
        } else {
            None
        };
        // Valid mapped samples are written once; only the silent tail is cleared.
        data[count * self.channels..].fill(0.0);
        for block in data[..count * self.channels].chunks_mut(Self::BLOCK_FRAMES * self.channels) {
            // Publish ring consumption once per block, including wrapped rings.
            // Scratch is fixed before stream start; every chunk contains whole frames.
            let samples = block.len() / self.channels * source_channels;
            let scratch = &mut self.scratch[..samples];
            let consumed = self.consumer.pop_slice(scratch);
            debug_assert_eq!(consumed, samples);
            // Decoder PCM is finite and normalized volume gives gain in [0, 1].
            // Mapping and attenuation cannot introduce non-finite samples.
            for (output, frame) in block
                .chunks_exact_mut(self.channels)
                .zip(scratch.chunks_exact(source_channels))
            {
                for (sample, source) in output.iter_mut().zip(&self.mapping) {
                    *sample = match source {
                        Some(source) => frame[*source] * volume,
                        None => 0.0,
                    };
                }
            }
        }
        if count > 0
            && self.consumer.occupied_len() <= self.consumer.capacity().get() / 4
            && self.shared.refill_armed.swap(false, Ordering::AcqRel)
        {
            let _ = self.shared.wakeup.try_send(());
        }
        if let Some(start) = start {
            self.clock.pending.push_back(ScheduledFrames {
                start,
                frames: count as u64,
                heard: 0,
            });
        }
    }
}

const PREFIX: &str = "PipeWire: ";
const TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq)]
struct Sink {
    id: u32,
    serial: String,
    name: String,
    positions: String,
}

struct Route {
    output_node: u32,
    output_port: u32,
    input_node: u32,
    input_port: u32,
}

#[derive(Default)]
struct Inventory {
    sinks: BTreeMap<u32, Sink>,
    default: Option<String>,
    default_metadata: Option<u32>,
    ports: BTreeMap<u32, String>,
    links: BTreeMap<u32, Route>,
    revision: u64,
    // Registry mutations invalidate confirmation immediately, even if process
    // is dispatched before the control loop next validates the new topology.
    route_confirmed: Rc<Cell<bool>>,
}

impl Inventory {
    fn changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.route_confirmed.set(false);
    }
}

// Drop listeners before their bound objects, including on global removal.
struct BoundProxy {
    _listener: Box<dyn pw::proxy::Listener>,
    _proxy: Box<dyn pw::proxy::ProxyT>,
}

struct Connection {
    _listener: pw::registry::Listener,
    _proxies: Rc<RefCell<BTreeMap<u32, BoundProxy>>>,
    mainloop: pw::main_loop::MainLoopRc,
    core: pw::core::CoreRc,
    _registry: pw::registry::RegistryRc,
    inventory: Rc<RefCell<Inventory>>,
}

impl Connection {
    fn new() -> Result<Self> {
        pw::init();
        let mainloop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&mainloop, None)?;
        let core = context.connect_rc(None).context("Connecting to PipeWire")?;
        let registry = core.get_registry_rc()?;
        let inventory = Rc::new(RefCell::new(Inventory::default()));
        let proxies = Rc::new(RefCell::new(BTreeMap::new()));
        let weak = registry.downgrade();
        let state = inventory.clone();
        let held = proxies.clone();
        let removed = inventory.clone();
        let removed_proxies = proxies.clone();
        let listener = registry
            .add_listener_local()
            .global(move |global| {
                let Some(registry) = weak.upgrade() else {
                    return;
                };
                let Some(props) = global.props else { return };
                if global.type_ == pw::types::ObjectType::Port {
                    if let Some(channel) = props.get("audio.channel") {
                        let mut state = state.borrow_mut();
                        state.ports.insert(global.id, channel.to_owned());
                        state.changed();
                    }
                } else if global.type_ == pw::types::ObjectType::Link {
                    let id = |key| props.get(key).and_then(|value| value.parse::<u32>().ok());
                    if let (
                        Some(output_node),
                        Some(output_port),
                        Some(input_node),
                        Some(input_port),
                    ) = (
                        id("link.output.node"),
                        id("link.output.port"),
                        id("link.input.node"),
                        id("link.input.port"),
                    ) {
                        let mut state = state.borrow_mut();
                        state.links.insert(
                            global.id,
                            Route {
                                output_node,
                                output_port,
                                input_node,
                                input_port,
                            },
                        );
                        state.changed();
                    }
                }
                if global.type_ == pw::types::ObjectType::Node
                    && props.get("media.class") == Some("Audio/Sink")
                {
                    let (Some(name), Some(serial)) =
                        (props.get("node.name"), props.get("object.serial"))
                    else {
                        return;
                    };
                    let id = global.id;
                    state.borrow_mut().sinks.insert(
                        id,
                        Sink {
                            id,
                            serial: serial.to_owned(),
                            name: name.to_owned(),
                            positions: props.get("audio.position").unwrap_or("").to_owned(),
                        },
                    );
                    state.borrow_mut().changed();
                    if let Ok(node) = registry.bind::<pw::node::Node, _>(global) {
                        let state = state.clone();
                        let listener = node
                            .add_listener_local()
                            .info(move |info| {
                                if let Some(props) = info.props()
                                    && let Some(positions) = props.get("audio.position")
                                {
                                    let mut state = state.borrow_mut();
                                    if let Some(sink) = state.sinks.get_mut(&id)
                                        && sink.positions != positions
                                    {
                                        sink.positions = positions.to_owned();
                                        state.changed();
                                    }
                                }
                            })
                            .register();
                        held.borrow_mut().insert(
                            id,
                            BoundProxy {
                                _listener: Box::new(listener),
                                _proxy: Box::new(node),
                            },
                        );
                    }
                } else if global.type_ == pw::types::ObjectType::Metadata
                    && props.get("metadata.name") == Some("default")
                {
                    let id = global.id;
                    if let Ok(metadata) = registry.bind::<pw::metadata::Metadata, _>(global) {
                        state.borrow_mut().default_metadata = Some(id);
                        let state = state.clone();
                        let listener = metadata
                            .add_listener_local()
                            .property(move |subject, key, _, value| {
                                if subject == 0 && key.is_none() {
                                    state.borrow_mut().default = None;
                                }
                                if subject == 0 && key == Some("default.audio.sink") {
                                    state.borrow_mut().default = value
                                        .and_then(|value| {
                                            serde_json::from_str::<serde_json::Value>(value).ok()
                                        })
                                        .and_then(|value| {
                                            value
                                                .get("name")
                                                .and_then(|name| name.as_str())
                                                .map(str::to_owned)
                                        });
                                }
                                0
                            })
                            .register();
                        held.borrow_mut().insert(
                            id,
                            BoundProxy {
                                _listener: Box::new(listener),
                                _proxy: Box::new(metadata),
                            },
                        );
                    }
                }
            })
            .global_remove(move |id| {
                // Release outside the inventory borrow: destroying a proxy may
                // dispatch callbacks that also need the inventory.
                let proxy = removed_proxies.borrow_mut().remove(&id);
                drop(proxy);
                let mut state = removed.borrow_mut();
                state.sinks.remove(&id);
                state.ports.remove(&id);
                state.links.remove(&id);
                if state.default_metadata == Some(id) {
                    state.default_metadata = None;
                    state.default = None;
                }
                state.changed();
            })
            .register();
        let connection = Self {
            mainloop,
            core,
            _registry: registry,
            _listener: listener,
            _proxies: proxies,
            inventory,
        };
        // First sync discovers globals; second receives bound node properties
        // and default metadata. No output is opened during device enumeration.
        connection.roundtrip()?;
        connection.roundtrip()?;
        Ok(connection)
    }

    fn roundtrip(&self) -> Result<()> {
        let done = Rc::new(Cell::new(false));
        let completed = done.clone();
        let sequence = self.core.sync(0)?;
        let error = Rc::new(RefCell::new(None));
        let failed = error.clone();
        let _listener = self
            .core
            .add_listener_local()
            .done(move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == sequence {
                    completed.set(true);
                }
            })
            .error(move |_, _, _, message| {
                *failed.borrow_mut() = Some(message.to_owned());
            })
            .register();
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(message) = error.borrow_mut().take() {
                bail!("PipeWire: {message}");
            }
            if done.get() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!("Timed out querying PipeWire devices");
            }
            self.mainloop.loop_().iterate(pw::loop_::Timeout::Finite(
                deadline.saturating_duration_since(Instant::now()),
            ));
        }
    }

    fn target(&self, name: Option<&str>) -> Result<Sink> {
        let inventory = self.inventory.borrow();
        let name = name
            .or(inventory.default.as_deref())
            .context("PipeWire has no default audio sink; select an explicit PipeWire device")?;
        inventory
            .sinks
            .values()
            .find(|sink| sink.name == name)
            .cloned()
            .with_context(|| format!("PipeWire audio sink not found: {name}"))
    }
}

#[derive(Default)]
struct NativeControl {
    stop: AtomicBool,
    wake_pending: AtomicBool,
    metadata: Mutex<Option<Arc<PlaybackMetadata>>>,
}

struct NativeStream {
    control: Arc<NativeControl>,
    wake: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for NativeStream {
    fn drop(&mut self) {
        self.control.stop.store(true, Ordering::Release);
        let _ = self.wake_owner();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl NativeStream {
    fn wake_owner(&self) -> Result<()> {
        if !self.control.wake_pending.swap(true, Ordering::AcqRel) && self.wake.send(()).is_err() {
            self.control.wake_pending.store(false, Ordering::Release);
            bail!("Cannot wake PipeWire output owner");
        }
        Ok(())
    }

    fn set_metadata(&self, metadata: Arc<PlaybackMetadata>) -> Result<()> {
        let replaced = self.control.metadata.lock().replace(metadata);
        // A last Arc may release all strings. Never do that while holding the mailbox.
        drop(replaced);
        self.wake_owner()
    }

    fn open(
        name: Option<&str>,
        source_layout: &[Channel],
        auto_mix: bool,
        metadata: Arc<PlaybackMetadata>,
        callback: OutputCallback,
        errors: crossbeam_channel::Sender<OutputError>,
    ) -> Result<Self> {
        let name = name.map(str::to_owned);
        let source_layout = source_layout.to_vec();
        let control = Arc::new(NativeControl {
            metadata: Mutex::new(Some(metadata)),
            ..NativeControl::default()
        });
        let owner_control = control.clone();
        let (wake, receiver) = pw::channel::channel();
        let (ready, receive) = crossbeam_channel::bounded(1);
        let thread = std::thread::Builder::new()
            .name("rivu-pipewire".into())
            .spawn(move || {
                if let Err(error) = run(
                    name.as_deref(),
                    &source_layout,
                    auto_mix,
                    callback,
                    &owner_control,
                    &ready,
                    receiver,
                ) {
                    let message = format!("{error:#}");
                    let _ = ready.try_send(Err(message.clone()));
                    let _ = errors.try_send(OutputError(message));
                }
            })?;
        let stream = Self {
            control,
            wake,
            thread: Some(thread),
        };
        receive
            .recv_timeout(TIMEOUT * 4)
            .context("Timed out opening PipeWire output")?
            .map_err(anyhow::Error::msg)?;
        Ok(stream)
    }
}

fn run(
    name: Option<&str>,
    source_layout: &[Channel],
    auto_mix: bool,
    mut callback: OutputCallback,
    control: &Arc<NativeControl>,
    ready: &crossbeam_channel::Sender<std::result::Result<(), String>>,
    wake: pw::channel::Receiver<()>,
) -> Result<()> {
    let connection = Connection::new()?;
    let wake_control = control.clone();
    let control_woken = Rc::new(Cell::new(false));
    let received_control = control_woken.clone();
    let _wake = wake.attach(connection.mainloop.loop_(), move |_| {
        wake_control.wake_pending.store(false, Ordering::Release);
        received_control.set(true);
    });
    let target = connection.target(name)?;
    let speakers = parse_positions(&target.positions).with_context(|| {
        format!(
            "PipeWire sink {} has no usable speaker layout ({:?})",
            target.name, target.positions
        )
    })?;
    let (stream_layout, mapping, mono) = if auto_mix {
        (
            source_layout,
            (0..source_layout.len()).map(Some).collect(),
            false,
        )
    } else {
        let mapping = channels::mapping(source_layout, &speakers)
            .with_context(|| format!("PipeWire sink {} cannot preserve this track", target.name))?;
        (
            speakers.as_slice(),
            mapping,
            speakers == [Channel::FrontCenter] && target.positions.contains("MONO"),
        )
    };
    callback.mapping = mapping;
    callback.channels = stream_layout.len();
    let rate = callback.rate;
    let shared = callback.shared.clone();
    let names: Vec<_> = stream_layout
        .iter()
        .map(|channel| {
            if mono {
                "MONO"
            } else {
                position_name(*channel)
            }
        })
        .collect();
    let positions = names.join(" ");
    let mut properties = properties! {
        "media.type" => "Audio", "media.category" => "Playback", "media.role" => "Music",
        "application.name" => "Rivu", "application.id" => "rivu",
        "application.icon-name" => "rivu", "node.name" => "rivu-native-output",
        "target.object" => target.serial.as_str(), "node.dont-reconnect" => "true",
        "node.dont-fallback" => "true", "stream.dont-remix" => (!auto_mix).to_string(),
        "channelmix.disable" => (!auto_mix).to_string(), "audio.channels" => stream_layout.len().to_string(),
        "audio.position" => format!("[ {positions} ]"), "audio.rate" => rate.to_string(),
        "node.rate" => format!("1/{rate}"),
    };
    if auto_mix {
        properties.insert("channelmix.normalize", "true");
    }
    let stream = pw::stream::StreamBox::new(&connection.core, "Rivu playback", properties)?;
    let metadata = control
        .metadata
        .lock()
        .take()
        .context("Missing initial PipeWire track metadata")?;
    update_stream_metadata(&stream, &metadata)?;
    drop(metadata);
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(rate);
    info.set_channels(stream_layout.len() as u32);
    let mut positions = [0; spa::param::audio::MAX_CHANNELS];
    for (position, speaker) in positions.iter_mut().zip(stream_layout) {
        *position = spa_position(*speaker);
    }
    if mono {
        positions[0] = spa::sys::SPA_AUDIO_CHANNEL_MONO;
    }
    info.set_position(positions);
    let expected = info;
    let format_ok = Rc::new(Cell::new(false));
    let checked = format_ok.clone();
    let format_error = Rc::new(Cell::new(false));
    let processing = format_ok.clone();
    let rejected = format_error.clone();
    let route_ok = connection.inventory.borrow().route_confirmed.clone();
    let routed = route_ok.clone();
    let _listener = stream
        .add_local_listener_with_user_data(callback)
        .param_changed(move |_, _, id, pod| {
            if id != spa::sys::SPA_PARAM_Format {
                return;
            }
            checked.set(false);
            if let Some(pod) = pod {
                let mut actual = spa::param::audio::AudioInfoRaw::new();
                let valid = actual.parse(pod).is_ok()
                    && actual.format() == expected.format()
                    && actual.rate() == expected.rate()
                    && actual.channels() == expected.channels()
                    && actual.position()[..actual.channels() as usize]
                        == expected.position()[..expected.channels() as usize];
                checked.set(valid);
                rejected.set(!valid);
            }
        })
        .process(move |stream, callback| {
            // Process runs on this dedicated loop, never on the UI/decoder thread.
            // All sample, mapping and clock storage was preallocated.
            if processing.get() {
                unsafe {
                    process(stream, callback, routed.get());
                }
            }
        })
        .register()?;
    let bytes = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::sys::SPA_TYPE_OBJECT_Format,
            id: spa::sys::SPA_PARAM_EnumFormat,
            properties: info.into(),
        }),
    )
    .map_err(|error| anyhow::anyhow!("Serializing PipeWire audio format: {error:?}"))?
    .0
    .into_inner();
    stream.connect(
        spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut [spa::pod::Pod::from_bytes(&bytes).context("Invalid PipeWire format pod")?],
    )?;
    let deadline = Instant::now() + TIMEOUT;
    let mut started = false;
    let mut active = true;
    let mut observed_revision = None;
    let failure = Rc::new(RefCell::new(None));
    let failed = failure.clone();
    let _core_listener = connection
        .core
        .add_listener_local()
        .error(move |_, _, _, message| {
            *failed.borrow_mut() = Some(message.to_owned());
        })
        .register();
    while !control.stop.load(Ordering::Acquire) {
        connection.mainloop.loop_().iterate(if started {
            pw::loop_::Timeout::Infinite
        } else {
            pw::loop_::Timeout::Finite(deadline.saturating_duration_since(Instant::now()))
        });
        if control.stop.load(Ordering::Acquire) {
            break;
        }
        if control_woken.replace(false) {
            let metadata = control.metadata.lock().take();
            if let Some(metadata) = metadata {
                update_stream_metadata(&stream, &metadata)?;
            }
        }
        {
            let graph = connection.inventory.borrow();
            if observed_revision != Some(graph.revision) {
                route_ok.set(route_matches(
                    &graph,
                    stream.node_id(),
                    target.id,
                    &names,
                    auto_mix,
                ));
                observed_revision = Some(graph.revision);
            }
        }
        if let Some(error) = failure.borrow_mut().take() {
            bail!("PipeWire output: {error}");
        }
        if format_error.get() {
            bail!("PipeWire changed the requested sample format, rate or speaker positions");
        }
        if connection.inventory.borrow().sinks.get(&target.id) != Some(&target) {
            bail!(
                "PipeWire target {} disappeared or changed its speaker layout; refusing to reroute",
                target.name
            );
        }
        if started && !route_ok.get() {
            bail!("PipeWire output links changed or disappeared; refusing an invalid route");
        }
        match stream.state() {
            pw::stream::StreamState::Error(error) => bail!("PipeWire output: {error}"),
            pw::stream::StreamState::Unconnected if started => {
                bail!("PipeWire output disconnected")
            }
            pw::stream::StreamState::Streaming if format_ok.get() && route_ok.get() => {
                if !started {
                    // Connect active to negotiate format and links even when
                    // opening paused. No ring data is consumed while paused.
                    if shared.paused.load(Ordering::Acquire) {
                        stream
                            .set_active(false)
                            .context("Pausing PipeWire output")?;
                        stream
                            .flush(false)
                            .context("Flushing paused PipeWire output")?;
                        active = false;
                    }
                    let _ = ready.send(Ok(()));
                    started = true;
                }
            }
            _ if !started && Instant::now() >= deadline => bail!(
                "PipeWire sink {} did not accept/link the requested speaker layout",
                target.name
            ),
            _ => {}
        }
        if started {
            let should_be_active = !(shared.paused.load(Ordering::Acquire)
                && shared.pause_settled.load(Ordering::Acquire));
            if active != should_be_active {
                // The callback only settles after every scheduled non-silent
                // frame retires. Deactivation preserves unread ring PCM and
                // leaves no scheduled frames to count during paused wall time.
                stream
                    .set_active(should_be_active)
                    .context("Changing PipeWire output activity")?;
                if !should_be_active {
                    stream
                        .flush(false)
                        .context("Flushing paused PipeWire output")?;
                }
                active = should_be_active;
            }
        }
    }
    route_ok.set(false);
    stream.disconnect()?;
    Ok(())
}

// Tags may contain NUL; encode that character without rejecting playable audio.
fn property_text(text: &str) -> Cow<'_, str> {
    if text.contains('\0') {
        Cow::Owned(text.replace('\0', "�"))
    } else {
        Cow::Borrowed(text)
    }
}

fn update_stream_metadata(stream: &pw::stream::Stream, metadata: &PlaybackMetadata) -> Result<()> {
    let title = if metadata.title.trim().is_empty() {
        "Rivu playback"
    } else {
        metadata.title.as_str()
    };
    let title = property_text(title);
    let artist = property_text(&metadata.artist);
    let album = property_text(&metadata.album);
    let properties = properties! {
        "media.name" => title.as_ref(),
        "media.title" => title.as_ref(),
        "media.artist" => artist.as_ref(),
        "media.album" => album.as_ref(),
    };
    // Empty artist/album values explicitly clear tags from the previous track.
    // SAFETY: this stream API has no 0.10.1 wrapper. Only its owner loop calls
    // it, and PipeWire copies the live property dictionary synchronously.
    let result = unsafe {
        pw::sys::pw_stream_update_properties(stream.as_raw_ptr(), &properties.as_raw().dict)
    };
    if result < 0 {
        return Err(std::io::Error::from_raw_os_error(-result))
            .context("Updating PipeWire track metadata");
    }
    Ok(())
}

// SAFETY: invoked only by the owning PipeWire loop while the stream is alive.
// Mapped SPA buffers are checked for shape/alignment before making an f32 slice;
// each dequeued buffer is queued exactly once, empty on failure.
unsafe fn process(stream: &pw::stream::Stream, callback: &mut OutputCallback, routed: bool) {
    unsafe {
        // Buffer has no size setter in 0.10.1; keep raw access so queued time
        // remains measured in frames rather than bytes or channel samples.
        let buffer = stream.dequeue_raw_buffer();
        if buffer.is_null() {
            return;
        }
        let result = render_buffer(stream, buffer, callback, routed);
        if !result {
            callback.shared.fail_timing(3);
        }
        stream.queue_raw_buffer(buffer);
    }
}

// SAFETY: buffer and its SPA data/chunks belong to a dequeued, mapped buffer.
// Empty every chunk before rejecting the shape: pw_buffer.size only accounts
// for queued time and does not stop stale SPA chunks from reaching the graph.
unsafe fn buffer_data(buffer: &mut pw::sys::pw_buffer) -> Option<&mut spa::sys::spa_data> {
    unsafe {
        buffer.size = 0;
        let spa_buffer = buffer.buffer.as_mut()?;
        if spa_buffer.datas.is_null() {
            return None;
        }
        let datas = std::slice::from_raw_parts_mut(spa_buffer.datas, spa_buffer.n_datas as usize);
        for data in datas.iter_mut() {
            if let Some(chunk) = data.chunk.as_mut() {
                chunk.offset = 0;
                chunk.size = 0;
                chunk.stride = 0;
                chunk.flags = 0;
            }
        }
        match datas {
            [data] if !data.chunk.is_null() => Some(data),
            _ => None,
        }
    }
}

unsafe fn render_buffer(
    stream: &pw::stream::Stream,
    buffer: *mut pw::sys::pw_buffer,
    callback: &mut OutputCallback,
    routed: bool,
) -> bool {
    unsafe {
        let Some(data) = buffer_data(&mut *buffer) else {
            return false;
        };
        let stride = callback.channels * std::mem::size_of::<f32>();
        if data.data.is_null() || !(data.data as usize).is_multiple_of(std::mem::align_of::<f32>())
        {
            return false;
        }
        let maximum = data.maxsize as usize / stride;
        let requested = (*buffer).requested as usize;
        let frames = if requested == 0 {
            maximum
        } else {
            maximum.min(requested)
        };
        let samples =
            std::slice::from_raw_parts_mut(data.data.cast::<f32>(), frames * callback.channels);
        let Ok(time) = stream.time() else {
            return false;
        };
        // 0.10.1 exposes Time safely, but get_nsec still has no safe wrapper.
        let now = pw::sys::pw_stream_get_nsec(stream.as_raw_ptr());
        let Some(timestamp) = output_timestamp(time.as_raw(), callback.rate, now) else {
            return false;
        };
        callback.render_native(samples, timestamp, routed);
        (*data.chunk).stride = stride as i32;
        (*data.chunk).size = (frames * stride) as u32;
        (*buffer).size = frames as u64;
        true
    }
}

fn output_timestamp(time: &pw::sys::pw_time, rate: u32, now: u64) -> Option<OutputTimestamp> {
    if time.now < 0 || time.rate.num == 0 || time.rate.denom == 0 || rate == 0 {
        return None;
    }
    let elapsed = now.checked_sub(time.now as u64)?;
    // Graph delay belongs to the snapshot origin; only the callback clock is
    // extrapolated. Advancing both origins would count loop dispatch latency as
    // extra device latency. Queued/buffered values are current stream frames.
    let graph_delay = time.delay.max(0) as u128 * u128::from(time.rate.num) * 1_000_000_000
        / u128::from(time.rate.denom);
    let delay = graph_delay.saturating_sub(u128::from(elapsed))
        + (u128::from(time.queued) + u128::from(time.buffered)) * 1_000_000_000 / u128::from(rate);
    let playback = now.checked_add(u64::try_from(delay).ok()?)?;
    Some(OutputTimestamp {
        callback: StreamInstant::from_nanos(now),
        playback: StreamInstant::from_nanos(playback),
    })
}

// An accepted format alone is insufficient: all outgoing links must reach the
// selected sink, and at least one must exist. Remixing may change the exposed
// speaker labels and port count; strict mode additionally requires one direct
// link per speaker with equal labels on both ends and no extra outgoing links.
fn route_matches(
    graph: &Inventory,
    source: u32,
    target: u32,
    speakers: &[&str],
    auto_mix: bool,
) -> bool {
    let routes = || {
        graph
            .links
            .values()
            .filter(|route| route.output_node == source)
    };
    if auto_mix {
        let mut routes = routes();
        return routes
            .next()
            .is_some_and(|route| route.input_node == target)
            && routes.all(|route| route.input_node == target);
    }
    if routes().count() != speakers.len() {
        return false;
    }
    speakers.iter().all(|speaker| {
        routes()
            .filter(|route| {
                route.input_node == target
                    && graph.ports.get(&route.output_port).map(String::as_str) == Some(*speaker)
                    && graph.ports.get(&route.input_port).map(String::as_str) == Some(*speaker)
            })
            .count()
            == 1
    })
}

fn parse_positions(text: &str) -> Result<Vec<Channel>> {
    let text = text.trim().trim_start_matches('[').trim_end_matches(']');
    let positions: Vec<_> = text
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|name| !name.is_empty())
        .map(|name| {
            let name = name.trim_matches('"');
            if name == "MONO" {
                return Ok(Channel::FrontCenter);
            }
            all_channels()
                .iter()
                .copied()
                .find(|channel| position_name(*channel) == name)
                .with_context(|| format!("Unsupported or unspecified PipeWire speaker {name:?}"))
        })
        .collect::<Result<_>>()?;
    channels::mapping(&positions, &positions)?;
    Ok(positions)
}

fn all_channels() -> &'static [Channel] {
    use Channel::*;
    &[
        FrontLeft,
        FrontRight,
        FrontCenter,
        Lfe,
        RearLeft,
        RearRight,
        RearCenter,
        SideLeft,
        SideRight,
        FrontLeftCenter,
        FrontRightCenter,
        TopCenter,
        TopFrontLeft,
        TopFrontCenter,
        TopFrontRight,
        TopRearLeft,
        TopRearCenter,
        TopRearRight,
    ]
}

fn position_name(channel: Channel) -> &'static str {
    use Channel::*;
    match channel {
        FrontLeft => "FL",
        FrontRight => "FR",
        FrontCenter => "FC",
        Lfe => "LFE",
        RearLeft => "RL",
        RearRight => "RR",
        RearCenter => "RC",
        SideLeft => "SL",
        SideRight => "SR",
        FrontLeftCenter => "FLC",
        FrontRightCenter => "FRC",
        TopCenter => "TC",
        TopFrontLeft => "TFL",
        TopFrontCenter => "TFC",
        TopFrontRight => "TFR",
        TopRearLeft => "TRL",
        TopRearCenter => "TRC",
        TopRearRight => "TRR",
    }
}

fn spa_position(channel: Channel) -> u32 {
    use Channel::*;
    use spa::sys::*;
    match channel {
        FrontLeft => SPA_AUDIO_CHANNEL_FL,
        FrontRight => SPA_AUDIO_CHANNEL_FR,
        FrontCenter => SPA_AUDIO_CHANNEL_FC,
        Lfe => SPA_AUDIO_CHANNEL_LFE,
        RearLeft => SPA_AUDIO_CHANNEL_RL,
        RearRight => SPA_AUDIO_CHANNEL_RR,
        RearCenter => SPA_AUDIO_CHANNEL_RC,
        SideLeft => SPA_AUDIO_CHANNEL_SL,
        SideRight => SPA_AUDIO_CHANNEL_SR,
        FrontLeftCenter => SPA_AUDIO_CHANNEL_FLC,
        FrontRightCenter => SPA_AUDIO_CHANNEL_FRC,
        TopCenter => SPA_AUDIO_CHANNEL_TC,
        TopFrontLeft => SPA_AUDIO_CHANNEL_TFL,
        TopFrontCenter => SPA_AUDIO_CHANNEL_TFC,
        TopFrontRight => SPA_AUDIO_CHANNEL_TFR,
        TopRearLeft => SPA_AUDIO_CHANNEL_TRL,
        TopRearCenter => SPA_AUDIO_CHANNEL_TRC,
        TopRearRight => SPA_AUDIO_CHANNEL_TRR,
    }
}

#[cfg(test)]
pub(super) fn test_backend(
    rate: u32,
    paused: bool,
    layout: &[Channel],
) -> (Output, impl FnMut(&mut [f32], u64) + use<>) {
    test_backend_with_capacity(rate, paused, layout, (rate / 4) as usize * layout.len())
}

#[cfg(test)]
pub(super) fn test_backend_with_capacity(
    rate: u32,
    paused: bool,
    layout: &[Channel],
    capacity: usize,
) -> (Output, impl FnMut(&mut [f32], u64) + use<>) {
    let (output, mut callback, analyzer) =
        pcm_tests::test_output_layout(rate, paused, layout, layout, capacity);
    (output, move |data, millis| {
        let _keep_analyzer_alive = &analyzer;
        callback.render(
            data,
            OutputTimestamp {
                callback: StreamInstant::from_millis(millis),
                playback: StreamInstant::from_millis(millis + 20),
            },
        );
    })
}

#[cfg(test)]
mod pcm_tests {
    use super::*;
    use crate::analysis::AnalysisFrame;
    use parking_lot::RwLock;

    #[test]
    fn pcm_capacity_preserves_normal_high_rate_and_multichannel_queues() {
        for (rate, channels, expected) in [
            (1, 2, 2048 * 2),
            (8000, 2, 2048 * 2),
            (44_100, 2, 11_025 * 2),
            (192_000, 32, 48_000 * 32),
            (768_000, 16, 192_000 * 16),
        ] {
            assert_eq!(output_capacity(rate, channels).unwrap(), expected);
        }
    }

    #[test]
    fn pcm_capacity_enforces_byte_budget_before_allocation() {
        // Check the exact boundary without allocating a large buffer.
        assert_eq!(
            output_capacity(262_144, 64).unwrap() * size_of::<f32>(),
            MAX_PCM_BUFFER_BYTES
        );
        assert!(output_capacity(262_148, 64).is_err());
        assert!(output_capacity(262_144, 65).is_err());
        // A tiny stereo WAV can advertise this rate in its header.
        let error = output_capacity(1_000_000_000, 2).unwrap_err();
        assert!(error.to_string().contains("16 MiB limit"));
    }

    #[test]
    fn pcm_capacity_rejects_invalid_formats_and_checked_arithmetic_overflow() {
        assert!(output_capacity(0, 2).is_err());
        assert!(output_capacity(48_000, 0).is_err());
        assert!(output_capacity(u32::MAX, usize::MAX).is_err());
        assert!(output_capacity(1, usize::MAX / 2048).is_err());
    }

    #[test]
    fn volume_changes_apply_perceptual_gain_to_rendered_samples() {
        let (mut output, mut callback, _analyzer) = test_output(1000, false);
        for (millis, (level, expected)) in [(0.0, 0.0), (0.5, 0.031622775), (1.0, 1.0)]
            .into_iter()
            .enumerate()
        {
            output.set_volume(level);
            assert_eq!(output.push(&[0.5, -0.25]), 1);
            let mut data = [1.0_f32; 2];
            callback.render(&mut data, timestamp(millis as u64, millis as u64 + 20));
            assert!((data[0] - 0.5 * expected).abs() < 0.0000001);
            assert!((data[1] + 0.25 * expected).abs() < 0.0000001);
            assert_eq!(callback.consumer.occupied_len(), 0);
        }
    }

    #[test]
    fn float_output_preserves_overs_and_applies_gain_before_device_conversion() {
        let (mut output, mut callback, _analyzer) = test_output(1000, false);
        for (index, volume) in [1.0, 0.9, 0.5, 0.0].into_iter().enumerate() {
            output.set_volume(volume);
            let samples = [4.0, -3.0, f32::MAX, -f32::MAX];
            assert_eq!(output.push(&samples), 2);
            let mut data = [0.0; 4];
            callback.render(&mut data, timestamp(index as u64, index as u64 + 20));
            let gain = crate::volume::gain(volume);
            assert_eq!(data, samples.map(|sample| sample * gain));
            if volume == 1.0 || volume == 0.9 {
                assert!(data[0] > 1.0 && data[1] < -1.0);
            }
        }
    }

    #[test]
    fn device_names_require_the_native_prefix_without_aliases() {
        assert_eq!(target_name(None).unwrap(), None);
        assert_eq!(target_name(Some("PipeWire: sink")).unwrap(), Some("sink"));
        for name in ["default", "pipewire", "hw:0", "sink"] {
            assert!(validate_device(Some(name)).is_err(), "{name}");
        }
    }

    #[test]
    fn monotonic_instants_saturate_future_spans_and_reject_overflow() {
        let now = StreamInstant::from_nanos(10);
        assert_eq!(
            now.duration_since(StreamInstant::from_nanos(20)),
            Duration::ZERO
        );
        assert_eq!(
            now.duration_since(StreamInstant::from_nanos(3)),
            Duration::from_nanos(7)
        );
        assert_eq!(
            now.checked_add(Duration::from_nanos(2)),
            Some(StreamInstant::from_nanos(12))
        );
        assert!(
            StreamInstant::from_nanos(u64::MAX)
                .checked_add(Duration::from_nanos(1))
                .is_none()
        );
        assert!(now.checked_add(Duration::from_secs(u64::MAX)).is_none());
    }

    #[test]
    fn block_render_preserves_wrapped_frames_and_mapping() {
        let frames = OutputCallback::BLOCK_FRAMES * 2 + 13;
        let source = [Channel::FrontLeft, Channel::FrontRight];
        let device = [
            Channel::FrontRight,
            Channel::FrontLeft,
            Channel::FrontCenter,
        ];
        let (mut output, mut callback, _analyzer) =
            test_output_layout(192_000, false, &source, &device, frames * 2);
        // Offset the ring so the next full write wraps across its allocation.
        output.push(&[0.0; 34]);
        callback.render(&mut [0.0_f32; 51], timestamp(0, 20));
        let input: Vec<_> = (0..frames)
            .flat_map(|i| {
                let value = i as f32 / frames as f32;
                [value, -value]
            })
            .collect();
        assert_eq!(output.push(&input), frames);
        let scratch = callback.scratch.as_ptr();
        let mut data = vec![1.0_f32; (frames + 7) * device.len() + 1];
        callback.render(&mut data, timestamp(1, 21));
        for (actual, expected) in data[..frames * device.len()]
            .chunks_exact(device.len())
            .zip(input.chunks_exact(source.len()))
        {
            assert_eq!(actual, &[expected[1], expected[0], 0.0]);
        }
        assert!(
            data[frames * device.len()..]
                .iter()
                .all(|sample| *sample == 0.0)
        );
        assert_eq!(callback.consumer.occupied_len(), 0);
        assert_eq!(callback.scratch.as_ptr(), scratch);
        assert_eq!(
            callback.scratch.len(),
            OutputCallback::BLOCK_FRAMES * source.len()
        );
    }

    fn test_output(rate: u32, paused: bool) -> (Output, OutputCallback, AnalysisWorker) {
        let stereo = [Channel::FrontLeft, Channel::FrontRight];
        test_output_layout(rate, paused, &stereo, &stereo, 128)
    }

    pub(super) fn test_output_layout(
        rate: u32,
        paused: bool,
        source: &[Channel],
        device: &[Channel],
        capacity: usize,
    ) -> (Output, OutputCallback, AnalysisWorker) {
        let analyzer =
            AnalysisWorker::new(Arc::new(RwLock::new(AnalysisFrame::default()))).unwrap();
        let (producer, consumer) = HeapRb::new(capacity).split();
        let shared = Arc::new(OutputShared::new(1.0, paused));
        let (_, errors) = bounded(4);
        let output = Output {
            stream: None,
            producer,
            shared: shared.clone(),
            rate,
            source_channels: source.len(),
            errors,
            submitted: 0,
        };
        let callback = OutputCallback {
            consumer,
            shared,
            channels: device.len(),
            scratch: vec![0.0; source.len() * OutputCallback::BLOCK_FRAMES],
            mapping: crate::channels::mapping(source, device).unwrap(),
            source_channels: source.len(),
            rate,
            clock: PlaybackClock::new(),
        };
        (output, callback, analyzer)
    }

    fn timestamp(callback_ms: u64, playback_ms: u64) -> OutputTimestamp {
        OutputTimestamp {
            callback: StreamInstant::from_millis(callback_ms),
            playback: StreamInstant::from_millis(playback_ms),
        }
    }
    fn timestamp_nanos(callback: u64, playback: u64) -> OutputTimestamp {
        OutputTimestamp {
            callback: StreamInstant::from_nanos(callback),
            playback: StreamInstant::from_nanos(playback),
        }
    }

    #[test]
    fn drain_waits_for_last_valid_frame_not_empty_ring_or_callback_end() {
        let (mut output, mut callback, _analyzer) = test_output(1000, false);
        output.push([[0.25, -0.25]; 3].as_flattened());
        let mut data = [0.0_f32; 16];
        callback.render(&mut data, timestamp(0, 20));
        assert_eq!(&data[..6], &[0.25, -0.25, 0.25, -0.25, 0.25, -0.25]);
        assert!(data[6..].iter().all(|sample| *sample == 0.0));
        assert_eq!(callback.consumer.occupied_len(), 0);
        assert!(!output.drained());
        assert_eq!(output.heard(), 0.0);
        callback.render(&mut data, timestamp(21, 40));
        assert_eq!(output.heard(), 0.001);
        assert!(!output.drained());
        callback.render(&mut data, timestamp(23, 48));
        assert!(output.drained());
        assert_eq!(output.heard(), 0.003);
    }

    #[test]
    fn close_does_not_count_device_queued_tail_as_heard() {
        let (mut output, mut callback, _analyzer) = test_output(1000, false);
        output.push([[0.5, -0.5]; 8].as_flattened());
        let mut data = [0.0_f32; 16];
        callback.render(&mut data, timestamp(0, 20));
        callback.render(&mut data, timestamp(22, 40));
        assert_eq!(output.close(), 0.002);
        assert!(!output.drained());
    }

    #[test]
    fn paused_tail_notifies_once_after_audible_frames_then_sleeps() {
        let (mut output, mut callback, _analyzer) = test_output(1000, false);
        output.push([[0.5, -0.5]; 4].as_flattened());
        let mut data = [0.0_f32; 16];
        callback.render(&mut data, timestamp(0, 20));
        output.pause(true);
        callback.render(&mut data, timestamp(21, 40));
        assert_eq!(output.heard(), 0.001);
        assert!(output.notifications().try_recv().is_err());
        callback.render(&mut data, timestamp(24, 44));
        assert_eq!(output.heard(), 0.004);
        assert_eq!(output.notifications().try_recv(), Ok(()));
        callback.render(&mut data, timestamp(50, 70));
        assert!(output.notifications().try_recv().is_err());

        output.pause(false);
        output.push([[0.25, -0.25]; 2].as_flattened());
        callback.render(&mut data, timestamp(60, 80));
        output.pause(true);
        callback.render(&mut data, timestamp(82, 102));
        assert_eq!(output.heard(), 0.006);
        assert_eq!(output.notifications().try_recv(), Ok(()));
        callback.render(&mut data, timestamp(81, 101));
        assert!(output.check_timing().is_err());
        assert_eq!(output.notifications().try_recv(), Ok(()));
    }

    #[test]
    fn pause_preserves_ring_and_resume_excludes_silence() {
        let (mut output, mut callback, _analyzer) = test_output(1000, true);
        output.push([[0.5, -0.5]; 2].as_flattened());
        let mut data = [1.0_f32; 16];
        callback.render(&mut data, timestamp(0, 20));
        assert_eq!(callback.consumer.occupied_len(), 4);
        assert!(data.iter().all(|sample| *sample == 0.0));
        assert_eq!(output.heard(), 0.0);
        output.pause(false);
        callback.render(&mut data, timestamp(8, 28));
        assert_eq!(&data[..4], &[0.5, -0.5, 0.5, -0.5]);
        output.pause(true);
        output.push([[0.25, -0.25]; 3].as_flattened());
        callback.render(&mut data, timestamp(30, 50));
        assert_eq!(output.heard(), 0.002);
        assert_eq!(callback.consumer.occupied_len(), 6);
        assert!(data.iter().all(|sample| *sample == 0.0));
        output.pause(false);
        callback.render(&mut data, timestamp(40, 60));
        assert_eq!(output.heard(), 0.002);
        callback.render(&mut data, timestamp(63, 83));
        assert_eq!(output.heard(), 0.005);
        assert!(output.drained());
    }

    #[test]
    fn settled_pause_preserves_unread_frames_across_duplicates_and_long_inactivity() {
        let (mut output, mut callback, _analyzer) = test_output(1000, false);
        let source: Vec<_> = (1..=8)
            .flat_map(|frame| [frame as f32, -(frame as f32)])
            .collect();
        assert_eq!(output.push(&source), 8);
        let mut data = [0.0; 4];
        callback.render(&mut data, timestamp(0, 20));
        assert_eq!(data, source[..4]);
        output.pause(true);
        callback.render(&mut data, timestamp(21, 41));
        assert_eq!(output.position(), 0.001);
        assert_eq!(callback.consumer.occupied_len(), 12);
        assert!(!output.shared.pause_settled.load(Ordering::Acquire));
        output.pause(true);
        callback.render(&mut data, timestamp(22, 42));
        assert_eq!(output.position(), 0.002);
        assert!(output.shared.pause_settled.load(Ordering::Acquire));
        assert!(callback.clock.pending.is_empty());
        output.pause(true);
        assert!(output.shared.pause_settled.load(Ordering::Acquire));

        // No process callbacks occur while inactive. A rapid resume/pause
        // before the next callback must not consume or strand any ring frames.
        output.pause(false);
        output.pause(true);
        callback.render(&mut data, timestamp(10_000, 10_020));
        assert_eq!(data, [0.0; 4]);
        assert_eq!(output.heard(), 0.002);
        assert_eq!(callback.consumer.occupied_len(), 12);
        assert!(output.shared.pause_settled.load(Ordering::Acquire));
        output.pause(false);
        output.pause(false);
        callback.render(&mut data, timestamp(20_000, 20_020));
        assert_eq!(data, source[4..8]);
        assert_eq!(output.position(), 0.002);
        assert_eq!(callback.consumer.occupied_len(), 8);
        output.pause(true);
        callback.render(&mut data, timestamp(20_022, 20_042));
        assert_eq!(output.position(), 0.004);
        assert_eq!(output.heard(), 0.004);
        assert_eq!(callback.consumer.occupied_len(), 8);
        assert!(output.shared.pause_settled.load(Ordering::Acquire));
        assert!(!output.drained());
        output.check_timing().unwrap();
    }

    #[test]
    fn refill_deadlines_keep_pcm_headroom_with_ten_wakes_per_second() {
        let stereo = [Channel::FrontLeft, Channel::FrontRight];
        for rate in [48_000, 192_000] {
            let capacity = rate as usize / 4 * 2;
            let (mut output, mut callback, _analyzer) =
                test_output_layout(rate, false, &stereo, &stereo, capacity);
            let input = vec![0.25; capacity];
            output.push(&input);
            let mut data = vec![0.0_f32; rate as usize / 10 * 2];
            let mut elapsed = Duration::ZERO;
            let mut wakes = 0;
            while elapsed < Duration::from_secs(1) {
                let delay = output.refill_delay(false);
                let occupied = output.producer.occupied_len() / 2;
                assert!(delay <= Duration::from_secs_f64(occupied as f64 / rate as f64 / 2.0));
                assert_eq!(delay, Duration::from_millis(100));
                callback.render(
                    &mut data,
                    timestamp(elapsed.as_millis() as u64, elapsed.as_millis() as u64 + 20),
                );
                assert!(data.iter().all(|sample| *sample == 0.25));
                assert!(output.notifications().try_recv().is_err());
                elapsed += delay;
                wakes += 1;
                output.push(&input);
            }
            assert_eq!(wakes, 10); // Fixed 10ms polling required 100 timer wakes.
        }
    }

    #[test]
    fn low_water_notifies_once_until_refill_and_empty_tail_is_bounded() {
        let (mut output, mut callback, _analyzer) = test_output(1000, false);
        output.push(&[0.25; 128]);
        callback.render(&mut [0.0_f32; 96], timestamp(0, 20));
        assert_eq!(output.notifications().try_recv(), Ok(()));
        assert_eq!(output.refill_delay(false), Duration::from_millis(8));
        callback.render(&mut [0.0_f32; 32], timestamp(1, 21));
        assert!(output.notifications().try_recv().is_err());
        assert_eq!(output.refill_delay(false), Duration::from_millis(1));
        assert!(output.refill_delay(true) >= Duration::from_millis(10));
        output.push(&[0.5; 64]);
        callback.render(&mut [0.0_f32; 34], timestamp(2, 22));
        assert_eq!(output.notifications().try_recv(), Ok(()));
        callback.render(&mut [0.0_f32; 100], timestamp(3, 23));
        assert!(output.notifications().try_recv().is_err());
    }

    #[test]
    fn native_unrouted_buffer_is_silent_without_consuming_then_routes_pcm() {
        let (mut output, mut callback, _analyzer) = test_output(1000, false);
        output.push([[0.5, -0.5]; 2].as_flattened());
        let mut data = [1.0_f32; 4];

        callback.render_native(&mut data, timestamp(0, 20), false);
        assert!(data.iter().all(|sample| *sample == 0.0));
        assert_eq!(callback.consumer.occupied_len(), 4);
        assert_eq!(output.heard(), 0.0);

        callback.render_native(&mut data, timestamp(1, 21), true);
        assert_eq!(data, [0.5, -0.5, 0.5, -0.5]);
        assert_eq!(callback.consumer.occupied_len(), 0);
    }

    #[test]
    fn playback_prediction_retreat_keeps_submission_order_and_drain() {
        let (mut output, mut callback, _analyzer) = test_output(1000, false);
        output.push([[0.5, -0.5]; 8].as_flattened());
        let mut data = [0.0_f32; 8];

        // A latency reduction pulls the backend estimate backwards while the
        // first queued span is still pending. This is not a callback-clock
        // regression and must not make the second span audible early.
        callback.render(&mut data, timestamp(0, 100));
        callback.render(&mut data, timestamp(1, 50));
        callback.render(&mut data, timestamp(60, 60));
        assert!(output.check_timing().is_ok());
        assert_eq!(output.heard(), 0.0);

        callback.render(&mut data, timestamp(108, 108));
        assert!(output.check_timing().is_ok());
        assert_eq!(output.heard(), 0.008);
        assert!(output.drained());
    }

    #[test]
    fn non_integer_frame_duration_never_hears_or_drains_early() {
        let (mut output, mut callback, _analyzer) = test_output(44_100, false);
        output.push([[0.5, -0.5]; 2].as_flattened());
        let mut data = [0.0_f32; 2];

        callback.render(&mut data, timestamp_nanos(0, 1_000_000));
        callback.render(&mut data, timestamp_nanos(1, 0));

        // ceil(1e9 / 44100) = 22676 ns; at 22675 ns no frame may be heard.
        callback.render(&mut data, timestamp_nanos(1_022_675, 1_022_675));
        assert_eq!(output.heard(), 0.0);
        assert!(!output.drained());

        callback.render(&mut data, timestamp_nanos(1_022_676, 1_022_676));
        assert_eq!(output.heard(), 1.0 / 44_100.0);
        assert!(!output.drained());

        callback.render(&mut data, timestamp_nanos(1_045_352, 1_045_352));
        assert!(output.drained());
        assert_eq!(output.heard(), 2.0 / 44_100.0);
    }

    #[test]
    fn invalid_or_stalled_clock_reports_failure_instead_of_premature_drain() {
        let (mut output, mut callback, _analyzer) = test_output(1000, false);
        output.push([[0.5, -0.5]; 1].as_flattened());
        let mut data = [0.0_f32; 2];
        callback.render(&mut data, timestamp(10, 30));
        callback.render(&mut data, timestamp(9, 31));
        assert!(output.check_timing().is_err());
        assert_eq!(output.heard(), 0.0);
        assert!(!output.drained());

        let (mut output, mut callback, _analyzer) = test_output(1000, false);
        for _ in 0..=callback.clock.pending.capacity() {
            output.push([[0.5, -0.5]; 1].as_flattened());
            callback.render(&mut data, timestamp(0, 30));
        }
        assert!(output.check_timing().is_err());
        assert_eq!(output.heard(), 0.0);
        assert!(!output.drained());
        assert_eq!(callback.consumer.occupied_len(), 2);
    }

    fn speaker_layouts() -> Vec<Vec<Channel>> {
        use Channel::*;
        vec![
            vec![FrontCenter],
            vec![FrontLeft, FrontRight],
            vec![FrontLeft, FrontRight, RearLeft, RearRight],
            vec![FrontLeft, FrontRight, FrontCenter, Lfe, RearLeft, RearRight],
            vec![
                FrontLeft,
                FrontRight,
                FrontCenter,
                Lfe,
                RearCenter,
                SideLeft,
                SideRight,
            ],
            vec![
                FrontLeft,
                FrontRight,
                FrontCenter,
                Lfe,
                RearLeft,
                RearRight,
                SideLeft,
                SideRight,
            ],
        ]
    }

    #[test]
    fn channel_markers_survive_partial_refill_ring_wrap_mapping_and_clock() {
        for source in speaker_layouts() {
            let mut device = source.clone();
            device.rotate_left(1);
            device.push(Channel::TopCenter);
            let channels = source.len();
            // Deliberately not divisible by the frame size: a frame itself wraps.
            let (mut output, mut callback, _analyzer) =
                test_output_layout(1000, false, &source, &device, channels * 3 + 1);
            let frames: Vec<f32> = (0..19)
                .flat_map(|frame| {
                    (0..channels)
                        .map(move |channel| ((frame * channels + channel + 1) as f32) / 200.0)
                })
                .collect();
            let mut submitted = 0;
            let mut rendered = 0;
            let mut ms = 0;
            while rendered < 19 {
                submitted += output.push(&frames[submitted * channels..]);
                let expected_count = (submitted - rendered).min(2);
                let mut data = vec![9.0_f32; device.len() * 2];
                callback.render(&mut data, timestamp(ms, ms + 10));
                for (index, frame) in data.chunks_exact(device.len()).enumerate() {
                    for (destination, sample) in frame.iter().enumerate() {
                        let expected = if index < expected_count {
                            source
                                .iter()
                                .position(|channel| *channel == device[destination])
                                .map_or(0.0, |channel| {
                                    frames[(rendered + index) * channels + channel]
                                })
                        } else {
                            0.0
                        };
                        assert_eq!(
                            *sample,
                            expected,
                            "{source:?}, frame {}, speaker {:?}",
                            rendered + index,
                            device[destination]
                        );
                    }
                }
                rendered += expected_count;
                ms += 2;
            }
            assert_eq!(submitted, 19);
            assert!(!output.drained());
            let mut silence = vec![1.0_f32; device.len() * 2];
            callback.render(&mut silence, timestamp(ms + 20, ms + 30));
            assert!(silence.iter().all(|sample| *sample == 0.0));
            assert!(output.drained());
            assert_eq!(output.heard(), 0.019);
            assert_eq!(output.position(), 0.019);
        }
    }
}

#[cfg(test)]
mod routing_tests {
    use super::*;
    use Channel::*;

    #[test]
    fn native_stream_drop_wakes_and_joins_its_owner_loop() {
        let control = Arc::new(NativeControl::default());
        let (wake, receiver) = pw::channel::channel();
        let (ready, started) = crossbeam_channel::bounded(1);
        let (finished, received) = crossbeam_channel::bounded(1);
        let thread = std::thread::spawn(move || {
            pw::init();
            let mainloop = pw::main_loop::MainLoopRc::new(None).unwrap();
            let woken = Rc::new(Cell::new(false));
            let notified = woken.clone();
            let _receiver = receiver.attach(mainloop.loop_(), move |_| notified.set(true));
            ready.send(()).unwrap();
            // Other loop sources may dispatch first. Bound the whole wait,
            // rather than assuming that one iteration receives our message.
            let deadline = Instant::now() + TIMEOUT;
            while !woken.get() && Instant::now() < deadline {
                mainloop.loop_().iterate(pw::loop_::Timeout::Finite(
                    deadline.saturating_duration_since(Instant::now()),
                ));
            }
            finished.send(woken.get()).unwrap();
        });
        started.recv_timeout(TIMEOUT).unwrap();
        drop(NativeStream {
            control: control.clone(),
            thread: Some(thread),
            wake,
        });
        assert!(control.stop.load(Ordering::Acquire));
        assert_eq!(received.try_recv(), Ok(true));
    }

    #[test]
    fn metadata_pause_and_drop_share_one_coalesced_owner_wake() {
        pw::init();
        let mainloop = pw::main_loop::MainLoopRc::new(None).unwrap();
        let control = Arc::new(NativeControl::default());
        let (wake, receiver) = pw::channel::channel();
        let layout = [FrontLeft, FrontRight];
        let (mut output, _callback, _analyzer) =
            super::pcm_tests::test_output_layout(1000, false, &layout, &layout, 128);
        output.stream = Some(NativeStream {
            control: control.clone(),
            wake,
            thread: None,
        });
        let first = Arc::new(PlaybackMetadata::default());
        let replaced = Arc::downgrade(&first);
        output.set_metadata(first).unwrap();
        for index in 0..64 {
            output.pause(index % 2 == 0);
            output
                .set_metadata(Arc::new(PlaybackMetadata {
                    title: index.to_string(),
                    ..PlaybackMetadata::default()
                }))
                .unwrap();
        }
        assert!(replaced.upgrade().is_none());
        let latest = control.metadata.lock().take().unwrap();
        assert_eq!(latest.title, "63");
        drop(output);
        assert!(control.stop.load(Ordering::Acquire));

        let count = Rc::new(Cell::new(0));
        let received = count.clone();
        let received_control = control.clone();
        let _receiver = receiver.attach(mainloop.loop_(), move |_| {
            received_control
                .wake_pending
                .store(false, Ordering::Release);
            received.set(received.get() + 1);
        });
        let deadline = Instant::now() + TIMEOUT;
        while count.get() == 0 && Instant::now() < deadline {
            mainloop.loop_().iterate(pw::loop_::Timeout::Finite(
                deadline.saturating_duration_since(Instant::now()),
            ));
        }
        assert_eq!(count.get(), 1);
        assert!(!control.wake_pending.load(Ordering::Acquire));
    }

    #[test]
    fn rejected_multiplane_buffer_cannot_replay_stale_chunks() {
        let mut chunks = [spa::sys::spa_chunk {
            offset: 8,
            size: 16,
            stride: 4,
            flags: spa::sys::SPA_CHUNK_FLAG_EMPTY as i32,
        }; 2];
        let mut datas: [spa::sys::spa_data; 2] = unsafe { std::mem::zeroed() };
        for (data, chunk) in datas.iter_mut().zip(&mut chunks) {
            data.chunk = chunk;
        }
        let mut spa_buffer: spa::sys::spa_buffer = unsafe { std::mem::zeroed() };
        spa_buffer.n_datas = 2;
        spa_buffer.datas = datas.as_mut_ptr();
        let mut buffer: pw::sys::pw_buffer = unsafe { std::mem::zeroed() };
        buffer.buffer = &mut spa_buffer;
        buffer.size = 4;

        assert!(unsafe { buffer_data(&mut buffer) }.is_none());
        assert_eq!(buffer.size, 0);
        assert!(chunks.iter().all(|chunk| chunk.size == 0));

        // A valid reused buffer must not retain EMPTY/CORRUPTED flags either.
        spa_buffer.n_datas = 1;
        buffer.buffer = &mut spa_buffer;
        chunks[0].flags =
            (spa::sys::SPA_CHUNK_FLAG_EMPTY | spa::sys::SPA_CHUNK_FLAG_CORRUPTED) as i32;
        assert!(unsafe { buffer_data(&mut buffer) }.is_some());
        assert_eq!(chunks[0].flags, 0);
    }

    #[test]
    fn explicit_sink_positions_preserve_rear_center_and_reject_unknowns() {
        assert_eq!(
            parse_positions("[ FL FR FC LFE RC SL SR ]").unwrap(),
            [
                FrontLeft,
                FrontRight,
                FrontCenter,
                Lfe,
                RearCenter,
                SideLeft,
                SideRight
            ]
        );
        assert_eq!(
            parse_positions("[\"FL\",\"FR\",\"RL\",\"RR\",\"FC\",\"LFE\"]").unwrap(),
            [FrontLeft, FrontRight, RearLeft, RearRight, FrontCenter, Lfe]
        );
        assert_eq!(parse_positions("[ MONO ]").unwrap(), [FrontCenter]);
        for invalid in [
            "",
            "[]",
            "[ FL FR UNKNOWN ]",
            "[ FL FR AUX0 ]",
            "[ FL FR FL ]",
        ] {
            assert!(parse_positions(invalid).is_err(), "{invalid}");
        }
        for channel in all_channels() {
            assert_eq!(
                parse_positions(position_name(*channel)).unwrap(),
                [*channel]
            );
        }
    }

    #[test]
    fn actual_links_must_include_lfe_and_rear_center_without_cross_routing() {
        let speakers = ["FL", "FR", "FC", "LFE", "RC", "SL", "SR"];
        let mut graph = Inventory::default();
        for (index, speaker) in speakers.iter().enumerate() {
            let index = index as u32;
            graph.ports.insert(100 + index, (*speaker).to_owned());
            graph.ports.insert(200 + index, (*speaker).to_owned());
            graph.links.insert(
                index,
                Route {
                    output_node: 1,
                    output_port: 100 + index,
                    input_node: 2,
                    input_port: 200 + index,
                },
            );
        }
        assert!(route_matches(&graph, 1, 2, &speakers, false));
        graph.route_confirmed.set(true);
        graph.ports.insert(203, "FL".into());
        graph.changed();
        assert!(!graph.route_confirmed.get());
        assert!(!route_matches(&graph, 1, 2, &speakers, false));
        graph.ports.insert(203, "LFE".into());
        graph.links.get_mut(&4).unwrap().input_node = 3;
        assert!(!route_matches(&graph, 1, 2, &speakers, false));
        graph.links.get_mut(&4).unwrap().input_node = 2;
        graph.links.remove(&4);
        assert!(!route_matches(&graph, 1, 2, &speakers, false));
    }

    #[test]
    fn auto_mix_accepts_remixed_speaker_labels_and_port_counts_only_for_selected_sink() {
        let mut graph = Inventory::default();
        // A mono stream may expose stereo ports after PipeWire remixing.
        for (index, speaker) in ["FL", "FR"].iter().enumerate() {
            let index = index as u32;
            graph.ports.insert(100 + index, (*speaker).to_owned());
            graph.ports.insert(200 + index, (*speaker).to_owned());
            graph.links.insert(
                index,
                Route {
                    output_node: 1,
                    output_port: 100 + index,
                    input_node: 2,
                    input_port: 200 + index,
                },
            );
        }
        assert!(route_matches(&graph, 1, 2, &["FC"], true));
        assert!(!route_matches(&graph, 1, 2, &["FC"], false));
        assert!(route_matches(&graph, 1, 2, &["FL", "FR"], false));

        // Remixing can also connect different speaker labels without changing
        // the target identity. Strict routing must still reject that graph.
        graph.ports.insert(200, "MONO".into());
        assert!(route_matches(&graph, 1, 2, &["FL", "FR"], true));
        assert!(!route_matches(&graph, 1, 2, &["FL", "FR"], false));
    }

    #[test]
    fn auto_mix_rejects_wrong_multiple_and_lost_targets() {
        let mut graph = Inventory::default();
        graph.links.insert(
            0,
            Route {
                output_node: 1,
                output_port: 100,
                input_node: 3,
                input_port: 200,
            },
        );
        assert!(!route_matches(&graph, 1, 2, &["FC"], true));
        graph.links.get_mut(&0).unwrap().input_node = 2;
        assert!(route_matches(&graph, 1, 2, &["FC"], true));
        graph.route_confirmed.set(true);

        // An unrelated stream may use a different sink; our stream may not
        // gain even one additional outgoing link to that sink.
        graph.links.insert(
            1,
            Route {
                output_node: 4,
                output_port: 101,
                input_node: 3,
                input_port: 201,
            },
        );
        graph.changed();
        assert!(!graph.route_confirmed.get());
        assert!(route_matches(&graph, 1, 2, &["FC"], true));
        graph.links.get_mut(&1).unwrap().output_node = 1;
        assert!(!route_matches(&graph, 1, 2, &["FC"], true));

        graph.links.get_mut(&1).unwrap().output_node = 4;
        graph.route_confirmed.set(true);
        graph.links.remove(&0);
        graph.changed();
        assert!(!graph.route_confirmed.get());
        assert!(!route_matches(&graph, 1, 2, &["FC"], true));
        graph.links.clear();
        assert!(!route_matches(&graph, 1, 2, &["FC"], true));
    }

    #[test]
    fn native_clock_uses_graph_ticks_and_stream_frames_not_channel_samples() {
        let mut time = unsafe { std::mem::zeroed::<pw::sys::pw_time>() };
        time.now = 2_000_000_000;
        time.rate.num = 1;
        time.rate.denom = 48_000;
        time.delay = 480;
        time.queued = 8;
        time.buffered = 16;
        let timestamp = output_timestamp(&time, 48_000, 2_000_000_000).unwrap();
        assert_eq!(
            timestamp.playback.duration_since(timestamp.callback),
            Duration::from_micros(10_500)
        );
        let later = output_timestamp(&time, 48_000, 2_004_000_000).unwrap();
        assert_eq!(later.playback, timestamp.playback);
        assert_eq!(
            later.callback.duration_since(timestamp.callback),
            Duration::from_millis(4)
        );
        assert_eq!(
            later.playback.duration_since(later.callback),
            Duration::from_micros(6_500)
        );
        let stale = output_timestamp(&time, 48_000, 2_020_000_000).unwrap();
        assert_eq!(
            stale.playback.duration_since(stale.callback),
            Duration::from_micros(500)
        );
        assert!(output_timestamp(&time, 48_000, 1_999_999_999).is_none());
        assert!(output_timestamp(&time, 48_000, u64::MAX).is_none());
        time.delay = -480;
        let timestamp = output_timestamp(&time, 48_000, 2_000_000_000).unwrap();
        assert_eq!(
            timestamp.playback.duration_since(timestamp.callback),
            Duration::from_micros(500)
        );
        time.rate.denom = 0;
        assert!(output_timestamp(&time, 48_000, 2_000_000_000).is_none());
        time.rate.denom = 48_000;
        time.now = -1;
        assert!(output_timestamp(&time, 48_000, 2_000_000_000).is_none());
    }
}
