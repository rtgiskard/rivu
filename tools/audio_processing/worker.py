#!/usr/bin/env python3
"""Local offline inference worker; stdout is the version2 JSONL event protocol.

Launch with the explicitly prepared interpreter and local request file:
  .cache/neural/moss-venv/bin/python tools/audio_processing/worker.py --request /abs/request.json
  .cache/neural/separate-venv/bin/python tools/audio_processing/worker.py --request /abs/request.json

request.schema.json and event.schema.json define the stable app interface. Only
progress/complete/error objects are written to stdout; diagnostics go to stderr.
Successful outputs also include result.json with model/source/request/worker/code
hashes, dependency pins, effective settings and phase-specific performance data.
The caller publishes complete results only, and cancels by terminating/reaping
the process, then cleaning its scratch directory. Sources are never rewritten.

MossFormer2 uses8s cores plus2s context; Kim/HTDemucs/DF3/NSNet2 use24s cores plus8s
context. Buffers are duration-bounded, not whole-file. Source sample rate,
channels and frame count are exact; explicit zero-phase resampling is compensated
and trimmed. Speech stereo is independent (stereo image may change); multichannel
and mono separation are rejected, never silently downmixed/duplicated. Model
processing is offline, never in an audio callback. There is no automatic model
download, dependency installation, device fallback, peak rescale or clipping.
"""
import argparse
from contextlib import ExitStack, redirect_stdout
import hashlib
import importlib.metadata
import json
import math
import os
from pathlib import Path
import resource
import shutil
import socket
import sys
import time

VERSION = 1
EVENT_VERSION = 2
CORE_SECONDS = 24
CONTEXT_SECONDS = 8
MAX_SECONDS = 4 * 60 * 60
MAX_OUTPUT_BYTES = 8 * 1024**3
MAX_REPORT_BYTES = 8 * 1024**2
RUNTIME_FILES = ("worker.py", "mossformer.py", "nsnet2.py", "request.schema.json", "event.schema.json")

class WorkerError(Exception):
    def __init__(self, code, message):
        super().__init__(message)
        self.code = code

def require(condition, code, message):
    if not condition:
        raise WorkerError(code, message)

def digest(path):
    h = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()

def canonical_digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()

def absolute_path(value, field):
    require(isinstance(value, str) and Path(value).is_absolute(), "invalid_request", field + " must be an absolute local path")
    return Path(value).resolve()

def read_json(path):
    require(path.stat().st_size <= 1024 * 1024, "invalid_request", "JSON exceeds 1 MiB")
    return json.loads(path.read_text())

def validate_request(request):
    require(isinstance(request, dict), "invalid_request", "request must be an object")
    allowed = {"version", "task_id", "operation", "input_path", "output_dir", "model_manifest", "device", "threads", "parameters"}
    require(not (set(request) - allowed), "invalid_request", "unknown request fields: " + str(sorted(set(request) - allowed)))
    require(request.get("version") == VERSION, "invalid_request", "version must be 1")
    require(isinstance(request.get("task_id"), str) and 0 < len(request["task_id"]) <= 128, "invalid_request", "task_id must be 1..128 characters")
    require(request.get("operation") in ("speech", "separate"), "invalid_request", "operation must be speech or separate")
    require(request.get("device", "cpu") in ("cpu", "cuda"), "invalid_request", "device must be cpu or cuda")
    require(type(request.get("threads", 4)) is int and 1 <= request.get("threads", 4) <= 8, "invalid_request", "threads must be 1..8")
    parameters = request.get("parameters", {})
    require(isinstance(parameters, dict), "invalid_request", "parameters must be an object")
    bounds = {"wet_mix":(0,1), "attenuation_limit_db":(0,100)} if request["operation"] == "speech" else {"overlap":(0,0.875)}
    allowed_parameters = set(bounds) | ({"post_filter"} if request["operation"] == "speech" else set())
    require(not (set(parameters)-allowed_parameters), "invalid_request", "unknown operation parameters")
    for name, (low, high) in bounds.items():
        if name in parameters:
            value = parameters[name]
            require(type(value) in (int, float) and math.isfinite(value) and low <= value <= high,
                    "invalid_request", name + " outside supported finite range")
    require("post_filter" not in parameters or type(parameters["post_filter"]) is bool,
            "invalid_request", "post_filter must be boolean")
    for field in ("input_path", "output_dir", "model_manifest"):
        absolute_path(request.get(field), field)
    return request

def validate_artifacts(path, operation):
    """Validate model construction inputs, never authorize inference."""
    manifest = read_json(path)
    require(isinstance(manifest, dict) and manifest.get("version") == 1 and manifest.get("operation") == operation,
            "model_manifest", "manifest version/operation mismatch")
    allowed_models = ("melband_roformer_kim", "htdemucs") if operation == "separate" else ("DeepFilterNet3", "nsnet2", "MossFormer2_SE_48K")
    require(manifest.get("id") in allowed_models, "model_manifest", "unsupported model id for operation")
    packages = manifest.get("packages")
    require(isinstance(packages, dict) and packages and all(isinstance(name, str) and name and isinstance(version, str) and version
            for name, version in packages.items()), "model_manifest", "exact model dependency pins required")
    require(isinstance(manifest.get("files"), dict) and manifest["files"], "model_manifest", "manifest has no hashed files")
    base = path.parent.resolve()
    for name, checksum in manifest["files"].items():
        require(isinstance(name, str) and name and not Path(name).is_absolute() and ".." not in Path(name).parts,
                "model_manifest", "artifact names must be bundle-relative")
        candidate = (base / name).resolve()
        require(candidate.is_relative_to(base) and candidate.is_file(), "model_missing", "missing/nonlocal model artifact: " + name)
        require(isinstance(checksum, str) and digest(candidate) == checksum, "model_hash", "SHA256 mismatch: " + name)
    require(manifest.get("license_evidence") in manifest["files"], "model_license", "hashed factual weight-license/provenance record required; not a permission gate")
    return manifest

def validate_manifest(path, operation):
    """Admit only a complete current provisioned bundle to inference."""
    manifest = validate_artifacts(path, operation)
    base = path.parent.resolve()
    runtime = manifest.get("runtime")
    require(isinstance(runtime, dict), "model_runtime", "provisioned runtime metadata required; run prepare.py or provision.py")
    require(runtime.get("worker") == str(base / "runtime/worker.py") and runtime.get("model_manifest") == str(path.resolve())
            and runtime.get("cwd_independent") is True, "model_runtime", "runtime paths must identify this bundle")
    interpreter = runtime.get("interpreter")
    require(isinstance(interpreter, str) and Path(interpreter).is_absolute() and Path(interpreter).is_file(),
            "model_runtime", "provisioned interpreter missing")
    require(runtime.get("interpreter_sha256") == digest(interpreter), "model_runtime", "provisioned interpreter SHA256 mismatch")
    for name in RUNTIME_FILES:
        require("runtime/" + name in manifest["files"], "model_runtime", "hashed runtime sidecar required: " + name)
    require(runtime.get("worker_notice") in manifest["files"], "model_runtime", "hashed worker notice required")
    dependencies = manifest.get("dependencies")
    require(isinstance(dependencies, dict) and dependencies, "model_dependencies", "provisioned dependency metadata required")
    normalized = {}
    for name, record in dependencies.items():
        require(isinstance(name, str) and name and isinstance(record, dict) and isinstance(record.get("version"), str)
                and record["version"] and isinstance(record.get("notices"), list)
                and isinstance(record.get("notice_status"), str) and record["notice_status"]
                and isinstance(record.get("license_classifiers"), list) and "license_expression" in record,
                "model_dependencies", "incomplete dependency metadata")
        require(all(isinstance(notice, str) and notice in manifest["files"] for notice in record["notices"]),
                "model_dependencies", "dependency notices must be hashed bundle artifacts")
        normalized[name.lower().replace("_", "-").replace(".", "-")] = record["version"]
    for name, version in manifest["packages"].items():
        recorded = normalized.get(name.lower().replace("_", "-").replace(".", "-"))
        require(recorded is not None and recorded.split("+")[0] == version.split("+")[0],
                "model_dependencies", "dependency metadata does not match model pin: " + name)
    return manifest

def deny_network():
    def blocked(*args, **kwargs):
        raise WorkerError("network_forbidden", "inference worker is offline; run explicit preparation separately")
    socket.create_connection = blocked
    socket.socket.connect = blocked
    socket.socket.connect_ex = blocked
    socket.getaddrinfo = blocked
    socket.gethostbyname = blocked
    socket.gethostbyname_ex = blocked
    socket.socket.sendto = blocked

def check_packages(manifest):
    for name, expected in manifest["packages"].items():
        try:
            actual = importlib.metadata.version(name)
        except importlib.metadata.PackageNotFoundError:
            raise WorkerError("dependency_missing", f"install {name}=={expected} with install.py") from None
        require(actual.split("+")[0] == expected.split("+")[0], "dependency_version", f"{name}: expected {expected}, found {actual}")

def resample_exact(audio, source_rate, target_rate):
    """Zero-phase polyphase FIR; output at t=0, no uncompensated filter delay."""
    if source_rate == target_rate:
        return audio
    from scipy.signal import resample_poly
    divisor = math.gcd(source_rate, target_rate)
    return resample_poly(audio, target_rate // divisor, source_rate // divisor, axis=0).astype("float32", copy=False)

def build_engine(manifest, base, device, controls=None):
    controls = controls or {}
    if manifest["id"] == "nsnet2":
        from nsnet2 import load_engine
        return load_engine(manifest, base, device, controls)
    import numpy as np
    import torch
    require(device != "cuda" or torch.cuda.is_available(), "device_unavailable", "CUDA requested but unavailable; no CPU fallback")
    torch.set_num_threads(int(os.environ["OMP_NUM_THREADS"]))
    torch.set_num_interop_threads(1)
    torch.manual_seed(0)
    torch.backends.cuda.matmul.allow_tf32 = False
    torch.backends.cudnn.allow_tf32 = False
    if manifest["id"] == "MossFormer2_SE_48K":
        from mossformer import load_engine
        return load_engine(manifest, base, device, controls)
    if manifest["id"] in ("melband_roformer_kim", "htdemucs"):
        from unblend import Separator
        from unblend.repo import ModelRepository
        repository = ModelRepository(metadata_path=base / "registry.json", extra_models=[])
        model = repository.get_model(manifest["id"])
        separator = Separator(model, device=device, dtype=torch.float32, compile=False,
                              chunk_batch_size=1, custom_kernels=False)
        def run(audio):
            tensor = torch.from_numpy(np.ascontiguousarray(audio.T))
            with torch.inference_mode():
                sources = separator.separate((tensor, 44100), shifts=0, split_overlap=controls.get("overlap", 0.75), seed=0)
            return sources.sources["vocals"].detach().cpu().numpy().T
        return 44100, run
    if manifest["id"] == "DeepFilterNet3":
        # Explicit local directory only. A default model name would download.
        os.environ["DEVICE"] = device
        from df.enhance import init_df, enhance
        from df.model import ModelParams
        from libdf import DF
        model, initial_state, _ = init_df(str(base / "DeepFilterNet3"), log_file=None, log_level="ERROR",
                                         post_filter=controls.get("post_filter", False))
        parameters = ModelParams()
        require(parameters.sr == 48000, "model_manifest", "DeepFilterNet3 config must be 48 kHz")
        def run(audio):
            state = DF(sr=48000, fft_size=parameters.fft_size, hop_size=parameters.hop_size,
                       nb_bands=parameters.nb_erb, min_nb_erb_freqs=parameters.min_nb_freqs)
            with torch.inference_mode():
                return enhance(model, state, torch.from_numpy(np.ascontiguousarray(audio.T)), pad=True,
                               atten_lim_db=controls.get("attenuation_limit_db", 80)).cpu().numpy().T
        return 48000, run

def process(request, emit):
    request = validate_request(request)
    source = absolute_path(request["input_path"], "input_path")
    destination = absolute_path(request["output_dir"], "output_dir")
    manifest_path = absolute_path(request["model_manifest"], "model_manifest")
    require(source.is_file(), "input_missing", "input file does not exist")
    require(not destination.exists(), "output_exists", "output_dir must not already exist")
    require(destination.parent.is_dir(), "output_parent", "output parent must already exist")
    emit("progress", stage="inspect", completed=0, total=1)
    manifest = validate_manifest(manifest_path, request["operation"])
    controls = request.get("parameters", {})
    require("post_filter" not in controls or manifest["id"] == "DeepFilterNet3", "invalid_request", "post_filter is supported only by DeepFilterNet3")
    dry = request["operation"] == "speech" and controls.get("wet_mix", 1.0) == 0.0
    if not dry:
        check_packages(manifest)
    import numpy as np
    import soundfile as sf
    info = sf.info(source)
    require(info.format in ("WAV", "WAVEX", "RF64") and info.subtype == "FLOAT", "input_format", "input must be IEEE float32 WAV/WAVEX/RF64")
    require(8000 <= info.samplerate <= 192000, "input_rate", "supported source rates: 8000..192000 Hz")
    require(info.channels in ((2,) if request["operation"] == "separate" else (1, 2)), "input_channels", "separation requires stereo; speech supports mono/stereo; multichannel downmix is never implicit")
    require(0 < info.frames <= MAX_SECONDS * info.samplerate, "input_duration", "input must be nonempty and <=4 hours")
    stems = ("vocals", "accompaniment") if request["operation"] == "separate" else ("speech",)
    output_bytes = info.frames * info.channels * 4 * len(stems) + 8192
    require(output_bytes <= MAX_OUTPUT_BYTES, "output_capacity", "result exceeds 8 GiB output limit")
    # RIFF's length applies to each file, not the combined separation outputs.
    output_format = "RF64" if info.frames * info.channels * 4 + 1024 >= 2**32 else "WAV"
    require(shutil.disk_usage(destination.parent).free > output_bytes + 256 * 1024**2, "disk_capacity", "insufficient output disk reserve")
    # Bind the input contents before/after inference; publication is denied if they change.
    source_hash = digest(source)
    for key in ("OMP_NUM_THREADS", "MKL_NUM_THREADS", "OPENBLAS_NUM_THREADS"):
        os.environ[key] = str(request.get("threads", 4))
    deny_network()
    model_rate = None
    load_seconds = 0.0
    if not dry:
        emit("progress", stage="load", completed=0, total=1)
        start = time.perf_counter()
        with redirect_stdout(sys.stderr):
            model_rate, infer = build_engine(manifest, manifest_path.parent, request.get("device", "cpu"), controls)
        load_seconds = time.perf_counter() - start
    destination.mkdir(mode=0o700)
    outputs = {stem: str(destination / (stem + ".wav")) for stem in stems}
    core_seconds, context_seconds = (8, 2) if manifest["id"] == "MossFormer2_SE_48K" else (CORE_SECONDS, CONTEXT_SECONDS)
    if dry:
        core_seconds, context_seconds = CORE_SECONDS, 0
    block_times = []
    sample_io_seconds = resampling_seconds = inference_seconds = 0.0
    peaks = dict.fromkeys(stems, 0.0)
    input_energy = output_energy = 0.0
    wall_start = time.perf_counter()
    cpu_start = time.process_time()
    try:
        with ExitStack() as stack:
            reader = stack.enter_context(sf.SoundFile(source))
            writers = {stem: stack.enter_context(sf.SoundFile(path, mode="w", samplerate=info.samplerate,
                       channels=info.channels, format=output_format, subtype="FLOAT"))
                       for stem, path in outputs.items()}
            step = core_seconds * info.samplerate
            context = context_seconds * info.samplerate
            for begin in range(0, info.frames, step):
                end = min(info.frames, begin + step)
                left, right = max(0, begin-context), min(info.frames, end+context)
                io_start = time.perf_counter()
                reader.seek(left)
                audio = reader.read(right-left, dtype="float32", always_2d=True)
                sample_io_seconds += time.perf_counter()-io_start
                require(audio.shape == (right-left, info.channels) and np.isfinite(audio).all(), "input_samples", "input changed/truncated or contains nonfinite samples")
                block_start = time.perf_counter()
                offset = begin-left
                original = audio[offset:offset+end-begin]
                if dry:
                    result = original
                else:
                    phase_start = time.perf_counter()
                    converted = resample_exact(audio, info.samplerate, model_rate)
                    resampling_seconds += time.perf_counter()-phase_start
                    phase_start = time.perf_counter()
                    with redirect_stdout(sys.stderr):
                        inferred = infer(converted)
                    inference_seconds += time.perf_counter()-phase_start
                    require(inferred.shape == converted.shape and np.isfinite(inferred).all(), "inference_output", "model returned wrong length/channels or nonfinite samples")
                    phase_start = time.perf_counter()
                    restored = resample_exact(inferred, model_rate, info.samplerate)
                    resampling_seconds += time.perf_counter()-phase_start
                    require(len(restored) >= len(audio), "alignment", "resampler returned insufficient source frames")
                    restored = restored[:len(audio)]
                    result = restored[offset:offset+end-begin].astype("float32", copy=False)
                    if request["operation"] == "speech":
                        wet = controls.get("wet_mix", 1.0)
                        if wet != 1.0:
                            result = original*(1.0-wet) + result*wet
                    del converted, inferred, restored
                chunks = {"vocals": result, "accompaniment": original-result} if len(stems) == 2 else {"speech":result}
                for stem, chunk in chunks.items():
                    if not dry:
                        require(np.isfinite(chunk).all(), "inference_output", "nonfinite output; refusing sanitization")
                    io_start = time.perf_counter()
                    writers[stem].write(chunk)
                    sample_io_seconds += time.perf_counter()-io_start
                    peaks[stem] = max(peaks[stem], abs(float(np.min(chunk))), abs(float(np.max(chunk))))
                # Accumulate in float64 without two full-size float64 temporaries.
                block_energy = float(np.einsum("ij,ij->", original, original, dtype=np.float64, optimize=False))
                input_energy += block_energy
                output_energy += block_energy if dry else float(np.einsum("ij,ij->", result, result, dtype=np.float64, optimize=False))
                block_times.append(time.perf_counter()-block_start)
                # Views in result/original/chunks otherwise retain the previous
                # context buffers while the next inference allocates its output.
                del audio, result, original, chunks, chunk
                emit("progress", stage="copy" if dry else "inference", completed=end, total=info.frames)
        require(digest(source) == source_hash, "source_changed", "source changed during processing")
        for path in outputs.values():
            output_info = sf.info(path)
            require((output_info.frames, output_info.channels, output_info.samplerate) == (info.frames, info.channels, info.samplerate), "alignment", "written result format/length mismatch")
        wall = time.perf_counter()-wall_start
        report = {"outputs":outputs, "sample_rate":info.samplerate, "channels":info.channels, "frames":info.frames,
                  "model":{"id":manifest["id"], "sha256":canonical_digest(manifest), "files":manifest["files"],
                           "provenance":manifest.get("provenance", {})},
                  "source_sha256":source_hash, "request_sha256":canonical_digest(request),
                  "worker_sha256":digest(__file__), "python":sys.version, "packages":manifest["packages"],
                  "implementation_sha256":{path.name:digest(path) for path in sorted(Path(__file__).parent.glob("*.py"))},
                  "interpreter":{"path":sys.executable,"sha256":digest(sys.executable)},
                  "installed_packages":{} if dry else {name:importlib.metadata.version(name) for name in manifest["packages"]},
                  "device":"cpu" if dry else request.get("device", "cpu"), "threads":request.get("threads", 4),
                  "processing":{"core_seconds":core_seconds,"context_seconds":context_seconds,
                    "inner_overlap":controls.get("overlap",0.75) if len(stems)==2 else None,"shifts":0,"precision":"float32",
                    "parameters":controls,"bypass":dry,"model_executed":not dry,
                    "resampling":None if dry else "scipy zero-phase resample_poly; ceil-length final trim",
                    "accompaniment":"source mixture minus source-rate vocals" if len(stems)==2 else None,
                    "speech_channels":("unchanged dry source" if dry else "independent; stereo image may change") if len(stems)==1 else None},
                  "metrics":{"load_seconds":load_seconds,"processing_wall_seconds":wall,
                    "processing_cpu_seconds":time.process_time()-cpu_start,"real_time_factor":wall/(info.frames/info.samplerate),
                    "inference_seconds":inference_seconds,"resampling_seconds":resampling_seconds,
                    "sample_read_write_seconds":sample_io_seconds,
                    "block_latency_seconds":dict(zip(("p50","p95","p99"),map(float,np.percentile(block_times,[50,95,99])))),
                    "first_block_seconds":block_times[0],"subsequent_block_seconds":block_times[1:],
                    "block_seconds":block_times,"peak_rss_kib":resource.getrusage(resource.RUSAGE_SELF).ru_maxrss,
                    "output_peaks":peaks,"input_rms":math.sqrt(input_energy/(info.frames*info.channels)),
                    "target_rms":math.sqrt(output_energy/(info.frames*info.channels))}}
        with (destination / "result.json").open("wb") as output:
            report_bytes = 1  # Final newline is included in the disk allowance.
            for text in json.JSONEncoder(indent=2, allow_nan=False).iterencode(report):
                chunk = text.encode("utf-8")
                report_bytes += len(chunk)
                require(report_bytes <= MAX_REPORT_BYTES, "report_capacity", "result report exceeds 8 MiB")
                output.write(chunk)
            output.write(b"\n")
        emit("complete", outputs={stem: stem + ".wav" for stem in stems},
             sample_rate=info.samplerate, channels=info.channels, frames=info.frames,
             model={"id":manifest["id"], "sha256":report["model"]["sha256"]})
    except BaseException:
        # Only this newly created task directory is ours; never remove preexisting data.
        shutil.rmtree(destination)
        raise

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--request", type=Path, required=True)
    args = parser.parse_args()
    task_id = ""
    def emit(event_type, **fields):
        print(json.dumps({"version":EVENT_VERSION,"task_id":task_id,"type":event_type,**fields}, allow_nan=False), flush=True)
    try:
        request = read_json(args.request)
        candidate_id = request.get("task_id") if isinstance(request, dict) else None
        task_id = candidate_id if isinstance(candidate_id, str) and 0 < len(candidate_id) <= 128 else ""
        process(request, emit)
        return 0
    except Exception as exc:
        emit("error", code=exc.code if isinstance(exc, WorkerError) else "worker_failure", message=str(exc))
        return 1

if __name__ == "__main__":
    sys.exit(main())
