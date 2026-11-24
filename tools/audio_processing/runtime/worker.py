#!/usr/bin/env python3
"""Read-only offline inference over bounded framed raw PCM stdin/stdout.

Invoke the prepared interpreter with worker.py --stream. Each frame is a one-byte
kind, a uint32 little-endian payload size, and at most 65536 payload bytes. Kind 0
is UTF-8 JSON, 1 is interleaved float32-le input, and 2 is
the empty input-end marker, which must be followed immediately by stdin EOF.
Each process handles one task; the producer must close stdin without waiting for
completion. Output kinds 17/18/19 are speech/vocals/accompaniment PCM at the
exact source format. Discard all PCM on error or unsuccessful exit, even if a
stem appears complete. Dry speech sends no PCM in either direction:
input-end/EOF and a validated speech="input" completion retain caller storage.
Only explicit export.py/evaluation writes files, including dry PCM exports.

The sequential context reader retains at most one bounded model window, not the
whole signal. MossFormer2 uses 8s cores plus 2s context; other models use 24s plus
8s. Algorithms, compensated resampling, independent speech channels, exact stem
complement and unclipped samples are unchanged. No network/device fallback.
"""
import argparse
from contextlib import redirect_stdout
import importlib.metadata
import json
import math
import os
from pathlib import Path
import socket
import sys
import time
import struct

sys.dont_write_bytecode = True
if __package__ and "." in __package__:
    from .. import common
else:
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
    import common
VERSION = 4
EVENT_VERSION = 4
CORE_SECONDS = 24
CONTEXT_SECONDS = 8
MAX_SECONDS = 4 * 60 * 60
MAX_FRAME_BYTES = 65536
MAX_WINDOW_BYTES = 128 * 1024**2
JSON_FRAME, INPUT_PCM, INPUT_END = 0, 1, 2
STEM_KINDS = {"speech":17, "vocals":18, "accompaniment":19}


def event_task_id(request):
    candidate = request.get("task_id") if isinstance(request, dict) else None
    return candidate if isinstance(candidate, str) and 0 < len(candidate) <= 128 else ""


def validate_request(request):
    common.require(isinstance(request, dict), "invalid_request", "request must be an object")
    allowed = {"version", "task_id", "operation", "sample_rate", "channels", "frames", "model_manifest", "device", "threads", "parameters"}
    common.require(not (set(request) - allowed), "invalid_request", "unknown request fields: " + str(sorted(set(request) - allowed)))
    common.require(request.get("version") == VERSION, "invalid_request", f"version must be {VERSION}")
    common.require(isinstance(request.get("task_id"), str) and 0 < len(request["task_id"]) <= 128, "invalid_request", "task_id must be 1..128 characters")
    common.require(request.get("operation") in ("speech", "separate"), "invalid_request", "operation must be speech or separate")
    common.require(request.get("device", "cpu") in ("cpu", "cuda"), "invalid_request", "device must be cpu or cuda")
    common.require(type(request.get("threads", 4)) is int and 1 <= request.get("threads", 4) <= 8, "invalid_request", "threads must be 1..8")
    parameters = request.get("parameters", {})
    common.require(isinstance(parameters, dict), "invalid_request", "parameters must be an object")
    bounds = {"wet_mix":(0,1), "attenuation_limit_db":(0,100)} if request["operation"] == "speech" else {"overlap":(0,0.875)}
    allowed_parameters = set(bounds) | ({"post_filter"} if request["operation"] == "speech" else set())
    common.require(not (set(parameters)-allowed_parameters), "invalid_request", "unknown operation parameters")
    for name, (low, high) in bounds.items():
        if name in parameters:
            value = parameters[name]
            common.require(type(value) in (int, float) and low <= value <= high,
                    "invalid_request", name + " outside supported finite range")
    common.require("post_filter" not in parameters or type(parameters["post_filter"]) is bool,
            "invalid_request", "post_filter must be boolean")
    common.absolute_path(request.get("model_manifest"), "model_manifest")
    rate, channels, frames = (request.get(key) for key in ("sample_rate", "channels", "frames"))
    common.require(type(rate) is int and 8000 <= rate <= 192000, "input_rate", "supported source rates: 8000..192000 Hz")
    common.require(type(channels) is int and channels in ((2,) if request["operation"] == "separate" else (1, 2)),
            "input_channels", "separation requires stereo; speech supports mono/stereo; multichannel downmix is never implicit")
    common.require(type(frames) is int and 0 < frames <= MAX_SECONDS * rate, "input_duration", "input must be nonempty and <=4 hours")
    return request


def validate_execution(manifest):
    """Bind admitted metadata to this executing worker and Python environment."""
    runtime = manifest["runtime"]
    common.require(Path(__file__).resolve() == Path(runtime["worker"]).resolve(),
            "model_runtime", "executing worker differs from provisioned worker")
    # Resolving a venv interpreter symlink would erase its environment identity.
    common.require(os.path.abspath(sys.executable) == runtime["interpreter"],
            "model_runtime", "executing interpreter differs from provisioned interpreter")


def deny_network():
    def blocked(*args, **kwargs):
        raise common.WorkerError("network_forbidden", "inference worker is offline; run explicit preparation separately")
    socket.create_connection = blocked
    socket.socket.connect = blocked
    socket.socket.connect_ex = blocked
    socket.getaddrinfo = blocked
    socket.gethostbyname = blocked
    socket.gethostbyname_ex = blocked
    socket.socket.sendto = blocked


def resample_exact(audio, source_rate, target_rate):
    """Zero-phase polyphase FIR; output at t=0, no uncompensated filter delay."""
    if source_rate == target_rate:
        return audio
    from scipy.signal import resample_poly
    divisor = math.gcd(source_rate, target_rate)
    return resample_poly(audio, target_rate // divisor, source_rate // divisor, axis=0).astype("float32", copy=False)

def build_engine(manifest, base, device, controls=None):
    controls = controls or {}
    import numpy as np
    import torch
    common.require(device != "cuda" or torch.cuda.is_available(), "device_unavailable", "CUDA requested but unavailable; no CPU fallback")
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
    if manifest["id"] == "melband_roformer_kim":
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
        common.require(parameters.sr == 48000, "model_manifest", "DeepFilterNet3 config must be 48 kHz")
        def run(audio):
            state = DF(sr=48000, fft_size=parameters.fft_size, hop_size=parameters.hop_size,
                       nb_bands=parameters.nb_erb, min_nb_erb_freqs=parameters.min_nb_freqs)
            with torch.inference_mode():
                return enhance(model, state, torch.from_numpy(np.ascontiguousarray(audio.T)), pad=True,
                               atten_lim_db=controls.get("attenuation_limit_db", 80)).cpu().numpy().T
        return 48000, run

def read_exact(stream, count, scratch=None):
    common.require(0 <= count <= MAX_FRAME_BYTES, "protocol", "frame exceeds 64 KiB")
    data = bytearray(count) if scratch is None else scratch[:count]
    target = memoryview(data) if scratch is None else data
    offset = 0
    while offset < count:
        received = stream.readinto(target[offset:] if offset else target)
        common.require(type(received) is int and 0 <= received <= count-offset,
                "protocol", "invalid frame read count")
        common.require(received != 0, "protocol", "truncated frame")
        offset += received
    return data


def read_frame(stream, scratch=None):
    header = read_exact(stream, 5, scratch)
    kind, size = struct.unpack("<BI", header)
    return kind, read_exact(stream, size, scratch)


def write_frame(stream, kind, payload):
    common.require(len(payload) <= MAX_FRAME_BYTES, "protocol", "frame exceeds 64 KiB")
    stream.write(struct.pack("<BI", kind, len(payload)))
    stream.write(payload)
    stream.flush()


class StreamInput:
    """Sequential input with bounded overlap retention and no seekable-file proxy."""
    def __init__(self, stream, request):
        self.stream = stream
        self.frames, self.channels = request["frames"], request["channels"]
        self.reuse_input = request.get("operation") == "speech" and request.get("parameters", {}).get("wet_mix", 1.0) == 0.0
        self.buffer = None
        self.left = self.right = self.received = 0
        self.pending = memoryview(b"")
        # Refill only after the pending slice has been copied into a window.
        self.frame_scratch = None if self.reuse_input else memoryview(bytearray(MAX_FRAME_BYTES))

    def window(self, left, right):
        import numpy as np
        common.require(self.left <= left <= self.right <= right <= self.frames, "protocol", "nonsequential input window")
        size = (right-left)*self.channels*4
        common.require(size <= MAX_WINDOW_BYTES, "memory_capacity", "model context window exceeds 128 MiB")
        audio = np.empty((right-left, self.channels), dtype="<f4")
        retained = self.right-left
        if retained:
            audio[:retained] = self.buffer[left-self.left:]
        target = memoryview(audio).cast("B")[retained*self.channels*4:]
        offset = 0
        while offset < len(target):
            if not self.pending:
                kind, payload = read_frame(self.stream, self.frame_scratch)
                common.require(kind == INPUT_PCM and len(payload) and len(payload) % (self.channels*4) == 0,
                        "protocol", "expected aligned input PCM")
                self.received += len(payload)//(self.channels*4)
                common.require(self.received <= self.frames, "protocol", "input exceeds declared frames")
                self.pending = payload
            count = min(len(target)-offset, len(self.pending))
            target[offset:offset+count] = self.pending[:count]
            self.pending = self.pending[count:]
            offset += count
        self.buffer, self.left, self.right = audio, left, right
        return audio

    def finish(self):
        expected = 0 if self.reuse_input else self.frames
        common.require(self.right == expected and self.received == expected and not self.pending,
                "protocol", "input frame count mismatch")
        kind, payload = read_frame(self.stream, self.frame_scratch)
        common.require(kind == INPUT_END and not payload, "protocol", "expected empty input end marker")
        common.require(not self.stream.read(1), "protocol", "unexpected data after input end marker")
        self.buffer = None


def _process_validated(request, source, write_pcm, emit, *, streaming):
    """Return completion fields and, for file exports, detailed evidence."""
    collect_report = not streaming
    rate, channels, frames = (request[key] for key in ("sample_rate", "channels", "frames"))
    manifest_path = Path(request["model_manifest"]).resolve()
    emit("progress", stage="inspect", completed=0, total=1)
    manifest = common.validate_manifest(manifest_path, request["operation"])
    if streaming:
        validate_execution(manifest)
    model = {"id":manifest["id"], "sha256":common.canonical_digest(manifest)}
    emit("progress", stage="inspect", completed=1, total=1, model=model)
    controls = request.get("parameters", {})
    common.require("post_filter" not in controls or manifest["id"] == "DeepFilterNet3", "invalid_request", "post_filter is supported only by DeepFilterNet3")
    dry = request["operation"] == "speech" and controls.get("wet_mix", 1.0) == 0.0
    if streaming and source.reuse_input:
        source.finish()
        return dict(outputs={"speech":"input"}, sample_rate=rate, channels=channels,
                    frames=frames, model=model), None
    if not dry:
        common.check_packages(manifest)
    for key in ("OMP_NUM_THREADS", "MKL_NUM_THREADS", "OPENBLAS_NUM_THREADS"):
        os.environ[key] = str(request.get("threads", 4))
    os.environ["HF_HUB_OFFLINE"] = os.environ["TRANSFORMERS_OFFLINE"] = "1"
    deny_network()
    import numpy as np
    model_rate = None
    if collect_report:
        load_seconds = 0.0
    if not dry:
        emit("progress", stage="load", completed=0, total=1)
        if collect_report:
            start = time.perf_counter()
        with redirect_stdout(sys.stderr):
            model_rate, infer = build_engine(manifest, manifest_path.parent, request.get("device", "cpu"), controls)
        if collect_report:
            load_seconds = time.perf_counter() - start
    stems = ("vocals", "accompaniment") if request["operation"] == "separate" else ("speech",)
    core_seconds, context_seconds = (8, 2) if manifest["id"] == "MossFormer2_SE_48K" else (CORE_SECONDS, CONTEXT_SECONDS)
    if dry:
        core_seconds, context_seconds = CORE_SECONDS, 0
    common.require((core_seconds+2*context_seconds)*rate*channels*4 <= MAX_WINDOW_BYTES,
            "memory_capacity", "model context window exceeds 128 MiB")
    if collect_report:
        block_times = []
        sample_io_seconds = resampling_seconds = inference_seconds = 0.0
        peaks = dict.fromkeys(stems, 0.0)
        input_energy = output_energy = 0.0
        wall_start = time.perf_counter()
        cpu_start = time.process_time()
    step, context = core_seconds*rate, context_seconds*rate
    stride = MAX_FRAME_BYTES//(channels*4)
    if request["operation"] == "separate":
        residual = np.empty((min(frames, stride), channels), dtype="<f4")
    for begin in range(0, frames, step):
        end = min(frames, begin+step)
        left, right = max(0, begin-context), min(frames, end+context)
        if collect_report:
            io_start = time.perf_counter()
        audio = source.window(left, right)
        if collect_report:
            sample_io_seconds += time.perf_counter()-io_start
        common.require(audio.shape == (right-left, channels) and audio.dtype == np.dtype("float32") and np.isfinite(audio).all(),
                "input_samples", "input changed/truncated or contains nonfinite samples")
        if collect_report:
            block_start = time.perf_counter()
        offset = begin-left
        original = audio[offset:offset+end-begin]
        if dry:
            result = original
        else:
            if collect_report:
                phase_start = time.perf_counter()
            converted = resample_exact(audio, rate, model_rate)
            if collect_report:
                resampling_seconds += time.perf_counter()-phase_start
                phase_start = time.perf_counter()
            with redirect_stdout(sys.stderr):
                inferred = infer(converted)
            if collect_report:
                inference_seconds += time.perf_counter()-phase_start
            common.require(inferred.shape == converted.shape and np.isfinite(inferred).all(), "inference_output", "model returned wrong length/channels or nonfinite samples")
            if collect_report:
                phase_start = time.perf_counter()
            restored = resample_exact(inferred, model_rate, rate)
            if collect_report:
                resampling_seconds += time.perf_counter()-phase_start
            common.require(len(restored) >= len(audio), "alignment", "resampler returned insufficient source frames")
            restored = restored[:len(audio)]
            result = restored[offset:offset+end-begin].astype("float32", copy=False)
            if request["operation"] == "speech":
                wet = controls.get("wet_mix", 1.0)
                if wet != 1.0:
                    result = original*(1.0-wet) + result*wet
            del converted, inferred, restored
            common.require(np.isfinite(result).all(), "inference_output", "nonfinite output; refusing sanitization")
        target = stems[0]
        if collect_report:
            io_start = time.perf_counter()
        for index in range(0, len(result), stride):
            write_pcm(target, np.ascontiguousarray(result[index:index+stride], dtype="<f4"))
        if collect_report:
            sample_io_seconds += time.perf_counter()-io_start
            peaks[target] = max(peaks[target], abs(float(np.min(result))), abs(float(np.max(result))))
        if request["operation"] == "separate":
            for index in range(0, len(result), stride):
                stop = min(index+stride, len(result))
                packet = residual[:stop-index]
                np.subtract(original[index:stop], result[index:stop], out=packet)
                low, high = float(packet.min()), float(packet.max())
                common.require(math.isfinite(low) and math.isfinite(high), "inference_output", "nonfinite output; refusing sanitization")
                if collect_report:
                    peaks["accompaniment"] = max(peaks["accompaniment"], abs(low), abs(high))
                    io_start = time.perf_counter()
                write_pcm("accompaniment", packet)
                if collect_report:
                    sample_io_seconds += time.perf_counter()-io_start
        if collect_report:
            block_energy = float(np.einsum("ij,ij->", original, original, dtype=np.float64, optimize=False))
            input_energy += block_energy
            output_energy += block_energy if dry else float(np.einsum("ij,ij->", result, result, dtype=np.float64, optimize=False))
            block_times.append(time.perf_counter()-block_start)
        del audio, result, original
        emit("progress", stage="copy" if dry else "inference", completed=end, total=frames)
    source_hash = source.finish()
    completion = dict(outputs=dict.fromkeys(stems, True), sample_rate=rate,
                      channels=channels, frames=frames, model=model)
    if streaming:
        return completion, None
    import resource
    wall = time.perf_counter()-wall_start
    worker_path = Path(__file__)
    implementation_sha256 = {path.name: common.digest(path)
                             for path in [Path(common.__file__), *sorted(worker_path.parent.glob("*.py"))]}
    report = {"outputs":dict.fromkeys(stems, True), "sample_rate":rate, "channels":channels, "frames":frames,
              "model":{**model, "files":manifest["files"],
                       "provenance":manifest.get("provenance", {})},
              "source_sha256":source_hash, "request_sha256":common.canonical_digest(request),
              "worker_sha256":implementation_sha256[worker_path.name], "python":sys.version, "packages":manifest["packages"],
              "implementation_sha256":implementation_sha256,
              "interpreter":{"path":sys.executable,"sha256":common.digest(sys.executable)},
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
    return completion, report


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
        common.require(kind == JSON_FRAME, "protocol", "first frame must be a JSON request")
        request = json.loads(payload)
        task_id = event_task_id(request)
        validate_request(request)
        source = StreamInput(sys.stdin.buffer, request)
        def write_pcm(stem, samples):
            write_frame(output, STEM_KINDS[stem], memoryview(samples).cast("B"))
        # Package diagnostics must never corrupt the binary stdout protocol.
        with redirect_stdout(sys.stderr):
            completion, _ = _process_validated(request, source, write_pcm, emit, streaming=True)
        emit("complete", **completion)
        return 0
    except Exception as exc:
        # ASCII JSON can expand one character to 12 bytes. Keep the whole error frame bounded.
        emit("error", code=exc.code if isinstance(exc, common.WorkerError) else "worker_failure", message=str(exc)[:4096])
        return 1


if __name__ == "__main__":
    sys.exit(main())
