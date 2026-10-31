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
use super::AudioOutputs;
use super::render::{ProgressReporter, check_cancel};
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
use rivu_protocol::model::{ProcessingOperation, ProcessingPhase, ProcessingTaskStatus};
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
enum Kind {
    Gtcrn,
    UlUnas,
}
impl Kind {
    fn from_id(id: &str) -> Result<Self> {
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

fn read_bounded(path: &Path, bound: u64) -> Result<Vec<u8>> {
    let file = File::open(path).with_context(|| format!("Opening {}", path.display()))?;
    ensure!(
        file.metadata()?.len() <= bound,
        "Native model file exceeds size bound"
    );
    let mut bytes = Vec::new();
    file.take(bound + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= bound,
        "Native model file grew beyond size bound"
    );
    Ok(bytes)
}

fn load_bundle(path: &Path, cancel: &AtomicBool) -> Result<(Manifest, Kind, Vec<u8>, String)> {
    let manifest_bytes = read_bounded(path, MAX_MANIFEST)?;
    let manifest: Manifest =
        serde_json::from_slice(&manifest_bytes).context("Reading native speech manifest")?;
    ensure!(
        manifest.version == 1 && manifest.operation == "speech" && manifest.backend == "onnx",
        "Invalid native speech manifest contract"
    );
    let kind = Kind::from_id(&manifest.id)?;
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
        let bytes = read_bounded(&file, MAX_BUNDLE.saturating_sub(total))?;
        total += bytes.len() as u64;
        let actual = format!("{:x}", Sha256::digest(&bytes));
        ensure!(
            actual == *expected,
            "Native model bundle SHA256 mismatch: {name}"
        );
        if name == &manifest.model {
            model = Some(bytes);
        }
    }
    let digest = format!("{:x}", Sha256::digest(&manifest_bytes));
    Ok((
        manifest,
        kind,
        model.context("Missing native ONNX model")?,
        digest,
    ))
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
struct Resampler<'a> {
    pcm: &'a audio::PcmAudio,
    target: u32,
    phases: usize,
    radius: i64,
    kernels: Vec<f32>,
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
impl<'a> Resampler<'a> {
    fn new(pcm: &'a audio::PcmAudio, target: u32, cancel: &AtomicBool) -> Result<Self> {
        let source = pcm.sample_rate();
        ensure!(
            (8_000..=384_000).contains(&source) && (8_000..=384_000).contains(&target),
            "Native speech resampling supports 8000..384000 Hz"
        );
        let cutoff = (target as f64 / source as f64).min(1.0);
        let radius = (16.0 / cutoff).ceil() as i64;
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
            pcm,
            target,
            phases,
            radius,
            kernels,
            cache: vec![0.0; 8192 * usize::from(pcm.channels())],
            start: 0,
            valid: 0,
        })
    }
    fn source(&mut self, index: i64, channel: usize) -> f32 {
        // Constant extension is used by the FIR, while STFT reflection occurs
        // at model-rate sample boundaries separately.
        let index = index.clamp(0, self.pcm.frames() as i64 - 1) as u64;
        if index < self.start || index >= self.start + self.valid as u64 {
            self.start = index.saturating_sub(512);
            self.valid = self.pcm.read_frames(self.start, &mut self.cache);
        }
        self.cache[(index - self.start) as usize * usize::from(self.pcm.channels()) + channel]
    }
    fn sample(&mut self, index: u64, channel: usize) -> f32 {
        let position = u128::from(index) * u128::from(self.pcm.sample_rate());
        let integer = (position / u128::from(self.target)) as i64;
        if self.pcm.sample_rate() == self.target {
            return self.source(integer, channel);
        }
        let remainder = position % u128::from(self.target);
        let phase = (remainder * self.phases as u128 / u128::from(self.target)) as usize;
        let taps = (2 * self.radius + 1) as usize;
        let mut value = 0.0;
        for i in 0..taps {
            let source = self.source(integer + i as i64 - self.radius, channel);
            value += source * self.kernels[phase * taps + i];
        }
        value
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
    config: &NeuralConfig,
    budget: Arc<audio::PcmBudget>,
    cancel: &AtomicBool,
    progress: &mut ProgressReporter<'_>,
    status: &mut ProcessingTaskStatus,
) -> Result<AudioOutputs> {
    check_cancel(cancel)?;
    ensure!(
        status.operation == ProcessingOperation::Speech,
        "Native speech engine cannot perform separation"
    );
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
    ensure!(
        input.layout().as_ref() == [audio::Channel::FrontCenter]
            || input.layout().as_ref() == [audio::Channel::FrontLeft, audio::Channel::FrontRight],
        "Native speech supports mono center or stereo left/right layouts only"
    );
    ensure!(
        (8_000..=384_000).contains(&input.sample_rate()),
        "Native speech supports 8000..384000 Hz source rates"
    );
    let length = converted_frames(input.frames(), input.sample_rate(), RATE)?;
    let wet = wet_weight(config.speech_wet_mix, config.speech_attenuation_limit_db)?;
    let path = config
        .speech_manifest
        .as_ref()
        .context("Missing native speech manifest")?;
    let (manifest, kind, model, hash) = load_bundle(path, cancel)?;
    status.model_id = Some(manifest.id);
    status.model_sha256 = Some(hash);
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
        .with_intra_threads(usize::from(config.threads))?
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
    let mut native = audio::PcmBuilder::new(RATE, Arc::clone(input.layout()), Arc::clone(&budget))?;
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
            native.push(&block[..emitted * channels])?;
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
    let native = native.finish()?;
    ensure!(
        native.frames() == length,
        "Native speech inverse STFT frame alignment mismatch"
    );
    drop(source);
    drop(states);
    drop(stfts);
    drop(session);
    drop(model);
    let mut restored = Resampler::new(&native, input.sample_rate(), cancel)?;
    let mut output =
        audio::PcmBuilder::new(input.sample_rate(), Arc::clone(input.layout()), budget)?;
    let mut dry = vec![0.0; 4096 * channels];
    let mut mixed = vec![0.0; dry.len()];
    let mut position = 0;
    while position < input.frames() {
        check_cancel(cancel)?;
        let count = input.read_frames(position, &mut dry);
        ensure!(count > 0, "Truncated native speech source PCM");
        for i in 0..count {
            for ch in 0..channels {
                let index = i * channels + ch;
                mixed[index] =
                    dry[index] * (1.0 - wet) + restored.sample(position + i as u64, ch) * wet;
            }
        }
        output.push(&mixed[..count * channels])?;
        position += count as u64;
    }
    check_cancel(cancel)?;
    let output = output.finish()?;
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
        let error = load_bundle(&path, &AtomicBool::new(false)).err().unwrap();
        assert!(error.to_string().contains("SHA256 mismatch"));
        let error = load_bundle(&path, &AtomicBool::new(true)).err().unwrap();
        assert!(error.to_string().contains("cancelled"));
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
