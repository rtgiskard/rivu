//! Native device output; callback state and ring buffers stay private.

use super::Channel;
use crate::analysis::{AnalysisControl, AnalysisWorker, TapFrame};
use anyhow::{Result, bail};
use crossbeam_channel::{Receiver, bounded};
use ringbuf::{
    HeapCons, HeapProd, HeapRb,
    traits::{Consumer, Observer, Producer, Split},
};
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};

mod layout;

mod pipewire;

pub fn devices() -> Result<Vec<String>> {
    let mut names = pipewire::devices()?;
    names.sort();
    names.dedup();
    Ok(names)
}

fn target_name(name: Option<&str>) -> Result<Option<&str>> {
    name.map(|name| {
        name.strip_prefix(pipewire::PREFIX)
            .ok_or_else(|| anyhow::anyhow!("Output device not found: {name}"))
    })
    .transpose()
}

pub(super) fn validate_device(name: Option<&str>) -> Result<()> {
    pipewire::validate(target_name(name)?)
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
    heard: AtomicU64,
    retired: AtomicU64,
    timing_error: AtomicU32,
    volume: AtomicU32,
}
impl OutputShared {
    fn new(volume: f32, paused: bool) -> Self {
        let (wakeup, notifications) = bounded(1);
        Self {
            paused: AtomicBool::new(paused),
            pause_settled: AtomicBool::new(false),
            wakeup,
            notifications,
            refill_armed: AtomicBool::new(false),
            heard: AtomicU64::new(0),
            retired: AtomicU64::new(0),
            timing_error: AtomicU32::new(0),
            volume: AtomicU32::new(volume.to_bits()),
        }
    }

    fn fail_timing(&self, code: u32) {
        if self.timing_error.swap(code, Ordering::AcqRel) != code {
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
    retired: u64,
}
impl PlaybackClock {
    fn new() -> Self {
        Self {
            pending: VecDeque::with_capacity(1024),
            last_callback: None,
            heard: 0,
            retired: 0,
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
            self.retired += newly_heard;
            span.heard = heard;
            if heard != span.frames {
                break;
            }
            self.pending.pop_front();
        }
        shared.heard.store(self.heard, Ordering::Release);
        shared.retired.store(self.retired, Ordering::Release);
    }
}

pub(super) struct Output {
    stream: Option<pipewire::NativeStream>,
    producer: HeapProd<f32>,
    shared: Arc<OutputShared>,
    rate: u32,
    source_channels: usize,
    errors: Receiver<OutputError>,
    submitted: u64,
}
impl Output {
    pub(super) fn new(
        name: Option<&str>,
        source_rate: u32,
        source_layout: &[Channel],
        volume: f32,
        paused: bool,
        pipewire_auto_mix: bool,
        analyzer: &AnalysisWorker,
    ) -> Result<Self> {
        let target = target_name(name)?;
        let rate = source_rate;
        if rate == 0 || source_layout.is_empty() {
            bail!("Invalid decoded sample rate or speaker layout");
        }
        let source_channels = source_layout.len();
        let (producer, consumer) =
            HeapRb::new((rate / 4).max(2048) as usize * source_channels).split();
        let shared = Arc::new(OutputShared::new(volume, paused));
        let (error_send, errors) = bounded(4);
        analyzer.reset(rate);
        let callback = OutputCallback {
            consumer,
            tap: analyzer.producer(),
            tap_gap: false,
            control: analyzer.control.clone(),
            shared: shared.clone(),
            channels: source_channels,
            scratch: vec![0.0; source_channels * OutputCallback::BLOCK_FRAMES],
            mapping: Vec::new(),
            analysis_weights: analysis_weights(source_layout),
            rate,
            clock: PlaybackClock::new(),
        };
        let stream = pipewire::NativeStream::open(
            target,
            source_layout,
            pipewire_auto_mix,
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
            .store(volume.to_bits(), Ordering::Release);
    }

    pub(super) fn has_room(&self) -> bool {
        self.producer.vacant_len() >= self.source_channels
    }

    pub(super) fn refill_delay(&self, eof: bool) -> Duration {
        let frames = if eof {
            self.submitted
                .saturating_sub(self.shared.retired.load(Ordering::Acquire))
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
        self.shared.retired.load(Ordering::Acquire) == self.submitted
    }

    pub(super) fn heard(&self) -> f64 {
        self.shared.heard.load(Ordering::Acquire) as f64 / self.rate as f64
    }
    pub(super) fn position(&self) -> f64 {
        // The source cursor advances only as scheduled PCM becomes audible.
        self.shared.retired.load(Ordering::Acquire) as f64 / self.rate as f64
    }
    pub(super) fn pause(&self, paused: bool) {
        self.shared.pause_settled.store(false, Ordering::Release);
        self.shared.paused.store(paused, Ordering::Release);
    }

    pub(super) fn notifications(&self) -> &Receiver<()> {
        &self.shared.notifications
    }

    pub(super) fn check_timing(&self) -> Result<()> {
        match self.shared.timing_error.load(Ordering::Acquire) {
            0 => Ok(()),
            1 => bail!("Audio output timestamp moved backwards"),
            2 => bail!("Audio output timing queue exhausted before playback advanced"),
            _ => bail!("PipeWire output returned invalid buffers or unavailable timing"),
        }
    }
    pub(super) fn close(&mut self) -> f64 {
        self.shared.paused.store(true, Ordering::Release);
        self.stream.take();
        self.heard()
    }
}

// Visualization paths fold decoded channels into two logical sides. Real
// output still uses the original permutation with independent speakers.
pub(super) fn analysis_weights(layout: &[Channel]) -> Vec<[f32; 2]> {
    use Channel::*;
    let mut weights: Vec<_> = layout
        .iter()
        .map(|channel| match channel {
            FrontLeft | RearLeft | SideLeft | FrontLeftCenter | TopFrontLeft | TopRearLeft => {
                [1.0, 0.0]
            }
            FrontRight | RearRight | SideRight | FrontRightCenter | TopFrontRight
            | TopRearRight => [0.0, 1.0],
            _ => [1.0, 1.0],
        })
        .collect();
    let total = weights.iter().fold([0.0_f32; 2], |sum, weight| {
        [sum[0] + weight[0], sum[1] + weight[1]]
    });
    for weight in &mut weights {
        weight[0] /= total[0].max(1.0);
        weight[1] /= total[1].max(1.0);
    }
    weights
}

struct OutputCallback {
    consumer: HeapCons<f32>,
    tap: HeapProd<TapFrame>,
    tap_gap: bool,
    shared: Arc<OutputShared>,
    control: Arc<AnalysisControl>,
    channels: usize,
    scratch: Vec<f32>,
    mapping: Vec<Option<usize>>,
    analysis_weights: Vec<[f32; 2]>,
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
        let analyze = self.control.enabled.load(Ordering::Relaxed);
        let epoch = self.control.epoch.load(Ordering::Relaxed);
        let source_channels = self.analysis_weights.len();
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
            if analyze {
                let gap = self.tap_gap;
                let weights = &self.analysis_weights;
                let accepted =
                    self.tap
                        .push_iter(scratch.chunks_exact(source_channels).enumerate().map(
                            |(index, frame)| {
                                let mut samples = [0.0; 2];
                                for (sample, weights) in frame.iter().zip(weights) {
                                    samples[0] += sample * weights[0];
                                    samples[1] += sample * weights[1];
                                }
                                TapFrame {
                                    samples,
                                    epoch,
                                    gap: gap && index == 0,
                                }
                            },
                        ));
                // A partial block leaves a discontinuity for the next accepted frame.
                self.tap_gap = accepted < samples / source_channels;
                if self.tap_gap {
                    self.control.tap_overflow.store(true, Ordering::Release);
                }
            }
            for (output, frame) in block
                .chunks_exact_mut(self.channels)
                .zip(scratch.chunks_exact(source_channels))
            {
                for (sample, source) in output.iter_mut().zip(&self.mapping) {
                    *sample = match source {
                        Some(source) => (frame[*source] * volume).clamp(-1.0, 1.0),
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

#[cfg(test)]
pub(super) fn test_backend(
    rate: u32,
    paused: bool,
    layout: &[Channel],
) -> (Output, impl FnMut(&mut [f32], u64) + use<>) {
    let (output, mut callback, analyzer) = tests::test_output_layout(
        rate,
        paused,
        layout,
        layout,
        (rate / 4) as usize * layout.len(),
    );
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
mod tests {
    use super::*;
    use crate::analysis::AnalysisFrame;
    use parking_lot::RwLock;

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
    fn block_render_preserves_wrapped_frames_mapping_and_analysis() {
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
        let (tap, mut analysis) = HeapRb::new(frames).split();
        callback.tap = tap;
        callback.control.enabled.store(true, Ordering::Release);
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
            let tap = analysis.try_pop().unwrap();
            assert_eq!(tap.samples, [expected[0], expected[1]]);
            assert!(!tap.gap);
        }
        assert!(
            data[frames * device.len()..]
                .iter()
                .all(|sample| *sample == 0.0)
        );
        assert!(analysis.try_pop().is_none());
        assert_eq!(callback.consumer.occupied_len(), 0);
        assert_eq!(callback.scratch.as_ptr(), scratch);
        assert_eq!(
            callback.scratch.len(),
            OutputCallback::BLOCK_FRAMES * source.len()
        );
    }

    #[test]
    fn tap_overflow_marks_the_next_accepted_frame() {
        let (mut output, mut callback, _analyzer) = test_output(192_000, false);
        let (tap, mut samples) = HeapRb::new(1).split();
        callback.tap = tap;
        callback.control.enabled.store(true, Ordering::Release);
        output.push(&[0.5; 6]);
        callback.render(&mut [0.0_f32; 6], timestamp(0, 20));
        assert!(!samples.try_pop().unwrap().gap);
        assert!(callback.tap_gap);
        assert!(callback.control.tap_overflow.swap(false, Ordering::AcqRel));
        output.push(&[0.25; 2]);
        callback.render(&mut [0.0_f32; 2], timestamp(1, 21));
        assert!(samples.try_pop().unwrap().gap);
        assert!(!callback.tap_gap);
        assert!(!callback.control.tap_overflow.load(Ordering::Acquire));
        output.push(&[0.0; 2]);
        callback.render(&mut [0.0_f32; 2], timestamp(2, 22));
        assert!(!samples.try_pop().unwrap().gap);
    }

    #[test]
    fn tap_partial_blocks_preserve_epoch_and_wrapped_gap() {
        let stereo = [Channel::FrontLeft, Channel::FrontRight];
        let frames = OutputCallback::BLOCK_FRAMES + 3;
        let (mut output, mut callback, _analyzer) =
            test_output_layout(192_000, false, &stereo, &stereo, frames * 2);
        let (mut tap, mut samples) = HeapRb::new(4).split();
        tap.push_iter((0..3).map(|_| TapFrame {
            samples: [0.0; 2],
            epoch: 0,
            gap: false,
        }));
        for _ in 0..3 {
            samples.try_pop().unwrap();
        }
        callback.tap = tap;
        callback.control.enabled.store(true, Ordering::Release);
        callback.control.epoch.store(17, Ordering::Release);
        output.push(&vec![0.5; frames * 2]);
        callback.render(&mut vec![0.0_f32; frames * 2], timestamp(0, 20));
        for _ in 0..4 {
            let frame = samples.try_pop().unwrap();
            assert_eq!(frame.samples, [0.5; 2]);
            assert_eq!(frame.epoch, 17);
            assert!(!frame.gap);
        }
        assert!(callback.tap_gap);
        output.push(&[0.25; 4]);
        callback.render(&mut [0.0_f32; 4], timestamp(1, 21));
        assert!(samples.try_pop().unwrap().gap);
        assert!(!samples.try_pop().unwrap().gap);
        assert!(!callback.tap_gap);
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
            tap: analyzer.producer(),
            tap_gap: false,
            shared,
            control: analyzer.control.clone(),
            channels: device.len(),
            scratch: vec![0.0; source.len() * OutputCallback::BLOCK_FRAMES],
            mapping: layout::mapping(source, device).unwrap(),
            analysis_weights: analysis_weights(source),
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
    fn software_pause_preserves_ring_and_resume_excludes_silence() {
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

    #[test]
    fn analyzer_downmix_never_replaces_native_lfe_or_surrounds() {
        let source = speaker_layouts().pop().unwrap();
        let (mut output, mut callback, _analyzer) =
            test_output_layout(1000, false, &source, &source, 64);
        let frame = [0.1_f32, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8];
        callback.control.enabled.store(true, Ordering::Release);
        output.push(&frame);
        let mut data = [0.0_f32; 8];
        callback.render(&mut data, timestamp(0, 10));
        assert_eq!(data, frame);
        callback.control.enabled.store(false, Ordering::Release);
        output.push(&frame);
        callback.render(&mut data, timestamp(1, 11));
        assert_eq!(data, frame);
    }
}
