//! Device output and rate conversion; callback state and ring buffers stay private.

use super::Channel;
use crate::analysis::{AnalysisControl, AnalysisWorker, TapFrame};
use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{Receiver, bounded};
use ringbuf::{
    HeapCons, HeapProd, HeapRb,
    traits::{Consumer, Observer, Producer, Split},
};
use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction, audioadapter_buffers::direct::InterleavedSlice,
};
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
};

#[path = "output_layout.rs"]
mod layout;

#[cfg(target_os = "linux")]
#[path = "output_pipewire.rs"]
mod pipewire;

pub fn devices() -> Result<Vec<String>> {
    let mut names = Vec::new();
    for device in cpal::default_host().output_devices()? {
        names.push(device.description()?.name().to_owned());
    }
    #[cfg(target_os = "linux")]
    if let Ok(native) = pipewire::devices() {
        names.extend(native);
    }
    names.sort();
    names.dedup();
    Ok(names)
}

pub(super) fn validate_device(name: Option<&str>) -> Result<()> {
    #[cfg(target_os = "linux")]
    if let Some(name) = name.and_then(|name| name.strip_prefix(pipewire::PREFIX)) {
        return pipewire::validate(name);
    }
    find_device(name).map(|_| ())
}

fn find_device(name: Option<&str>) -> Result<cpal::Device> {
    let host = cpal::default_host();
    if let Some(name) = name {
        for device in host.output_devices()? {
            if device.description()?.name() == name {
                return Ok(device);
            }
        }
        bail!("Output device not found: {name}");
    }
    if let Some(device) = host.default_output_device() {
        return Ok(device);
    }
    host.output_devices()
        .context("No default audio output device")?
        .next()
        .context("No audio output devices available")
}

pub(super) struct Converter {
    sinc: Option<Async<f32>>,
    input: Vec<f32>,
    output: Vec<f32>,
    fill: usize,
    delay: usize,
    input_count: u64,
    output_count: u64,
    ratio: f64,
    channels: usize,
}
impl Converter {
    const CHUNK: usize = 1024;
    pub(super) fn new(source_rate: u32, output_rate: u32, channels: usize) -> Result<Self> {
        if source_rate == 0 || output_rate == 0 || channels == 0 {
            bail!("Invalid resampler rate or channel count");
        }
        let ratio = output_rate as f64 / source_rate as f64;
        let sinc = if source_rate == output_rate {
            None
        } else {
            Some(Async::new_sinc(
                ratio,
                1.0,
                &SincInterpolationParameters {
                    sinc_len: 256,
                    f_cutoff: Some(0.95),
                    interpolation: SincInterpolationType::Quadratic,
                    oversampling_factor: 128,
                    window: WindowFunction::BlackmanHarris2,
                },
                Self::CHUNK,
                channels,
                FixedAsync::Input,
            )?)
        };
        let delay = sinc.as_ref().map_or(0, Resampler::output_delay);
        let output = sinc.as_ref().map_or_else(Vec::new, |sinc| {
            vec![0.0; channels * sinc.output_frames_max()]
        });
        Ok(Self {
            sinc,
            input: vec![0.0; channels * Self::CHUNK],
            output,
            fill: 0,
            delay,
            input_count: 0,
            output_count: 0,
            ratio,
            channels,
        })
    }
    pub(super) fn push(&mut self, samples: &[f32], output: &mut Vec<f32>) -> Result<()> {
        if !samples.len().is_multiple_of(self.channels) {
            bail!("Decoded audio ends in an incomplete interleaved frame");
        }
        let frames = samples.len() / self.channels;
        self.input_count += frames as u64;
        if self.sinc.is_none() {
            output.extend_from_slice(samples);
            self.output_count += frames as u64;
            return Ok(());
        }
        let mut remaining = samples;
        while !remaining.is_empty() {
            let count = (Self::CHUNK - self.fill).min(remaining.len() / self.channels);
            let end = count * self.channels;
            self.input[self.fill * self.channels..(self.fill + count) * self.channels]
                .copy_from_slice(&remaining[..end]);
            self.fill += count;
            remaining = &remaining[end..];
            if self.fill == Self::CHUNK {
                self.process(output, usize::MAX)?;
            }
        }
        Ok(())
    }
    fn process(&mut self, output: &mut Vec<f32>, limit: usize) -> Result<()> {
        let input = InterleavedSlice::new(&self.input, self.channels, Self::CHUNK)?;
        let output_frames = self.output.len() / self.channels;
        let mut output_buffer =
            InterleavedSlice::new_mut(&mut self.output, self.channels, output_frames)?;
        let (_, count) = self
            .sinc
            .as_mut()
            .expect("sinc converter")
            .process_into_buffer(&input, &mut output_buffer, None)?;
        let skip = self.delay.min(count);
        self.delay -= skip;
        let end = count.min(skip.saturating_add(limit));
        output.extend_from_slice(&self.output[self.channels * skip..self.channels * end]);
        self.output_count += (end - skip) as u64;
        self.fill = 0;
        Ok(())
    }
    pub(super) fn finish(&mut self, output: &mut Vec<f32>) -> Result<()> {
        if self.sinc.is_none() {
            return Ok(());
        }
        let target = (self.input_count as f64 * self.ratio).round() as u64;
        while self.output_count < target {
            self.input[self.channels * self.fill..].fill(0.0);
            self.process(output, (target - self.output_count) as usize)?;
        }
        Ok(())
    }
}

struct OutputShared {
    paused: AtomicBool,
    heard: AtomicU64,
    retired: AtomicU64,
    xruns: AtomicU64,
    timing_error: AtomicU32,
    volume: AtomicU32,
}
impl OutputShared {
    fn new(volume: f32, paused: bool) -> Self {
        Self {
            paused: AtomicBool::new(paused),
            heard: AtomicU64::new(0),
            retired: AtomicU64::new(0),
            xruns: AtomicU64::new(0),
            timing_error: AtomicU32::new(0),
            volume: AtomicU32::new(volume.to_bits()),
        }
    }
}

struct ScheduledFrames {
    start: cpal::StreamInstant,
    frames: u64,
    heard: u64,
}

// Kept entirely in the PCM callback. Allocation happens before starting the
// stream; a broken clock / excessive backend queue is reported, never guessed.
struct PlaybackClock {
    pending: VecDeque<ScheduledFrames>,
    last_callback: Option<cpal::StreamInstant>,
    last_playback: Option<cpal::StreamInstant>,
    heard: u64,
    retired: u64,
    xruns: u64,
}
impl PlaybackClock {
    fn new() -> Self {
        Self {
            pending: VecDeque::with_capacity(1024),
            last_callback: None,
            last_playback: None,
            heard: 0,
            retired: 0,
            xruns: 0,
        }
    }

    fn advance(
        &mut self,
        timestamp: cpal::OutputStreamTimestamp,
        rate: u32,
        shared: &OutputShared,
    ) {
        let xruns = shared.xruns.load(Ordering::Acquire);
        if self.xruns != xruns {
            // CPAL may discard device buffers while recovering an xrun. It
            // cannot report which samples survived: do not count unconfirmed
            // samples as heard, or wait forever for the discarded buffers.
            self.retired += self
                .pending
                .drain(..)
                .map(|span| span.frames - span.heard)
                .sum::<u64>();
            self.xruns = xruns;
            // A shorter device queue can move the playback prediction back,
            // even though the stream's callback clock remains monotonic.
            self.last_playback = None;
        }
        if self
            .last_callback
            .is_some_and(|last| timestamp.callback < last)
            || self
                .last_playback
                .is_some_and(|last| timestamp.playback < last)
        {
            shared.timing_error.store(1, Ordering::Release);
            return;
        }
        self.last_callback = Some(timestamp.callback);
        self.last_playback = Some(timestamp.playback);
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
enum OutputStream {
    Cpal {
        _stream: cpal::Stream,
    },
    #[cfg(target_os = "linux")]
    PipeWire {
        _stream: pipewire::NativeStream,
    },
}

pub(super) struct Output {
    stream: Option<OutputStream>,
    producer: HeapProd<f32>,
    shared: Arc<OutputShared>,
    rate: u32,
    source_channels: usize,
    errors: Receiver<cpal::Error>,
    submitted: u64,
}
impl Output {
    pub(super) fn new(
        name: Option<&str>,
        source_rate: u32,
        source_layout: &[Channel],
        volume: f32,
        paused: bool,
        analyzer: &AnalysisWorker,
    ) -> Result<Self> {
        #[cfg(target_os = "linux")]
        if let Some(target) = name.and_then(|name| name.strip_prefix(pipewire::PREFIX)) {
            return Self::native(
                Some(target),
                source_rate,
                source_layout,
                volume,
                paused,
                analyzer,
            );
        }
        let device = find_device(name)?;
        #[cfg(target_os = "linux")]
        if source_layout.len() > 2 {
            let id = device.id()?;
            if id.host() == cpal::HostId::Alsa && matches!(id.id(), "default" | "pipewire") {
                return Self::native(None, source_rate, source_layout, volume, paused, analyzer);
            }
        }
        let (supported, mapping) = layout::negotiate(&device, source_rate, source_layout)?;
        let config = supported.config();
        let rate = config.sample_rate;
        if config.channels == 0 || rate == 0 {
            bail!("Invalid output device configuration");
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
            control: analyzer.control.clone(),
            shared: shared.clone(),
            channels: config.channels as usize,
            frame: vec![0.0; source_channels],
            mapping,
            analysis_weights: analysis_weights(source_layout),
            rate,
            clock: PlaybackClock::new(),
        };
        let shared_errors = shared.clone();
        macro_rules! build {
            ($sample:ty) => {{
                let mut callback = callback;
                device.build_output_stream(
                    config,
                    move |data: &mut [$sample], info| callback.render(data, info.timestamp()),
                    move |error| {
                        report_stream_error(error, &shared_errors, &error_send);
                    },
                    None,
                )?
            }};
        }
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => build!(f32),
            cpal::SampleFormat::F64 => build!(f64),
            cpal::SampleFormat::I8 => build!(i8),
            cpal::SampleFormat::I16 => build!(i16),
            cpal::SampleFormat::I24 => build!(cpal::I24),
            cpal::SampleFormat::I32 => build!(i32),
            cpal::SampleFormat::I64 => build!(i64),
            cpal::SampleFormat::U8 => build!(u8),
            cpal::SampleFormat::U16 => build!(u16),
            cpal::SampleFormat::U24 => build!(cpal::U24),
            cpal::SampleFormat::U32 => build!(u32),
            cpal::SampleFormat::U64 => build!(u64),
            other => bail!("Unsupported device sample format: {other:?}"),
        };
        // Software pause keeps timestamps advancing and never depends on
        // optional backend / hardware pause support.
        stream.play()?;
        Ok(Self {
            stream: Some(OutputStream::Cpal { _stream: stream }),
            producer,
            shared,
            rate,
            source_channels,
            errors,
            submitted: 0,
        })
    }

    #[cfg(target_os = "linux")]
    fn native(
        target: Option<&str>,
        rate: u32,
        source_layout: &[Channel],
        volume: f32,
        paused: bool,
        analyzer: &AnalysisWorker,
    ) -> Result<Self> {
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
            control: analyzer.control.clone(),
            shared: shared.clone(),
            channels: source_channels,
            frame: vec![0.0; source_channels],
            mapping: Vec::new(),
            analysis_weights: analysis_weights(source_layout),
            rate,
            clock: PlaybackClock::new(),
        };
        let stream = pipewire::NativeStream::open(target, source_layout, callback, error_send)?;
        Ok(Self {
            stream: Some(OutputStream::PipeWire { _stream: stream }),
            producer,
            shared,
            rate,
            source_channels,
            errors,
            submitted: 0,
        })
    }
    pub(super) fn rate(&self) -> u32 {
        self.rate
    }

    pub(super) fn errors(&self) -> &Receiver<cpal::Error> {
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

    // Return complete frames consumed; never publish a partial interleaved frame.
    pub(super) fn push(&mut self, samples: &[f32]) -> usize {
        let frames = (samples.len() / self.source_channels)
            .min(self.producer.vacant_len() / self.source_channels);
        let count = self
            .producer
            .push_slice(&samples[..frames * self.source_channels]);
        let frames = count / self.source_channels;
        self.submitted += frames as u64;
        frames
    }

    pub(super) fn drained(&self) -> bool {
        self.shared.retired.load(Ordering::Acquire) == self.submitted
    }

    pub(super) fn heard(&self) -> f64 {
        self.shared.heard.load(Ordering::Acquire) as f64 / self.rate as f64
    }
    pub(super) fn position(&self) -> f64 {
        // Lost device buffers advance the source cursor, but are not listened time.
        self.shared.retired.load(Ordering::Acquire) as f64 / self.rate as f64
    }
    pub(super) fn pause(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Release);
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
fn report_stream_error(
    error: cpal::Error,
    shared: &OutputShared,
    errors: &crossbeam_channel::Sender<cpal::Error>,
) {
    if error.kind() == cpal::ErrorKind::Xrun {
        shared.xruns.fetch_add(1, Ordering::Release);
    } else {
        let _ = errors.try_send(error);
    }
}

// Only the visualization tap is downmixed. Real output uses the permutation
// above, with independent LFE and surrounds and zero-filled unused speakers.
fn analysis_weights(layout: &[Channel]) -> Vec<[f32; 2]> {
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
    shared: Arc<OutputShared>,
    control: Arc<AnalysisControl>,
    channels: usize,
    frame: Vec<f32>,
    mapping: Vec<Option<usize>>,
    analysis_weights: Vec<[f32; 2]>,
    rate: u32,
    clock: PlaybackClock,
}
impl OutputCallback {
    fn render<T: cpal::Sample + cpal::FromSample<f32>>(
        &mut self,
        data: &mut [T],
        timestamp: cpal::OutputStreamTimestamp,
    ) {
        self.clock.advance(timestamp, self.rate, &self.shared);
        let paused = self.shared.paused.load(Ordering::Acquire);
        let volume = f32::from_bits(self.shared.volume.load(Ordering::Relaxed));
        let analyze = self.control.enabled.load(Ordering::Relaxed);
        let epoch = self.control.epoch.load(Ordering::Relaxed);
        // Snapshot availability: valid frames form one prefix, even if the
        // producer refills concurrently after an underflow.
        let mut count = if paused || self.shared.timing_error.load(Ordering::Acquire) != 0 {
            0
        } else {
            (self.consumer.occupied_len() / self.frame.len()).min(data.len() / self.channels)
        };
        if count > 0 && self.clock.pending.len() == self.clock.pending.capacity() {
            self.shared.timing_error.store(2, Ordering::Release);
            count = 0;
        }
        data.fill(T::from_sample(0.0));
        for output in data.chunks_exact_mut(self.channels).take(count) {
            // One pop publishes consumption of the entire frame, even when
            // the ring wraps. Scratch storage is allocated before stream start.
            let consumed = self.consumer.pop_slice(&mut self.frame);
            debug_assert_eq!(consumed, self.frame.len());
            if analyze {
                let mut samples = [0.0; 2];
                for (sample, weights) in self.frame.iter().zip(&self.analysis_weights) {
                    samples[0] += sample * weights[0];
                    samples[1] += sample * weights[1];
                }
                let _ = self.tap.try_push(TapFrame { samples, epoch });
            }
            for (sample, source) in output.iter_mut().zip(&self.mapping) {
                if let Some(source) = source {
                    *sample = T::from_sample((self.frame[*source] * volume).clamp(-1.0, 1.0));
                }
            }
        }
        if count > 0 {
            self.clock.pending.push_back(ScheduledFrames {
                start: timestamp.playback,
                frames: count as u64,
                heard: 0,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::AnalysisFrame;
    use parking_lot::RwLock;

    fn test_output(rate: u32, paused: bool) -> (Output, OutputCallback, AnalysisWorker) {
        let stereo = [Channel::FrontLeft, Channel::FrontRight];
        test_output_layout(rate, paused, &stereo, &stereo, 128)
    }

    fn test_output_layout(
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
            shared,
            control: analyzer.control.clone(),
            channels: device.len(),
            frame: vec![0.0; source.len()],
            mapping: layout::mapping(source, device).unwrap(),
            analysis_weights: analysis_weights(source),
            rate,
            clock: PlaybackClock::new(),
        };
        (output, callback, analyzer)
    }

    fn timestamp(callback_ms: u64, playback_ms: u64) -> cpal::OutputStreamTimestamp {
        cpal::OutputStreamTimestamp {
            callback: cpal::StreamInstant::from_millis(callback_ms),
            playback: cpal::StreamInstant::from_millis(playback_ms),
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
    fn xrun_recovers_without_counting_unconfirmed_audio_and_fatal_errors_survive() {
        let (mut output, mut callback, _analyzer) = test_output(1000, false);
        let (errors, received) = bounded(4);
        output.push([[0.5, -0.5]; 8].as_flattened());
        let mut data = [0.0_f32; 16];
        callback.render(&mut data, timestamp(0, 20));
        callback.render(&mut data, timestamp(22, 40));
        report_stream_error(cpal::ErrorKind::Xrun.into(), &output.shared, &errors);
        assert!(received.try_recv().is_err());
        output.push([[0.25, -0.25]; 3].as_flattened());
        // Recovery shortened the device queue: its new prediction precedes
        // the previous callback's 40 ms playback timestamp.
        callback.render(&mut data, timestamp(30, 30));
        assert!(output.check_timing().is_ok());
        assert_eq!(&data[..6], &[0.25, -0.25, 0.25, -0.25, 0.25, -0.25]);
        assert_eq!(output.heard(), 0.002);
        assert!(!output.drained());
        callback.render(&mut data, timestamp(33, 53));
        assert!(output.drained());
        assert_eq!(output.heard(), 0.005);
        for kind in [
            cpal::ErrorKind::DeviceNotAvailable,
            cpal::ErrorKind::BackendError,
        ] {
            report_stream_error(kind.into(), &output.shared, &errors);
            assert_eq!(received.try_recv().unwrap().kind(), kind);
        }
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

    #[test]
    fn resampling_preserves_duration_pitch_and_opposite_phase_stereo() {
        let input: Vec<f32> = (0..11_025)
            .flat_map(|index| {
                let sample = (std::f32::consts::TAU * 440.0 * index as f32 / 44_100.0).sin() * 0.5;
                [sample, -sample]
            })
            .collect();
        let mut converter = Converter::new(44_100, 48_000, 2).unwrap();
        let mut output = Vec::new();
        for chunk in input.chunks(137 * 2) {
            converter.push(chunk, &mut output).unwrap();
        }
        converter.finish(&mut output).unwrap();
        assert_eq!(
            output.len() / 2,
            12_000,
            "No padding or filter delay may change the audible duration"
        );
        assert!(
            output
                .chunks_exact(2)
                .all(|frame| (frame[0] + frame[1]).abs() < 1e-6)
        );
        let power = |frequency: f32| {
            let mut real = 0.0;
            let mut imag = 0.0;
            for (index, frame) in output.chunks_exact(2).enumerate() {
                let phase = std::f32::consts::TAU * frequency * index as f32 / 48_000.0;
                real += frame[0] * phase.cos();
                imag += frame[0] * phase.sin();
            }
            real * real + imag * imag
        };
        assert!(
            power(440.0) > power(880.0) * 100.0,
            "Resampling must not change pitch"
        );
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
    fn resampling_keeps_channel_windows_isolated_at_every_supported_width() {
        for source in speaker_layouts() {
            let channels = source.len();
            for (input_rate, output_rate) in [
                (44_100, 48_000),
                (48_000, 44_100),
                (96_000, 48_000),
                (48_000, 48_000),
            ] {
                let frame_count = channels * 1024;
                let input: Vec<f32> = (0..frame_count)
                    .flat_map(|frame| {
                        (0..channels).map(move |channel| {
                            if frame / 1024 == channel {
                                (std::f32::consts::TAU * 300.0 * frame as f32 / input_rate as f32)
                                    .sin()
                                    * 0.5
                            } else {
                                0.0
                            }
                        })
                    })
                    .collect();
                let mut converter = Converter::new(input_rate, output_rate, channels).unwrap();
                let mut converted = Vec::new();
                for packet in input.chunks(137 * channels) {
                    converter.push(packet, &mut converted).unwrap();
                }
                converter.finish(&mut converted).unwrap();
                assert_eq!(
                    converted.len() / channels,
                    (frame_count as f64 * output_rate as f64 / input_rate as f64).round() as usize
                );
                for speaker in 0..channels {
                    let start = ((speaker * 1024 + 300) as f64 * output_rate as f64
                        / input_rate as f64)
                        .ceil() as usize;
                    let end = ((speaker * 1024 + 724) as f64 * output_rate as f64
                        / input_rate as f64)
                        .floor() as usize;
                    let mut energy = vec![0.0_f64; channels];
                    for frame in converted[start * channels..end * channels].chunks_exact(channels)
                    {
                        for (sum, sample) in energy.iter_mut().zip(frame) {
                            *sum += f64::from(*sample).powi(2);
                        }
                    }
                    assert!(energy[speaker] > 1.0, "Lost speaker {:?}", source[speaker]);
                    for (channel, sum) in energy.iter().enumerate() {
                        if channel != speaker {
                            assert!(
                                *sum < 1e-10,
                                "Crosstalk {:?} -> {:?}",
                                source[speaker],
                                source[channel]
                            );
                        }
                    }
                }
            }
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
