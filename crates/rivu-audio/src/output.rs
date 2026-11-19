//! Native device output; callback state and ring buffers stay private.
//! Native PipeWire streams keep a fixed target and explicit speaker positions.
//! Following the system default requires a new stream, not a move of the old route.

use super::{Channel, PlaybackMetadata, channels};
use crate::capture::PcmControl;
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
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering, fence},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

// Preserve the quarter-second queue (at least 2048 frames) without letting
// decoded metadata request unbounded memory. Callback scratch is at most 1/8
// of this budget because it holds only 256 frames.
const MAX_PCM_BUFFER_BYTES: usize = rivu_protocol::pcm::PCM_OUTPUT_MAX_BYTES;

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

/// One native-output lifetime. Times are PipeWire monotonic nanoseconds, not UTC.
/// Peaks are measured after Rivu's final gain/mapping, before PipeWire conversion.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct OutputDiagnosticsSnapshot {
    pub(super) segment_id: u64,
    pub(super) closed: bool,
    pub(super) paused: bool,
    pub(super) input_finished: bool,
    pub(super) source_rate: u32,
    pub(super) source_channels: usize,
    pub(super) stream_rate: u32,
    pub(super) stream_channels: usize,
    pub(super) stream_format: &'static str,
    pub(super) stream_layout: Arc<str>,
    pub(super) sink_name: Option<Arc<str>>,
    pub(super) remix_allowed: bool,
    pub(super) stream_resample_quality: Option<i32>,
    pub(super) graph_rate: Option<u32>,
    pub(super) graph_quantum: Option<u64>,
    pub(super) hardware_rate: Option<u32>,
    pub(super) hardware_format: Option<&'static str>,
    pub(super) callback_count: u64,
    pub(super) started: bool,
    pub(super) startup_wait_callbacks: u64,
    pub(super) startup_wait_frames: u64,
    pub(super) underrun_events: u64,
    pub(super) underrun_callbacks: u64,
    pub(super) missing_frames: u64,
    pub(super) last_underrun_at_ns: Option<u64>,
    // An empty prefix cannot distinguish a stalled producer from its final tail.
    // Keep it visible but unconfirmed until more PCM or finish_input resolves it.
    pub(super) pending_callbacks: u64,
    pub(super) pending_missing_frames: u64,
    pub(super) pending_since_ns: Option<u64>,
    pub(super) ring_frames: usize,
    pub(super) ring_capacity_frames: usize,
    pub(super) ring_low_watermark: Option<usize>,
    // Non-overlapping 250ms windows; hold/counters last for the entire segment.
    pub(super) sample_peak: Option<f32>,
    pub(super) peak_hold: Option<f32>,
    pub(super) overrange_frames: u64,
    pub(super) peak_window_start_ns: Option<u64>,
    pub(super) last_callback_at_ns: Option<u64>,
    pub(super) observed_at_ns: Option<u64>,
}

struct StreamFormat {
    rate: u32,
    channels: usize,
    layout: Arc<str>,
    sink_name: Option<Arc<str>>,
    remix_allowed: bool,
    resample_quality: Option<i32>,
}

const INPUT_OPEN: u64 = u64::MAX;
const NO_OBSERVATION: u64 = u64::MAX;
const PEAK_WINDOW_NS: u64 = 250_000_000;
static NEXT_OUTPUT_SEGMENT: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
struct CallbackDiagnostics {
    callback_count: u64,
    started: bool,
    startup_wait_callbacks: u64,
    startup_wait_frames: u64,
    underrun_events: u64,
    underrun_active: bool,
    underrun_callbacks: u64,
    missing_frames: u64,
    last_underrun_at_ns: u64,
    pending_callbacks: u64,
    pending_missing_frames: u64,
    pending_since_ns: u64,
    pending_last_at_ns: u64,
    consumed_frames: u64,
    ring_low_watermark: u64,
    // An empty ring may be the final frame, even when this callback was full.
    // Only a later nonempty prefix proves that this zero was not a terminal tail.
    pending_empty_ring: bool,
    sample_peak: f32,
    peak_hold: f32,
    overrange_frames: u64,
    peak_window_start_ns: u64,
    peak_window_has_audio: bool,
    last_callback_at_ns: u64,
    graph_rate: u32,
    graph_quantum: u64,
}

impl Default for CallbackDiagnostics {
    fn default() -> Self {
        Self {
            callback_count: 0,
            started: false,
            startup_wait_callbacks: 0,
            startup_wait_frames: 0,
            underrun_events: 0,
            underrun_active: false,
            underrun_callbacks: 0,
            missing_frames: 0,
            last_underrun_at_ns: NO_OBSERVATION,
            pending_callbacks: 0,
            pending_missing_frames: 0,
            pending_since_ns: NO_OBSERVATION,
            pending_last_at_ns: NO_OBSERVATION,
            consumed_frames: 0,
            ring_low_watermark: NO_OBSERVATION,
            pending_empty_ring: false,
            sample_peak: 0.0,
            peak_hold: 0.0,
            overrange_frames: 0,
            peak_window_start_ns: NO_OBSERVATION,
            peak_window_has_audio: false,
            last_callback_at_ns: NO_OBSERVATION,
            graph_rate: 0,
            graph_quantum: 0,
        }
    }
}

impl CallbackDiagnostics {
    fn begin(&mut self, now: u64) {
        self.callback_count += 1;
        self.last_callback_at_ns = now;
        let window = now / PEAK_WINDOW_NS * PEAK_WINDOW_NS;
        if self.peak_window_start_ns != window {
            self.peak_window_start_ns = window;
            self.sample_peak = 0.0;
            self.peak_window_has_audio = false;
        }
    }

    fn clear_pending(&mut self) {
        self.pending_callbacks = 0;
        self.pending_missing_frames = 0;
        self.pending_since_ns = NO_OBSERVATION;
        self.pending_last_at_ns = NO_OBSERVATION;
    }

    fn confirm_pending(&mut self) {
        if self.pending_callbacks != 0 {
            if !self.underrun_active {
                self.underrun_events += 1;
                self.underrun_active = true;
            }
            self.underrun_callbacks += self.pending_callbacks;
            self.missing_frames += self.pending_missing_frames;
            self.last_underrun_at_ns = self.pending_last_at_ns;
            self.clear_pending();
        }
        if self.pending_empty_ring {
            self.ring_low_watermark = 0;
            self.pending_empty_ring = false;
        }
    }

    fn resolve_end(&mut self, end: u64) {
        if end == INPUT_OPEN {
            return;
        }
        if end > self.consumed_frames {
            // The final watermark proves there is more PCM beyond this gap.
            self.confirm_pending();
        } else {
            self.clear_pending();
            self.pending_empty_ring = false;
            self.underrun_active = false;
        }
    }

    fn record_supply(&mut self, requested: usize, count: usize, remaining: usize, end: u64) {
        if count != 0 {
            self.confirm_pending();
            self.started = true;
            self.consumed_frames += count as u64;
        }
        self.resolve_end(end);
        if requested != 0 && count == requested {
            self.underrun_active = false;
        }
        if requested == 0 || end == self.consumed_frames {
            return;
        }
        if !self.started {
            self.startup_wait_callbacks += 1;
            self.startup_wait_frames += requested as u64;
            return;
        }
        if remaining == 0 {
            self.pending_empty_ring = true;
        } else {
            self.ring_low_watermark = self.ring_low_watermark.min(remaining as u64);
        }
        let missing = requested - count;
        if missing != 0 {
            if self.pending_callbacks == 0 {
                self.pending_since_ns = self.last_callback_at_ns;
            }
            self.pending_callbacks += 1;
            self.pending_missing_frames += missing as u64;
            self.pending_last_at_ns = self.last_callback_at_ns;
        }
        self.resolve_end(end);
    }

    fn record_peak(&mut self, peak: f32, overrange_frames: u64) {
        self.peak_window_has_audio = true;
        self.sample_peak = self.sample_peak.max(peak);
        self.peak_hold = self.peak_hold.max(peak);
        self.overrange_frames += overrange_frames;
    }
}

// Single callback writer; readers retry off the realtime path. Atomic payloads
// make even an interrupted read race-free. No allocation, mutex or logging is
// needed to publish a coherent callback snapshot.
#[derive(Default)]
struct DiagnosticStorage {
    sequence: AtomicU64,
    callback_count: AtomicU64,
    started: AtomicBool,
    startup_wait_callbacks: AtomicU64,
    startup_wait_frames: AtomicU64,
    underrun_events: AtomicU64,
    underrun_active: AtomicBool,
    underrun_callbacks: AtomicU64,
    missing_frames: AtomicU64,
    last_underrun_at_ns: AtomicU64,
    pending_callbacks: AtomicU64,
    pending_missing_frames: AtomicU64,
    pending_since_ns: AtomicU64,
    pending_last_at_ns: AtomicU64,
    consumed_frames: AtomicU64,
    ring_low_watermark: AtomicU64,
    pending_empty_ring: AtomicBool,
    sample_peak: AtomicU32,
    peak_hold: AtomicU32,
    overrange_frames: AtomicU64,
    peak_window_start_ns: AtomicU64,
    peak_window_has_audio: AtomicBool,
    last_callback_at_ns: AtomicU64,
    graph_rate: AtomicU32,
    graph_quantum: AtomicU64,
}

impl DiagnosticStorage {
    fn new() -> Self {
        let shared = Self::default();
        shared.publish(&CallbackDiagnostics::default());
        shared
    }

    fn publish(&self, state: &CallbackDiagnostics) {
        self.sequence.fetch_add(1, Ordering::AcqRel);
        self.callback_count
            .store(state.callback_count, Ordering::Relaxed);
        self.started.store(state.started, Ordering::Relaxed);
        self.startup_wait_callbacks
            .store(state.startup_wait_callbacks, Ordering::Relaxed);
        self.startup_wait_frames
            .store(state.startup_wait_frames, Ordering::Relaxed);
        self.underrun_events
            .store(state.underrun_events, Ordering::Relaxed);
        self.underrun_active
            .store(state.underrun_active, Ordering::Relaxed);
        self.underrun_callbacks
            .store(state.underrun_callbacks, Ordering::Relaxed);
        self.missing_frames
            .store(state.missing_frames, Ordering::Relaxed);
        self.last_underrun_at_ns
            .store(state.last_underrun_at_ns, Ordering::Relaxed);
        self.pending_callbacks
            .store(state.pending_callbacks, Ordering::Relaxed);
        self.pending_missing_frames
            .store(state.pending_missing_frames, Ordering::Relaxed);
        self.pending_since_ns
            .store(state.pending_since_ns, Ordering::Relaxed);
        self.pending_last_at_ns
            .store(state.pending_last_at_ns, Ordering::Relaxed);
        self.consumed_frames
            .store(state.consumed_frames, Ordering::Relaxed);
        self.ring_low_watermark
            .store(state.ring_low_watermark, Ordering::Relaxed);
        self.pending_empty_ring
            .store(state.pending_empty_ring, Ordering::Relaxed);
        self.sample_peak
            .store(state.sample_peak.to_bits(), Ordering::Relaxed);
        self.peak_hold
            .store(state.peak_hold.to_bits(), Ordering::Relaxed);
        self.overrange_frames
            .store(state.overrange_frames, Ordering::Relaxed);
        self.peak_window_start_ns
            .store(state.peak_window_start_ns, Ordering::Relaxed);
        self.peak_window_has_audio
            .store(state.peak_window_has_audio, Ordering::Relaxed);
        self.last_callback_at_ns
            .store(state.last_callback_at_ns, Ordering::Relaxed);
        self.graph_rate.store(state.graph_rate, Ordering::Relaxed);
        self.graph_quantum
            .store(state.graph_quantum, Ordering::Relaxed);
        self.sequence.fetch_add(1, Ordering::Release);
    }

    fn snapshot(&self) -> CallbackDiagnostics {
        loop {
            let sequence = self.sequence.load(Ordering::Acquire);
            if sequence & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let state = CallbackDiagnostics {
                callback_count: self.callback_count.load(Ordering::Relaxed),
                started: self.started.load(Ordering::Relaxed),
                startup_wait_callbacks: self.startup_wait_callbacks.load(Ordering::Relaxed),
                startup_wait_frames: self.startup_wait_frames.load(Ordering::Relaxed),
                underrun_events: self.underrun_events.load(Ordering::Relaxed),
                underrun_active: self.underrun_active.load(Ordering::Relaxed),
                underrun_callbacks: self.underrun_callbacks.load(Ordering::Relaxed),
                missing_frames: self.missing_frames.load(Ordering::Relaxed),
                last_underrun_at_ns: self.last_underrun_at_ns.load(Ordering::Relaxed),
                pending_callbacks: self.pending_callbacks.load(Ordering::Relaxed),
                pending_missing_frames: self.pending_missing_frames.load(Ordering::Relaxed),
                pending_since_ns: self.pending_since_ns.load(Ordering::Relaxed),
                pending_last_at_ns: self.pending_last_at_ns.load(Ordering::Relaxed),
                consumed_frames: self.consumed_frames.load(Ordering::Relaxed),
                ring_low_watermark: self.ring_low_watermark.load(Ordering::Relaxed),
                pending_empty_ring: self.pending_empty_ring.load(Ordering::Relaxed),
                sample_peak: f32::from_bits(self.sample_peak.load(Ordering::Relaxed)),
                peak_hold: f32::from_bits(self.peak_hold.load(Ordering::Relaxed)),
                overrange_frames: self.overrange_frames.load(Ordering::Relaxed),
                peak_window_start_ns: self.peak_window_start_ns.load(Ordering::Relaxed),
                peak_window_has_audio: self.peak_window_has_audio.load(Ordering::Relaxed),
                last_callback_at_ns: self.last_callback_at_ns.load(Ordering::Relaxed),
                graph_rate: self.graph_rate.load(Ordering::Relaxed),
                graph_quantum: self.graph_quantum.load(Ordering::Relaxed),
            };
            fence(Ordering::Acquire);
            if self.sequence.load(Ordering::Relaxed) == sequence {
                return state;
            }
        }
    }
}

struct OutputShared {
    paused: AtomicBool,
    pause_settled: AtomicBool,
    wakeup: crossbeam_channel::Sender<()>,
    notifications: Receiver<()>,
    default_target_changed: AtomicBool,
    refill_armed: AtomicBool,
    worker_wake_pending: AtomicBool,
    cursor_wake_pending: AtomicBool,
    // The same audible cursor backs playback accounting and PCM observation.
    pcm: Arc<PcmControl>,
    timing_error: AtomicU32,
    // Combined master/source amplitude gain, prepared outside the realtime callback.
    gain: AtomicU32,
    input_end: AtomicU64,
    diagnostics: DiagnosticStorage,
}
impl OutputShared {
    fn new(gain: f32, paused: bool, pcm: &Arc<PcmControl>) -> Self {
        let (wakeup, notifications) = bounded(1);
        Self {
            paused: AtomicBool::new(paused),
            pause_settled: AtomicBool::new(paused),
            wakeup,
            notifications,
            default_target_changed: AtomicBool::new(false),
            refill_armed: AtomicBool::new(false),
            worker_wake_pending: AtomicBool::new(false),
            cursor_wake_pending: AtomicBool::new(false),
            pcm: Arc::clone(pcm),
            timing_error: AtomicU32::new(0),
            gain: AtomicU32::new(gain.to_bits()),
            input_end: AtomicU64::new(INPUT_OPEN),
            diagnostics: DiagnosticStorage::new(),
        }
    }

    fn fail_timing(&self, code: u32) {
        if self
            .timing_error
            .compare_exchange(0, code, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.worker_wake_pending.store(true, Ordering::Release);
        }
    }

    fn check_timing(&self) -> Result<()> {
        match self.timing_error.load(Ordering::Acquire) {
            0 => Ok(()),
            1 => bail!("Audio output timestamp moved backwards"),
            2 => bail!("Audio output timing queue exhausted before playback advanced"),
            4 => bail!("Cannot wake PipeWire output owner"),
            _ => bail!("PipeWire output returned invalid buffers or unavailable timing"),
        }
    }

    // Channel/waker calls stay outside process (including owner-wake failure).
    // Swapping before delivery leaves any concurrently published intent pending.
    fn forward_notifications(&self) {
        if self.worker_wake_pending.swap(false, Ordering::AcqRel) {
            let _ = self.wakeup.try_send(());
        }
        if self.cursor_wake_pending.swap(false, Ordering::AcqRel) {
            self.pcm.notify();
        }
    }

    fn publish_default_target_changed(&self, changed: bool) {
        // Only the PipeWire owner writes this flag; stable routes need no RMW.
        if self.default_target_changed.load(Ordering::Relaxed) != changed {
            self.default_target_changed
                .store(changed, Ordering::Release);
            if changed {
                let _ = self.wakeup.try_send(());
            }
        }
    }
}

// Declared before the loop/stream so final delivery follows callback retirement,
// including early errors and unwinding. It never outlives this output segment.
struct OwnerNotifications(Arc<OutputShared>);

impl Drop for OwnerNotifications {
    fn drop(&mut self) {
        self.0.forward_notifications();
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
    notified_heard: u64,
}
impl PlaybackClock {
    fn new() -> Self {
        Self {
            pending: VecDeque::with_capacity(1024),
            last_callback: None,
            heard: 0,
            notified_heard: 0,
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
        shared.pcm.heard_frame.store(self.heard, Ordering::Release);
        if self.heard != self.notified_heard
            && (self.heard - self.notified_heard >= rivu_protocol::pcm::PCM_BLOCK_FRAMES as u64
                || self.pending.is_empty())
        {
            self.notified_heard = self.heard;
            if shared.pcm.wants_cursor_notification() {
                shared.cursor_wake_pending.store(true, Ordering::Release);
            }
        }
    }
}

pub(super) struct OutputOptions<'a> {
    pub(super) device: Option<&'a str>,
    pub(super) gain: f32,
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
    // The shared PCM cursor belongs to the next epoch once output is rebuilt.
    // Retain this epoch's final count after its callback owner has joined.
    final_heard_frames: Option<u64>,
    segment_id: u64,
    format: StreamFormat,
    final_diagnostics: Option<OutputDiagnosticsSnapshot>,
    #[cfg(test)]
    on_close: Option<Box<dyn FnOnce() + Send>>,
}
impl Output {
    pub(super) fn new(
        source_rate: u32,
        source_layout: &[Channel],
        options: OutputOptions<'_>,
        analyzer: &Arc<PcmControl>,
    ) -> Result<Self> {
        let target = target_name(options.device)?;
        let rate = source_rate;
        let source_channels = source_layout.len();
        if source_channels > rivu_protocol::pcm::MAX_PCM_CHANNELS {
            bail!("Decoded speaker layout exceeds native PCM channel limit");
        }
        let capacity = output_capacity(rate, source_channels)?;
        analyzer
            .window()
            .retain_output(capacity / source_channels, rate, source_channels)?;
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
        let shared = Arc::new(OutputShared::new(options.gain, options.paused, analyzer));
        let (error_send, errors) = bounded(4);
        analyzer.reset(rate, source_layout);
        let callback = OutputCallback {
            consumer,
            shared: shared.clone(),
            channels: source_channels,
            scratch,
            mapping: Vec::new(),
            source_channels,
            rate,
            clock: PlaybackClock::new(),
            diagnostics: CallbackDiagnostics::default(),
            graph_position: None,
        };
        let (stream, format) = NativeStream::open(
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
            final_heard_frames: None,
            segment_id: NEXT_OUTPUT_SEGMENT.fetch_add(1, Ordering::Relaxed),
            format,
            final_diagnostics: None,
            #[cfg(test)]
            on_close: None,
        })
    }

    pub(super) fn errors(&self) -> &Receiver<OutputError> {
        &self.errors
    }

    pub(super) fn default_target_changed(&self) -> bool {
        self.shared.default_target_changed.load(Ordering::Acquire)
    }

    pub(super) fn set_gain(&self, gain: f32) {
        self.shared.gain.store(gain.to_bits(), Ordering::Release);
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
            self.submitted.saturating_sub(self.heard_frames())
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
        assert_eq!(
            self.shared.input_end.load(Ordering::Acquire),
            INPUT_OPEN,
            "PCM submitted after finish_input"
        );
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

    /// Publish only after every decoder/processing tail frame has been admitted.
    /// This watermark is permanent for this output; seeks/rebuilds use a new one.
    pub(super) fn finish_input(&mut self) {
        self.shared
            .input_end
            .store(self.submitted, Ordering::Release);
    }

    pub(super) fn diagnostics(&self) -> OutputDiagnosticsSnapshot {
        if let Some(snapshot) = &self.final_diagnostics {
            return snapshot.clone();
        }
        self.diagnostics_at(monotonic_now())
    }

    fn diagnostics_at(&self, observed_at_ns: Option<u64>) -> OutputDiagnosticsSnapshot {
        if let Some(snapshot) = &self.final_diagnostics {
            return snapshot.clone();
        }
        let mut state = self.shared.diagnostics.snapshot();
        let end = self.shared.input_end.load(Ordering::Acquire);
        // finish_input may follow the last callback, including while paused.
        // Resolve its published prefix here too without mutating callback state.
        state.resolve_end(end);
        let observed = |value| (value != NO_OBSERVATION).then_some(value);
        OutputDiagnosticsSnapshot {
            segment_id: self.segment_id,
            closed: self.final_heard_frames.is_some(),
            paused: self.shared.paused.load(Ordering::Acquire),
            input_finished: end != INPUT_OPEN,
            source_rate: self.rate,
            source_channels: self.source_channels,
            stream_rate: self.format.rate,
            stream_channels: self.format.channels,
            stream_format: "F32LE",
            stream_layout: Arc::clone(&self.format.layout),
            sink_name: self.format.sink_name.clone(),
            remix_allowed: self.format.remix_allowed,
            stream_resample_quality: self.format.resample_quality,
            graph_rate: (state.graph_rate != 0).then_some(state.graph_rate),
            graph_quantum: (state.graph_quantum != 0).then_some(state.graph_quantum),
            hardware_rate: None,
            hardware_format: None,
            callback_count: state.callback_count,
            started: state.started,
            startup_wait_callbacks: state.startup_wait_callbacks,
            startup_wait_frames: state.startup_wait_frames,
            underrun_events: state.underrun_events,
            underrun_callbacks: state.underrun_callbacks,
            missing_frames: state.missing_frames,
            last_underrun_at_ns: observed(state.last_underrun_at_ns),
            pending_callbacks: state.pending_callbacks,
            pending_missing_frames: state.pending_missing_frames,
            pending_since_ns: observed(state.pending_since_ns),
            ring_frames: self.producer.occupied_len() / self.source_channels,
            ring_capacity_frames: self.producer.capacity().get() / self.source_channels,
            ring_low_watermark: observed(state.ring_low_watermark).map(|frames| frames as usize),
            sample_peak: (state.peak_window_has_audio
                && observed_at_ns.is_none_or(|now| {
                    now.saturating_sub(state.peak_window_start_ns) < PEAK_WINDOW_NS
                }))
            .then_some(state.sample_peak),
            peak_hold: state.started.then_some(state.peak_hold),
            overrange_frames: state.overrange_frames,
            peak_window_start_ns: observed(state.peak_window_start_ns),
            last_callback_at_ns: observed(state.last_callback_at_ns),
            observed_at_ns,
        }
    }

    pub(super) fn drained(&self) -> bool {
        self.heard_frames() == self.submitted
    }

    fn heard_frames(&self) -> u64 {
        self.final_heard_frames
            .unwrap_or_else(|| self.shared.pcm.heard_frame.load(Ordering::Acquire))
    }

    pub(super) fn heard(&self) -> f64 {
        self.heard_frames() as f64 / self.rate as f64
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
            self.shared.forward_notifications();
        }
    }

    pub(super) fn notifications(&self) -> &Receiver<()> {
        &self.shared.notifications
    }

    pub(super) fn check_timing(&self) -> Result<()> {
        self.shared.check_timing()
    }
    pub(super) fn close(&mut self) -> f64 {
        if self.final_heard_frames.is_none() {
            self.shared.paused.store(true, Ordering::Release);
            self.stream.take();
            #[cfg(test)]
            if let Some(on_close) = self.on_close.take() {
                on_close();
            }
            self.final_heard_frames = Some(self.shared.pcm.heard_frame.load(Ordering::Acquire));
            self.final_diagnostics = Some(self.diagnostics());
        }
        self.heard()
    }

    pub(super) fn submitted(&self) -> u64 {
        self.submitted
    }

    #[cfg(test)]
    pub(super) fn pcm_control_for_test(&self) -> &Arc<PcmControl> {
        &self.shared.pcm
    }

    #[cfg(test)]
    pub(super) fn on_close_for_test(&mut self, on_close: impl FnOnce() + Send + 'static) {
        self.on_close = Some(Box::new(on_close));
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
    diagnostics: CallbackDiagnostics,
    // PipeWire owns this mapping and invalidates it through io_changed before
    // release. Only its owner loop reads it, and only inside process callbacks.
    graph_position: Option<usize>,
}
impl OutputCallback {
    // Fixed storage bounds callback memory independently of device buffer size.
    const BLOCK_FRAMES: usize = 256;

    fn observe_graph_clock(&mut self) {
        let (rate, quantum) = match self.graph_position {
            Some(position) => {
                // SAFETY: io_changed validates the mapped area and clears it
                // before release. PipeWire updates the driver clock before
                // process; this stream has no separate RT_PROCESS thread.
                let clock = unsafe { &(*(position as *const spa::sys::spa_io_position)).clock };
                graph_clock(clock)
            }
            None => (0, 0),
        };
        self.diagnostics.graph_rate = rate;
        self.diagnostics.graph_quantum = quantum;
    }

    fn render_native(&mut self, data: &mut [f32], timestamp: OutputTimestamp, routed: bool) {
        if routed {
            self.render(data, timestamp);
        } else {
            // Keep unconfirmed PipeWire routes silent without consuming PCM.
            data.fill(0.0);
            self.diagnostics.begin(timestamp.callback.0);
            if !self.shared.paused.load(Ordering::Acquire) && !self.diagnostics.started {
                self.diagnostics.record_supply(
                    data.len() / self.channels,
                    0,
                    self.consumer.occupied_len() / self.source_channels,
                    self.shared.input_end.load(Ordering::Acquire),
                );
            }
            self.shared.diagnostics.publish(&self.diagnostics);
        }
    }

    fn render(&mut self, data: &mut [f32], timestamp: OutputTimestamp) {
        self.diagnostics.begin(timestamp.callback.0);
        self.clock.advance(timestamp, self.rate, &self.shared);
        let paused = self.shared.paused.load(Ordering::Acquire);
        if paused
            && self.clock.pending.is_empty()
            && !self.shared.pause_settled.swap(true, Ordering::AcqRel)
        {
            self.shared
                .worker_wake_pending
                .store(true, Ordering::Release);
        }
        let gain = f32::from_bits(self.shared.gain.load(Ordering::Relaxed));
        let source_channels = self.source_channels;
        // Acquire the final watermark before ring availability: observing EOF
        // must also observe every preceding producer publication. Re-read it
        // below to catch finish_input racing this callback's short prefix.
        let _end = self.shared.input_end.load(Ordering::Acquire);
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
        let mut peak = 0.0_f32;
        let mut overrange_frames = 0;
        for block in data[..count * self.channels].chunks_mut(Self::BLOCK_FRAMES * self.channels) {
            // Publish ring consumption once per block, including wrapped rings.
            // Scratch is fixed before stream start; every chunk contains whole frames.
            let samples = block.len() / self.channels * source_channels;
            let scratch = &mut self.scratch[..samples];
            let consumed = self.consumer.pop_slice(scratch);
            debug_assert_eq!(consumed, samples);
            // ReplayGain may boost finite PCM beyond f32's representable range.
            // Preserve ordinary overs; only saturate floating-point overflow.
            for (output, frame) in block
                .chunks_exact_mut(self.channels)
                .zip(scratch.chunks_exact(source_channels))
            {
                let mut frame_peak = 0.0_f32;
                for (sample, source) in output.iter_mut().zip(&self.mapping) {
                    *sample = match source {
                        Some(source) => (frame[*source] * gain).clamp(-f32::MAX, f32::MAX),
                        None => 0.0,
                    };
                    frame_peak = frame_peak.max(sample.abs());
                }
                peak = peak.max(frame_peak);
                overrange_frames += u64::from(frame_peak > 1.0);
            }
        }
        if count > 0
            && self.consumer.occupied_len() <= self.consumer.capacity().get() / 4
            && self.shared.refill_armed.swap(false, Ordering::AcqRel)
        {
            self.shared
                .worker_wake_pending
                .store(true, Ordering::Release);
        }
        if let Some(start) = start {
            self.clock.pending.push_back(ScheduledFrames {
                start,
                frames: count as u64,
                heard: 0,
            });
        }
        if count != 0 {
            self.diagnostics.record_peak(peak, overrange_frames);
        }
        if !paused && self.shared.timing_error.load(Ordering::Acquire) == 0 {
            self.diagnostics.record_supply(
                data.len() / self.channels,
                count,
                self.consumer.occupied_len() / source_channels,
                self.shared.input_end.load(Ordering::Acquire),
            );
        }
        self.shared.diagnostics.publish(&self.diagnostics);
    }
}

const PREFIX: &str = "PipeWire: ";
const TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_REPLACEMENT_GRACE: Duration = Duration::from_millis(250);

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

struct RouteValidation {
    source: u32,
    target: Sink,
    speakers: Vec<&'static str>,
    auto_mix: bool,
    confirmed_once: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RouteLoss {
    TargetGone,
    TargetChanged,
    Links,
    Disconnected,
}

impl RouteLoss {
    fn error(self, target: &Sink) -> anyhow::Error {
        match self {
            Self::TargetGone | Self::TargetChanged => anyhow::anyhow!(
                "PipeWire target {} disappeared or changed its speaker layout; refusing to reroute",
                target.name
            ),
            Self::Links => anyhow::anyhow!(
                "PipeWire output links changed or disappeared; refusing an invalid route"
            ),
            Self::Disconnected => anyhow::anyhow!("PipeWire output disconnected"),
        }
    }
}

#[derive(Default)]
struct Inventory {
    sinks: BTreeMap<u32, Sink>,
    default: Option<String>,
    default_metadata: Option<u32>,
    ports: BTreeMap<u32, String>,
    links: BTreeMap<u32, Route>,
    // Registry callbacks revalidate before a process callback can consume PCM.
    route_validation: Option<RouteValidation>,
    route_confirmed: Rc<Cell<bool>>,
}

impl Inventory {
    fn sink(&self, name: &str) -> Option<&Sink> {
        self.sinks.values().find(|sink| sink.name == name)
    }

    fn changed(&mut self) {
        self.validate_route();
    }

    fn validate_route(&mut self) {
        let confirmed = self.route_validation.as_ref().is_some_and(|route| {
            self.sinks.get(&route.target.id) == Some(&route.target)
                && route_matches(
                    self,
                    route.source,
                    route.target.id,
                    &route.speakers,
                    route.auto_mix,
                )
        });
        if confirmed {
            self.route_validation.as_mut().unwrap().confirmed_once = true;
        }
        self.route_confirmed.set(confirmed);
    }

    fn retire_route(&mut self) {
        self.route_validation = None;
        self.route_confirmed.set(false);
    }

    fn route_loss(&self, target: &Sink, disconnected: bool) -> Option<RouteLoss> {
        match self.sinks.get(&target.id) {
            Some(sink) if sink == target => {}
            Some(sink) if sink.serial == target.serial => return Some(RouteLoss::TargetChanged),
            _ => return Some(RouteLoss::TargetGone),
        }
        // Missing links are normal during initial negotiation. Once the
        // registry confirmed this route, losing it is significant even if
        // the stream has not yet reached readiness.
        if !self.route_confirmed.get()
            && self
                .route_validation
                .as_ref()
                .is_some_and(|route| route.confirmed_once)
        {
            Some(RouteLoss::Links)
        } else if disconnected {
            Some(RouteLoss::Disconnected)
        } else {
            None
        }
    }

    fn has_replacement_default(&self, target: &Sink) -> bool {
        self.default
            .as_deref()
            .filter(|name| *name != target.name || self.sinks.get(&target.id) != Some(target))
            .and_then(|name| self.sink(name))
            .is_some_and(|sink| sink.serial != target.serial)
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
                        {
                            let mut state = state.borrow_mut();
                            state.default_metadata = Some(id);
                            state.default = None;
                        }
                        let state = state.clone();
                        let listener = metadata
                            .add_listener_local()
                            .property(move |subject, key, _, value| {
                                let mut state = state.borrow_mut();
                                if state.default_metadata != Some(id) || subject != 0 {
                                    return 0;
                                }
                                if key.is_none() {
                                    state.default = None;
                                } else if key == Some("default.audio.sink") {
                                    state.default = value
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
            .sink(name)
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
    ) -> Result<(Self, StreamFormat)> {
        let name = name.map(str::to_owned);
        let source_layout = source_layout.to_vec();
        let control = Arc::new(NativeControl {
            metadata: Mutex::new(Some(metadata)),
            ..NativeControl::default()
        });
        let owner_control = control.clone();
        let (wake, receiver) = pw::channel::channel();
        let (ready, receive) = crossbeam_channel::bounded(1);
        let shared = Arc::clone(&callback.shared);
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
                    let _ = shared.wakeup.try_send(());
                }
            })?;
        let stream = Self {
            control,
            wake,
            thread: Some(thread),
        };
        let format = receive
            .recv_timeout(TIMEOUT * 4)
            .context("Timed out opening PipeWire output")?
            .map_err(anyhow::Error::msg)?;
        Ok((stream, format))
    }
}

enum StreamExit {
    Closed,
    Retarget,
}

#[derive(Debug, PartialEq, Eq)]
enum RouteAction {
    Keep,
    AwaitDefault,
    Retire,
}

struct ReplacementWait {
    loss: RouteLoss,
    deadline: Instant,
}

impl ReplacementWait {
    fn on_route(
        wait: &mut Option<Self>,
        loss: Option<RouteLoss>,
        default_managed: bool,
        replacement: bool,
        opening_deadline: Option<Instant>,
        now: impl FnOnce() -> Instant,
    ) -> std::result::Result<RouteAction, RouteLoss> {
        if let Some(loss) = loss
            && (!default_managed || loss == RouteLoss::TargetChanged)
        {
            return Err(loss);
        }
        if let Some(wait) = wait {
            return if now() >= wait.deadline {
                Err(wait.loss)
            } else if replacement {
                Ok(RouteAction::Retire)
            } else {
                Ok(RouteAction::AwaitDefault)
            };
        }
        let Some(loss) = loss else {
            return Ok(RouteAction::Keep);
        };
        if replacement {
            return Ok(RouteAction::Retire);
        }
        let now = now();
        let deadline = now + DEFAULT_REPLACEMENT_GRACE;
        let deadline = opening_deadline.map_or(deadline, |opening| deadline.min(opening));
        if now >= deadline {
            return Err(loss);
        }
        *wait = Some(Self { loss, deadline });
        Ok(RouteAction::AwaitDefault)
    }
}

struct StreamOwner<'a> {
    control: &'a NativeControl,
    control_woken: &'a Cell<bool>,
    ready: &'a crossbeam_channel::Sender<std::result::Result<StreamFormat, String>>,
    initial_metadata: Option<Arc<PlaybackMetadata>>,
}

fn run(
    name: Option<&str>,
    source_layout: &[Channel],
    auto_mix: bool,
    mut callback: OutputCallback,
    control: &Arc<NativeControl>,
    ready: &crossbeam_channel::Sender<std::result::Result<StreamFormat, String>>,
    wake: pw::channel::Receiver<()>,
) -> Result<()> {
    let notifications = OwnerNotifications(Arc::clone(&callback.shared));
    let connection = Connection::new()?;
    let wake_control = control.clone();
    let control_woken = Rc::new(Cell::new(false));
    let received_control = control_woken.clone();
    let _wake = wake.attach(connection.mainloop.loop_(), move |_| {
        wake_control.wake_pending.store(false, Ordering::Release);
        received_control.set(true);
    });
    let initial_metadata = control.metadata.lock().take();
    let mut owner = StreamOwner {
        control,
        control_woken: &control_woken,
        ready,
        initial_metadata,
    };
    // One extra attempt on the same inventory. No PCM has entered the ring
    // before readiness, so there is no playback position to hand off yet.
    for _ in 0..2 {
        if control.stop.load(Ordering::Acquire) {
            return Ok(());
        }
        match run_stream(
            &connection,
            name,
            source_layout,
            auto_mix,
            &mut callback,
            &mut owner,
            &notifications.0,
        )? {
            StreamExit::Closed => return Ok(()),
            StreamExit::Retarget => {
                callback.graph_position = None;
                callback.clock.last_callback = None;
                callback.diagnostics = CallbackDiagnostics::default();
                callback.shared.diagnostics.publish(&callback.diagnostics);
            }
        }
    }
    bail!("PipeWire default audio sink changed again while opening output")
}

fn pin_to_target(properties: &mut pw::properties::Properties, target: &Sink, follow_default: bool) {
    properties.insert("target.object", target.serial.as_str());
    properties.insert("node.dont-reconnect", "true");
    properties.insert("node.dont-fallback", "true");
    if follow_default {
        // WirePlumber otherwise errors and destroys a strict stream when its
        // target vanishes, possibly before the replacement default arrives.
        // Keep the pinning policy; the owner bounds the wait and rebuilds.
        properties.insert("node.linger", "true");
    }
}

fn checked_stream_state(
    core_error: Option<String>,
    format_rejected: bool,
    state: impl FnOnce() -> pw::stream::StreamState,
) -> Result<pw::stream::StreamState> {
    if let Some(error) = core_error {
        bail!("PipeWire output: {error}");
    }
    if format_rejected {
        bail!("PipeWire changed the requested sample format, rate or speaker positions");
    }
    match state() {
        pw::stream::StreamState::Error(error) => bail!("PipeWire output: {error}"),
        state => Ok(state),
    }
}

fn run_stream(
    connection: &Connection,
    name: Option<&str>,
    source_layout: &[Channel],
    auto_mix: bool,
    callback: &mut OutputCallback,
    owner: &mut StreamOwner<'_>,
    shared: &OutputShared,
) -> Result<StreamExit> {
    let control = owner.control;
    let target = connection.target(name)?;
    let follow_default = name.is_none();
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
    let names: Vec<_> = stream_layout
        .iter()
        .map(|channel| if mono { "MONO" } else { channel.short_name() })
        .collect();
    let positions: Arc<str> = Arc::from(names.join(" "));
    let mut properties = properties! {
        "media.type" => "Audio", "media.category" => "Playback", "media.role" => "Music",
        "application.name" => "Rivu", "application.id" => "rivu",
        "application.icon-name" => "rivu", "node.name" => "rivu-native-output",
        "stream.dont-remix" => (!auto_mix).to_string(),
        "channelmix.disable" => (!auto_mix).to_string(), "audio.channels" => stream_layout.len().to_string(),
        "audio.position" => format!("[ {positions} ]"), "audio.rate" => rate.to_string(),
        "node.rate" => format!("1/{rate}"),
        // When the graph rate differs, PipeWire's default quality 4 attenuates
        // 20 kHz by 3 dB at 44.1->48 kHz. Quality 10 stays flat to 20 kHz with
        // ~2 ms filter latency; client.conf stream.properties still override it.
        "resample.quality" => "10",
    };
    pin_to_target(&mut properties, &target, follow_default);
    if auto_mix {
        properties.insert("channelmix.normalize", "true");
    }
    let stream = pw::stream::StreamBox::new(&connection.core, "Rivu playback", properties)?;
    let metadata = owner
        .initial_metadata
        .as_deref()
        .context("Missing initial PipeWire track metadata")?;
    update_stream_metadata(&stream, metadata)?;
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(rate);
    info.set_channels(stream_layout.len() as u32);
    let mut spa_positions = [0; spa::param::audio::MAX_CHANNELS];
    for (position, speaker) in spa_positions.iter_mut().zip(stream_layout) {
        *position = spa_position(*speaker);
    }
    if mono {
        spa_positions[0] = spa::sys::SPA_AUDIO_CHANNEL_MONO;
    }
    info.set_position(spa_positions);
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
        .io_changed(|_, callback, id, area, size| {
            if id == spa::sys::SPA_IO_Position {
                callback.graph_position = (!area.is_null()
                    && size as usize >= size_of::<spa::sys::spa_io_position>()
                    && (area as usize).is_multiple_of(align_of::<spa::sys::spa_io_position>()))
                .then_some(area as usize);
                // A replaced/removed mapping cannot certify the current graph
                // clock. Publish unknown until the next process observes it.
                callback.diagnostics.graph_rate = 0;
                callback.diagnostics.graph_quantum = 0;
                callback.shared.diagnostics.publish(&callback.diagnostics);
            }
        })
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
    // Without RT_PROCESS, process dispatches on this mainloop. Notification
    // intents are forwarded after iterate, with no additional self-wakeup.
    stream.connect(
        spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut [spa::pod::Pod::from_bytes(&bytes).context("Invalid PipeWire format pod")?],
    )?;
    let deadline = Instant::now() + TIMEOUT;
    let mut started = false;
    let mut active = true;
    {
        let mut inventory = connection.inventory.borrow_mut();
        inventory.route_validation = Some(RouteValidation {
            source: stream.node_id(),
            target: target.clone(),
            speakers: names,
            auto_mix,
            confirmed_once: false,
        });
        inventory.validate_route();
    }
    let failure = Rc::new(RefCell::new(None));
    let failed = failure.clone();
    let _core_listener = connection
        .core
        .add_listener_local()
        .error(move |_, _, _, message| {
            *failed.borrow_mut() = Some(message.to_owned());
        })
        .register();
    let mut replacement_wait: Option<ReplacementWait> = None;
    let exit = loop {
        if control.stop.load(Ordering::Acquire) {
            break StreamExit::Closed;
        }
        let wait_until = replacement_wait
            .as_ref()
            .map(|wait| wait.deadline)
            .or((!started).then_some(deadline));
        connection.mainloop.loop_().iterate(match wait_until {
            Some(end) => pw::loop_::Timeout::Finite(end.saturating_duration_since(Instant::now())),
            None => pw::loop_::Timeout::Infinite,
        });
        shared.forward_notifications();
        if control.stop.load(Ordering::Acquire) {
            break StreamExit::Closed;
        }
        if owner.control_woken.replace(false) {
            let metadata = control.metadata.lock().take();
            if let Some(metadata) = metadata {
                update_stream_metadata(&stream, &metadata)?;
            }
        }
        // Backend failures stay fatal even while awaiting default metadata.
        let core_error = failure.borrow_mut().take();
        let stream_state = checked_stream_state(core_error, format_error.get(), || stream.state())?;
        shared.check_timing()?;
        let (replacement, loss) = {
            let mut inventory = connection.inventory.borrow_mut();
            if let Some(route) = inventory.route_validation.as_mut() {
                let source = stream.node_id();
                if route.source != source {
                    route.source = source;
                    inventory.validate_route();
                }
            }
            (
                follow_default && inventory.has_replacement_default(&target),
                inventory.route_loss(
                    &target,
                    stream_state == pw::stream::StreamState::Unconnected,
                ),
            )
        };
        let was_waiting = replacement_wait.is_some();
        let action = ReplacementWait::on_route(
            &mut replacement_wait,
            loss,
            follow_default,
            replacement,
            (!started).then_some(deadline),
            Instant::now,
        )
        .map_err(|loss| loss.error(&target))?;
        if action == RouteAction::AwaitDefault {
            if !was_waiting {
                // Registry removals can precede replacement default metadata.
                // Keep dispatching events, but never certify this route again.
                connection.inventory.borrow_mut().retire_route();
                shared.publish_default_target_changed(false);
            }
            continue;
        }
        if started {
            shared.publish_default_target_changed(replacement);
            if action == RouteAction::Retire {
                break StreamExit::Closed;
            }
        } else if replacement {
            break StreamExit::Retarget;
        }
        match stream_state {
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
                    owner.initial_metadata = None;
                    let _ = owner.ready.send(Ok(StreamFormat {
                        rate,
                        channels: stream_layout.len(),
                        layout: Arc::clone(&positions),
                        sink_name: Some(Arc::from(target.name.as_str())),
                        remix_allowed: auto_mix,
                        resample_quality: stream
                            .properties()
                            .get("resample.quality")
                            .and_then(|value| value.parse().ok()),
                    }));
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
    };
    connection.inventory.borrow_mut().retire_route();
    stream.disconnect()?;
    Ok(exit)
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
        callback.observe_graph_clock();
        callback.render_native(samples, timestamp, routed);
        (*data.chunk).stride = stride as i32;
        (*data.chunk).size = (frames * stride) as u32;
        (*buffer).size = frames as u64;
        true
    }
}

fn monotonic_now() -> Option<u64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: CLOCK_MONOTONIC writes exactly one initialized timespec. This
    // non-callback query shares PipeWire's clock domain for event age reporting.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } != 0 {
        return None;
    }
    u64::try_from(time.tv_sec)
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(u64::try_from(time.tv_nsec).ok()?)
}

fn graph_clock(clock: &spa::sys::spa_io_clock) -> (u32, u64) {
    if clock.flags & spa::sys::SPA_IO_CLOCK_FLAG_NO_RATE != 0
        || clock.rate.num == 0
        || clock.rate.denom == 0
        || !clock.rate.denom.is_multiple_of(clock.rate.num)
    {
        return (0, 0);
    }
    // The driver's cycle duration is graph ticks, not stream.requested (which
    // the resampler expresses at the source rate), and never a hardware format.
    (clock.rate.denom / clock.rate.num, clock.duration)
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
                .find(|channel| channel.short_name() == name)
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
        callback.shared.forward_notifications();
    })
}

#[cfg(test)]
mod pcm_tests {
    use super::*;

    #[test]
    fn diagnostic_startup_and_pause_are_not_underruns() {
        let (mut output, mut callback, _pcm) = test_output(1000, true);
        let mut data = [9.0; 8];
        callback.render(&mut data, timestamp(0, 20));
        let initial = output.diagnostics_at(Some(0));
        assert_eq!(initial.callback_count, 1);
        assert_eq!(initial.startup_wait_frames, 0);
        assert_eq!(initial.sample_peak, None);
        assert_eq!(initial.peak_hold, None);
        output.pause(false);
        callback.render(&mut data, timestamp(10, 30));
        callback.render(&mut data, timestamp(10_000, 10_020));
        let waiting = output.diagnostics_at(Some(10_000_000_000));
        assert!(!waiting.started);
        assert_eq!(waiting.startup_wait_callbacks, 2);
        assert_eq!(waiting.startup_wait_frames, 8);
        assert_eq!(waiting.underrun_events, 0);
        assert_eq!(waiting.pending_missing_frames, 0);
        assert_eq!(waiting.ring_low_watermark, None);

        output.push(&[0.25; 8]);
        callback.render(&mut data, timestamp(10_001, 10_021));
        // Once PCM starts, even the very next short callback is diagnosed.
        callback.render(&mut data, timestamp(10_002, 10_022));
        let pending = output.diagnostics_at(Some(10_002_000_000));
        assert!(pending.started);
        assert_eq!(pending.pending_callbacks, 1);
        assert_eq!(pending.pending_missing_frames, 4);
        output.pause(true);
        callback.render(&mut data, timestamp(20_000, 20_020));
        let paused = output.diagnostics_at(Some(20_000_000_000));
        assert_eq!(paused.pending_callbacks, pending.pending_callbacks);
        assert_eq!(
            paused.pending_missing_frames,
            pending.pending_missing_frames
        );
        assert_eq!(paused.pending_since_ns, pending.pending_since_ns);
        assert_eq!(paused.startup_wait_frames, 8);
        assert_eq!(paused.sample_peak, None);
        output.pause(false);
        output.push(&[0.5; 8]);
        callback.render(&mut data, timestamp(20_001, 20_021));
        let resumed = output.diagnostics_at(Some(20_001_000_000));
        assert_eq!(resumed.underrun_events, 1);
        assert_eq!(resumed.underrun_callbacks, 1);
        assert_eq!(resumed.missing_frames, 4);
        assert_eq!(resumed.last_underrun_at_ns, Some(10_002_000_000));
        assert_eq!(resumed.pending_missing_frames, 0);
    }

    #[test]
    fn consecutive_short_callbacks_form_one_event_until_a_full_callback() {
        let (mut output, mut callback, _pcm) = test_output(1000, false);
        let mut data = [0.0; 8];
        output.push(&[0.25; 2]);
        callback.render(&mut data, timestamp(0, 20));
        callback.render(&mut data, timestamp(4, 24));
        output.push(&[0.25; 2]);
        callback.render(&mut data, timestamp(8, 28));
        let ongoing = output.diagnostics_at(Some(8_000_000));
        assert_eq!(ongoing.underrun_events, 1);
        assert_eq!(ongoing.underrun_callbacks, 2);
        assert_eq!(ongoing.missing_frames, 7);
        assert_eq!(ongoing.pending_callbacks, 1);
        assert_eq!(ongoing.pending_missing_frames, 3);
        output.push(&[0.25; 8]);
        callback.render(&mut data, timestamp(12, 32));
        let recovered = output.diagnostics_at(Some(12_000_000));
        assert_eq!(recovered.underrun_events, 1);
        assert_eq!(recovered.underrun_callbacks, 3);
        assert_eq!(recovered.missing_frames, 10);
        assert_eq!(recovered.pending_callbacks, 0);
        assert_eq!(recovered.last_underrun_at_ns, Some(8_000_000));
        callback.render(&mut data, timestamp(16, 36));
        output.push(&[0.25; 8]);
        callback.render(&mut data, timestamp(20, 40));
        let second = output.diagnostics_at(Some(20_000_000));
        assert_eq!(second.underrun_events, 2);
        assert_eq!(second.underrun_callbacks, 4);
        assert_eq!(second.missing_frames, 14);
    }

    #[test]
    fn final_watermark_excludes_short_and_empty_eof_tails_in_both_orderings() {
        for finish_before_callback in [false, true] {
            for frames in [0, 2, 4] {
                let (mut output, mut callback, _pcm) = test_output(1000, false);
                output.push(&vec![0.5; frames * 2]);
                if finish_before_callback {
                    output.finish_input();
                }
                callback.render(&mut [0.0; 8], timestamp(0, 20));
                callback.render(&mut [0.0; 8], timestamp(4, 24));
                if !finish_before_callback {
                    output.pause(true);
                    // No callback is needed to reconcile the just-published
                    // watermark, including a final push consumed before EOF.
                    output.finish_input();
                }
                output.finish_input();
                let final_state = output.diagnostics_at(Some(4_000_000));
                assert!(final_state.input_finished);
                assert_eq!(final_state.underrun_events, 0);
                assert_eq!(final_state.underrun_callbacks, 0);
                assert_eq!(final_state.missing_frames, 0);
                assert_eq!(final_state.pending_missing_frames, 0);
                assert_eq!(final_state.pending_callbacks, 0);
                assert_eq!(final_state.pending_since_ns, None);
                assert_eq!(final_state.ring_low_watermark, None);
                output.pause(false);
                callback.render(&mut [0.0; 8], timestamp(24, 44));
                assert!(output.drained());
                assert_eq!(output.diagnostics().missing_frames, 0);
            }
        }
    }

    #[test]
    fn watermark_after_stale_availability_preserves_real_starvation() {
        let mut state = CallbackDiagnostics::default();
        state.begin(0);
        state.record_supply(4, 4, 0, INPUT_OPEN);
        state.begin(4_000_000);
        // Availability was sampled empty; the producer then published two
        // final frames and EOF before this callback classified its zero prefix.
        // EOF at frame six cannot excuse silence inserted at frame four.
        state.record_supply(4, 0, 2, 6);
        assert_eq!(state.underrun_events, 1);
        assert_eq!(state.underrun_callbacks, 1);
        assert_eq!(state.missing_frames, 4);
        assert_eq!(state.pending_missing_frames, 0);
        state.begin(8_000_000);
        state.record_supply(4, 2, 0, 6);
        assert_eq!(state.underrun_events, 1);
        assert_eq!(state.missing_frames, 4);
        assert_eq!(state.pending_missing_frames, 0);
        assert_eq!(state.consumed_frames, 6);
    }

    #[test]
    fn final_publication_after_a_pending_gap_confirms_only_the_nonterminal_gap() {
        let (mut output, mut callback, _pcm) = test_output(1000, false);
        output.push(&[0.5; 8]);
        callback.render(&mut [0.0; 8], timestamp(0, 20));
        callback.render(&mut [0.0; 8], timestamp(4, 24));
        output.push(&[0.5; 4]);
        output.finish_input();
        let published = output.diagnostics_at(Some(4_000_000));
        assert_eq!(published.underrun_events, 1);
        assert_eq!(published.missing_frames, 4);
        assert_eq!(published.pending_missing_frames, 0);
        callback.render(&mut [0.0; 8], timestamp(8, 28));
        let consumed = output.diagnostics_at(Some(8_000_000));
        assert_eq!(consumed.underrun_events, 1);
        assert_eq!(consumed.missing_frames, 4);
        assert_eq!(consumed.pending_missing_frames, 0);
    }

    #[test]
    fn diagnostic_ring_watermark_excludes_startup_pause_and_terminal_zero() {
        let (mut output, mut callback, _pcm) = test_output(1000, false);
        callback.render(&mut [0.0; 8], timestamp(0, 20));
        output.push(&[0.25; 16]);
        callback.render(&mut [0.0; 8], timestamp(4, 24));
        let first = output.diagnostics_at(Some(4_000_000));
        assert_eq!(first.ring_frames, 4);
        assert_eq!(first.ring_capacity_frames, 64);
        assert_eq!(first.ring_low_watermark, Some(4));
        output.pause(true);
        callback.render(&mut [0.0; 8], timestamp(8, 28));
        assert_eq!(output.diagnostics().ring_low_watermark, Some(4));
        output.pause(false);
        callback.render(&mut [0.0; 8], timestamp(12, 32));
        output.finish_input();
        let final_state = output.diagnostics_at(Some(12_000_000));
        assert_eq!(final_state.ring_frames, 0);
        assert_eq!(final_state.ring_low_watermark, Some(4));
    }

    #[test]
    fn diagnostic_peak_is_post_gain_per_frame_and_windowed_without_resetting_hold() {
        let source = [Channel::FrontLeft, Channel::FrontRight];
        let device = [
            Channel::FrontRight,
            Channel::FrontLeft,
            Channel::FrontCenter,
        ];
        let (mut output, mut callback, _pcm) =
            test_output_layout(1000, false, &source, &device, 128);
        output.set_gain(2.0);
        output.push(&[0.75, -0.75, 0.25, -0.25, 0.5, -0.5]);
        callback.render(&mut [0.0; 12], timestamp(0, 20));
        let boosted = output.diagnostics_at(Some(0));
        assert_eq!(boosted.sample_peak, Some(1.5));
        assert_eq!(boosted.peak_hold, Some(1.5));
        assert_eq!(boosted.overrange_frames, 1);
        assert_eq!(boosted.peak_window_start_ns, Some(0));
        assert_eq!(boosted.stream_format, "F32LE");
        assert_eq!(boosted.source_channels, 2);
        assert_eq!(boosted.stream_channels, 3);
        assert_eq!(boosted.stream_layout.as_ref(), "FR FL FC");

        callback.render(&mut [0.0; 12], timestamp(250, 270));
        let empty = output.diagnostics_at(Some(250_000_000));
        assert_eq!(empty.sample_peak, None);
        assert_eq!(empty.peak_hold, Some(1.5));
        output.set_gain(0.0);
        output.push(&[f32::MAX, -f32::MAX]);
        callback.render(&mut [0.0; 3], timestamp(251, 271));
        let muted = output.diagnostics_at(Some(251_000_000));
        assert_eq!(muted.sample_peak, Some(0.0));
        assert_eq!(muted.peak_hold, Some(1.5));
        assert_eq!(muted.overrange_frames, 1);
        assert_eq!(output.diagnostics_at(Some(500_000_000)).sample_peak, None);
        output.set_gain(2.0);
        output.push(&[f32::MAX, -f32::MAX]);
        callback.render(&mut [0.0; 3], timestamp(501, 521));
        let overflow = output.diagnostics_at(Some(501_000_000));
        assert_eq!(overflow.sample_peak, Some(f32::MAX));
        assert_eq!(overflow.peak_hold, Some(f32::MAX));
        assert_eq!(overflow.overrange_frames, 2);
    }

    #[test]
    fn closed_segment_keeps_unconfirmed_waits_and_never_follows_the_next_pcm_epoch() {
        let (mut output, mut callback, pcm) = test_output(1000, false);
        output.push(&[0.5; 4]);
        callback.render(&mut [0.0; 8], timestamp(0, 20));
        output.close();
        let retired = output.diagnostics();
        assert!(retired.closed);
        assert!(!retired.input_finished);
        assert_eq!(retired.pending_missing_frames, 2);
        assert_eq!(retired.underrun_events, 0);
        assert_eq!(retired.peak_hold, Some(0.5));
        pcm.reset(48_000, &[Channel::FrontCenter]);
        callback.render(&mut [0.0; 8], timestamp(50, 70));
        assert_eq!(output.diagnostics(), retired);
        let (next, _, _) = test_output(1000, false);
        assert_ne!(next.diagnostics().segment_id, retired.segment_id);
        assert_eq!(next.diagnostics().callback_count, 0);
    }

    #[test]
    #[should_panic(expected = "PCM submitted after finish_input")]
    fn finished_input_cannot_accept_more_pcm() {
        let (mut output, _callback, _pcm) = test_output(1000, false);
        output.finish_input();
        output.push(&[0.25; 2]);
    }

    #[test]
    fn graph_diagnostics_use_driver_clock_and_leave_hardware_unknown() {
        let (output, mut callback, _pcm) = test_output(44_100, false);
        let mut position: spa::sys::spa_io_position = unsafe { std::mem::zeroed() };
        position.clock.rate.num = 1;
        position.clock.rate.denom = 48_000;
        position.clock.duration = 256;
        callback.graph_position = Some((&raw mut position) as usize);
        callback.observe_graph_clock();
        callback.render(&mut [0.0; 8], timestamp(0, 20));
        let known = output.diagnostics_at(Some(0));
        assert_eq!(known.stream_rate, 44_100);
        assert_eq!(known.graph_rate, Some(48_000));
        assert_eq!(known.graph_quantum, Some(256));
        assert_eq!(known.hardware_rate, None);
        assert_eq!(known.hardware_format, None);
        position.clock.duration = 128;
        callback.observe_graph_clock();
        callback.render(&mut [0.0; 8], timestamp(4, 24));
        assert_eq!(output.diagnostics().graph_quantum, Some(128));
        position.clock.flags = spa::sys::SPA_IO_CLOCK_FLAG_NO_RATE;
        callback.observe_graph_clock();
        callback.render(&mut [0.0; 8], timestamp(8, 28));
        assert_eq!(output.diagnostics().graph_rate, None);
        assert_eq!(output.diagnostics().graph_quantum, None);
        position.clock.flags = 0;
        position.clock.rate.num = 7;
        assert_eq!(graph_clock(&position.clock), (0, 0));
        callback.graph_position = None;
        callback.observe_graph_clock();
        callback.render(&mut [0.0; 8], timestamp(12, 32));
        assert_eq!(output.diagnostics().graph_rate, None);
    }

    #[test]
    fn normal_prefetch_retains_fragmented_native_pcm_until_audible_at_high_rates() {
        use rivu_analysis::{AnalysisEngine, AnalysisFrame, AnalysisSettings, DisplayDemand};
        let layout = [Channel::FrontLeft, Channel::FrontRight];
        for rate in [48_000, 192_000, 384_000] {
            let capacity = output_capacity(rate, layout.len()).unwrap();
            let (mut output, mut callback, pcm) =
                test_output_layout(rate, false, &layout, &layout, capacity);
            pcm.set_enabled(true);
            pcm.set_playback_active(true);
            let mut producer = pcm.audio_producer();
            let window = pcm.window();
            let settings = AnalysisSettings {
                fft_size: 512,
                ..AnalysisSettings::from(&rivu_format::config::Config::default())
            };
            let demand = DisplayDemand {
                enabled: true,
                ..Default::default()
            };
            let mut analysis = AnalysisEngine::new(settings);
            let mut frame = AnalysisFrame::default();
            let mut next = 1;
            let mut offset = 0;
            let mut accepted = 0;
            let mut fragment = 0;
            let now = Instant::now();
            let mut device = vec![0.0; rate as usize / 10 * layout.len()];
            for round in 0..12 {
                while output.vacant_frames() > 0 {
                    // Decoder fragments must not exhaust descriptors before
                    // their admitted output reaches the audible cursor.
                    let frames = [1, 37, 512, 103, 1024][fragment % 5].min(output.vacant_frames());
                    fragment += 1;
                    let native: Vec<_> = (0..frames).flat_map(|_| [0.25, -0.5]).collect();
                    let start = output.submitted;
                    assert_eq!(output.push(&native), frames);
                    producer.push(start, &native);
                }
                assert_eq!(output.refill_delay(false), Duration::from_millis(100));
                callback.render(&mut device, timestamp(round * 100, round * 100 + 20));
                let state = pcm.state();
                analysis.configure(settings, demand, state, 1, &mut frame);
                let previous = accepted;
                while next <= window.sequence() {
                    let block = window
                        .block(next)
                        .expect("prefetched audible PCM was overwritten");
                    if offset == block.len() {
                        if next < window.sequence() {
                            next += 1;
                            offset = 0;
                            continue;
                        }
                        break;
                    }
                    let mut header = block.header();
                    header.start_frame += offset as u64;
                    header.output_frame += offset as u64;
                    header.media_start += offset as f64 * header.media_step;
                    header.gap &= offset == 0;
                    if header.output_frame
                        > state.heard_frame + rivu_protocol::pcm::PCM_BLOCK_FRAMES as u64
                    {
                        break;
                    }
                    assert_eq!(header.start_frame, accepted);
                    assert_eq!(header.output_frame, accepted);
                    assert_eq!(block.layout(), [0, 1]);
                    let count = block.len() - offset;
                    assert!(analysis.ingest(
                        header,
                        (offset..block.len()).map(|index| {
                            let sample = [block.sample(index, 0), block.sample(index, 1)];
                            assert_eq!(sample, [0.25, -0.5]);
                            sample
                        }),
                        &mut frame
                    ));
                    accepted += count as u64;
                    offset = block.len();
                }
                assert!(
                    accepted > previous,
                    "normal refill starved audible analysis at {rate} Hz"
                );
                analysis.request_frame();
                assert!(analysis.publish(&mut frame, now + Duration::from_millis(round * 100)));
                assert_eq!([frame.rms_left, frame.rms_right], [0.25, 0.5]);
            }
        }
    }

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
        assert!(output_capacity(1_000_000_000, 2).is_err());
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
            output.set_gain(crate::volume::gain(level));
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
            output.set_gain(crate::volume::gain(volume));
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
    fn replaygain_boost_preserves_overs_but_never_overflows_finite_pcm() {
        let (mut output, mut callback, _analyzer) = test_output(1000, false);
        output.set_gain(2.0);
        assert_eq!(output.push(&[0.75, -0.75, f32::MAX, -f32::MAX]), 2);
        let mut data = [0.0; 4];
        callback.render(&mut data, timestamp(0, 20));
        assert_eq!(data, [1.5, -1.5, f32::MAX, -f32::MAX]);
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

    fn test_output(rate: u32, paused: bool) -> (Output, OutputCallback, Arc<PcmControl>) {
        let stereo = [Channel::FrontLeft, Channel::FrontRight];
        test_output_layout(rate, paused, &stereo, &stereo, 128)
    }

    pub(super) fn test_output_layout(
        rate: u32,
        paused: bool,
        source: &[Channel],
        device: &[Channel],
        capacity: usize,
    ) -> (Output, OutputCallback, Arc<PcmControl>) {
        let analyzer = Arc::new(PcmControl::default());
        analyzer
            .window()
            .retain_output(capacity / source.len(), rate, source.len())
            .unwrap();
        let (producer, consumer) = HeapRb::new(capacity).split();
        analyzer.reset(rate, source);
        let shared = Arc::new(OutputShared::new(1.0, paused, &analyzer));
        let (_, errors) = bounded(4);
        let output = Output {
            stream: None,
            producer,
            shared: shared.clone(),
            rate,
            source_channels: source.len(),
            errors,
            submitted: 0,
            final_heard_frames: None,
            segment_id: NEXT_OUTPUT_SEGMENT.fetch_add(1, Ordering::Relaxed),
            format: StreamFormat {
                rate,
                channels: device.len(),
                layout: Arc::from(
                    device
                        .iter()
                        .map(|channel| channel.short_name())
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
                sink_name: None,
                remix_allowed: false,
                resample_quality: None,
            },
            final_diagnostics: None,
            on_close: None,
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
            diagnostics: CallbackDiagnostics::default(),
            graph_position: None,
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
    fn pcm_cursor_uses_audible_frames_and_notifies_by_block_or_tail() {
        let stereo = [Channel::FrontLeft, Channel::FrontRight];
        let block = rivu_protocol::pcm::PCM_BLOCK_FRAMES as u64;
        let frames = block * 2 + 1;
        let samples = vec![0.25; frames as usize * stereo.len()];
        let (mut output, mut callback, pcm) =
            test_output_layout(1000, false, &stereo, &stereo, samples.len());
        pcm.set_playback_active(true);
        pcm.set_enabled(true);
        let updates = pcm.subscribe();
        while updates.try_recv().is_ok() {}
        assert_eq!(output.push(&samples), frames as usize);
        callback.render(&mut vec![0.0; samples.len()], timestamp(0, 20));
        output.shared.forward_notifications();
        assert_eq!(pcm.state().heard_frame, 0);
        assert!(updates.try_recv().is_err());
        callback.render(&mut [], timestamp(20 + block - 1, 40 + block - 1));
        output.shared.forward_notifications();
        assert_eq!(pcm.state().heard_frame, block - 1);
        assert!(updates.try_recv().is_err());
        callback.render(&mut [], timestamp(20 + block, 40 + block));
        assert_eq!(pcm.state().heard_frame, block);
        assert!(updates.try_recv().is_err());
        output.shared.forward_notifications();
        updates.try_recv().unwrap();
        callback.render(&mut [], timestamp(20 + block * 2, 40 + block * 2));
        assert_eq!(pcm.state().heard_frame, block * 2);
        output.shared.forward_notifications();
        updates.try_recv().unwrap();
        callback.render(&mut [], timestamp(20 + frames, 40 + frames));
        assert_eq!(pcm.state().heard_frame, frames);
        assert!(output.drained());
        output.shared.forward_notifications();
        updates.try_recv().unwrap();
        callback.render(&mut [], timestamp(21 + frames, 41 + frames));
        output.shared.forward_notifications();
        assert!(updates.try_recv().is_err());

        output.pause(true);
        pcm.set_playback_active(false);
        pcm.set_enabled(false);
        pcm.set_enabled(true);
        pcm.set_playback_active(true);
        assert_eq!(pcm.state().heard_frame, frames);
        pcm.reset(1000, &stereo);
        assert_eq!(pcm.state().heard_frame, 0);
    }

    #[test]
    fn disabled_capture_keeps_audible_cursor_without_notifications() {
        let (mut output, mut callback, pcm) = test_output(1000, false);
        let updates = pcm.subscribe();
        while updates.try_recv().is_ok() {}
        output.push(&[0.25; 16]);
        callback.render(&mut [0.0; 16], timestamp(0, 20));
        callback.render(&mut [], timestamp(28, 48));
        assert_eq!(pcm.state().heard_frame, 8);
        assert!(output.drained());
        output.shared.forward_notifications();
        assert!(updates.try_recv().is_err());
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
    fn retired_output_keeps_final_count_when_shared_pcm_epoch_is_reused() {
        for frames in [2, 8] {
            let (mut output, mut callback, pcm) = test_output(1000, false);
            output.push([[0.5, -0.5]; 8].as_flattened());
            callback.render(&mut [0.0; 16], timestamp(0, 20));
            output.on_close_for_test(move || {
                callback.render(&mut [], timestamp(20 + frames, 40 + frames));
            });
            let heard = frames as f64 / 1000.0;
            assert_eq!(output.close(), heard);
            let drained = output.drained();
            let refill_delay = output.refill_delay(true);
            assert_eq!(drained, frames == 8);
            pcm.reset(48_000, &[Channel::FrontCenter]);
            assert_eq!(pcm.state().heard_frame, 0);
            for next_epoch_frames in [0, 1, 48_000] {
                pcm.heard_frame.store(next_epoch_frames, Ordering::Release);
                assert_eq!(output.close(), heard);
                assert_eq!(output.heard(), heard);
                assert_eq!(output.position(), heard);
                assert_eq!(output.drained(), drained);
                assert_eq!(output.refill_delay(true), refill_delay);
            }
        }
    }

    #[test]
    fn paused_tail_notifies_once_after_audible_frames_then_sleeps() {
        let (mut output, mut callback, _analyzer) = test_output(1000, false);
        output.push([[0.5, -0.5]; 4].as_flattened());
        let mut data = [0.0_f32; 16];
        callback.render(&mut data, timestamp(0, 20));
        output.pause(true);
        callback.render(&mut data, timestamp(21, 40));
        output.shared.forward_notifications();
        assert_eq!(output.heard(), 0.001);
        assert!(output.notifications().try_recv().is_err());
        callback.render(&mut data, timestamp(24, 44));
        assert_eq!(output.heard(), 0.004);
        assert!(output.notifications().try_recv().is_err());
        output.shared.forward_notifications();
        assert_eq!(output.notifications().try_recv(), Ok(()));
        callback.render(&mut data, timestamp(50, 70));
        output.shared.forward_notifications();
        assert!(output.notifications().try_recv().is_err());

        output.pause(false);
        output.push([[0.25, -0.25]; 2].as_flattened());
        callback.render(&mut data, timestamp(60, 80));
        output.pause(true);
        callback.render(&mut data, timestamp(82, 102));
        assert_eq!(output.heard(), 0.006);
        output.shared.forward_notifications();
        assert_eq!(output.notifications().try_recv(), Ok(()));
        callback.render(&mut data, timestamp(81, 101));
        assert!(output.check_timing().is_err());
        output.shared.forward_notifications();
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
                output.shared.forward_notifications();
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
    fn owner_exit_forwards_coalesced_notifications_and_final_cursor() {
        for fail in [false, true] {
            let (mut output, mut callback, pcm) = test_output(1000, false);
            pcm.set_playback_active(true);
            pcm.set_enabled(true);
            let updates = pcm.subscribe();
            while updates.try_recv().is_ok() {}
            output.push(&[0.25; 128]);

            let result = {
                let _notifications = OwnerNotifications(Arc::clone(&output.shared));
                // Refill, final cursor, pause settlement and a possible error
                // all arrive before the owner gets another forwarding turn.
                callback.render(&mut [0.0; 128], timestamp(0, 20));
                callback.render(&mut [], timestamp(84, 104));
                output.pause(true);
                callback.render(&mut [], timestamp(85, 105));
                if fail {
                    callback.render(&mut [], timestamp(84, 104));
                }
                assert!(output.notifications().try_recv().is_err());
                assert!(updates.try_recv().is_err());
                drop(callback);
                output.check_timing()
            };

            assert_eq!(result.is_err(), fail);
            assert_eq!(output.heard(), 0.064);
            assert!(output.shared.pause_settled.load(Ordering::Acquire));
            assert_eq!(output.notifications().try_recv(), Ok(()));
            assert_eq!(updates.try_recv(), Ok(()));
            assert_eq!(pcm.state().heard_frame, 64);
            output.shared.forward_notifications();
            assert!(output.notifications().try_recv().is_err());
            assert!(updates.try_recv().is_err());
        }
    }

    #[test]
    fn low_water_notifies_once_until_refill_and_empty_tail_is_bounded() {
        let (mut output, mut callback, _analyzer) = test_output(1000, false);
        output.push(&[0.25; 128]);
        callback.render(&mut [0.0_f32; 96], timestamp(0, 20));
        assert!(output.notifications().try_recv().is_err());
        output.shared.forward_notifications();
        assert_eq!(output.notifications().try_recv(), Ok(()));
        assert_eq!(output.refill_delay(false), Duration::from_millis(8));
        callback.render(&mut [0.0_f32; 32], timestamp(1, 21));
        output.shared.forward_notifications();
        assert!(output.notifications().try_recv().is_err());
        assert_eq!(output.refill_delay(false), Duration::from_millis(1));
        assert!(output.refill_delay(true) >= Duration::from_millis(10));
        output.push(&[0.5; 64]);
        callback.render(&mut [0.0_f32; 34], timestamp(2, 22));
        output.shared.forward_notifications();
        assert_eq!(output.notifications().try_recv(), Ok(()));
        callback.render(&mut [0.0_f32; 100], timestamp(3, 23));
        output.shared.forward_notifications();
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
    fn only_default_managed_streams_linger_without_relaxing_the_pin() {
        let target = Sink {
            id: 2,
            serial: "42".into(),
            name: "selected".into(),
            positions: "FL FR".into(),
        };
        for (follow_default, linger) in [(false, None), (true, Some("true"))] {
            let mut props = pw::properties::PropertiesBox::new();
            pin_to_target(&mut props, &target, follow_default);
            assert_eq!(props.get("target.object"), Some("42"));
            assert_eq!(props.get("node.dont-reconnect"), Some("true"));
            assert_eq!(props.get("node.dont-fallback"), Some("true"));
            assert_eq!(props.get("node.linger"), linger);
        }
    }

    #[test]
    fn backend_errors_are_not_classified_as_recoverable_route_loss() {
        use pw::stream::StreamState;

        // Even a missing-object error from another operation is fatal. The
        // expected policy error is prevented at the source, never filtered.
        for message in ["no such object", "defined target not found", "broken pipe"] {
            let error = checked_stream_state(Some(message.into()), false, || StreamState::Paused)
                .unwrap_err();
            assert_eq!(error.to_string(), format!("PipeWire output: {message}"));
        }
        let error = checked_stream_state(None, false, || {
            StreamState::Error("link activation failed".into())
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "PipeWire output: link activation failed");
        let error = checked_stream_state(None, true, || StreamState::Paused).unwrap_err();
        assert_eq!(
            error.to_string(),
            "PipeWire changed the requested sample format, rate or speaker positions"
        );
    }

    #[test]
    fn default_route_loss_waits_for_a_registered_replacement_without_resetting() {
        let target = Sink {
            id: 2,
            serial: "2".into(),
            name: "old".into(),
            positions: "FL FR".into(),
        };
        let replacement = Sink {
            id: 3,
            serial: "3".into(),
            name: "new".into(),
            positions: target.positions.clone(),
        };
        let now = Instant::now();
        for (opening_deadline, first_loss) in [
            (None, RouteLoss::Links),
            (Some(now + TIMEOUT), RouteLoss::TargetGone),
        ] {
            let mut graph = Inventory::default();
            graph.sinks.insert(target.id, target.clone());
            graph.default = Some(target.name.clone());
            graph.route_validation = Some(RouteValidation {
                source: 1,
                target: target.clone(),
                speakers: vec!["FL", "FR"],
                auto_mix: true,
                confirmed_once: false,
            });
            let mut wait = None;
            let decide = |wait: &mut Option<ReplacementWait>, graph: &Inventory, at| {
                ReplacementWait::on_route(
                    wait,
                    graph.route_loss(&target, false),
                    true,
                    graph.has_replacement_default(&target),
                    opening_deadline,
                    || at,
                )
            };
            assert_eq!(decide(&mut wait, &graph, now), Ok(RouteAction::Keep));
            let link = Route {
                output_node: 1,
                output_port: 10,
                input_node: target.id,
                input_port: 20,
            };
            graph.links.insert(4, link);
            graph.changed();
            assert!(graph.route_confirmed.get());
            let link = graph.links.remove(&4).unwrap();
            if first_loss == RouteLoss::TargetGone {
                graph.sinks.remove(&target.id);
            }
            graph.changed();
            assert!(!graph.route_confirmed.get());
            assert_eq!(graph.route_loss(&target, false), Some(first_loss));
            assert_eq!(
                decide(&mut wait, &graph, now),
                Ok(RouteAction::AwaitDefault)
            );
            graph.retire_route();

            let expiry = now + DEFAULT_REPLACEMENT_GRACE;
            let before_expiry = expiry - Duration::from_millis(1);
            graph.links.insert(4, link);
            graph.changed();
            assert!(!graph.route_confirmed.get());
            graph.sinks.remove(&target.id);
            assert_eq!(
                graph.route_loss(&target, false),
                Some(RouteLoss::TargetGone)
            );
            graph.default = Some(replacement.name.clone());
            assert_eq!(
                decide(&mut wait, &graph, before_expiry),
                Ok(RouteAction::AwaitDefault)
            );

            graph.sinks.insert(replacement.id, replacement.clone());
            // A registered replacement cannot be used until the default says so.
            graph.default = Some(target.name.clone());
            assert_eq!(
                decide(&mut wait, &graph, before_expiry),
                Ok(RouteAction::AwaitDefault)
            );
            graph.default = Some(replacement.name.clone());
            assert_eq!(
                decide(&mut wait, &graph, before_expiry),
                Ok(RouteAction::Retire)
            );
            assert_eq!(decide(&mut wait, &graph, expiry), Err(first_loss));
        }
    }

    #[test]
    fn replacement_grace_excludes_pinned_and_layout_failures_and_caps_opening_wait() {
        let now = Instant::now();
        let mut wait = None;
        let decide = |wait: &mut Option<ReplacementWait>, loss, default_managed, replacement| {
            ReplacementWait::on_route(wait, Some(loss), default_managed, replacement, None, || now)
        };
        for loss in [
            RouteLoss::TargetGone,
            RouteLoss::Links,
            RouteLoss::Disconnected,
        ] {
            assert_eq!(decide(&mut wait, loss, false, true), Err(loss));
        }
        assert_eq!(
            decide(&mut wait, RouteLoss::TargetChanged, true, true),
            Err(RouteLoss::TargetChanged)
        );
        assert!(wait.is_none());

        let opening_deadline = now + Duration::from_millis(10);
        assert_eq!(
            ReplacementWait::on_route(
                &mut wait,
                Some(RouteLoss::Disconnected),
                true,
                false,
                Some(opening_deadline),
                || now,
            ),
            Ok(RouteAction::AwaitDefault)
        );
        assert_eq!(wait.as_ref().unwrap().deadline, opening_deadline);
        assert_eq!(
            ReplacementWait::on_route(&mut wait, None, true, true, Some(opening_deadline), || {
                opening_deadline
            }),
            Err(RouteLoss::Disconnected)
        );
    }

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
            assert_eq!(parse_positions(channel.short_name()).unwrap(), [*channel]);
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
        let sink = Sink {
            id: 2,
            serial: "2".into(),
            name: "selected".into(),
            positions: speakers.join(" "),
        };
        graph.sinks.insert(sink.id, sink.clone());
        graph.route_validation = Some(RouteValidation {
            source: 1,
            target: sink,
            speakers: speakers.to_vec(),
            auto_mix: false,
            confirmed_once: false,
        });
        graph.validate_route();
        assert!(graph.route_confirmed.get());
        graph.ports.insert(203, "FL".into());
        graph.changed();
        assert!(!graph.route_confirmed.get());
        assert!(!route_matches(&graph, 1, 2, &speakers, false));
        graph.ports.insert(203, "LFE".into());
        graph.changed();
        assert!(graph.route_confirmed.get());
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
        let sink = Sink {
            id: 2,
            serial: "2".into(),
            name: "selected".into(),
            positions: "FL FR".into(),
        };
        graph.sinks.insert(sink.id, sink.clone());
        graph.route_validation = Some(RouteValidation {
            source: 1,
            target: sink,
            speakers: vec!["FC"],
            auto_mix: true,
            confirmed_once: false,
        });
        graph.validate_route();
        assert!(graph.route_confirmed.get());

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
        assert!(graph.route_confirmed.get());
        assert!(route_matches(&graph, 1, 2, &["FC"], true));
        graph.links.get_mut(&1).unwrap().output_node = 1;
        graph.changed();
        assert!(!graph.route_confirmed.get());
        assert!(!route_matches(&graph, 1, 2, &["FC"], true));

        graph.links.get_mut(&1).unwrap().output_node = 4;
        graph.changed();
        assert!(graph.route_confirmed.get());
        let sink = graph.sinks.remove(&2).unwrap();
        graph.changed();
        assert!(!graph.route_confirmed.get());
        graph.sinks.insert(2, sink);
        graph.changed();
        assert!(graph.route_confirmed.get());
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
