//! Bounded duplex PCM transport. Frames are kind:u8 + length:u32-le + payload.
//! JSON (0), input PCM (1), input end (2), speech/vocals/accompaniment PCM (17/18/19).
//! Payloads are at most 64 KiB; PCM is interleaved little-endian float32, not WAV.
use super::render::{ProgressReporter, check_cancel};
use super::{
    AudioOutputs, ModelOutcome, ModelProvenance, ModelRequest, WORKER_EVENT_VERSION,
    WORKER_REQUEST_VERSION,
};
use anyhow::{Context, Result, bail, ensure};
use rivu_audio as audio;
use rivu_protocol::model::{ProcessingOperation, ProcessingPhase};
use std::{
    io::{Read, Write},
    process::{Child, Command, Stdio},
    sync::Arc,
    thread,
    time::Duration,
};

const MAX_FRAME: usize = 65536;
const JSON: u8 = 0;
const INPUT: u8 = 1;
const INPUT_END: u8 = 2;
const SPEECH: u8 = 17;
const VOCALS: u8 = 18;
const ACCOMPANIMENT: u8 = 19;

struct NeuralChild {
    process: Child,
    reaped: bool,
}

impl NeuralChild {
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        let exit = self.process.try_wait()?;
        self.reaped = exit.is_some();
        Ok(exit)
    }
}

impl Drop for NeuralChild {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.process.kill();
            let _ = self.process.wait();
        }
    }
}

fn nonblocking(pipe: &impl std::os::fd::AsRawFd) -> Result<()> {
    let fd = pipe.as_raw_fd();
    // SAFETY: fd belongs to a live borrowed pipe; fcntl does not retain it.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn frame(buffer: &mut Vec<u8>, kind: u8, payload: &[u8]) -> Result<()> {
    ensure!(payload.len() <= MAX_FRAME, "Neural frame exceeds 64 KiB");
    buffer.clear();
    buffer.push(kind);
    buffer.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buffer.extend_from_slice(payload);
    Ok(())
}

#[derive(Default)]
struct Decoder {
    header: [u8; 5],
    header_read: usize,
    payload: Vec<u8>,
    payload_read: usize,
}

impl Decoder {
    fn is_empty(&self) -> bool {
        self.header_read == 0
    }

    // At most one frame/read per call. The caller bounds rounds and checks cancellation.
    fn read(
        &mut self,
        input: &mut impl Read,
        mut consume: impl FnMut(u8, &[u8]) -> Result<()>,
    ) -> Result<Option<bool>> {
        let target = if self.header_read < 5 {
            &mut self.header[self.header_read..]
        } else {
            &mut self.payload[self.payload_read..]
        };
        let n = match input.read(target) {
            Ok(0) => {
                ensure!(self.is_empty(), "Truncated neural frame");
                return Ok(Some(false));
            }
            Ok(n) => n,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => return Ok(Some(true)),
            Err(error) => return Err(error.into()),
        };
        if self.header_read < 5 {
            self.header_read += n;
            if self.header_read == 5 {
                let size = u32::from_le_bytes(self.header[1..].try_into().unwrap()) as usize;
                ensure!(size <= MAX_FRAME, "Neural frame exceeds 64 KiB");
                self.payload.resize(size, 0);
                self.payload_read = 0;
            }
        } else {
            self.payload_read += n;
        }
        if self.header_read == 5 && self.payload_read == self.payload.len() {
            consume(self.header[0], &self.payload)?;
            self.header_read = 0;
        }
        Ok(Some(true))
    }
}

pub(super) struct PreparedModel {
    native: bool,
    id: Option<String>,
    manifest: std::path::PathBuf,
}

pub(super) fn prepare(
    operation: ProcessingOperation,
    config: &rivu_format::config::NeuralConfig,
    sample_rate: u32,
    layout: &[audio::Channel],
    provenance: &mut ModelProvenance,
) -> Result<PreparedModel> {
    ensure!(
        matches!(
            operation,
            ProcessingOperation::Speech | ProcessingOperation::Separate
        ),
        "Invalid model operation"
    );
    let speech = operation == ProcessingOperation::Speech;
    ensure!(
        layout == [audio::Channel::FrontLeft, audio::Channel::FrontRight]
            || (speech && layout == [audio::Channel::FrontCenter]),
        "Neural graph node has unsupported channel layout (speaker layout); speech supports mono/stereo; vocal separation requires a stereo source"
    );
    let manifest = if speech {
        &config.speech_manifest
    } else {
        &config.separation_manifest
    }
    .as_ref()
    .context("Missing model manifest")?;
    let mut contents = Vec::new();
    std::fs::File::open(manifest)
        .context("Opening model manifest")?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut contents)?;
    ensure!(
        contents.len() <= 1024 * 1024,
        "Model manifest exceeds 1 MiB"
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&contents).context("Reading model manifest")?;
    provenance.id = metadata["id"].as_str().map(str::to_owned);
    if let Some(declared) = metadata.get("operation") {
        ensure!(
            declared == if speech { "speech" } else { "separate" },
            "Model manifest operation does not match the requested operation"
        );
    }
    if metadata["backend"] == "onnx" {
        ensure!(speech, "Native ONNX models support speech only");
        ensure!(
            config.device == rivu_format::config::NeuralDevice::Cpu,
            "Native GTCRN/UL-UNAS support CPU only; no device fallback"
        );
        ensure!(
            !config.speech_post_filter,
            "post_filter is supported only by DeepFilterNet3, not native GTCRN/UL-UNAS"
        );
        ensure!(
            (8_000..=384_000).contains(&sample_rate),
            "Native speech supports 8000..384000 Hz source rates"
        );
        #[cfg(feature = "onnx")]
        if let Some(id) = metadata["id"].as_str() {
            super::native_speech::Kind::from_id(id)?;
        }
        #[cfg(not(feature = "onnx"))]
        bail!("Native ONNX inference requires a build with the onnx feature enabled");
    } else {
        ensure!(
            metadata.get("backend").is_none() || metadata["backend"] == "python",
            "Unsupported model backend"
        );
        ensure!(
            (8_000..=192_000).contains(&sample_rate),
            "Python neural models support 8000..192000 Hz source rates"
        );
        ensure!(
            !speech || !config.speech_post_filter || metadata["id"] == "DeepFilterNet3",
            "post_filter is supported only by DeepFilterNet3"
        );
        ensure!(
            config.worker.is_file(),
            "Neural worker does not exist: {}",
            config.worker.display()
        );
        // Reject configured path mismatches before materializing PCM. The worker
        // binds its actual process identity and validates artifact hashes itself.
        if let Some(runtime) = metadata.get("runtime") {
            let interpreter = if speech {
                &config.speech_interpreter
            } else {
                &config.separation_interpreter
            };
            let absolute_interpreter = std::path::absolute(interpreter)?;
            ensure!(
                runtime["worker"].as_str().map(std::path::Path::new)
                    == Some(config.worker.canonicalize()?.as_path())
                    && runtime["model_manifest"].as_str().map(std::path::Path::new)
                        == Some(manifest.canonicalize()?.as_path())
                    && runtime["interpreter"].as_str().map(std::path::Path::new)
                        == Some(absolute_interpreter.as_path()),
                "Configured neural runtime differs from the provisioned model runtime"
            );
        }
    }
    Ok(PreparedModel {
        native: metadata["backend"] == "onnx",
        id: provenance.id.clone(),
        manifest: manifest.clone(),
    })
}

pub(super) fn run(
    request: ModelRequest<'_>,
    mut model: PreparedModel,
    progress: &mut ProgressReporter<'_>,
) -> ModelOutcome {
    let mut provenance = ModelProvenance {
        id: model.id.take(),
        sha256: None,
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_model(request, model, progress, &mut provenance)
    }))
    .unwrap_or_else(|_| Err(anyhow::anyhow!("Audio processing worker panicked")));
    ModelOutcome { provenance, result }
}

fn run_model(
    request: ModelRequest<'_>,
    model: PreparedModel,
    progress: &mut ProgressReporter<'_>,
    provenance: &mut ModelProvenance,
) -> Result<AudioOutputs> {
    let ModelRequest {
        task_id,
        operation,
        input,
        config,
        budget,
        cancel,
    } = request;
    check_cancel(cancel)?;
    let speech = operation == ProcessingOperation::Speech;
    if model.native {
        #[cfg(feature = "onnx")]
        return super::native_speech::run(input, config, budget, cancel, progress, provenance);
        #[cfg(not(feature = "onnx"))]
        bail!("Native ONNX inference requires a build with the onnx feature enabled");
    }
    let mut parameters = if speech {
        serde_json::json!({"wet_mix":config.speech_wet_mix,"attenuation_limit_db":config.speech_attenuation_limit_db})
    } else {
        serde_json::json!({"overlap":config.separation_overlap})
    };
    if speech && config.speech_post_filter {
        parameters["post_filter"] = serde_json::Value::Bool(true);
    }
    let task_id = task_id.to_string();
    let request = serde_json::to_vec(&serde_json::json!({
        "version":WORKER_REQUEST_VERSION,"task_id":task_id,"operation":if speech {"speech"} else {"separate"},
        "sample_rate":input.sample_rate(),"channels":input.channels(),"frames":input.frames(),
        "model_manifest":model.manifest,"parameters":parameters,"threads":config.threads,
        "device":if config.device == rivu_format::config::NeuralDevice::Cpu {"cpu"} else {"cuda"}
    }))?;
    let mut outgoing = Vec::with_capacity(MAX_FRAME + 5);
    frame(&mut outgoing, JSON, &request)?;
    let interpreter = if speech {
        &config.speech_interpreter
    } else {
        &config.separation_interpreter
    };
    let mut child = NeuralChild {
        process: Command::new(interpreter)
            .arg(&config.worker)
            .arg("--stream")
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env("HF_HUB_OFFLINE", "1")
            .env("TRANSFORMERS_OFFLINE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Starting local neural interpreter")?,
        reaped: false,
    };
    let mut diagnostics = Vec::with_capacity(MAX_FRAME);
    let result = (|| -> Result<AudioOutputs> {
        let mut stdin = Some(child.process.stdin.take().context("Missing worker stdin")?);
        let mut stdout = child
            .process
            .stdout
            .take()
            .context("Missing worker stdout")?;
        let mut stderr = child
            .process
            .stderr
            .take()
            .context("Missing worker stderr")?;
        nonblocking(stdin.as_ref().unwrap())?;
        nonblocking(&stdout)?;
        nonblocking(&stderr)?;
        let mut decoder = Decoder::default();
        let mut sent = 0;
        let mut input_frame = 0;
        let mut end_sent = false;
        let channels = usize::from(input.channels());
        let mut samples = vec![0.0f32; 8192 * channels];
        let mut decoded = Vec::with_capacity(MAX_FRAME / 4);
        let mut builders = [None, None, None];
        let mut frames = [0u64; 3];
        let mut complete = None;
        let mut exit = None;
        let mut stdout_eof = false;
        let mut stderr_eof = false;
        let mut block = [0u8; 8192];
        loop {
            check_cancel(cancel)?;
            let mut active = false;
            if stdin.is_some() {
                for _ in 0..8 {
                    if sent == outgoing.len() {
                        if end_sent {
                            stdin.take();
                            break;
                        }
                        if input_frame == input.frames() {
                            frame(&mut outgoing, INPUT_END, &[])?;
                            end_sent = true;
                        } else {
                            let count = input.read_frames(input_frame, &mut samples);
                            ensure!(count > 0, "Truncated retained neural input");
                            outgoing.clear();
                            outgoing.push(INPUT);
                            outgoing
                                .extend_from_slice(&((count * channels * 4) as u32).to_le_bytes());
                            for sample in &samples[..count * channels] {
                                outgoing.extend_from_slice(&sample.to_le_bytes());
                            }
                            input_frame += count as u64;
                        }
                        sent = 0;
                    }
                    match stdin.as_mut().unwrap().write(&outgoing[sent..]) {
                        Ok(0) => bail!("Neural input pipe closed"),
                        Ok(n) => {
                            sent += n;
                            active = true;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {
                            stdin.take();
                            break;
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
            }
            if !stdout_eof {
                for _ in 0..16 {
                    let state = decoder.read(&mut stdout, |kind, payload| {
                        ensure!(complete.is_none(), "Neural data after completion");
                        if kind == JSON {
                            let event: serde_json::Value = serde_json::from_slice(payload)?;
                            ensure!(
                                event["version"] == WORKER_EVENT_VERSION
                                    && event["task_id"] == task_id,
                                "Neural event identity/version mismatch"
                            );
                            if let Some(id) = event["model"]["id"].as_str() {
                                provenance.id = Some(id.to_owned());
                            }
                            if let Some(hash) = event["model"]["sha256"].as_str() {
                                provenance.sha256 = Some(hash.to_owned());
                            }
                            match event["type"].as_str() {
                                Some("progress") => {
                                    #[cfg(test)]
                                    if let Some(triggered) = super::PANIC_ON_NEURAL_PROGRESS.take()
                                    {
                                        triggered.store(true, std::sync::atomic::Ordering::Release);
                                        panic!("injected neural worker progress panic");
                                    }
                                    progress.report(
                                        ProcessingPhase::Processing,
                                        event["completed"].as_u64().unwrap_or(0),
                                        event["total"].as_u64(),
                                    );
                                }
                                Some("error") => {
                                    bail!("Neural processing failed: {}", event["message"])
                                }
                                Some("complete") => complete = Some(event),
                                _ => bail!("Unknown neural event type"),
                            }
                        } else {
                            let index = match kind {
                                SPEECH if speech => 0,
                                VOCALS if !speech => 1,
                                ACCOMPANIMENT if !speech => 2,
                                _ => bail!("Unexpected neural PCM stream"),
                            };
                            ensure!(
                                !payload.is_empty() && payload.len() % (channels * 4) == 0,
                                "Unaligned neural PCM frame"
                            );
                            let count = (payload.len() / (channels * 4)) as u64;
                            ensure!(
                                count <= input.frames().saturating_sub(frames[index]),
                                "Neural output exceeds source length"
                            );
                            decoded.clear();
                            decoded.extend(
                                payload
                                    .as_chunks::<4>()
                                    .0
                                    .iter()
                                    .map(|bytes| f32::from_le_bytes(*bytes)),
                            );
                            if builders[index].is_none() {
                                builders[index] = Some(audio::PcmBuilder::new(
                                    input.sample_rate(),
                                    Arc::clone(input.layout()),
                                    Arc::clone(&budget),
                                )?);
                            }
                            builders[index].as_mut().unwrap().push(&decoded)?;
                            frames[index] += count;
                        }
                        Ok(())
                    })?;
                    match state {
                        Some(false) => {
                            stdout_eof = true;
                            break;
                        }
                        Some(true) => active = true,
                        None => break,
                    }
                }
            }
            if !stderr_eof {
                for _ in 0..8 {
                    match stderr.read(&mut block) {
                        Ok(0) => {
                            stderr_eof = true;
                            break;
                        }
                        Ok(n) => {
                            let keep = n.min(MAX_FRAME.saturating_sub(diagnostics.len()));
                            diagnostics.extend_from_slice(&block[..keep]);
                            active = true;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) => return Err(error.into()),
                    }
                }
            }
            if exit.is_none() {
                exit = child.try_wait()?;
            }
            // stdout EOF is not process exit: continue checking cancellation until reaped.
            if stdout_eof
                && stderr_eof
                && let Some(exit) = exit
            {
                ensure!(exit.success(), "Neural interpreter failed: {exit}");
                break;
            }
            if !active {
                thread::sleep(Duration::from_millis(2));
            }
        }
        check_cancel(cancel)?;
        let event = complete.context("Neural worker exited without completion")?;
        ensure!(
            end_sent && sent == outgoing.len(),
            "Neural worker did not consume all input"
        );
        ensure!(
            event["sample_rate"].as_u64() == Some(u64::from(input.sample_rate()))
                && event["channels"].as_u64() == Some(u64::from(input.channels()))
                && event["frames"].as_u64() == Some(input.frames()),
            "Neural completion alignment mismatch"
        );
        let expected: &[(usize, &str)] = if speech {
            &[(0, "speech")]
        } else {
            &[(1, "vocals"), (2, "accompaniment")]
        };
        ensure!(
            event["outputs"].as_object().map(|outputs| outputs.len()) == Some(expected.len()),
            "Neural completion outputs mismatch"
        );
        for &(index, key) in expected {
            ensure!(
                frames[index] == input.frames() && event["outputs"][key] == true,
                "Missing or truncated neural output: {key}"
            );
        }
        ensure!(event["model"]["id"].as_str().is_some(), "Missing model id");
        ensure!(
            event["model"]["sha256"].as_str().is_some(),
            "Missing model hash"
        );
        Ok(AudioOutputs {
            denoised: builders[0]
                .take()
                .map(audio::PcmBuilder::finish)
                .transpose()?,
            vocals: builders[1]
                .take()
                .map(audio::PcmBuilder::finish)
                .transpose()?,
            accompaniment: builders[2]
                .take()
                .map(audio::PcmBuilder::finish)
                .transpose()?,
        })
    })();
    result.with_context(|| {
        format!(
            "Local neural worker diagnostics: {}",
            String::from_utf8_lossy(&diagnostics)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Cursor, sync::atomic::AtomicBool};

    #[test]
    fn runtime_preflight_binds_worker_manifest_and_lexical_interpreter_identity() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let manifest = directory.path().join("manifest.json");
        let worker = directory.path().join("worker.py");
        let other_worker = directory.path().join("other-worker.py");
        let interpreter = directory.path().join("venv-python");
        let other_interpreter = directory.path().join("other-venv-python");
        std::fs::write(&worker, b"not executed by preflight").unwrap();
        std::fs::write(&other_worker, b"not executed by preflight").unwrap();
        let executable = std::env::current_exe().unwrap();
        symlink(&executable, &interpreter).unwrap();
        symlink(&executable, &other_interpreter).unwrap();
        let config = rivu_format::config::NeuralConfig {
            worker: worker.clone(),
            speech_manifest: Some(manifest.clone()),
            speech_interpreter: interpreter.clone(),
            ..Default::default()
        };
        let write_manifest = |runtime: &serde_json::Value| {
            std::fs::write(
                &manifest,
                serde_json::json!({
                    "id":"DeepFilterNet3", "operation":"speech", "runtime":runtime,
                })
                .to_string(),
            )
            .unwrap();
        };
        let runtime = serde_json::json!({
            "worker":worker, "model_manifest":manifest, "interpreter":interpreter,
        });
        write_manifest(&runtime);
        let prepare_model = |config: &rivu_format::config::NeuralConfig| {
            prepare(
                ProcessingOperation::Speech,
                config,
                48_000,
                &[audio::Channel::FrontCenter],
                &mut ModelProvenance::default(),
            )
        };
        assert!(prepare_model(&config).is_ok());
        let mut changed = config.clone();
        changed.worker = other_worker;
        assert!(
            prepare_model(&changed)
                .err()
                .unwrap()
                .to_string()
                .contains("Configured neural runtime")
        );
        changed = config.clone();
        changed.speech_interpreter = other_interpreter;
        // Canonicalizing both would wrongly accept two distinct virtualenv paths.
        assert_eq!(
            changed.speech_interpreter.canonicalize().unwrap(),
            interpreter.canonicalize().unwrap()
        );
        assert!(
            prepare_model(&changed)
                .err()
                .unwrap()
                .to_string()
                .contains("Configured neural runtime")
        );
        let mut changed_runtime = runtime;
        changed_runtime["model_manifest"] = serde_json::json!(directory.path().join("other.json"));
        write_manifest(&changed_runtime);
        assert!(
            prepare_model(&config)
                .err()
                .unwrap()
                .to_string()
                .contains("Configured neural runtime")
        );
    }

    #[test]
    fn subprocess_success_preserves_pcm_and_maps_stems() {
        subprocess_fixture(ProcessingOperation::Separate, 2, 8201, "success", None);
    }

    #[test]
    fn subprocess_failure_after_pcm_discards_unpublished_output() {
        subprocess_fixture(
            ProcessingOperation::Speech,
            1,
            8201,
            "failure-after-pcm",
            None,
        );
    }

    #[cfg(not(feature = "onnx"))]
    #[test]
    fn disabled_onnx_rejects_native_manifest_without_python_fallback() {
        subprocess_fixture(ProcessingOperation::Speech, 1, 1, "onnx-disabled", None);
    }

    #[cfg(feature = "onnx")]
    #[test]
    fn enabled_onnx_rejects_invalid_native_manifest_without_python_fallback() {
        subprocess_fixture(ProcessingOperation::Speech, 1, 1, "onnx-invalid", None);
    }

    #[test]
    fn subprocess_complete_then_nonzero_exit_is_failure() {
        subprocess_fixture(
            ProcessingOperation::Speech,
            1,
            8201,
            "complete-then-nonzero",
            None,
        );
    }

    #[test]
    fn subprocess_multi_window_duplex_backpressure_preserves_pcm() {
        // More than the Linux maximum default pipe capacity in both directions;
        // the worker flushes each output window before reading the next input.
        subprocess_fixture(ProcessingOperation::Speech, 1, 524_325, "success", None);
    }

    #[test]
    fn neural_rejects_wrong_speakers_and_reversed_stereo_without_starting_worker() {
        use audio::Channel::*;
        for (operation, layout) in [
            (ProcessingOperation::Speech, vec![FrontLeft]),
            (ProcessingOperation::Speech, vec![RearLeft, RearRight]),
            (ProcessingOperation::Separate, vec![FrontRight, FrontLeft]),
        ] {
            subprocess_fixture(
                operation,
                layout.len() as u16,
                1,
                "unsupported-layout",
                Some(Arc::from(layout)),
            );
        }
    }

    fn subprocess_fixture(
        operation: ProcessingOperation,
        channels: u16,
        frames: usize,
        outcome: &str,
        layout: Option<Arc<[audio::Channel]>>,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let worker = directory.path().join("pcm_worker.py");
        let started = directory.path().join("worker-started");
        // Requires python3 on PATH; only the standard library is used, not models.
        std::fs::write(
            &worker,
            format!(r#"import json, pathlib, signal, struct, sys
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
assert request['version'] == {WORKER_REQUEST_VERSION}
assert request['sample_rate'] == 48000
assert request['frames'] > 0
speech = request['operation'] == 'speech'
assert speech or request['operation'] == 'separate'
channels = request['channels']
assert channels == (1 if speech else 2)
outcome = request['model_manifest'].rsplit('/', 1)[-1]
streams = [(17, 'speech', 1)] if speech else [(19, 'accompaniment', -1), (18, 'vocals', 1)]
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
        for offset in range(0, len(data), 4096):
            send(kind, data[offset:offset + 4096])
    send(0, json.dumps(dict(version={WORKER_EVENT_VERSION}, task_id=request['task_id'], type='progress',
        completed=len(pcm) // (4 * channels), total=request['frames'],
        model=dict(id='stdlib-fixture', sha256='fixture-hash'))).encode())
    sys.stdout.buffer.flush()
assert sys.stdin.buffer.read() == b''
expected = [((i % 17) - 8) / 4 for i in range(request['frames'] * channels)]
assert pcm == struct.pack('<%df' % len(expected), *expected)
if outcome == 'failure-after-pcm':
    send(0, json.dumps(dict(version={WORKER_EVENT_VERSION}, task_id=request['task_id'], type='error',
        code='inference', message='fixture failure after PCM')).encode())
    sys.stdout.buffer.flush()
    raise SystemExit(7)
send(0, json.dumps(dict(version={WORKER_EVENT_VERSION}, task_id=request['task_id'], type='complete',
    sample_rate=request['sample_rate'], channels=channels, frames=request['frames'],
    outputs={{name: True for _, name, _ in streams}},
    model=dict(id='stdlib-fixture', sha256='fixture-hash'))).encode())
sys.stdout.buffer.flush()
if outcome == 'complete-then-nonzero':
    raise SystemExit(7)
"#),
        )
        .unwrap();
        // Exercise this exact interpreter/script before using absence of its marker.
        // Also require the admission error below, so an interpreter failure or a
        // child killed before Python executes cannot masquerade as valid rejection.
        let probe = Command::new("python3")
            .arg(&worker)
            .arg("--admission-probe")
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .status()
            .expect("admission fixture requires python3 on PATH");
        assert_eq!(probe.code(), Some(73));
        assert_eq!(std::fs::read(&started).unwrap(), b"python worker entered");
        std::fs::remove_file(&started).unwrap();
        let manifest = if matches!(outcome, "onnx-disabled" | "onnx-invalid") {
            &b"{\"backend\":\"onnx\"}"[..]
        } else {
            &b"{\"id\":\"stdlib-fixture\",\"sha256\":\"fixture-hash\"}"[..]
        };
        std::fs::write(directory.path().join(outcome), manifest).unwrap();
        let config = rivu_format::config::NeuralConfig {
            speech_interpreter: "python3".into(),
            separation_interpreter: "python3".into(),
            worker,
            speech_manifest: Some(directory.path().join(outcome)),
            separation_manifest: Some(directory.path().join(outcome)),
            ..Default::default()
        };
        let budget = audio::PcmBudget::new(64 * 1024 * 1024);
        let expected: Vec<f32> = (0..frames * usize::from(channels))
            .map(|i| ((i % 17) as f32 - 8.0) / 4.0)
            .collect();
        let layout = layout.unwrap_or_else(|| {
            if channels == 1 {
                Arc::from([audio::Channel::FrontCenter])
            } else {
                Arc::from([audio::Channel::FrontLeft, audio::Channel::FrontRight])
            }
        });
        let mut builder = audio::PcmBuilder::new(48_000, layout, Arc::clone(&budget)).unwrap();
        builder.push(&expected).unwrap();
        let input = builder.finish().unwrap();
        let source_bytes = budget.used_bytes();
        let (tx, rx) = async_channel::bounded(8);
        let mut progress = ProgressReporter::new(&tx, 7);
        progress.report(ProcessingPhase::Rendering, 0, None);
        let mut prepared_provenance = ModelProvenance::default();
        let outcome_result = match prepare(
            operation,
            &config,
            input.sample_rate(),
            input.layout(),
            &mut prepared_provenance,
        ) {
            Ok(model) => run(
                ModelRequest {
                    task_id: 7,
                    operation,
                    input: &input,
                    config: &config,
                    budget: Arc::clone(&budget),
                    cancel: &AtomicBool::new(false),
                },
                model,
                &mut progress,
            ),
            Err(error) => ModelOutcome {
                provenance: prepared_provenance,
                result: Err(error),
            },
        };
        let result = outcome_result.result;
        let provenance = outcome_result.provenance;
        if matches!(
            outcome,
            "unsupported-layout" | "onnx-disabled" | "onnx-invalid"
        ) {
            let error = match result {
                Ok(_) => panic!("rejected input must not publish PCM"),
                Err(error) => error,
            };
            if outcome == "unsupported-layout" {
                assert!(error.to_string().contains("speaker layout"), "{error:#}");
            } else if outcome == "onnx-disabled" {
                assert!(error.to_string().contains("onnx feature"), "{error:#}");
            } else {
                // The native consumer must parse the manifest, rather than try Python.
                assert!(
                    error.downcast_ref::<serde_json::Error>().is_some(),
                    "{error:#}"
                );
            }
            assert!(
                !started.exists(),
                "rejected input started the Python worker"
            );
            assert_eq!(rx.len(), 1);
            assert_eq!(budget.used_bytes(), source_bytes);
            return;
        }
        assert_eq!(std::fs::read(&started).unwrap(), b"python worker entered");
        assert_eq!(rx.len(), 2);
        assert!(matches!(
            rx.try_recv().unwrap(),
            super::super::Event::Progress {
                phase: ProcessingPhase::Rendering,
                ..
            }
        ));
        assert!(matches!(
            rx.try_recv().unwrap(),
            super::super::Event::Progress {
                phase: ProcessingPhase::Processing,
                ..
            }
        ));
        if outcome != "success" {
            let error = match result {
                Ok(_) => panic!("failed worker must not publish PCM"),
                Err(error) => format!("{error:#}"),
            };
            if outcome == "failure-after-pcm" {
                assert!(error.contains("fixture failure after PCM"), "{error}");
            }
            assert_eq!(budget.used_bytes(), source_bytes);
            assert_eq!(provenance.id.as_deref(), Some("stdlib-fixture"));
            assert_eq!(provenance.sha256.as_deref(), Some("fixture-hash"));
            return;
        }
        let outputs = result
            .expect("stdlib neural worker must complete successfully (requires python3 on PATH)");
        assert_eq!(provenance.id.as_deref(), Some("stdlib-fixture"));
        assert_eq!(provenance.sha256.as_deref(), Some("fixture-hash"));
        let speech = operation == ProcessingOperation::Speech;
        for (pcm, present, gain) in [
            (outputs.denoised, speech, 1.0),
            (outputs.vocals, !speech, 1.0),
            (outputs.accompaniment, !speech, -1.0),
        ] {
            assert_eq!(pcm.is_some(), present);
            if let Some(pcm) = pcm {
                assert_eq!(pcm.sample_rate(), 48_000);
                assert_eq!(pcm.channels(), channels);
                assert_eq!(pcm.layout(), input.layout());
                assert_eq!(pcm.frames(), frames as u64);
                let mut actual = vec![0.0; expected.len()];
                assert_eq!(pcm.read_frames(0, &mut actual), frames);
                for (actual, expected) in actual.iter().zip(&expected) {
                    assert_eq!(actual.to_bits(), (expected * gain).to_bits());
                }
            }
        }
    }

    #[test]
    fn framing_rejects_oversized_and_every_truncated_prefix() {
        let mut decoder = Decoder::default();
        let mut oversized = Cursor::new([JSON, 1, 0, 1, 0]);
        assert!(decoder.read(&mut oversized, |_, _| Ok(())).is_err());
        let wire = [JSON, 2, 0, 0, 0, b'{', b'}'];
        for end in 1..wire.len() {
            let mut decoder = Decoder::default();
            let mut truncated = Cursor::new(&wire[..end]);
            loop {
                match decoder.read(&mut truncated, |_, _| panic!("accepted truncated frame")) {
                    Ok(Some(true)) => {}
                    Err(_) => break,
                    other => panic!("unexpected truncated-frame result: {other:?}"),
                }
            }
        }
    }

    #[test]
    fn framing_preserves_wire_bits_across_short_reads_and_transient_errors() {
        struct Fragmented {
            input: Cursor<Vec<u8>>,
            calls: usize,
        }
        impl Read for Fragmented {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.calls += 1;
                match self.calls {
                    2 => Err(std::io::ErrorKind::WouldBlock.into()),
                    4 => Err(std::io::ErrorKind::Interrupted.into()),
                    _ => {
                        let count = buffer.len().min(3);
                        self.input.read(&mut buffer[..count])
                    }
                }
            }
        }
        // Independent wire oracle: -0.0, 1.25, -1.25 as little-endian f32.
        let payload = [0, 0, 0, 128, 0, 0, 160, 63, 0, 0, 160, 191];
        let mut wire = vec![SPEECH, 12, 0, 0, 0];
        wire.extend_from_slice(&payload);
        wire.extend_from_slice(&[INPUT_END, 0, 0, 0, 0]);
        let mut encoded = Vec::new();
        frame(&mut encoded, SPEECH, &payload).unwrap();
        assert_eq!(encoded, wire[..17]);
        let mut input = Fragmented {
            input: Cursor::new(wire),
            calls: 0,
        };
        let mut decoder = Decoder::default();
        let mut observed = Vec::new();
        loop {
            let state = decoder
                .read(&mut input, |kind, data| {
                    observed.push((kind, data.to_vec()));
                    Ok(())
                })
                .unwrap();
            if state == Some(false) {
                break;
            }
        }
        assert_eq!(observed, [(SPEECH, payload.to_vec()), (INPUT_END, vec![])]);
        assert!(decoder.is_empty());
    }

    #[test]
    fn framing_accepts_exact_limit_and_rejects_oversized_output() {
        let payload = vec![42; MAX_FRAME];
        let mut encoded = Vec::new();
        frame(&mut encoded, SPEECH, &payload).unwrap();
        assert_eq!(&encoded[..5], &[SPEECH, 0, 0, 1, 0]);
        let mut input = Cursor::new(encoded);
        let mut decoder = Decoder::default();
        let mut received = 0;
        while decoder
            .read(&mut input, |kind, data| {
                assert_eq!(kind, SPEECH);
                assert_eq!(data, payload);
                received += 1;
                Ok(())
            })
            .unwrap()
            == Some(true)
        {}
        assert_eq!(received, 1);
        assert!(frame(&mut Vec::new(), SPEECH, &vec![0; MAX_FRAME + 1]).is_err());
    }
}
