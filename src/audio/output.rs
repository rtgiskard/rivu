//! Device output and rate conversion; callback state and ring buffers stay private.

use super::Stereo;
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
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};

pub fn devices() -> Result<Vec<String>> {
    let mut names = Vec::new();
    for device in cpal::default_host().output_devices()? {
        names.push(device.description()?.name().to_owned());
    }
    names.sort();
    names.dedup();
    Ok(names)
}

pub(super) fn validate_device(name: Option<&str>) -> Result<()> {
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
}
impl Converter {
    const CHUNK: usize = 1024;
    pub(super) fn new(source_rate: u32, output_rate: u32) -> Result<Self> {
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
                2,
                FixedAsync::Input,
            )?)
        };
        let delay = sinc.as_ref().map_or(0, Resampler::output_delay);
        let output = sinc
            .as_ref()
            .map_or_else(Vec::new, |sinc| vec![0.0; 2 * sinc.output_frames_max()]);
        Ok(Self {
            sinc,
            input: vec![0.0; 2 * Self::CHUNK],
            output,
            fill: 0,
            delay,
            input_count: 0,
            output_count: 0,
            ratio,
        })
    }
    pub(super) fn push(&mut self, frames: &[Stereo], output: &mut Vec<Stereo>) -> Result<()> {
        self.input_count += frames.len() as u64;
        if self.sinc.is_none() {
            output.extend_from_slice(frames);
            self.output_count += frames.len() as u64;
            return Ok(());
        }
        for frame in frames {
            self.input[2 * self.fill] = frame[0];
            self.input[2 * self.fill + 1] = frame[1];
            self.fill += 1;
            if self.fill == Self::CHUNK {
                self.process(output, usize::MAX)?;
            }
        }
        Ok(())
    }
    fn process(&mut self, output: &mut Vec<Stereo>, limit: usize) -> Result<()> {
        let input = InterleavedSlice::new(&self.input, 2, Self::CHUNK)?;
        let output_frames = self.output.len() / 2;
        let mut output_buffer = InterleavedSlice::new_mut(&mut self.output, 2, output_frames)?;
        let (_, count) = self
            .sinc
            .as_mut()
            .expect("sinc converter")
            .process_into_buffer(&input, &mut output_buffer, None)?;
        let skip = self.delay.min(count);
        self.delay -= skip;
        let end = count.min(skip.saturating_add(limit));
        output.extend(
            self.output[2 * skip..2 * end]
                .as_chunks::<2>()
                .0
                .iter()
                .copied(),
        );
        self.output_count += (end - skip) as u64;
        self.fill = 0;
        Ok(())
    }
    pub(super) fn finish(&mut self, output: &mut Vec<Stereo>) -> Result<()> {
        if self.sinc.is_none() {
            return Ok(());
        }
        let target = (self.input_count as f64 * self.ratio).round() as u64;
        while self.output_count < target {
            self.input[2 * self.fill..].fill(0.0);
            self.process(output, (target - self.output_count) as usize)?;
        }
        Ok(())
    }
}

struct OutputShared {
    paused: AtomicBool,
    consumed: AtomicU64,
    volume: AtomicU32,
}
pub(super) struct Output {
    stream: Option<cpal::Stream>,
    producer: HeapProd<Stereo>,
    shared: Arc<OutputShared>,
    rate: u32,
    errors: Receiver<cpal::Error>,
}
impl Output {
    pub(super) fn new(
        name: Option<&str>,
        volume: f32,
        paused: bool,
        analyzer: &AnalysisWorker,
    ) -> Result<Self> {
        let device = find_device(name)?;
        let supported = device.default_output_config()?;
        let config = supported.config();
        let rate = config.sample_rate;
        if config.channels == 0 || rate == 0 {
            bail!("Invalid output device configuration");
        }
        let (producer, consumer) = HeapRb::new((rate / 4).max(2048) as usize).split();
        let shared = Arc::new(OutputShared {
            paused: AtomicBool::new(paused),
            consumed: AtomicU64::new(0),
            volume: AtomicU32::new(volume.to_bits()),
        });
        let (error_send, errors) = bounded(4);
        analyzer.reset(rate);
        let tap = analyzer.producer();
        let control = analyzer.control.clone();
        macro_rules! build {
            ($sample:ty) => {{
                let shared_callback = shared.clone();
                let mut consumer = consumer;
                let mut tap = tap;
                let channels = config.channels as usize;
                device.build_output_stream(
                    config,
                    move |data: &mut [$sample], _| {
                        render(
                            data,
                            channels,
                            &mut consumer,
                            &mut tap,
                            &shared_callback,
                            &control,
                        );
                    },
                    move |error| {
                        let _ = error_send.try_send(error);
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
        if !paused {
            stream.play()?;
        }
        Ok(Self {
            stream: Some(stream),
            producer,
            shared,
            rate,
            errors,
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
        self.producer.vacant_len() > 0
    }

    pub(super) fn push(&mut self, frames: &[Stereo]) -> usize {
        self.producer.push_slice(frames)
    }

    pub(super) fn drained(&self) -> bool {
        self.producer.occupied_len() == 0
    }

    pub(super) fn heard(&self) -> f64 {
        self.shared.consumed.load(Ordering::Acquire) as f64 / self.rate as f64
    }
    pub(super) fn pause(&self, paused: bool) -> Result<()> {
        self.shared.paused.store(paused, Ordering::Release);
        if let Some(stream) = &self.stream {
            if paused {
                stream.pause()?;
            } else {
                stream.play()?;
            }
        }
        Ok(())
    }
    pub(super) fn close(&mut self) -> f64 {
        self.shared.paused.store(true, Ordering::Release);
        self.stream.take();
        self.heard()
    }
}
fn render<T: cpal::Sample + cpal::FromSample<f32>>(
    data: &mut [T],
    channels: usize,
    consumer: &mut HeapCons<Stereo>,
    tap: &mut HeapProd<TapFrame>,
    shared: &OutputShared,
    control: &AnalysisControl,
) {
    let paused = shared.paused.load(Ordering::Acquire);
    let volume = f32::from_bits(shared.volume.load(Ordering::Relaxed));
    let analyze = control.enabled.load(Ordering::Relaxed);
    let epoch = control.epoch.load(Ordering::Relaxed);
    let mut consumed = 0;
    for output in data.chunks_exact_mut(channels) {
        let samples = if paused {
            [0.0; 2]
        } else if let Some(samples) = consumer.try_pop() {
            consumed += 1;
            if analyze {
                let _ = tap.try_push(TapFrame { samples, epoch });
            }
            samples
        } else {
            [0.0; 2]
        };
        output.fill(T::from_sample(0.0));
        if channels == 1 {
            output[0] = T::from_sample(((samples[0] + samples[1]) * 0.5 * volume).clamp(-1.0, 1.0));
        } else {
            output[0] = T::from_sample((samples[0] * volume).clamp(-1.0, 1.0));
            output[1] = T::from_sample((samples[1] * volume).clamp(-1.0, 1.0));
        }
    }
    if consumed > 0 {
        shared.consumed.fetch_add(consumed, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::AnalysisFrame;
    use parking_lot::RwLock;

    #[test]
    fn heard_frames_exclude_pause_and_underflow() {
        let analyzer =
            AnalysisWorker::new(Arc::new(RwLock::new(AnalysisFrame::default()))).unwrap();
        let mut tap = analyzer.producer();
        let (mut producer, mut consumer) = HeapRb::new(8).split();
        let shared = OutputShared {
            paused: AtomicBool::new(false),
            consumed: AtomicU64::new(0),
            volume: AtomicU32::new(1.0f32.to_bits()),
        };
        producer.push_slice(&[[0.25, -0.25]; 3]);
        let mut data = [0.0_f32; 16];
        render(
            &mut data,
            2,
            &mut consumer,
            &mut tap,
            &shared,
            &analyzer.control,
        );
        assert_eq!(shared.consumed.load(Ordering::Acquire), 3);
        assert_eq!(&data[..6], &[0.25, -0.25, 0.25, -0.25, 0.25, -0.25]);
        assert!(data[6..].iter().all(|sample| *sample == 0.0));
        producer.push_slice(&[[0.5, -0.5]; 2]);
        shared.paused.store(true, Ordering::Release);
        render(
            &mut data,
            2,
            &mut consumer,
            &mut tap,
            &shared,
            &analyzer.control,
        );
        assert_eq!(shared.consumed.load(Ordering::Acquire), 3);
        assert_eq!(consumer.occupied_len(), 2);
        assert!(data.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn resampling_preserves_duration_pitch_and_opposite_phase_stereo() {
        let input: Vec<Stereo> = (0..11_025)
            .map(|index| {
                let sample = (std::f32::consts::TAU * 440.0 * index as f32 / 44_100.0).sin() * 0.5;
                [sample, -sample]
            })
            .collect();
        let mut converter = Converter::new(44_100, 48_000).unwrap();
        let mut output = Vec::new();
        for chunk in input.chunks(137) {
            converter.push(chunk, &mut output).unwrap();
        }
        converter.finish(&mut output).unwrap();
        assert_eq!(
            output.len(),
            12_000,
            "No padding or filter delay may change the audible duration"
        );
        assert!(
            output
                .iter()
                .all(|frame| (frame[0] + frame[1]).abs() < 1e-6)
        );
        let power = |frequency: f32| {
            let mut real = 0.0;
            let mut imag = 0.0;
            for (index, frame) in output.iter().enumerate() {
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
}
