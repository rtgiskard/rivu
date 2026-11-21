#!/usr/bin/env python3
"""Explicit offline evaluation/export adapter; never used by app processing.

--request names an evaluation job with input_path/output_dir and model controls.
The destination must be new. Only this explicitly requested directory is written;
the worker's internal processing consumes/produces PCM and never writes artifacts.
"""
import argparse
from contextlib import ExitStack
import json
import hashlib
import os
from pathlib import Path
import shutil
import sys

sys.dont_write_bytecode = True
if __package__:
    from .runtime import worker
    from . import common
else:
    from runtime import worker
    import common


def completed_export_report(completed, request, source_sha256):
    """Read a report only after the exporter confirms this task completed."""
    task_id = request["task_id"]
    events = [json.loads(line) for line in completed.stdout.splitlines()]
    last = events[-1] if events else None
    if (completed.returncode or not isinstance(last, dict) or last.get("type") != "complete"
            or last.get("version") != worker.EVENT_VERSION or last.get("task_id") != task_id):
        raise RuntimeError(json.dumps({"task_id": task_id, "exit": completed.returncode,
                                       "events": events, "stderr": completed.stderr}))
    report, _ = common.read_export_report(Path(request["output_dir"]) / "result.json", source_sha256,
                                          operation=request["operation"], request=request,
                                          mismatch=f"worker input does not match task {task_id!r}")
    return report


class FileInput:
    def __init__(self, path, stack):
        import soundfile as sf
        self.path = Path(path)
        self.stream = stack.enter_context(self.path.open("rb", buffering=0))
        self.identity = self._identity()
        self.reader = stack.enter_context(sf.SoundFile(self.stream))

    def admit(self):
        self.sha256 = self._digest()
        try:
            stat = self.path.stat()
            unchanged = ((stat.st_dev, stat.st_ino) == self.identity[:2]
                         and self._identity() == self.identity)
        except OSError:
            unchanged = False
        common.require(unchanged, "source_changed", "source changed during admission")

    def _identity(self):
        stat = os.fstat(self.stream.fileno())
        return (stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns, stat.st_ctime_ns)

    def _digest(self):
        digest = hashlib.sha256()
        offset = 0
        while offset < self.identity[2]:
            block = os.pread(self.stream.fileno(), min(1024*1024, self.identity[2]-offset), offset)
            common.require(block, "source_changed", "source truncated while hashing")
            digest.update(block)
            offset += len(block)
        return digest.hexdigest()

    def window(self, left, right):
        self.reader.seek(left)
        return self.reader.read(right-left, dtype="float32", always_2d=True)

    def finish(self):
        try:
            stat = self.path.stat()
            unchanged = ((stat.st_dev, stat.st_ino) == self.identity[:2]
                         and self._identity() == self.identity
                         and self._digest() == self.sha256
                         and self._identity() == self.identity)
        except OSError:
            unchanged = False
        common.require(unchanged, "source_changed", "source changed during processing")
        return self.sha256


def process_export(request, emit):
    """Explicitly export PCM and detailed evaluation evidence, without clobbering."""
    common.require(isinstance(request, dict), "invalid_request", "export request must be an object")
    common.require(request.get("version") == 1, "invalid_request", "export job version must be 1")
    source = common.absolute_path(request.get("input_path"), "input_path")
    destination = common.absolute_path(request.get("output_dir"), "output_dir")
    common.require(source.is_file(), "input_missing", "input file does not exist")
    common.require(not destination.exists(), "output_exists", "output_dir must not already exist")
    common.require(destination.parent.is_dir(), "output_parent", "output parent must already exist")
    with ExitStack() as stack:
        source_input = FileInput(source, stack)
        return _export_admitted(request, emit, destination, source_input)


def _export_admitted(request, emit, destination, source_input):
    import soundfile as sf
    info = source_input.reader
    common.require(info.format in ("WAV", "WAVEX", "RF64") and info.subtype == "FLOAT", "input_format", "input must be IEEE float32 WAV/WAVEX/RF64")
    control = {key:value for key, value in request.items() if key not in ("input_path", "output_dir")}
    control.update(version=worker.VERSION, sample_rate=info.samplerate, channels=info.channels, frames=info.frames)
    worker.validate_request(control)
    stems = ("vocals", "accompaniment") if control["operation"] == "separate" else ("speech",)
    output_bytes = info.frames*info.channels*4*len(stems)+8192
    common.require(output_bytes <= 8*1024**3, "output_capacity", "export exceeds 8 GiB output limit")
    common.require(shutil.disk_usage(destination.parent).free > output_bytes+256*1024**2, "disk_capacity", "insufficient explicit export disk reserve")
    output_format = "RF64" if info.frames*info.channels*4+1024 >= 2**32 else "WAV"
    outputs = {stem:str(destination/(stem+".wav")) for stem in stems}
    source_input.admit()
    destination.mkdir(mode=0o700)
    try:
        with ExitStack() as stack:
            writers = {stem:stack.enter_context(sf.SoundFile(path, mode="w", samplerate=info.samplerate,
                       channels=info.channels, format=output_format, subtype="FLOAT")) for stem, path in outputs.items()}
            completion, report = worker._process_validated(
                control, source_input, lambda stem, samples: writers[stem].write(samples),
                emit, streaming=False)
        hashes = {}
        for stem, path in outputs.items():
            with Path(path).open("rb") as stream:
                actual = sf.info(stream)
                common.require((actual.frames, actual.channels, actual.samplerate) == (info.frames, info.channels, info.samplerate),
                               "alignment", "written result format/length mismatch")
                stream.seek(0)
                hashes[stem] = common.digest_stream(stream)
        report["outputs"] = outputs
        report["output_sha256"] = hashes
        report["export_request_sha256"] = common.canonical_digest(request)
        with (destination/"result.json").open("x", encoding="utf-8", newline="\n") as stream:
            size = 1
            for text in json.JSONEncoder(indent=2, allow_nan=False).iterencode(report):
                size += len(text.encode("utf-8"))
                common.require(size <= common.MAX_REPORT_BYTES, "report_capacity", "result report exceeds 8 MiB")
                stream.write(text)
            stream.write("\n")
        emit("complete", **completion)
        return report
    except BaseException:
        shutil.rmtree(destination)
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--request", type=Path, required=True)
    args = parser.parse_args()
    task_id = ""
    def emit(kind, **fields):
        print(json.dumps({"version":worker.EVENT_VERSION, "task_id":task_id, "type":kind, **fields}, allow_nan=False), flush=True)
    try:
        request = common.read_json(args.request)
        task_id = worker.event_task_id(request)
        process_export(request, emit)
        return 0
    except Exception as exc:
        emit("error", code=exc.code if isinstance(exc, common.WorkerError) else "export_failure", message=str(exc))
        return 1


if __name__ == "__main__":
    sys.exit(main())
