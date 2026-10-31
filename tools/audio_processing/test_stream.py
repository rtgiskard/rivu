"""Deterministic transport/lifecycle coverage, never real-model quality evidence."""
import importlib.util
import io
import json
import os
from pathlib import Path
import select
import struct
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

from runtime import worker
from test_worker import bundle_fixture

ROOT = Path(__file__).resolve().parents[2]


def encode(kind, payload=b""):
    return struct.pack("<BI", kind, len(payload))+bytes(payload)


def input_stream(request, audio):
    result = io.BytesIO()
    worker.write_frame(result, worker.JSON_FRAME, json.dumps(request).encode())
    data = memoryview(audio).cast("B")
    for offset in range(0, len(data), 8192):
        worker.write_frame(result, worker.INPUT_PCM, data[offset:offset+8192])
    worker.write_frame(result, worker.INPUT_END, b"")
    return result.getvalue()


class FramingTests(unittest.TestCase):
    def test_fragmented_frames_limits_and_truncated_prefixes(self):
        class Fragmented(io.BytesIO):
            def read(self, count):
                if not 0 <= count <= worker.MAX_FRAME_BYTES:
                    raise AssertionError("unbounded read")
                return super().read(min(count, 3))
        wire = b"\x11\x04\x00\x00\x00abcd"
        self.assertEqual(worker.read_frame(Fragmented(wire)), (17, bytearray(b"abcd")))
        for end in range(len(wire)):
            with self.subTest(end=end), self.assertRaisesRegex(worker.WorkerError, "truncated"):
                worker.read_frame(Fragmented(wire[:end]))
        with self.assertRaisesRegex(worker.WorkerError, "64 KiB"):
            worker.read_frame(Fragmented(b"\x00\x01\x00\x01\x00"))
        payload = b"x" * 65536
        self.assertEqual(worker.read_frame(Fragmented(b"\x11\x00\x00\x01\x00" + payload)), (17, payload))
        output = io.BytesIO()
        worker.write_frame(output, 17, b"abcd")
        self.assertEqual(output.getvalue(), wire)
        with self.assertRaisesRegex(worker.WorkerError, "64 KiB"):
            worker.write_frame(output, 17, payload + b"x")
        self.assertEqual(output.getvalue(), wire)

    def test_legacy_paths_and_invalid_dimensions_are_rejected(self):
        request = {"version":2,"task_id":"test","operation":"speech","sample_rate":8000,
                   "channels":1,"frames":8,"model_manifest":"/manifest.json"}
        for fields in (
            {"input_path":"/input.wav"}, {"output_dir":"/tmp/result"}, {"frames":True}, {"frames":0},
            {"channels":3}, {"channels":True}, {"sample_rate":7999}, {"sample_rate":192001},
            {"sample_rate":8000.0}, {"frames":4*60*60*8000+1}, {"operation":"separate"},
        ):
            with self.subTest(fields=fields), self.assertRaises(worker.WorkerError):
                worker.validate_request({**request, **fields})
        for fields in ({"frames":1}, {"frames":4*60*60*8000},
                       {"sample_rate":192000, "channels":2, "frames":4*60*60*192000},
                       {"operation":"separate", "channels":2}):
            with self.subTest(valid=fields):
                worker.validate_request({**request, **fields})

    def test_pcm_window_overlap_is_bounded_and_exact(self):
        import numpy as np
        audio = np.arange(10000, dtype="float32").reshape(-1, 2)
        stream = io.BytesIO()
        for offset in range(0, len(audio), 113):
            worker.write_frame(stream, worker.INPUT_PCM, memoryview(audio[offset:offset+113]).cast("B"))
        worker.write_frame(stream, worker.INPUT_END, b"")
        stream.seek(0)
        source = worker.StreamInput(stream, {"frames":len(audio), "channels":2})
        for left, right in ((0, 1700), (1300, 3000), (2600, 4300), (3900, 5000)):
            np.testing.assert_array_equal(source.window(left, right), audio[left:right])
            self.assertLessEqual(source.buffer.nbytes, 1700*2*4)
        self.assertEqual(source.finish(), __import__("hashlib").sha256(memoryview(audio).cast("B")).hexdigest())
        self.assertIsNone(source.buffer)

    def test_input_end_and_sample_counts_are_mandatory(self):
        import numpy as np
        audio = np.zeros((8, 1), dtype="float32")
        for ending in (b"", encode(worker.INPUT_PCM, b"\0"*4), encode(worker.INPUT_END, b"x"),
                       encode(worker.JSON_FRAME, b"{}")):
            with self.subTest(ending=ending):
                source = worker.StreamInput(io.BytesIO(encode(worker.INPUT_PCM, memoryview(audio).cast("B"))+ending),
                                            {"frames":8,"channels":1})
                source.window(0, 8)
                with self.assertRaises(worker.WorkerError):
                    source.finish()
        source = worker.StreamInput(io.BytesIO(encode(worker.INPUT_PCM, b"\0"*36)), {"frames":8,"channels":1})
        with self.assertRaisesRegex(worker.WorkerError, "exceeds"):
            source.window(0, 8)
        for payload in (b"", b"\0"*3):
            source = worker.StreamInput(io.BytesIO(encode(worker.INPUT_PCM, payload)), {"frames":8,"channels":1})
            with self.subTest(payload=payload), self.assertRaisesRegex(worker.WorkerError, "aligned input PCM"):
                source.window(0, 8)

    def test_window_capacity_is_checked_before_allocation(self):
        source = worker.StreamInput(io.BytesIO(), {"frames":100,"channels":2})
        with patch.object(worker, "MAX_WINDOW_BYTES", 32), self.assertRaisesRegex(worker.WorkerError, "128 MiB"):
            source.window(0, 100)


class StreamEntrypointTests(unittest.TestCase):
    def test_invalid_requests_fail_without_pcm_or_manifest_access(self):
        request = {"version":2,"task_id":"invalid-stream","operation":"speech","sample_rate":8000,
                   "channels":1,"frames":8,"model_manifest":"/missing-manifest.json"}
        for fields, code in (
            ({"channels":3}, "input_channels"),
            ({"frames":0}, "input_duration"),
            ({"parameters":{"wet_mix":2}}, "invalid_request"),
            ({"unexpected":True}, "invalid_request"),
        ):
            with self.subTest(fields=fields):
                wire = encode(worker.JSON_FRAME, json.dumps({**request, **fields}).encode())
                completed = subprocess.run([sys.executable, "-B", worker.__file__, "--stream"],
                                           input=wire, capture_output=True, timeout=30)
                self.assertNotEqual(completed.returncode, 0)
                stream = io.BytesIO(completed.stdout)
                kind, payload = worker.read_frame(stream)
                self.assertEqual(kind, worker.JSON_FRAME)
                event = json.loads(payload)
                self.assertEqual((event["type"], event["code"], event["task_id"]),
                                 ("error", code, request["task_id"]))
                self.assertEqual(stream.read(), b"")

    def test_real_dry_worker_preserves_pcm_bits(self):
        import numpy as np
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory).resolve()
            bundle_fixture(base)
            audio = np.linspace(-1.25, 1.25, 2*(8000+37), dtype="<f4").reshape(-1, 2)
            audio[0, 0] = -0.0
            request = {"version":2,"task_id":"real-dry-stream","operation":"speech","sample_rate":8000,
                       "channels":2,"frames":len(audio),"model_manifest":str(base / "manifest.json"),
                       "parameters":{"wet_mix":0}}
            completed = subprocess.run([sys.executable, "-B", worker.__file__, "--stream"],
                                       input=input_stream(request, audio), capture_output=True, cwd=base, timeout=30)
            self.assertEqual(completed.returncode, 0, completed.stderr.decode())
            stream, pcm, events = io.BytesIO(completed.stdout), bytearray(), []
            while stream.tell() < len(completed.stdout):
                self.assertFalse(events and events[-1]["type"] in ("complete", "error"))
                kind, payload = worker.read_frame(stream)
                if kind == worker.JSON_FRAME:
                    events.append(json.loads(payload))
                else:
                    self.assertEqual(kind, worker.STEM_KINDS["speech"])
                    pcm.extend(payload)
            self.assertEqual(pcm, audio.tobytes())
            self.assertEqual(events[-1]["type"], "complete")
            self.assertEqual((events[-1]["frames"], events[-1]["channels"], events[-1]["sample_rate"]),
                             (len(audio), 2, 8000))


class LocalBackendTransportTests(unittest.TestCase):
    # This substitutes only model inference. It does not claim model quality,
    # artifact correctness or prepared-runtime dependency compatibility.
    def run_backend(self, operation, wet=1.0, malformed=None, channels=2, frames=3*8000+37,
                    inference_failure=None, held_open_suffix=None):
        import numpy as np
        scratch = ROOT/".cache/neural/tests"
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as directory:
            base = Path(directory)
            bundle_fixture(base, operation, "MossFormer2_SE_48K" if operation == "speech" else "melband_roformer_kim")
            audio = np.linspace(-1.25, 1.25, frames*channels, dtype="<f4").reshape(-1, channels)
            audio[0, 0] = -0.0
            request = {"version":2,"task_id":"local-transport","operation":operation,"sample_rate":8000,
                       "channels":channels,"frames":frames,"model_manifest":str(base/"manifest.json"),"threads":1,
                       "parameters":{"wet_mix":wet} if operation == "speech" else {"overlap":0.75}}
            # The audit hook rejects file writes, dirs, renames and removes even
            # outside the fixture root; dependencies/admission may only read.
            script = r'''
import os, sys
sys.dont_write_bytecode = True
sys.path.insert(0, sys.argv[1])
import worker
import numpy as np
mutations = []
def audit(event, args):
    if event == "open":
        mode, flags = args[1:3]
        if (isinstance(mode, str) and any(c in mode for c in "wax+")) or (flags & (os.O_WRONLY|os.O_RDWR|os.O_CREAT|os.O_TRUNC|os.O_APPEND)):
            mutations.append((event, args))
            raise AssertionError("implicit file write: " + str(args[0]))
    if event in ("os.mkdir", "os.remove", "os.rename", "os.rmdir", "os.link", "os.symlink", "os.truncate", "os.chmod", "os.chown", "os.utime"):
        mutations.append((event, args))
        raise AssertionError("implicit filesystem mutation: " + event)
sys.addaudithook(audit)
worker.CORE_SECONDS, worker.CONTEXT_SECONDS = 1, 1
dry, failure = sys.argv[2] == "True", sys.argv[3]
def check_packages(manifest):
    if dry:
        raise AssertionError("dry processing loaded model dependencies")
worker.check_packages = check_packages
worker.importlib.metadata.version = lambda name: "1"
def infer(samples):
    if dry:
        raise AssertionError("dry processing invoked inference")
    if failure == "shape":
        return samples[:-1]
    if failure == "nonfinite":
        return np.full_like(samples, np.nan)
    print("bounded model diagnostic")
    return samples * np.float32(0.5)
def build_engine(*args):
    if dry:
        raise AssertionError("dry processing constructed a model")
    return 8000, infer
worker.build_engine = build_engine
# Gate the real stdin read after END. A terminal success must never bypass EOF,
# even if a regression removes the read and would otherwise race the parent.
if sys.argv[4] != "None":
    ready_fd, release_fd = int(sys.argv[4]), int(sys.argv[5])
    original_read_frame, original_write_frame = worker.read_frame, worker.write_frame
    state = {"end": False, "eof": False}
    class GuardedInput:
        def read(self, count):
            if state["end"]:
                assert count == 1
                os.write(ready_fd, b"R")
                assert os.read(release_fd, 1) == b"G"
                result = sys.__stdin__.buffer.read(count)
                state["eof"] = result == b""
                return result
            return sys.__stdin__.buffer.read(count)
    class Input:
        buffer = GuardedInput()
    sys.stdin = Input()
    def read_frame(stream):
        kind, payload = original_read_frame(stream)
        if kind == worker.INPUT_END:
            state["end"] = True
        return kind, payload
    def write_frame(stream, kind, payload):
        if kind == worker.JSON_FRAME and json.loads(bytes(payload))["type"] == "complete":
            assert state["eof"], "completion bypassed stdin EOF"
        return original_write_frame(stream, kind, payload)
    import json
    worker.read_frame, worker.write_frame = read_frame, write_frame
sys.argv = ["worker.py", "--stream"]
result = worker.main()
if mutations:
    raise AssertionError("implicit filesystem mutations attempted: " + repr(mutations))
raise SystemExit(result)
'''
            wire = input_stream(request, audio)
            if malformed is not None:
                wire = malformed(wire)
            command = [sys.executable, "-B", "-c", script, str(Path(worker.__file__).parent),
                       str(operation == "speech" and wet == 0), str(inference_failure)]
            environment = {**os.environ,"PYTHONDONTWRITEBYTECODE":"1"}
            if held_open_suffix is None:
                completed = subprocess.run(command + ["None"], input=wire, capture_output=True,
                                           timeout=30, cwd=base, env=environment)
            else:
                ready_read, ready_write = os.pipe()
                release_read, release_write = os.pipe()
                try:
                    with subprocess.Popen(command + [str(ready_write), str(release_read)],
                                          stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                          stderr=subprocess.PIPE, cwd=base, env=environment,
                                          pass_fds=(ready_write, release_read)) as process:
                        try:
                            process.stdin.write(wire)
                            process.stdin.flush()
                            # This is a positive barrier, not a timed assertion of
                            # silence: the child is about to read past END while
                            # the producer still owns its open stdin.
                            self.assertTrue(select.select([ready_read], [], [], 30)[0],
                                            "worker never reached the EOF guard")
                            self.assertEqual(os.read(ready_read, 1), b"R")
                            if held_open_suffix:
                                process.stdin.write(held_open_suffix)
                                process.stdin.flush()
                            process.stdin.close()
                            process.stdin = None
                            os.write(release_write, b"G")
                            stdout, stderr = process.communicate(timeout=30)
                            completed = subprocess.CompletedProcess(command, process.returncode, stdout, stderr)
                        finally:
                            if process.poll() is None:
                                process.kill()
                                process.wait()
                finally:
                    for fd in (ready_read, ready_write, release_read, release_write):
                        os.close(fd)
            events, outputs = [], {17:[], 18:[], 19:[]}
            stream = io.BytesIO(completed.stdout)
            while stream.tell() < len(completed.stdout):
                self.assertFalse(events and events[-1]["type"] in ("complete", "error"),
                                 "worker emitted data after a terminal event")
                kind, payload = worker.read_frame(stream)
                if kind == worker.JSON_FRAME:
                    event = json.loads(payload)
                    self.assertEqual(event["version"], 3)
                    self.assertEqual(event["task_id"], request["task_id"])
                    events.append(event)
                else:
                    self.assertIn(kind, outputs)
                    self.assertTrue(payload)
                    self.assertEqual(len(payload) % (channels*4), 0)
                    outputs[kind].append(np.frombuffer(payload, dtype="<f4").reshape(-1, channels))
            return completed, audio, events, {key:np.concatenate(parts) for key, parts in outputs.items() if parts}

    def test_speech_wet_and_dry_are_aligned_without_disk(self):
        import numpy as np
        for wet in (0.0, 0.25, 1.0):
            with self.subTest(wet=wet):
                completed, audio, events, outputs = self.run_backend("speech", wet)
                self.assertEqual(completed.returncode, 0, completed.stderr.decode())
                self.assertEqual(events[-1]["type"], "complete")
                self.assertEqual(events[-1]["outputs"], {"speech":True})
                self.assertEqual(set(outputs), {17})
                self.assertEqual((events[-1]["sample_rate"], events[-1]["channels"]), (8000, 2))
                expected = audio if wet == 0 else audio*0.5 if wet == 1 else audio*(1-wet)+(audio*0.5)*wet
                np.testing.assert_array_equal(outputs[17].view("uint32"), expected.view("uint32"))
                self.assertEqual(events[-1]["frames"], len(audio))
                if wet:
                    self.assertIn(b"bounded model diagnostic", completed.stderr)

    def test_separation_exact_complement_without_disk(self):
        import numpy as np
        completed, audio, events, outputs = self.run_backend("separate")
        self.assertEqual(completed.returncode, 0, completed.stderr.decode())
        self.assertEqual(events[-1]["outputs"], {"vocals":True,"accompaniment":True})
        self.assertEqual(set(outputs), {18, 19})
        self.assertEqual((events[-1]["sample_rate"], events[-1]["channels"], events[-1]["frames"]),
                         (8000, 2, len(audio)))
        np.testing.assert_array_equal(outputs[18], audio*0.5)
        np.testing.assert_array_equal(outputs[19].view("uint32"), (audio-outputs[18]).view("uint32"))

    def test_missing_end_after_pcm_is_failure_not_completion(self):
        completed, audio, events, outputs = self.run_backend("speech", malformed=lambda data:data[:-5])
        self.assertNotEqual(completed.returncode, 0)
        self.assertEqual(events[-1]["type"], "error")
        self.assertEqual(events[-1]["code"], "protocol")
        self.assertEqual(outputs[17].shape, audio.shape)
        self.assertNotIn("complete", [event["type"] for event in events])

    def test_trailing_input_is_failure_not_completion(self):
        for suffix in (b"x", encode(worker.INPUT_PCM, b"\0"*4),
                       encode(worker.INPUT_END), encode(worker.JSON_FRAME, b"{}")):
            with self.subTest(suffix=suffix):
                completed, _, events, _ = self.run_backend(
                    "speech", wet=0, frames=37, malformed=lambda wire:wire+suffix)
                self.assertNotEqual(completed.returncode, 0)
                self.assertEqual(events[-1]["type"], "error")
                self.assertEqual(events[-1]["code"], "protocol")
                self.assertNotIn("complete", [event["type"] for event in events])

    def test_end_waits_for_held_open_stdin_to_close(self):
        completed, audio, events, outputs = self.run_backend(
            "speech", wet=0, frames=37, held_open_suffix=b"")
        self.assertEqual(completed.returncode, 0, completed.stderr.decode())
        self.assertEqual(events[-1]["type"], "complete")
        self.assertEqual(outputs[17].shape, audio.shape)

    def test_delayed_trailing_byte_after_end_is_rejected(self):
        completed, _, events, _ = self.run_backend(
            "speech", wet=0, frames=37, held_open_suffix=b"x")
        self.assertNotEqual(completed.returncode, 0)
        self.assertEqual(events[-1]["type"], "error")
        self.assertEqual(events[-1]["code"], "protocol")
        self.assertNotIn("complete", [event["type"] for event in events])

    def test_invalid_pcm_is_rejected_before_any_output(self):
        for name, suffix, code in (
            ("empty", encode(worker.INPUT_PCM), "protocol"),
            ("unaligned", encode(worker.INPUT_PCM, b"\0"*3), "protocol"),
            ("excess", encode(worker.INPUT_PCM, b"\0"*8), "protocol"),
            ("nonfinite", encode(worker.INPUT_PCM, struct.pack("<f", float("nan"))), "input_samples"),
            ("premature_end", encode(worker.INPUT_END), "protocol"),
            ("oversized", b"\x01\x01\x00\x01\x00", "protocol"),
        ):
            def malformed(wire):
                request_end = 5 + int.from_bytes(wire[1:5], "little")
                return wire[:request_end] + suffix
            with self.subTest(name=name):
                completed, _, events, outputs = self.run_backend("speech", wet=0, channels=1, frames=1,
                                                                 malformed=malformed)
                self.assertNotEqual(completed.returncode, 0)
                self.assertEqual(events[-1]["type"], "error")
                self.assertEqual(events[-1]["code"], code)
                self.assertEqual(outputs, {})
                self.assertNotIn("complete", [event["type"] for event in events])

    def test_invalid_inference_is_not_sanitized_or_completed(self):
        for failure in ("shape", "nonfinite"):
            with self.subTest(failure=failure):
                completed, _, events, outputs = self.run_backend("speech", frames=37, inference_failure=failure)
                self.assertNotEqual(completed.returncode, 0)
                self.assertEqual(events[-1]["type"], "error")
                self.assertEqual(events[-1]["code"], "inference_output")
                self.assertEqual(outputs, {})
                self.assertNotIn("complete", [event["type"] for event in events])

    def test_mono_dry_short_and_partial_tail_preserve_pcm_bits(self):
        import numpy as np
        for frames in (1, 8000+37):
            with self.subTest(frames=frames):
                completed, audio, events, outputs = self.run_backend("speech", wet=0, channels=1, frames=frames)
                self.assertEqual(completed.returncode, 0, completed.stderr.decode())
                self.assertEqual(events[-1]["type"], "complete")
                self.assertEqual((events[-1]["sample_rate"], events[-1]["channels"], events[-1]["frames"]),
                                 (8000, 1, frames))
                self.assertEqual(set(outputs), {17})
                np.testing.assert_array_equal(outputs[17].view("uint32"), audio.view("uint32"))

    @unittest.skipUnless(importlib.util.find_spec("jsonschema"), "jsonschema required for event protocol validation")
    def test_actual_stream_events_validate_against_protocol_schema(self):
        import jsonschema
        completed, _, events, _ = self.run_backend("speech", frames=37)
        self.assertEqual(completed.returncode, 0, completed.stderr.decode())
        self.assertEqual(events[-1]["type"], "complete")
        validator = jsonschema.Draft202012Validator(json.loads(Path(worker.__file__).with_name("event.schema.json").read_text()))
        for event in events:
            validator.validate(event)


if __name__ == "__main__":
    unittest.main()
