#!/usr/bin/env python3
"""Explicit offline evaluation/export adapter; never used by app processing.

--request names an evaluation job with input_path/output_dir and model controls.
The destination must be new. Only this explicitly requested directory is written;
the worker's internal processing consumes/produces PCM and never writes artifacts.
"""
import argparse
from contextlib import ExitStack
import json
from pathlib import Path
import shutil
import sys

sys.dont_write_bytecode = True
if __package__:
    from .runtime import worker
else:
    from runtime import worker


class FileInput:
    def __init__(self, path, reader):
        self.path, self.reader = path, reader
        self.sha256 = worker.digest(path)

    def window(self, left, right):
        self.reader.seek(left)
        return self.reader.read(right-left, dtype="float32", always_2d=True)

    def finish(self):
        worker.require(worker.digest(self.path) == self.sha256, "source_changed", "source changed during processing")
        return self.sha256


def process_export(request, emit):
    """Explicitly export PCM and detailed evaluation evidence, without clobbering."""
    worker.require(isinstance(request, dict), "invalid_request", "export request must be an object")
    worker.require(request.get("version") == 1, "invalid_request", "export job version must be 1")
    source = worker.absolute_path(request.get("input_path"), "input_path")
    destination = worker.absolute_path(request.get("output_dir"), "output_dir")
    worker.require(source.is_file(), "input_missing", "input file does not exist")
    worker.require(not destination.exists(), "output_exists", "output_dir must not already exist")
    worker.require(destination.parent.is_dir(), "output_parent", "output parent must already exist")
    import soundfile as sf
    info = sf.info(source)
    worker.require(info.format in ("WAV", "WAVEX", "RF64") and info.subtype == "FLOAT", "input_format", "input must be IEEE float32 WAV/WAVEX/RF64")
    control = {key:value for key, value in request.items() if key not in ("input_path", "output_dir")}
    control.update(version=worker.VERSION, sample_rate=info.samplerate, channels=info.channels, frames=info.frames)
    worker.validate_request(control)
    stems = ("vocals", "accompaniment") if control["operation"] == "separate" else ("speech",)
    output_bytes = info.frames*info.channels*4*len(stems)+8192
    worker.require(output_bytes <= 8*1024**3, "output_capacity", "export exceeds 8 GiB output limit")
    worker.require(shutil.disk_usage(destination.parent).free > output_bytes+256*1024**2, "disk_capacity", "insufficient explicit export disk reserve")
    output_format = "RF64" if info.frames*info.channels*4+1024 >= 2**32 else "WAV"
    outputs = {stem:str(destination/(stem+".wav")) for stem in stems}
    completion = None
    def capture(kind, **fields):
        nonlocal completion
        if kind == "complete":
            completion = fields
        else:
            emit(kind, **fields)
    destination.mkdir(mode=0o700)
    try:
        with ExitStack() as stack:
            reader = stack.enter_context(sf.SoundFile(source))
            writers = {stem:stack.enter_context(sf.SoundFile(path, mode="w", samplerate=info.samplerate,
                       channels=info.channels, format=output_format, subtype="FLOAT")) for stem, path in outputs.items()}
            report = worker._process_validated(control, FileInput(source, reader), lambda stem, samples:writers[stem].write(samples), capture)
        for path in outputs.values():
            actual = sf.info(path)
            worker.require((actual.frames, actual.channels, actual.samplerate) == (info.frames, info.channels, info.samplerate),
                           "alignment", "written result format/length mismatch")
        report["outputs"] = outputs
        report["export_request_sha256"] = worker.canonical_digest(request)
        with (destination/"result.json").open("x") as stream:
            size = 1
            for text in json.JSONEncoder(indent=2, allow_nan=False).iterencode(report):
                size += len(text.encode())
                worker.require(size <= worker.MAX_REPORT_BYTES, "report_capacity", "result report exceeds 8 MiB")
                stream.write(text)
            stream.write("\n")
        worker.require(completion is not None, "protocol", "missing PCM completion")
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
        request = worker.read_json(args.request)
        task_id = request.get("task_id", "")
        process_export(request, emit)
        return 0
    except Exception as exc:
        emit("error", code=exc.code if isinstance(exc, worker.WorkerError) else "export_failure", message=str(exc))
        return 1


if __name__ == "__main__":
    sys.exit(main())
