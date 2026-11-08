//! Official streaming GTCRN / UL-UNAS graphs with complete final-hop synthesis.
//!
//! Reference revisions, notices and hashes live in `tools/audio_processing/onnx_speech.py`.
//! FFT512/hop256 is centered with reflection padding; GTCRN uses sqrt(periodic
//! Hann), UL-UNAS periodic Hann. The last hop is completed with samples reflected
//! against the original signal length: ceil(N/256)+1 centered spectra, with at
//! least two for short inputs. OLA divides by the actual window-square sum and
//! each emitted hop is cropped to the original frame count. This preserves full
//! overlap coverage at every retained sample instead of magnifying f32 FFT error
//! in a single-window partial tail. It intentionally differs from the unpadded
//! upstream example for non-hop-aligned input. One-sample inputs extend constantly;
//! other short inputs use repeated reflection rather than PyTorch's rejecting pad.
use super::render::{ProgressReporter, check_cancel};
use super::{AudioOutputs, ModelProvenance};
use anyhow::{Context, Result, bail, ensure};
use ort::session::{
    Session,
    builder::GraphOptimizationLevel,
    run_options::{HasSelectedOutputs, OutputSelector, RunOptions},
};
use ort::value::Tensor;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex, num_complex::Complex32};
use rivu_audio as audio;
use rivu_format::config::{NeuralConfig, NeuralDevice};
use rivu_protocol::model::ProcessingPhase;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::{Component, Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
};

const FFT: usize = 512;
const HOP: usize = 256;
const BINS: usize = FFT / 2 + 1;
const RATE: u32 = 16_000;
const MAX_MANIFEST: u64 = 256 * 1024;
const MAX_BUNDLE: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Gtcrn,
    UlUnas,
}
impl Kind {
    pub(super) fn from_id(id: &str) -> Result<Self> {
        match id {
            "GTCRN" => Ok(Self::Gtcrn),
            "UL_UNAS" => Ok(Self::UlUnas),
            _ => bail!("Unsupported native speech model: {id}"),
        }
    }
    fn model_hash(self) -> &'static str {
        match self {
            Self::Gtcrn => "b4718df6228e7bdf1a8a435cf98f838636eb2fd331acabf86ba87c5192ebcb87",
            Self::UlUnas => "f2e804d54d6a88f4f82f44d86c9f1cf646db2509bfca935cfbfc5fcd8cbfac3b",
        }
    }
    fn state_shapes(self) -> [&'static [usize]; 3] {
        match self {
            Self::Gtcrn => [&[2, 1, 16, 16, 33], &[2, 3, 1, 1, 16], &[2, 1, 33, 16]],
            Self::UlUnas => [&[1, 5358], &[1, 402], &[1, 1056]],
        }
    }
}

#[derive(Deserialize)]
struct Manifest {
    version: u32,
    operation: String,
    id: String,
    backend: String,
    model: String,
    files: BTreeMap<String, String>,
    license_evidence: String,
}

pub(super) struct PreparedModel {
    kind: Kind,
    model: Vec<u8>,
    wet: f32,
    threads: u8,
}

fn relative_path(root: &Path, name: &str) -> Result<PathBuf> {
    ensure!(
        !name.is_empty() && name.len() <= 4096 && !name.contains('\\'),
        "Invalid native model relative path"
    );
    let path = Path::new(name);
    ensure!(
        path.components()
            .all(|part| matches!(part, Component::Normal(_))),
        "Native model path must be bundle-relative without traversal"
    );
    let resolved = root
        .join(path)
        .canonicalize()
        .context("Resolving native bundle file")?;
    ensure!(
        resolved.starts_with(root),
        "Native model file escapes its bundle"
    );
    ensure!(
        resolved.is_file(),
        "Native model bundle entry is not a file"
    );
    Ok(resolved)
}

fn read_bounded(path: &Path, bound: u64, cancel: &AtomicBool) -> Result<Vec<u8>> {
    check_cancel(cancel)?;
    let mut file = File::open(path).with_context(|| format!("Opening {}", path.display()))?;
    let length = file.metadata()?.len();
    ensure!(length <= bound, "Native model file exceeds size bound");
    let mut bytes = Vec::with_capacity(length as usize);
    loop {
        check_cancel(cancel)?;
        let remaining = (bound + 1).saturating_sub(bytes.len() as u64);
        let count = file
            .by_ref()
            .take(remaining.min(65536))
            .read_to_end(&mut bytes)?;
        if count == 0 {
            break;
        }
        ensure!(
            bytes.len() as u64 <= bound,
            "Native model file grew beyond size bound"
        );
    }
    Ok(bytes)
}

pub(super) fn prepare(
    path: &Path,
    manifest_bytes: &[u8],
    config: &NeuralConfig,
    cancel: &AtomicBool,
    provenance: &mut ModelProvenance,
) -> Result<PreparedModel> {
    check_cancel(cancel)?;
    ensure!(
        config.device == NeuralDevice::Cpu,
        "Native GTCRN/UL-UNAS support CPU only; no device fallback"
    );
    ensure!(
        (1..=8).contains(&config.threads),
        "Native ONNX threads must be 1..8"
    );
    ensure!(
        !config.speech_post_filter,
        "post_filter is supported only by DeepFilterNet3, not native GTCRN/UL-UNAS"
    );
    let wet = wet_weight(config.speech_wet_mix, config.speech_attenuation_limit_db)?;
    ensure!(
        manifest_bytes.len() as u64 <= MAX_MANIFEST,
        "Native model manifest exceeds size bound"
    );
    let manifest: Manifest =
        serde_json::from_slice(manifest_bytes).context("Reading native speech manifest")?;
    provenance.id = Some(manifest.id);
    ensure!(
        manifest.version == 1 && manifest.operation == "speech" && manifest.backend == "onnx",
        "Invalid native speech manifest contract"
    );
    let kind = Kind::from_id(provenance.id.as_deref().expect("manifest has an id"))?;
    ensure!(
        !manifest.files.is_empty() && manifest.files.len() <= 64,
        "Native model files must contain 1..64 entries"
    );
    ensure!(
        manifest.files.contains_key(&manifest.model)
            && manifest.files.contains_key(&manifest.license_evidence),
        "Native model and license evidence must be hashed bundle entries"
    );
    ensure!(
        manifest.files[&manifest.model] == kind.model_hash(),
        "Native speech requires the exact pinned official model SHA256"
    );
    let root = path
        .canonicalize()?
        .parent()
        .context("Native manifest has no bundle directory")?
        .to_owned();
    let mut total = 0u64;
    let mut model = None;
    for (name, expected) in &manifest.files {
        check_cancel(cancel)?;
        ensure!(
            expected.len() == 64
                && expected
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "Invalid native bundle SHA256"
        );
        let file = relative_path(&root, name)?;
        let bytes = read_bounded(&file, MAX_BUNDLE - total, cancel)?;
        total += bytes.len() as u64;
        let mut digest = Sha256::new();
        for block in bytes.chunks(65536) {
            check_cancel(cancel)?;
            digest.update(block);
        }
        let actual = format!("{:x}", digest.finalize());
        ensure!(
            actual == *expected,
            "Native model bundle SHA256 mismatch: {name}"
        );
        if name == &manifest.model {
            model = Some(bytes);
        }
    }
    check_cancel(cancel)?;
    provenance.sha256 = Some(format!("{:x}", Sha256::digest(manifest_bytes)));
    Ok(PreparedModel {
        kind,
        model: model.context("Missing native ONNX model")?,
        wet,
        threads: config.threads,
    })
}

// Borrowing tensor views avoids copying recurrent state into each request.
// Runtime-owned outputs become the next frame's inputs, instead of copying the
// large convolution cache back to Rust-owned storage every 16 ms.
struct State {
    input: Tensor<f32>,
    caches: [Tensor<f32>; 3],
    outputs: [RunOptions<HasSelectedOutputs>; 2],
    next: usize,
}
impl State {
    fn names(kind: Kind) -> [&'static str; 3] {
        match kind {
            Kind::Gtcrn => ["conv_cache_out", "tra_cache_out", "inter_cache_out"],
            Kind::UlUnas => ["conv_cache_out", "tfa_cache_out", "inter_cache_out"],
        }
    }
    fn new(kind: Kind) -> Result<Self> {
        let [a, b, c] = kind.state_shapes();
        let zero =
            |shape: &[usize]| Tensor::from_array((shape, vec![0.0f32; shape.iter().product()]));
        let first = [zero(a)?, zero(b)?, zero(c)?];
        let second = [zero(a)?, zero(b)?, zero(c)?];
        let alias = |tensor: &Tensor<f32>| {
            tensor
                .view()
                .try_upgrade()
                .map_err(|_| anyhow::anyhow!("Native tensor cannot retain owned state"))
        };
        let caches = [alias(&second[0])?, alias(&second[1])?, alias(&second[2])?];
        let options = |bank: [Tensor<f32>; 3]| -> Result<RunOptions<HasSelectedOutputs>> {
            let mut selector =
                OutputSelector::default().preallocate("enh", zero(&[1, BINS, 1, 2])?);
            for (name, tensor) in Self::names(kind).into_iter().zip(bank) {
                selector = selector.preallocate(name, tensor);
            }
            Ok(RunOptions::new()?.with_outputs(selector))
        };
        Ok(Self {
            input: zero(&[1, BINS, 1, 2])?,
            caches,
            outputs: [options(first)?, options(second)?],
            next: 0,
        })
    }
    fn process(
        &mut self,
        session: &mut Session,
        spectrum: &mut [f32; BINS * 2],
        kind: Kind,
    ) -> Result<()> {
        self.input.extract_tensor_mut().1.copy_from_slice(spectrum);
        let mut outputs = session.run_with_options(
            ort::inputs![
                &self.input,
                &self.caches[0],
                &self.caches[1],
                &self.caches[2]
            ],
            &self.outputs[self.next],
        )?;
        ensure!(
            outputs.len() == 4,
            "Native speech ONNX output count mismatch"
        );
        let (shape, enhanced) = outputs[0].try_extract_tensor::<f32>()?;
        ensure!(
            shape.as_ref() == [1, BINS as i64, 1, 2] && enhanced.len() == BINS * 2,
            "Native speech enhanced spectrum shape mismatch"
        );
        ensure!(
            enhanced.iter().all(|x| x.is_finite()),
            "Native speech produced non-finite spectrum"
        );
        spectrum.copy_from_slice(enhanced);
        // Removing by name is independent of the shifting output indices.
        let names = Self::names(kind);
        for ((cache, name), expected) in self.caches.iter_mut().zip(names).zip(kind.state_shapes())
        {
            let value = outputs
                .remove(name)
                .with_context(|| format!("Missing native ONNX output {name}"))?;
            let (shape, data) = value.try_extract_tensor::<f32>()?;
            ensure!(
                shape.iter().copied().eq(expected.iter().map(|&x| x as i64)),
                "Native speech recurrent cache shape mismatch"
            );
            ensure!(
                data.iter().all(|x| x.is_finite()),
                "Native speech produced non-finite cache"
            );
            *cache = value.downcast::<ort::value::TensorValueType<f32>>()?;
        }
        self.next ^= 1;
        Ok(())
    }
}

struct Stft {
    window: [f32; FFT],
    fft: Arc<dyn RealToComplex<f32>>,
    ifft: Arc<dyn ComplexToReal<f32>>,
    scratch_forward: Vec<Complex32>,
    scratch_inverse: Vec<Complex32>,
    time: [f32; FFT],
    frequency: [Complex32; BINS],
    overlap: [f32; FFT],
    weight: [f32; FFT],
}
impl Stft {
    fn new(kind: Kind) -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT);
        let ifft = planner.plan_fft_inverse(FFT);
        Self {
            window: std::array::from_fn(|i| {
                let hann = 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / FFT as f32).cos();
                if kind == Kind::Gtcrn {
                    hann.sqrt()
                } else {
                    hann
                }
            }),
            scratch_forward: fft.make_scratch_vec(),
            scratch_inverse: ifft.make_scratch_vec(),
            fft,
            ifft,
            time: [0.0; FFT],
            frequency: [Complex32::new(0.0, 0.0); BINS],
            overlap: [0.0; FFT],
            weight: [0.0; FFT],
        }
    }
    fn analyze(&mut self, frame: &[f32; FFT], spectrum: &mut [f32; BINS * 2]) -> Result<()> {
        for ((dst, &src), &win) in self.time.iter_mut().zip(frame).zip(&self.window) {
            *dst = src * win;
        }
        self.fft.process_with_scratch(
            &mut self.time,
            &mut self.frequency,
            &mut self.scratch_forward,
        )?;
        for (pair, value) in spectrum
            .as_chunks_mut::<2>()
            .0
            .iter_mut()
            .zip(&self.frequency)
        {
            pair[0] = value.re;
            pair[1] = value.im;
        }
        Ok(())
    }
    fn synthesize(&mut self, spectrum: &[f32; BINS * 2]) -> Result<()> {
        for (dst, pair) in self.frequency.iter_mut().zip(spectrum.as_chunks::<2>().0) {
            *dst = Complex32::new(pair[0], pair[1]);
        }
        self.frequency[0].im = 0.0;
        self.frequency[BINS - 1].im = 0.0;
        self.ifft.process_with_scratch(
            &mut self.frequency,
            &mut self.time,
            &mut self.scratch_inverse,
        )?;
        for i in 0..FFT {
            self.overlap[i] += self.time[i] * self.window[i] / FFT as f32;
            self.weight[i] += self.window[i] * self.window[i];
        }
        Ok(())
    }
    fn sample(&self, i: usize) -> Result<f32> {
        ensure!(
            self.weight[i] > 1e-11,
            "Native speech inverse STFT has zero window coverage"
        );
        let sample = self.overlap[i] / self.weight[i];
        ensure!(
            sample.is_finite(),
            "Native speech inverse STFT produced non-finite PCM"
        );
        Ok(sample)
    }
    fn advance(&mut self) {
        self.overlap.copy_within(HOP.., 0);
        self.overlap[HOP..].fill(0.0);
        self.weight.copy_within(HOP.., 0);
        self.weight[HOP..].fill(0.0);
    }
}

fn reflect(index: i64, length: u64) -> u64 {
    if length <= 1 {
        return 0;
    }
    let period = 2 * (length as i64 - 1);
    let index = index.rem_euclid(period);
    if index < length as i64 {
        index as u64
    } else {
        (period - index) as u64
    }
}

// Cached source access and fixed polyphase sinc kernels. This is a centered,
// delay-compensated low-pass resampler, not linear interpolation or a model-rate
// relabel. Kernel support is 16 zero crossings per side; rate ratios determine
// exact phases once. Coefficients and context are independent of track length.
struct SincKernel {
    source: u32,
    target: u32,
    phases: usize,
    radius: i64,
    kernels: Vec<f32>,
}
struct Resampler<'a> {
    pcm: &'a audio::PcmAudio,
    kernel: SincKernel,
    cache: Vec<f32>,
    start: u64,
    valid: usize,
}
fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}
impl SincKernel {
    fn new(source: u32, target: u32, cancel: &AtomicBool) -> Result<Self> {
        ensure!(
            (8_000..=384_000).contains(&source) && (8_000..=384_000).contains(&target),
            "Native speech resampling supports 8000..384000 Hz"
        );
        let cutoff = (target as f64 / source as f64).min(1.0);
        let radius = if source == target {
            0
        } else {
            (16.0 / cutoff).ceil() as i64
        };
        let phases = (target / gcd(source, target)) as usize;
        let taps = (2 * radius + 1) as usize;
        let mut kernels = Vec::new();
        if source != target {
            kernels
                .try_reserve_exact(phases * taps)
                .context("Allocating bounded native resampler kernels")?;
            for phase in 0..phases {
                if phase % 64 == 0 {
                    check_cancel(cancel)?;
                }
                let frac = phase as f64 / phases as f64;
                let start = kernels.len();
                let mut sum = 0.0f64;
                for tap in -radius..=radius {
                    let distance = (tap as f64 - frac) * cutoff;
                    let x = std::f64::consts::PI * distance;
                    let sinc = if x.abs() < 1e-12 { 1.0 } else { x.sin() / x };
                    // Hann-tapered sinc; compact support avoids endpoint ringing
                    // from truncating an untapered interpolation kernel.
                    let window = if distance.abs() <= 16.0 {
                        0.5 + 0.5 * (std::f64::consts::PI * distance / 16.0).cos()
                    } else {
                        0.0
                    };
                    let value = cutoff * sinc * window;
                    kernels.push(value as f32);
                    sum += value;
                }
                for value in &mut kernels[start..] {
                    *value /= sum as f32;
                }
            }
        }
        Ok(Self {
            source,
            target,
            phases,
            radius,
            kernels,
        })
    }
    fn position(&self, index: u64) -> i64 {
        (u128::from(index) * u128::from(self.source) / u128::from(self.target)) as i64
    }
    fn sample(&self, index: u64, mut source: impl FnMut(i64) -> f32) -> f32 {
        let integer = self.position(index);
        if self.source == self.target {
            return source(integer);
        }
        let remainder = u128::from(index) * u128::from(self.source) % u128::from(self.target);
        let phase = (remainder * self.phases as u128 / u128::from(self.target)) as usize;
        let taps = (2 * self.radius + 1) as usize;
        let mut value = 0.0;
        for i in 0..taps {
            value += source(integer + i as i64 - self.radius) * self.kernels[phase * taps + i];
        }
        value
    }
}
impl<'a> Resampler<'a> {
    fn new(pcm: &'a audio::PcmAudio, target: u32, cancel: &AtomicBool) -> Result<Self> {
        Ok(Self {
            pcm,
            kernel: SincKernel::new(pcm.sample_rate(), target, cancel)?,
            cache: vec![0.0; 8192 * usize::from(pcm.channels())],
            start: 0,
            valid: 0,
        })
    }
    fn sample(&mut self, index: u64, channel: usize) -> f32 {
        // Split these borrows so both seekable and streaming restoration use
        // the exact same phases, coefficients and f32 accumulation order.
        let pcm = self.pcm;
        let start = &mut self.start;
        let valid = &mut self.valid;
        let cache = &mut self.cache;
        self.kernel.sample(index, |index| {
            let index = index.clamp(0, pcm.frames() as i64 - 1) as u64;
            if index < *start || index >= *start + *valid as u64 {
                *start = index.saturating_sub(512);
                *valid = pcm.read_frames(*start, cache);
            }
            cache[(index - *start) as usize * usize::from(pcm.channels()) + channel]
        })
    }
}

// Consume completed model hops directly. Only the next FIR support interval
// remains live; endpoint extension is identical to the seekable resampler.
struct StreamingResampler {
    kernel: SincKernel,
    channels: usize,
    length: u64,
    output_frames: u64,
    received: u64,
    next: u64,
    start: u64,
    pending: std::collections::VecDeque<f32>,
    block: Vec<f32>,
}
impl StreamingResampler {
    fn new(
        source: u32,
        target: u32,
        channels: usize,
        length: u64,
        output_frames: u64,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        let kernel = SincKernel::new(source, target, cancel)?;
        let capacity = (HOP + 2 * kernel.radius as usize + 2) * channels;
        Ok(Self {
            kernel,
            channels,
            length,
            output_frames,
            received: 0,
            next: 0,
            start: 0,
            pending: std::collections::VecDeque::with_capacity(capacity),
            block: vec![0.0; 4096 * channels],
        })
    }
    fn push(
        &mut self,
        samples: &[f32],
        cancel: &AtomicBool,
        mut emit: impl FnMut(u64, &[f32]) -> Result<()>,
    ) -> Result<()> {
        check_cancel(cancel)?;
        ensure!(
            samples.len().is_multiple_of(self.channels) && samples.len() <= HOP * self.channels,
            "Native restoration requires complete bounded model hops"
        );
        let count = samples.len() / self.channels;
        ensure!(
            self.received + count as u64 <= self.length,
            "Native restoration input overflow"
        );
        self.pending.extend(samples.iter().copied());
        self.received += count as u64;
        while self.next < self.output_frames {
            check_cancel(cancel)?;
            let begin = self.next;
            let mut count = 0;
            while count < self.block.len() / self.channels && self.next < self.output_frames {
                let right = self.kernel.position(self.next) + self.kernel.radius;
                if self.received < self.length && right >= self.received as i64 {
                    break;
                }
                for channel in 0..self.channels {
                    self.block[count * self.channels + channel] =
                        self.kernel.sample(self.next, |index| {
                            let index = index.clamp(0, self.length as i64 - 1) as u64;
                            self.pending[(index - self.start) as usize * self.channels + channel]
                        });
                }
                self.next += 1;
                count += 1;
            }
            if count == 0 {
                break;
            }
            emit(begin, &self.block[..count * self.channels])?;
            // The next output may skip beyond the received prefix when
            // downsampling. Never discard samples that have not arrived yet.
            let keep = (self.kernel.position(self.next) - self.kernel.radius).max(0) as u64;
            let keep = keep.min(self.received);
            let discard = (keep - self.start) as usize * self.channels;
            self.pending.drain(..discard);
            self.start = keep;
        }
        Ok(())
    }
    fn finish(self) -> Result<()> {
        ensure!(
            self.received == self.length && self.next == self.output_frames,
            "Native speech inverse STFT/restoration frame alignment mismatch"
        );
        Ok(())
    }
}

struct SpeechOutput<'a> {
    input: &'a audio::PcmAudio,
    restored: StreamingResampler,
    output: audio::PcmBuilder,
    dry: Vec<f32>,
    wet: f32,
}
impl<'a> SpeechOutput<'a> {
    fn new(
        input: &'a audio::PcmAudio,
        length: u64,
        wet: f32,
        budget: Arc<audio::PcmBudget>,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        let channels = usize::from(input.channels());
        Ok(Self {
            input,
            restored: StreamingResampler::new(
                RATE,
                input.sample_rate(),
                channels,
                length,
                input.frames(),
                cancel,
            )?,
            output: audio::PcmBuilder::new(
                input.sample_rate(),
                Arc::clone(input.layout()),
                budget,
            )?,
            dry: vec![0.0; 4096 * channels],
            wet,
        })
    }
    fn push(&mut self, samples: &[f32], cancel: &AtomicBool) -> Result<()> {
        let Self {
            input,
            restored,
            output,
            dry,
            wet,
        } = self;
        restored.push(samples, cancel, |position, enhanced| {
            let count = enhanced.len() / usize::from(input.channels());
            let mixed = &mut dry[..enhanced.len()];
            ensure!(
                input.read_frames(position, mixed) == count,
                "Truncated native speech source PCM"
            );
            for (dry, enhanced) in mixed.iter_mut().zip(enhanced) {
                *dry = *dry * (1.0 - *wet) + enhanced * *wet;
            }
            check_cancel(cancel)?;
            output.push(mixed)
        })
    }
    fn finish(self, cancel: &AtomicBool) -> Result<audio::PcmAudio> {
        check_cancel(cancel)?;
        self.restored.finish()?;
        self.output.finish()
    }
}
fn converted_frames(frames: u64, source: u32, target: u32) -> Result<u64> {
    ensure!(
        frames > 0 && frames <= u64::from(source) * 4 * 60 * 60,
        "Native speech input must be nonempty and <=4 hours"
    );
    Ok((u128::from(frames) * u128::from(target)).div_ceil(u128::from(source)) as u64)
}
fn wet_weight(wet: f32, attenuation: f32) -> Result<f32> {
    ensure!(
        wet.is_finite() && (0.0..=1.0).contains(&wet),
        "Native speech wet mix must be 0..1"
    );
    ensure!(
        attenuation.is_finite() && (0.0..=100.0).contains(&attenuation),
        "Native speech attenuation limit must be 0..100 dB"
    );
    // Same time-domain attenuation-floor blend used by DeepFilterNet enhance:
    // 0 dB is dry, 80 dB retains 1e-4 of the dry signal before wet/dry mix.
    Ok(wet * (1.0 - 10.0f32.powf(-attenuation / 20.0)))
}

pub(super) fn run(
    input: &audio::PcmAudio,
    prepared: PreparedModel,
    budget: Arc<audio::PcmBudget>,
    cancel: &AtomicBool,
    progress: &mut ProgressReporter<'_>,
) -> Result<AudioOutputs> {
    check_cancel(cancel)?;
    let PreparedModel {
        kind,
        model,
        wet,
        threads,
    } = prepared;
    let length = converted_frames(input.frames(), input.sample_rate(), RATE)?;
    progress.report(ProcessingPhase::Processing, 0, Some(input.frames()));
    check_cancel(cancel)?;
    if wet == 0.0 {
        progress.report(
            ProcessingPhase::Processing,
            input.frames(),
            Some(input.frames()),
        );
        return Ok(AudioOutputs {
            denoised: Some(input.clone()),
            ..Default::default()
        });
    }
    // This fallible call must precede *every* ort API. ort's implicit loader
    // panics on a missing library; explicit initialization reports an error.
    let runtime = std::env::var_os("ORT_DYLIB_PATH")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "libonnxruntime.so".into());
    ort::init_from(&runtime).context("Loading native ONNX Runtime; install ONNX Runtime >=1.23 or set ORT_DYLIB_PATH (no Python/network fallback)")?.with_telemetry(false).commit();
    let mut session = Session::builder()?
        .with_optimization_level(GraphOptimizationLevel::Level3)?
        .with_intra_threads(usize::from(threads))?
        .with_inter_threads(1)?
        .commit_from_memory(&model)
        .context("Loading pinned native speech ONNX graph")?;
    ensure!(
        session.inputs().len() == 4 && session.outputs().len() == 4,
        "Native speech graph must have four inputs and outputs"
    );
    let channels = usize::from(input.channels());
    let mut source = Resampler::new(input, RATE, cancel)?;
    let mut states: Vec<_> = (0..channels)
        .map(|_| State::new(kind))
        .collect::<Result<_>>()?;
    let mut stfts: Vec<_> = (0..channels).map(|_| Stft::new(kind)).collect();
    let mut analysis = vec![[0.0; FFT]; channels];
    let mut spectrum = [0.0; BINS * 2];
    let mut block = vec![0.0; HOP * channels];
    let mut output = SpeechOutput::new(input, length, wet, budget, cancel)?;
    for frame in 0..=length.div_ceil(HOP as u64).max(1) {
        check_cancel(cancel)?;
        let center = frame * HOP as u64;
        let emitted = if frame == 0 {
            0
        } else {
            (length - (frame - 1) * HOP as u64).min(HOP as u64) as usize
        };
        for ch in 0..channels {
            if frame == 0 {
                for (i, sample) in analysis[ch].iter_mut().enumerate() {
                    *sample = source.sample(reflect(i as i64 - HOP as i64, length), ch);
                }
            } else {
                analysis[ch].copy_within(HOP.., 0);
                for (i, sample) in analysis[ch].iter_mut().enumerate().skip(HOP) {
                    *sample =
                        source.sample(reflect(center as i64 + i as i64 - HOP as i64, length), ch);
                }
            }
            stfts[ch].analyze(&analysis[ch], &mut spectrum)?;
            states[ch].process(&mut session, &mut spectrum, kind)?;
            check_cancel(cancel)?;
            stfts[ch].synthesize(&spectrum)?;
            for i in 0..emitted {
                block[i * channels + ch] = stfts[ch].sample(i)?;
            }
        }
        if emitted > 0 {
            output.push(&block[..emitted * channels], cancel)?;
        }
        for stft in &mut stfts {
            stft.advance();
        }
        progress.report(
            ProcessingPhase::Processing,
            ((u128::from(center) * u128::from(input.sample_rate()) / u128::from(RATE)) as u64)
                .min(input.frames()),
            Some(input.frames()),
        );
    }
    let output = output.finish(cancel)?;
    ensure!(
        output.frames() == input.frames()
            && output.sample_rate() == input.sample_rate()
            && output.layout() == input.layout(),
        "Native speech output alignment mismatch"
    );
    progress.report(
        ProcessingPhase::Processing,
        input.frames(),
        Some(input.frames()),
    );
    Ok(AudioOutputs {
        denoised: Some(output),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_expansion_checks_cancellation_between_bounded_output_batches() {
        let cancel = AtomicBool::new(false);
        let mut restored =
            StreamingResampler::new(RATE, 384000, 1, HOP as u64, (HOP * 24) as u64, &cancel)
                .unwrap();
        let mut emitted = 0;
        let error = restored
            .push(&[0.25; HOP], &cancel, |position, block| {
                assert_eq!(position, 0);
                emitted += block.len();
                cancel.store(true, std::sync::atomic::Ordering::Release);
                Ok(())
            })
            .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert_eq!(emitted, 4096);
        assert!(restored.finish().is_err());
    }

    fn pcm(rate: u32, data: &[f32], budget: Arc<audio::PcmBudget>) -> audio::PcmAudio {
        let mut builder = audio::PcmBuilder::new(
            rate,
            Arc::from([audio::Channel::FrontLeft, audio::Channel::FrontRight]),
            budget,
        )
        .unwrap();
        builder.push(data).unwrap();
        builder.finish().unwrap()
    }

    #[test]
    fn streamed_restoration_matches_seekable_fir_and_dry_mix_at_every_boundary() {
        let cancel = AtomicBool::new(false);
        for rate in [8000, 16000, 44100, 48000, 384000] {
            for frames in [1, 2, 17, 255, 256, 257, 513, 8195] {
                let budget = audio::PcmBudget::new(8 * 1024 * 1024);
                let dry: Vec<f32> = (0..frames * 2)
                    .map(|i| (i as f32 * 0.037).sin() * 0.2)
                    .collect();
                let input = pcm(rate, &dry, Arc::clone(&budget));
                let original_bytes = budget.used_bytes();
                let length = converted_frames(frames as u64, rate, RATE).unwrap();
                let native: Vec<f32> = (0..length as usize * 2)
                    .map(|i| {
                        if i < 2 || i >= (length as usize - 1) * 2 {
                            if i % 2 == 0 { 0.9 } else { -0.7 }
                        } else {
                            (i as f32 * 0.173).cos() * 0.3
                        }
                    })
                    .collect();
                let reference = pcm(RATE, &native, audio::PcmBudget::new(8 * 1024 * 1024));
                let mut seekable = Resampler::new(&reference, rate, &cancel).unwrap();
                let wet = 0.73;
                let mut output =
                    SpeechOutput::new(&input, length, wet, Arc::clone(&budget), &cancel).unwrap();
                let capacity = output.restored.pending.capacity();
                for block in native.chunks(HOP * 2) {
                    output.push(block, &cancel).unwrap();
                    assert_eq!(output.restored.pending.capacity(), capacity);
                    assert!(
                        output.restored.pending.len()
                            <= (2 * output.restored.kernel.radius as usize + 2) * 2
                    );
                    assert!(budget.used_bytes() <= 2 * original_bytes);
                }
                let output = output.finish(&cancel).unwrap();
                assert_eq!(output.frames(), frames as u64);
                assert_eq!(output.sample_rate(), rate);
                assert_eq!(output.layout(), input.layout());
                assert_eq!(budget.used_bytes(), 2 * original_bytes);
                let mut actual = vec![0.0; dry.len()];
                assert_eq!(output.read_frames(0, &mut actual), frames);
                for frame in 0..frames {
                    for channel in 0..2 {
                        let i = frame * 2 + channel;
                        let expected =
                            dry[i] * (1.0 - wet) + seekable.sample(frame as u64, channel) * wet;
                        assert_eq!(
                            actual[i].to_bits(),
                            expected.to_bits(),
                            "rate {rate}, frames {frames}, sample {i}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn streaming_final_hop_preserves_centered_stft_reflection() {
        let cancel = AtomicBool::new(false);
        for kind in [Kind::Gtcrn, Kind::UlUnas] {
            for length in [1, 2, 255, 256, 257, 511, 512, 513, 777] {
                let samples: Vec<f32> = (0..length).map(|i| (i as f32 * 0.17).sin()).collect();
                let reconstructed = identity(kind, &samples);
                let mut restored =
                    StreamingResampler::new(RATE, RATE, 1, length as u64, length as u64, &cancel)
                        .unwrap();
                let mut actual = Vec::new();
                for hop in reconstructed.chunks(HOP) {
                    restored
                        .push(hop, &cancel, |_, block| {
                            actual.extend_from_slice(block);
                            Ok(())
                        })
                        .unwrap();
                }
                restored.finish().unwrap();
                assert_eq!(actual, reconstructed);
                for (a, b) in actual.into_iter().zip(samples) {
                    assert!((a - b).abs() < 2e-6);
                }
            }
        }
    }

    #[test]
    fn cancellation_discards_partially_streamed_output_and_retry_is_clean() {
        let budget = audio::PcmBudget::new(8 * 1024 * 1024);
        let input = pcm(48000, &vec![0.25; 20000 * 2], Arc::clone(&budget));
        let original_bytes = budget.used_bytes();
        let length = converted_frames(input.frames(), input.sample_rate(), RATE).unwrap();
        let cancel = AtomicBool::new(false);
        let mut output =
            SpeechOutput::new(&input, length, 1.0, Arc::clone(&budget), &cancel).unwrap();
        output.push(&vec![0.5; HOP * 2], &cancel).unwrap();
        assert!(budget.used_bytes() > original_bytes);
        cancel.store(true, std::sync::atomic::Ordering::Release);
        assert!(
            output
                .push(&vec![0.5; HOP * 2], &cancel)
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
        drop(output);
        assert_eq!(budget.used_bytes(), original_bytes);
        cancel.store(false, std::sync::atomic::Ordering::Release);
        let mut output =
            SpeechOutput::new(&input, length, 1.0, Arc::clone(&budget), &cancel).unwrap();
        let samples = vec![0.5; length as usize * 2];
        for hop in samples.chunks(HOP * 2) {
            output.push(hop, &cancel).unwrap();
        }
        let output = output.finish(&cancel).unwrap();
        let mut actual = vec![0.0; 20000 * 2];
        assert_eq!(output.read_frames(0, &mut actual), 20000);
        assert!(actual.into_iter().all(|sample| (sample - 0.5).abs() < 2e-6));
        drop(output);
        assert_eq!(budget.used_bytes(), original_bytes);
    }
    fn identity(kind: Kind, samples: &[f32]) -> Vec<f32> {
        let mut stft = Stft::new(kind);
        let mut output = Vec::new();
        let mut spectrum = [0.0; BINS * 2];
        for frame in 0..=samples.len().div_ceil(HOP).max(1) {
            let input = std::array::from_fn(|i| {
                samples
                    [reflect((frame * HOP + i) as i64 - HOP as i64, samples.len() as u64) as usize]
            });
            stft.analyze(&input, &mut spectrum).unwrap();
            stft.synthesize(&spectrum).unwrap();
            if frame != 0 {
                let emitted = (samples.len() - (frame - 1) * HOP).min(HOP);
                for i in 0..emitted {
                    output.push(stft.sample(i).unwrap());
                }
            }
            stft.advance();
        }
        output
    }
    #[test]
    fn centered_stft_reconstructs_short_hops_and_all_tail_lengths() {
        for kind in [Kind::Gtcrn, Kind::UlUnas] {
            for length in [1, 2, 17, 255, 256, 512, 513, 1023, 1024, 1031]
                .into_iter()
                .chain(257..512)
            {
                let input: Vec<_> = (0..length)
                    .map(|i| ((i as f32 * 0.173).sin() + (i as f32 * 0.031).cos()) * 0.25)
                    .collect();
                let output = identity(kind, &input);
                assert_eq!(input.len(), output.len());
                for (i, (&a, &b)) in input.iter().zip(&output).enumerate() {
                    assert!(
                        (a - b).abs() < 2e-6,
                        "{kind:?} length {length} index {i}: {a} != {b}"
                    );
                }
            }
        }
    }
    #[test]
    fn impulses_at_both_boundaries_are_not_shifted_or_dropped() {
        for kind in [Kind::Gtcrn, Kind::UlUnas] {
            for length in [1, 255, 256, 257, 777] {
                for index in [0, length - 1] {
                    let mut input = vec![0.0; length];
                    input[index] = 1.0;
                    let output = identity(kind, &input);
                    for (&a, &b) in input.iter().zip(&output) {
                        assert!((a - b).abs() < 2e-6);
                    }
                }
            }
        }
    }
    #[test]
    fn reflection_and_control_boundaries_are_exact() {
        assert_eq!(
            (-4..=7).map(|i| reflect(i, 4)).collect::<Vec<_>>(),
            [2, 3, 2, 1, 0, 1, 2, 3, 2, 1, 0, 1]
        );
        assert_eq!(reflect(-100, 1), 0);
        assert_eq!(wet_weight(0.0, 80.0).unwrap(), 0.0);
        assert_eq!(wet_weight(1.0, 0.0).unwrap(), 0.0);
        assert!((wet_weight(1.0, 20.0).unwrap() - 0.9).abs() < 1e-6);
        assert!(wet_weight(f32::NAN, 80.0).is_err());
        assert!(wet_weight(1.0, -1.0).is_err());
        assert_eq!(converted_frames(1, 48000, RATE).unwrap(), 1);
        assert_eq!(converted_frames(769, 48000, RATE).unwrap(), 257);
    }
    #[test]
    fn native_duration_accepts_four_hours_and_rejects_empty_or_longer_input() {
        for source in [8_000, 48_000, 384_000] {
            let maximum = u64::from(source) * 4 * 60 * 60;
            assert_eq!(
                converted_frames(maximum, source, RATE).unwrap(),
                u64::from(RATE) * 4 * 60 * 60
            );
            assert!(converted_frames(0, source, RATE).is_err());
            assert!(converted_frames(maximum + 1, source, RATE).is_err());
        }
    }
    #[test]
    fn polyphase_sinc_preserves_dc_and_has_no_delay() {
        let cancel = AtomicBool::new(false);
        for rate in [8000, 16000, 44100, 48000, 96000] {
            let budget = audio::PcmBudget::new(8 * 1024 * 1024);
            let mut builder =
                audio::PcmBuilder::new(rate, Arc::from([audio::Channel::FrontCenter]), budget)
                    .unwrap();
            builder.push(&vec![0.25; 1001]).unwrap();
            let input = builder.finish().unwrap();
            let mut resampler = Resampler::new(&input, RATE, &cancel).unwrap();
            for i in 0..converted_frames(1001, rate, RATE).unwrap() {
                assert!((resampler.sample(i, 0) - 0.25).abs() < 2e-6);
            }
        }
        let budget = audio::PcmBudget::new(8 * 1024 * 1024);
        let mut builder =
            audio::PcmBuilder::new(48000, Arc::from([audio::Channel::FrontCenter]), budget)
                .unwrap();
        let mut input = vec![0.0; 3001];
        input[1500] = 1.0;
        builder.push(&input).unwrap();
        let input = builder.finish().unwrap();
        let mut resampler = Resampler::new(&input, RATE, &cancel).unwrap();
        let output: Vec<_> = (0..1001).map(|i| resampler.sample(i, 0)).collect();
        assert_eq!(
            output
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
                .unwrap()
                .0,
            500
        );
    }
    #[test]
    fn bundle_paths_reject_traversal_absolute_and_escaping_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        for path in [
            "",
            "../model.onnx",
            "/tmp/model.onnx",
            "a/../../model.onnx",
            "a\\model.onnx",
        ] {
            assert!(relative_path(&root, path).is_err());
        }
        std::os::unix::fs::symlink("/etc/passwd", root.join("escape")).unwrap();
        assert!(relative_path(&root, "escape").is_err());
    }
    #[test]
    fn native_bundle_rejects_modified_pinned_model() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("model.onnx"), b"modified graph").unwrap();
        std::fs::write(directory.path().join("weight-license.txt"), b"notice").unwrap();
        let path = directory.path().join("manifest.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "version":1, "operation":"speech", "id":"GTCRN", "backend":"onnx",
                "model":"model.onnx", "license_evidence":"weight-license.txt",
                "files":{"model.onnx":Kind::Gtcrn.model_hash(),
                         "weight-license.txt":format!("{:x}",Sha256::digest(b"notice"))}
            }))
            .unwrap(),
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        // Preparation consumes this snapshot even if the manifest path changes.
        std::fs::write(&path, b"not a manifest anymore").unwrap();
        for wet in [0.0, 1.0] {
            let config = NeuralConfig {
                speech_wet_mix: wet,
                ..Default::default()
            };
            let mut provenance = ModelProvenance::default();
            let error = prepare(
                &path,
                &bytes,
                &config,
                &AtomicBool::new(false),
                &mut provenance,
            )
            .err()
            .unwrap();
            assert!(error.to_string().contains("SHA256 mismatch"));
            assert_eq!(provenance.id.as_deref(), Some("GTCRN"));
            assert!(provenance.sha256.is_none());
        }
        let error = prepare(
            &path,
            &bytes,
            &NeuralConfig::default(),
            &AtomicBool::new(true),
            &mut ModelProvenance::default(),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("cancelled"));
    }

    #[test]
    fn native_bundle_has_an_independent_cumulative_size_bound() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("a-license"), b"x").unwrap();
        File::create(directory.path().join("b-extra"))
            .unwrap()
            .set_len(MAX_BUNDLE)
            .unwrap();
        let path = directory.path().join("manifest.json");
        let bytes = serde_json::to_vec(&serde_json::json!({
            "version":1, "operation":"speech", "id":"GTCRN", "backend":"onnx",
            "model":"model.onnx", "license_evidence":"a-license",
            "files":{"model.onnx":Kind::Gtcrn.model_hash(),
                     "a-license":format!("{:x}", Sha256::digest(b"x")),
                     "b-extra":"0".repeat(64)}
        }))
        .unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let mut provenance = ModelProvenance::default();
        let error = prepare(
            &path,
            &bytes,
            &NeuralConfig {
                speech_wet_mix: 0.0,
                ..Default::default()
            },
            &AtomicBool::new(false),
            &mut provenance,
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("size bound"));
        assert_eq!(provenance.id.as_deref(), Some("GTCRN"));
        assert!(provenance.sha256.is_none());
    }

    #[test]
    fn prepared_dry_execution_needs_no_bundle_paths_or_onnx_runtime() {
        let budget = audio::PcmBudget::new(64 * 1024);
        let input = pcm(48_000, &[0.25, -0.25, -0.0, 0.0], Arc::clone(&budget));
        let retained = budget.used_bytes();
        let prepared = PreparedModel {
            kind: Kind::Gtcrn,
            model: Vec::new(),
            wet: 0.0,
            threads: 1,
        };
        let (tx, _rx) = async_channel::bounded(8);
        let mut progress = ProgressReporter::new(&tx, 7);
        let result = run(
            &input,
            prepared,
            Arc::clone(&budget),
            &AtomicBool::new(false),
            &mut progress,
        )
        .unwrap();
        let output = result.denoised.unwrap();
        assert_eq!(budget.used_bytes(), retained);
        let mut samples = [0.0; 4];
        assert_eq!(output.read_frames(0, &mut samples), 2);
        assert_eq!(
            samples.map(f32::to_bits),
            [0.25f32, -0.25, -0.0, 0.0].map(f32::to_bits)
        );
    }
    #[test]
    fn downsampling_rejects_above_nyquist_instead_of_aliasing() {
        let budget = audio::PcmBudget::new(8 * 1024 * 1024);
        let mut builder =
            audio::PcmBuilder::new(48000, Arc::from([audio::Channel::FrontCenter]), budget)
                .unwrap();
        let signal: Vec<_> = (0..6001)
            .map(|i| (std::f32::consts::TAU * 0.4 * i as f32).cos())
            .collect();
        builder.push(&signal).unwrap();
        let input = builder.finish().unwrap();
        let mut resampler = Resampler::new(&input, RATE, &AtomicBool::new(false)).unwrap();
        for i in 100..1900 {
            assert!(resampler.sample(i, 0).abs() < 0.002);
        }
    }
}
