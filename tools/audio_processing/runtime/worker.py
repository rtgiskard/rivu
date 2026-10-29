#!/usr/bin/env python3
"""Read-only offline inference over bounded framed raw PCM stdin/stdout.

Invoke the prepared interpreter with worker.py --stream. Each frame is a one-byte
kind, a uint32 little-endian payload size, and at most 65536 payload bytes. Kind 0
is UTF-8 JSON (request v2/events v3), 1 is interleaved float32-le input, and 2 is
the empty input-end marker, which must be followed immediately by stdin EOF.
Each process handles one task; the producer must close stdin without waiting for
completion. Output kinds 17/18/19 are speech/vocals/accompaniment
PCM at the exact source format. Only explicit export.py/evaluation writes files.

The sequential context reader retains at most one bounded model window, not the
whole signal. MossFormer2 uses 8s cores plus 2s context; other models use 24s plus
8s. Algorithms, compensated resampling, independent speech channels, exact stem
complement and unclipped samples are unchanged. No network/device fallback.
"""
import argparse
from contextlib import redirect_stdout
import hashlib
import importlib.metadata
import json
import math
import os
from pathlib import Path
import resource
import socket
import sys
import time
import struct

sys.dont_write_bytecode = True
VERSION = 2
EVENT_VERSION = 3
CORE_SECONDS = 24
CONTEXT_SECONDS = 8
MAX_SECONDS = 4 * 60 * 60
MAX_FRAME_BYTES = 65536
MAX_WINDOW_BYTES = 128 * 1024**2
MAX_REPORT_BYTES = 8 * 1024**2
JSON_FRAME, INPUT_PCM, INPUT_END = 0, 1, 2
STEM_KINDS = {"speech":17, "vocals":18, "accompaniment":19}
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
    allowed = {"version", "task_id", "operation", "sample_rate", "channels", "frames", "model_manifest", "device", "threads", "parameters"}
    require(not (set(request) - allowed), "invalid_request", "unknown request fields: " + str(sorted(set(request) - allowed)))
    require(request.get("version") == VERSION, "invalid_request", "version must be 2")
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
    absolute_path(request.get("model_manifest"), "model_manifest")
    rate, channels, frames = (request.get(key) for key in ("sample_rate", "channels", "frames"))
    require(type(rate) is int and 8000 <= rate <= 192000, "input_rate", "supported source rates: 8000..192000 Hz")
    require(type(channels) is int and channels in ((2,) if request["operation"] == "separate" else (1, 2)),
            "input_channels", "separation requires stereo; speech supports mono/stereo; multichannel downmix is never implicit")
    require(type(frames) is int and 0 < frames <= MAX_SECONDS * rate, "input_duration", "input must be nonempty and <=4 hours")
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
        if __package__:
            from .nsnet2 import load_engine
        else:
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
        if __package__:
            from .mossformer import load_engine
        else:
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

def read_exact(stream, count):
    require(0 <= count <= MAX_FRAME_BYTES, "protocol", "frame exceeds 64 KiB")
    data = bytearray(count)
    offset = 0
    while offset < count:
        chunk = stream.read(count-offset)
        require(bool(chunk), "protocol", "truncated frame")
        data[offset:offset+len(chunk)] = chunk
        offset += len(chunk)
    return data


def read_frame(stream):
    header = read_exact(stream, 5)
    kind, size = struct.unpack("<BI", header)
    return kind, read_exact(stream, size)


def write_frame(stream, kind, payload):
    require(len(payload) <= MAX_FRAME_BYTES, "protocol", "frame exceeds 64 KiB")
    stream.write(struct.pack("<BI", kind, len(payload)))
    stream.write(payload)
    stream.flush()


class StreamInput:
    """Sequential input with bounded overlap retention and no seekable-file proxy."""
    def __init__(self, stream, request):
        self.stream = stream
        self.frames, self.channels = request["frames"], request["channels"]
        self.buffer = None
        self.left = self.right = self.received = 0
        self.pending = memoryview(b"")
        self.hasher = hashlib.sha256()

    def window(self, left, right):
        import numpy as np
        require(self.left <= left <= self.right <= right <= self.frames, "protocol", "nonsequential input window")
        size = (right-left)*self.channels*4
        require(size <= MAX_WINDOW_BYTES, "memory_capacity", "model context window exceeds 128 MiB")
        audio = np.empty((right-left, self.channels), dtype="<f4")
        retained = self.right-left
        if retained:
            audio[:retained] = self.buffer[left-self.left:]
        target = memoryview(audio).cast("B")[retained*self.channels*4:]
        offset = 0
        while offset < len(target):
            if not self.pending:
                kind, payload = read_frame(self.stream)
                require(kind == INPUT_PCM and len(payload) and len(payload) % (self.channels*4) == 0,
                        "protocol", "expected aligned input PCM")
                self.received += len(payload)//(self.channels*4)
                require(self.received <= self.frames, "protocol", "input exceeds declared frames")
                self.hasher.update(payload)
                self.pending = memoryview(payload)
            count = min(len(target)-offset, len(self.pending))
            target[offset:offset+count] = self.pending[:count]
            self.pending = self.pending[count:]
            offset += count
        self.buffer, self.left, self.right = audio, left, right
        return audio

    def finish(self):
        require(self.right == self.frames and self.received == self.frames and not self.pending,
                "protocol", "input frame count mismatch")
        kind, payload = read_frame(self.stream)
        require(kind == INPUT_END and not payload, "protocol", "expected empty input end marker")
        require(not self.stream.read(1), "protocol", "unexpected data after input end marker")
        self.buffer = None
        return self.hasher.hexdigest()


def _process_validated(request, source, write_pcm, emit):
    """Infer an entrypoint-validated request; emit PCM and return evidence in RAM."""
    rate, channels, frames = (request[key] for key in ("sample_rate", "channels", "frames"))
    manifest_path = Path(request["model_manifest"]).resolve()
    emit("progress", stage="inspect", completed=0, total=1)
    manifest = validate_manifest(manifest_path, request["operation"])
    controls = request.get("parameters", {})
    require("post_filter" not in controls or manifest["id"] == "DeepFilterNet3", "invalid_request", "post_filter is supported only by DeepFilterNet3")
    dry = request["operation"] == "speech" and controls.get("wet_mix", 1.0) == 0.0
    if not dry:
        check_packages(manifest)
    for key in ("OMP_NUM_THREADS", "MKL_NUM_THREADS", "OPENBLAS_NUM_THREADS"):
        os.environ[key] = str(request.get("threads", 4))
    os.environ["HF_HUB_OFFLINE"] = os.environ["TRANSFORMERS_OFFLINE"] = "1"
    deny_network()
    import numpy as np
    model_rate = None
    load_seconds = 0.0
    if not dry:
        emit("progress", stage="load", completed=0, total=1)
        start = time.perf_counter()
        with redirect_stdout(sys.stderr):
            model_rate, infer = build_engine(manifest, manifest_path.parent, request.get("device", "cpu"), controls)
        load_seconds = time.perf_counter() - start
    stems = ("vocals", "accompaniment") if request["operation"] == "separate" else ("speech",)
    core_seconds, context_seconds = (8, 2) if manifest["id"] == "MossFormer2_SE_48K" else (CORE_SECONDS, CONTEXT_SECONDS)
    if dry:
        core_seconds, context_seconds = CORE_SECONDS, 0
    require((core_seconds+2*context_seconds)*rate*channels*4 <= MAX_WINDOW_BYTES,
            "memory_capacity", "model context window exceeds 128 MiB")
    block_times = []
    sample_io_seconds = resampling_seconds = inference_seconds = 0.0
    peaks = dict.fromkeys(stems, 0.0)
    input_energy = output_energy = 0.0
    wall_start = time.perf_counter()
    cpu_start = time.process_time()
    step, context = core_seconds*rate, context_seconds*rate
    for begin in range(0, frames, step):
        end = min(frames, begin+step)
        left, right = max(0, begin-context), min(frames, end+context)
        io_start = time.perf_counter()
        audio = source.window(left, right)
        sample_io_seconds += time.perf_counter()-io_start
        require(audio.shape == (right-left, channels) and audio.dtype == np.dtype("float32") and np.isfinite(audio).all(),
                "input_samples", "input changed/truncated or contains nonfinite samples")
        block_start = time.perf_counter()
        offset = begin-left
        original = audio[offset:offset+end-begin]
        if dry:
            result = original
        else:
            phase_start = time.perf_counter()
            converted = resample_exact(audio, rate, model_rate)
            resampling_seconds += time.perf_counter()-phase_start
            phase_start = time.perf_counter()
            with redirect_stdout(sys.stderr):
                inferred = infer(converted)
            inference_seconds += time.perf_counter()-phase_start
            require(inferred.shape == converted.shape and np.isfinite(inferred).all(), "inference_output", "model returned wrong length/channels or nonfinite samples")
            phase_start = time.perf_counter()
            restored = resample_exact(inferred, model_rate, rate)
            resampling_seconds += time.perf_counter()-phase_start
            require(len(restored) >= len(audio), "alignment", "resampler returned insufficient source frames")
            restored = restored[:len(audio)]
            result = restored[offset:offset+end-begin].astype("float32", copy=False)
            if request["operation"] == "speech":
                wet = controls.get("wet_mix", 1.0)
                if wet != 1.0:
                    result = original*(1.0-wet) + result*wet
            del converted, inferred, restored
        for stem in stems:
            chunk = original-result if stem == "accompaniment" else result
            require(np.isfinite(chunk).all(), "inference_output", "nonfinite output; refusing sanitization")
            io_start = time.perf_counter()
            # Transport copies at most 64 KiB, even for a strided model result.
            stride = MAX_FRAME_BYTES//(channels*4)
            for index in range(0, len(chunk), stride):
                write_pcm(stem, np.ascontiguousarray(chunk[index:index+stride], dtype="<f4"))
            sample_io_seconds += time.perf_counter()-io_start
            peaks[stem] = max(peaks[stem], abs(float(np.min(chunk))), abs(float(np.max(chunk))))
            del chunk
        block_energy = float(np.einsum("ij,ij->", original, original, dtype=np.float64, optimize=False))
        input_energy += block_energy
        output_energy += block_energy if dry else float(np.einsum("ij,ij->", result, result, dtype=np.float64, optimize=False))
        block_times.append(time.perf_counter()-block_start)
        del audio, result, original
        emit("progress", stage="copy" if dry else "inference", completed=end, total=frames)
    source_hash = source.finish()
    wall = time.perf_counter()-wall_start
    report = {"outputs":dict.fromkeys(stems, True), "sample_rate":rate, "channels":channels, "frames":frames,
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
                "processing_cpu_seconds":time.process_time()-cpu_start,"real_time_factor":wall/(frames/rate),
                "inference_seconds":inference_seconds,"resampling_seconds":resampling_seconds,
                "sample_read_write_seconds":sample_io_seconds,
                "block_latency_seconds":dict(zip(("p50","p95","p99"),map(float,np.percentile(block_times,[50,95,99])))),
                "first_block_seconds":block_times[0],"subsequent_block_seconds":block_times[1:],
                "block_seconds":block_times,"peak_rss_kib":resource.getrusage(resource.RUSAGE_SELF).ru_maxrss,
                "output_peaks":peaks,"input_rms":math.sqrt(input_energy/(frames*channels)),
                "target_rms":math.sqrt(output_energy/(frames*channels))}}
    report_bytes = 0
    for text in json.JSONEncoder(allow_nan=False).iterencode(report):
        report_bytes += len(text.encode("utf-8"))
        require(report_bytes <= MAX_REPORT_BYTES, "report_capacity", "result report exceeds 8 MiB")
    emit("complete", outputs=report["outputs"], sample_rate=rate, channels=channels, frames=frames,
         model={"id":manifest["id"], "sha256":report["model"]["sha256"]})
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--stream", action="store_true", required=True)
    parser.parse_args()
    task_id = ""
    output = sys.stdout.buffer
    def emit(event_type, **fields):
        payload = json.dumps({"version":EVENT_VERSION,"task_id":task_id,"type":event_type,**fields}, allow_nan=False).encode()
        write_frame(output, JSON_FRAME, payload)
    try:
        kind, payload = read_frame(sys.stdin.buffer)
        require(kind == JSON_FRAME, "protocol", "first frame must be a JSON request")
        request = json.loads(payload)
        candidate_id = request.get("task_id") if isinstance(request, dict) else None
        task_id = candidate_id if isinstance(candidate_id, str) and 0 < len(candidate_id) <= 128 else ""
        validate_request(request)
        source = StreamInput(sys.stdin.buffer, request)
        def write_pcm(stem, samples):
            write_frame(output, STEM_KINDS[stem], memoryview(samples).cast("B"))
        # Package diagnostics must never corrupt the binary stdout protocol.
        with redirect_stdout(sys.stderr):
            _process_validated(request, source, write_pcm, emit)
        return 0
    except Exception as exc:
        emit("error", code=exc.code if isinstance(exc, WorkerError) else "worker_failure", message=str(exc)[:8192])
        return 1


if __name__ == "__main__":
    sys.exit(main())
