//! Subprocess scenarios share the input, transport and budget setup, but assert
//! admission rejection separately from outcomes that require a running worker.
use super::*;
use crate::Event;
use rivu_format::config::NeuralConfig;
use std::path::{Path, PathBuf};

pub(super) const MONO: &[audio::Channel] = &[audio::Channel::FrontCenter];
pub(super) const STEREO: &[audio::Channel] =
    &[audio::Channel::FrontLeft, audio::Channel::FrontRight];

// Call once at test entry, outside any loop: bound setup/probe as well as I/O
// and reaping. A watchdog in the Python worker cannot cover all of these.
pub(super) fn in_test_parent() -> bool {
    rivu_utils::test_support::run_in_subprocess(std::time::Duration::from_secs(30))
}

#[derive(Clone, Copy)]
pub(super) enum Worker {
    Wet(&'static str),
    Dry(&'static str),
    RetainedStems,
    RealDry,
    NativeManifest,
}

impl Worker {
    fn is_dry(self) -> bool {
        matches!(self, Self::Dry(_) | Self::RealDry)
    }

    fn install(self, directory: &Path, model_id: &str) -> (NeuralConfig, String) {
        let scripted = |name: &str, manifest: serde_json::Value| {
            let worker = directory.join("pcm_worker.py");
            let path = directory.join(name);
            std::fs::write(&worker, SCRIPTED_WORKER).unwrap();
            std::fs::write(&path, manifest.to_string()).unwrap();
            (worker, PathBuf::from("python3"), path, "a".repeat(64))
        };
        let (worker, interpreter, manifest, hash) = match self {
            Self::Wet(name) | Self::Dry(name) => {
                scripted(name, serde_json::json!({"id": model_id}))
            }
            Self::RetainedStems => {
                scripted("retained-success", serde_json::json!({"id": model_id}))
            }
            Self::NativeManifest => scripted("native", serde_json::json!({"backend": "onnx"})),
            Self::RealDry => return crate::tests::real_dry_speech_bundle(directory),
        };
        let config = NeuralConfig {
            speech_interpreter: interpreter.clone(),
            separation_interpreter: interpreter,
            worker,
            speech_manifest: Some(manifest.clone()),
            separation_manifest: Some(manifest),
            speech_wet_mix: if self.is_dry() { 0.0 } else { 1.0 },
            ..Default::default()
        };
        (config, hash)
    }
}

pub(super) struct Fixture {
    directory: tempfile::TempDir,
    worker: Worker,
    config: NeuralConfig,
    operation: ProcessingOperation,
    retain: ModelOutputRetention,
    model_id: &'static str,
    hash: String,
    budget: Arc<audio::PcmBudget>,
    input: audio::PcmAudio,
    expected: Vec<f32>,
    source_bytes: u64,
}

impl Fixture {
    pub(super) fn new(
        worker: Worker,
        retain: ModelOutputRetention,
        layout: &[audio::Channel],
        frames: usize,
    ) -> Self {
        let (operation, model_id) = match retain {
            ModelOutputRetention::Speech => (ProcessingOperation::Speech, "MossFormer2_SE_48K"),
            _ => (ProcessingOperation::Separate, "melband_roformer_kim"),
        };
        let directory = tempfile::tempdir().unwrap();
        let (config, hash) = worker.install(directory.path(), model_id);
        let expected: Vec<f32> = (0..frames * layout.len())
            .map(|i| {
                if worker.is_dry() && i == 0 {
                    -0.0
                } else {
                    ((i % 17) as f32 - 8.0) / 4.0
                }
            })
            .collect();
        let layout = Arc::<[audio::Channel]>::from(layout);
        let build_input = |budget: &Arc<audio::PcmBudget>| {
            let mut builder =
                audio::PcmBuilder::new(48_000, Arc::clone(&layout), Arc::clone(budget)).unwrap();
            builder.push(&expected).unwrap();
            builder.finish().unwrap()
        };
        let mut budget = audio::PcmBudget::new(64 * 1024 * 1024);
        let mut input = build_input(&budget);
        if matches!(worker, Worker::RetainedStems) {
            // Charge only the source and requested stems, including directory
            // charges. Building the omitted stem and dropping it later cannot fit.
            let limit = input.retained_bytes() * (1 + retained_count(retain));
            drop(input);
            budget = audio::PcmBudget::new(limit);
            input = build_input(&budget);
        }
        assert_eq!(usize::from(input.channels()), layout.len());
        assert_eq!(input.frames(), frames as u64);
        let source_bytes = budget.used_bytes();
        Self {
            directory,
            worker,
            config,
            operation,
            retain,
            model_id,
            hash,
            budget,
            input,
            expected,
            source_bytes,
        }
    }

    fn run(&self) -> (ModelOutcome, async_channel::Receiver<Event>) {
        let (tx, rx) = async_channel::bounded(8);
        let mut progress = ProgressReporter::new(&tx, 7);
        progress.report(ProcessingPhase::Rendering, 0, None);
        let mut provenance = ModelProvenance::default();
        let cancel = Cancellation::new().unwrap();
        let outcome = match prepare(
            self.operation,
            &self.config,
            self.input.sample_rate(),
            self.input.layout(),
            &mut provenance,
            cancel.flag(),
        ) {
            Ok(model) => run(
                ModelRequest {
                    task_id: 7,
                    operation: self.operation,
                    retain: self.retain,
                    input: &self.input,
                    config: &self.config,
                    budget: Arc::clone(&self.budget),
                    cancel: &cancel,
                },
                model,
                &mut progress,
            ),
            Err(error) => ModelOutcome {
                provenance,
                result: Err(error),
            },
        };
        (outcome, rx)
    }

    pub(super) fn rejection(&self) -> anyhow::Error {
        let started = self.directory.path().join("worker-started");
        // Prove this exact interpreter and script can leave the marker. Its
        // absence on the real path is meaningful only together with the error.
        let probe = Command::new(&self.config.speech_interpreter)
            .arg(&self.config.worker)
            .arg("--admission-probe")
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .status()
            .expect("admission fixture requires python3 on PATH");
        assert_eq!(probe.code(), Some(73));
        assert_eq!(std::fs::read(&started).unwrap(), b"python worker entered");
        std::fs::remove_file(&started).unwrap();

        let (outcome, rx) = self.run();
        let error = outcome
            .result
            .err()
            .expect("rejected input must not publish PCM");
        assert!(
            !started.exists(),
            "rejected input started the Python worker"
        );
        assert_eq!(rx.len(), 1);
        assert_eq!(self.budget.used_bytes(), self.source_bytes);
        error
    }

    fn run_worker(&self) -> Result<AudioOutputs> {
        let (outcome, rx) = self.run();
        if !matches!(self.worker, Worker::RealDry) {
            let started = self.directory.path().join("worker-started");
            assert_eq!(std::fs::read(started).unwrap(), b"python worker entered");
        }
        assert_eq!(rx.len(), 2);
        for expected in [ProcessingPhase::Rendering, ProcessingPhase::Processing] {
            assert!(matches!(
                rx.try_recv().unwrap(), Event::Progress { phase, .. } if phase == expected
            ));
        }
        assert_eq!(outcome.provenance.id.as_deref(), Some(self.model_id));
        assert_eq!(
            outcome.provenance.sha256.as_deref(),
            Some(self.hash.as_str())
        );
        outcome.result
    }

    pub(super) fn assert_failure(&self, expected: &str) {
        let error = self
            .run_worker()
            .err()
            .expect("failed worker must not publish PCM");
        let message = format!("{error:#}");
        assert!(
            message.contains(expected),
            "expected {expected:?}, got {message}"
        );
        assert_eq!(self.budget.used_bytes(), self.source_bytes);
    }

    pub(super) fn assert_success(&self) {
        let outputs = self
            .run_worker()
            .expect("neural worker must complete successfully");
        match self.worker {
            Worker::RealDry => assert_eq!(
                self.budget.used_bytes(),
                self.source_bytes,
                "dry output duplicated PCM storage"
            ),
            Worker::RetainedStems => assert_eq!(
                self.budget.used_bytes(),
                self.source_bytes * (1 + retained_count(self.retain))
            ),
            _ => {}
        }
        let retained = self.retain.streams();
        for (pcm, present, gain) in [
            (outputs.denoised, retained[0], 1.0),
            (outputs.vocals, retained[1], 1.0),
            (outputs.accompaniment, retained[2], -1.0),
        ] {
            assert_eq!(pcm.is_some(), present);
            if let Some(pcm) = pcm {
                assert_eq!(pcm.sample_rate(), 48_000);
                assert_eq!(pcm.channels(), self.input.channels());
                assert_eq!(pcm.layout(), self.input.layout());
                assert_eq!(pcm.frames(), self.input.frames());
                let mut actual = vec![0.0; self.expected.len()];
                assert_eq!(pcm.read_frames(0, &mut actual) as u64, self.input.frames());
                for (actual, expected) in actual.iter().zip(&self.expected) {
                    assert_eq!(actual.to_bits(), (expected * gain).to_bits());
                }
            }
        }
    }
}

fn retained_count(retain: ModelOutputRetention) -> u64 {
    retain.streams().into_iter().filter(|&keep| keep).count() as u64
}

// Deliberately independent wire implementation. No model packages required.
const SCRIPTED_WORKER: &str = r#"import json, pathlib, signal, struct, sys
pathlib.Path(__file__).with_name('worker-started').write_bytes(b'python worker entered')
if sys.argv[1:] == ['--admission-probe']:
    raise SystemExit(73)
signal.alarm(10)  # A broken duplex exchange must fail rather than hang the suite.

def read_frame():
    header = sys.stdin.buffer.read(5)
    assert len(header) == 5
    kind, size = struct.unpack('<BI', header)
    payload = sys.stdin.buffer.read(size)
    assert len(payload) == size
    return kind, payload

def send(kind, payload):
    sys.stdout.buffer.write(struct.pack('<BI', kind, len(payload)) + payload)

assert sys.argv[1:] == ['--stream']
kind, payload = read_frame()
assert kind == 0
request = json.loads(payload)
assert request['version'] == 4
assert request['sample_rate'] == 48000
assert request['frames'] > 0
speech = request['operation'] == 'speech'
assert speech or request['operation'] == 'separate'
channels = request['channels']
assert channels in ((1, 2) if speech else (2,))
outcome = request['model_manifest'].rsplit('/', 1)[-1]
streams = [(17, 'speech', 1)] if speech else [(19, 'accompaniment', -1), (18, 'vocals', 1)]
if speech and request['parameters']['wet_mix'] == 0:
    kind, payload = read_frame()
    assert kind == 2 and not payload, 'dry speech must not transport PCM'
    assert sys.stdin.buffer.read() == b''
    send(0, json.dumps(dict(version=4, task_id=request['task_id'], type='progress', stage='inference',
        completed=request['frames'], total=request['frames'],
        model=dict(id='MossFormer2_SE_48K' if speech else 'melband_roformer_kim', sha256='a'*64))).encode())
    if outcome == 'dry-unexpected-pcm':
        send(17, struct.pack('<f', 0))
    send(0, json.dumps(dict(version=4, task_id=request['task_id'], type='complete',
        sample_rate=request['sample_rate'], channels=channels,
        frames=request['frames'] + (1 if outcome == 'dry-misaligned' else 0),
        outputs=dict(speech=True if outcome == 'dry-stream-output' else 'input'),
        model=dict(id='MossFormer2_SE_48K' if speech else 'melband_roformer_kim', sha256='a'*64))).encode())
    sys.stdout.buffer.flush()
    raise SystemExit(7 if outcome == 'dry-nonzero' else 0)
send(0, json.dumps(dict(version=4, task_id=request['task_id'], type='progress', stage='inference',
    completed=0, total=request['frames'], model=dict(id='MossFormer2_SE_48K' if speech else 'melband_roformer_kim', sha256='a'*64))).encode())
sys.stdout.buffer.flush()
pcm = bytearray()
while True:
    kind, payload = read_frame()
    if kind == 2:
        assert not payload
        break
    assert kind == 1
    pcm.extend(payload)
    values = struct.unpack('<%df' % (len(payload) // 4), payload)
    for kind, name, gain in streams:
        data = struct.pack('<%df' % len(values), *(gain * x for x in values))
        if name == 'accompaniment':
            if outcome == 'discard-nonfinite':
                data = struct.pack('<f', float('nan')) + data[4:]
            elif outcome == 'discard-infinity':
                data = struct.pack('<f', float('inf')) + data[4:]
            elif outcome == 'discard-unaligned':
                data = data[:-1]
            elif outcome == 'discard-empty':
                send(kind, b'')
            elif outcome == 'discard-wrong-stream':
                kind = 17
            elif outcome == 'discard-excess':
                data += bytes(channels * 4)
            elif outcome == 'discard-truncated':
                data = data[:-channels * 4]
        for offset in range(0, len(data), 4096):
            send(kind, data[offset:offset + 4096])
    send(0, json.dumps(dict(version=4, task_id=request['task_id'], type='progress', stage='inference',
        completed=len(pcm) // (4 * channels), total=request['frames'],
        model=dict(id='MossFormer2_SE_48K' if speech else 'melband_roformer_kim', sha256='a'*64))).encode())
    sys.stdout.buffer.flush()
assert sys.stdin.buffer.read() == b''
expected = [((i % 17) - 8) / 4 for i in range(request['frames'] * channels)]
assert pcm == struct.pack('<%df' % len(expected), *expected)
if outcome == 'failure-after-pcm':
    send(0, json.dumps(dict(version=4, task_id=request['task_id'], type='error',
        code='inference', message='fixture failure after PCM')).encode())
    sys.stdout.buffer.flush()
    raise SystemExit(7)
if outcome != 'discard-missing-completion':
    outputs = {name: True for _, name, _ in streams}
    if outcome == 'discard-completion-outputs':
        outputs.pop('accompaniment')
    model = dict(id='MossFormer2_SE_48K' if speech else 'melband_roformer_kim', sha256='a'*64)
    if outcome == 'discard-missing-provenance':
        model.pop('sha256')
    send(0, json.dumps(dict(version=4, task_id=request['task_id'], type='complete',
        sample_rate=request['sample_rate'], channels=channels,
        frames=request['frames'] + (1 if outcome == 'discard-completion-frames' else 0),
        outputs=outputs, model=model)).encode())
    if outcome == 'discard-after-completion':
        send(19, bytes(channels * 4))
sys.stdout.buffer.flush()
if outcome == 'complete-then-nonzero':
    raise SystemExit(7)
"#;
