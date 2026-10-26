"""Behavioral checks; optional real-model tests use explicitly prepared manifests.

Set RIVU_SPEECH_MANIFEST, RIVU_KIM_MANIFEST and/or RIVU_HTDEMUCS_MANIFEST.
Run with the corresponding prepared interpreter: python -m unittest discover -s tools/audio_processing -p 'test_*.py'
"""
import importlib.util
import json
import os
from pathlib import Path
import tempfile
from unittest.mock import patch
import unittest
from worker import RUNTIME_FILES, WorkerError, canonical_digest, deny_network, digest, process, resample_exact, validate_artifacts, validate_manifest, validate_request

ROOT = Path(__file__).resolve().parents[2]

def bundle_fixture(base, operation="speech", model="nsnet2", provenance=None):
    """Current admission fixture, not a downloadable or executable model."""
    import sys
    runtime = base / "runtime"
    runtime.mkdir(exist_ok=True)
    for name in RUNTIME_FILES:
        (runtime / name).write_text("Synthetic sidecar fixture: " + name)
    evidence = base / "weight-license.txt"
    evidence.write_text("Factual test provenance; no permission grant.")
    manifest = {"version":1, "operation":operation, "id":model, "packages":{"fixture":"1"},
                "license_evidence":"weight-license.txt", "provenance":provenance or {},
                "runtime":{"worker":str(runtime / "worker.py"), "interpreter":os.path.abspath(sys.executable),
                           "interpreter_sha256":digest(sys.executable), "model_manifest":str(base / "manifest.json"),
                           "worker_notice":"weight-license.txt", "cwd_independent":True},
                "dependencies":{"fixture":{"version":"1", "notices":["weight-license.txt"],
                    "license_expression":None, "license_classifiers":[], "notice_status":"test evidence"}},
                "files":{str(path.relative_to(base)):digest(path) for path in [evidence, *(runtime / name for name in RUNTIME_FILES)]}}
    (base / "manifest.json").write_text(json.dumps(manifest))
    return manifest

class ProvisionTests(unittest.TestCase):
    def test_initial_construction_then_reprovision_preserves_source(self):
        import io
        import sys
        from contextlib import redirect_stdout
        from email.message import Message
        from types import SimpleNamespace
        import provision

        metadata = Message()
        metadata["Name"] = "fixture"
        distribution = SimpleNamespace(metadata=metadata, version="1", files=[])
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "prepared"
            source.mkdir()
            evidence = source / "weight-license.txt"
            evidence.write_text("Construction provenance fixture.")
            manifest_path = source / "manifest.json"
            manifest_path.write_text(json.dumps({"version":1, "operation":"speech", "id":"nsnet2",
                "packages":{"fixture":"1"}, "license_evidence":"weight-license.txt",
                "files":{"weight-license.txt":digest(evidence)}}))
            with patch("provision.importlib.metadata.distributions", return_value=[distribution]), \
                 patch("worker.importlib.metadata.version", return_value="1"), redirect_stdout(io.StringIO()):
                construction = manifest_path.read_bytes()
                bundled = root / "bundled"
                with patch.object(sys, "argv", ["provision.py", "--manifest", str(manifest_path),
                                               "--directory", str(bundled)]):
                    provision.main()
                self.assertEqual(validate_manifest(bundled / "manifest.json", "speech")["id"], "nsnet2")
                self.assertEqual(manifest_path.read_bytes(), construction)
                with patch.object(sys, "argv", ["provision.py", "--manifest", str(manifest_path),
                                               "--directory", str(source), "--populate-new"]):
                    provision.main()
                self.assertEqual(validate_manifest(manifest_path, "speech")["id"], "nsnet2")
                before = manifest_path.read_bytes()
                target = root / "reprovisioned"
                with patch.object(sys, "argv", ["provision.py", "--manifest", str(manifest_path),
                                               "--directory", str(target)]):
                    provision.main()
            current = validate_manifest(target / "manifest.json", "speech")
            self.assertEqual(current["runtime"]["worker"], str(target / "runtime/worker.py"))
            self.assertEqual(manifest_path.read_bytes(), before)
            self.assertEqual((target / "weight-license.txt").read_bytes(), evidence.read_bytes())

class ProtocolTests(unittest.TestCase):
    def request(self):
        return {"version":1,"task_id":"test","operation":"speech","input_path":"/input.wav",
                "output_dir":"/new-output","model_manifest":"/manifest.json"}

    def test_controls_reject_nonfinite_wrong_operation_and_unknown(self):
        for parameters in ({"wet_mix":float("nan")}, {"wet_mix":1.1}, {"overlap":0.5}, {"wet_mix":True}):
            with self.subTest(parameters=parameters), self.assertRaises(WorkerError):
                validate_request({**self.request(),"parameters":parameters})
        with self.assertRaises(WorkerError):
            validate_request({**self.request(),"threads":9})
        with self.assertRaises(WorkerError):
            validate_request({**self.request(),"input_path":"relative.wav"})

    def test_manifest_hash_and_path_binding(self):
        scratch = ROOT / ".cache/neural/tests"
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as directory:
            base = Path(directory)
            artifact = base / "weight-license.txt"
            manifest = bundle_fixture(base)
            path = base / "manifest.json"
            path.write_text(json.dumps(manifest))
            self.assertEqual(validate_manifest(path,"speech"),manifest)
            artifact.write_text("changed")
            with self.assertRaisesRegex(WorkerError,"SHA256 mismatch"):
                validate_manifest(path,"speech")
            manifest["files"] = {"../outside":"0"*64}
            path.write_text(json.dumps(manifest))
            with self.assertRaises(WorkerError):
                validate_manifest(path,"speech")

    def test_distinct_models_select_per_manifest_without_license_gate(self):
        scratch = ROOT / ".cache/neural/tests"
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as directory:
            base = Path(directory)
            for operation, models in (("separate", ("melband_roformer_kim", "htdemucs")),
                                      ("speech", ("MossFormer2_SE_48K", "nsnet2", "DeepFilterNet3"))):
                for model in models:
                    with self.subTest(model=model):
                        manifest = bundle_fixture(base, operation, model)
                        path = base / "manifest.json"
                        path.write_text(json.dumps(manifest))
                        self.assertEqual(validate_manifest(path,operation)["id"],model)
                        with self.assertRaises(WorkerError):
                            validate_manifest(path,"speech" if operation=="separate" else "separate")
            manifest["id"] = "unknown-engine"
            path.write_text(json.dumps(manifest))
            with self.assertRaisesRegex(WorkerError,"unsupported model"):
                validate_manifest(path,operation)

    def test_construction_manifest_cannot_be_admitted_for_inference(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            manifest = bundle_fixture(base)
            manifest.pop("runtime")
            manifest.pop("dependencies")
            path = base / "manifest.json"
            path.write_text(json.dumps(manifest))
            self.assertEqual(validate_artifacts(path, "speech"), manifest)
            with self.assertRaisesRegex(WorkerError, "provisioned runtime"):
                validate_manifest(path, "speech")

    def test_runtime_and_dependency_metadata_are_required_and_bound(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            mutations = (
                lambda value: value.pop("dependencies"),
                lambda value: value["runtime"].update(model_manifest="/different/manifest.json"),
                lambda value: value["runtime"].update(interpreter_sha256="0" * 64),
                lambda value: value["files"].pop("runtime/worker.py"),
                lambda value: value["dependencies"]["fixture"].update(version="2"),
                lambda value: value["dependencies"]["fixture"].update(notices=["missing.txt"]),
            )
            for mutate in mutations:
                manifest = bundle_fixture(base)
                mutate(manifest)
                path = base / "manifest.json"
                path.write_text(json.dumps(manifest))
                with self.subTest(mutation=mutate), self.assertRaises(WorkerError):
                    validate_manifest(path, "speech")

    def test_config_hash_is_canonical_and_changes_with_controls(self):
        self.assertEqual(canonical_digest({"a":1,"b":2}),canonical_digest({"b":2,"a":1}))
        self.assertNotEqual(canonical_digest({"wet_mix":1}),canonical_digest({"wet_mix":0}))

    def test_inference_offline_guard_blocks_dns_and_udp(self):
        import socket
        from unittest.mock import patch
        with patch.object(socket,"getaddrinfo"), patch.object(socket,"gethostbyname"), patch.object(socket,"gethostbyname_ex"), \
             patch.object(socket,"create_connection"), patch.object(socket.socket,"connect"), \
             patch.object(socket.socket,"connect_ex"), patch.object(socket.socket,"sendto"):
            deny_network()
            with self.assertRaisesRegex(WorkerError,"offline"):
                socket.getaddrinfo("example.com",443)
            with socket.socket(socket.AF_INET,socket.SOCK_DGRAM) as connection:
                with self.assertRaisesRegex(WorkerError,"offline"):
                    connection.sendto(b"blocked",("127.0.0.1",9))

class SignalTests(unittest.TestCase):
    def test_resampling_absolute_impulse_alignment_and_length(self):
        import numpy as np
        for rate in (8000,16000,44100,48000,96000,192000):
            with self.subTest(rate=rate):
                signal = np.zeros((rate*2+37,2),dtype="float32")
                signal[rate,0] = 1
                signal[rate//2,1] = 1
                output = resample_exact(resample_exact(signal,rate,48000),48000,rate)[:len(signal)]
                self.assertEqual(output.shape,signal.shape)
                self.assertEqual(int(np.argmax(output[:,0])),rate)
                self.assertEqual(int(np.argmax(output[:,1])),rate//2)
                self.assertTrue(np.isfinite(output).all())

@unittest.skipUnless(importlib.util.find_spec("numpy") and importlib.util.find_spec("soundfile"),
                     "NumPy and soundfile required for real WAV container behavior")
class OutputContainerTests(unittest.TestCase):
    @unittest.skipUnless(importlib.util.find_spec("jsonschema"), "jsonschema required for event protocol validation")
    def test_actual_dry_worker_events_validate_against_protocol_schema(self):
        import subprocess
        import sys
        import jsonschema
        import numpy as np
        import soundfile as sf
        schema = json.loads((Path(__file__).parent / "event.schema.json").read_text())
        validator = jsonschema.Draft202012Validator(schema)
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory).resolve()
            bundle_fixture(base)
            source = base / "input.wav"
            sf.write(source, np.array([[0.125, -0.25], [1.25, -1.25]], dtype="float32"), 44100, subtype="FLOAT")
            request = {**ProtocolTests().request(), "input_path":str(source),
                       "output_dir":str(base / "result"), "model_manifest":str(base / "manifest.json"),
                       "parameters":{"wet_mix":0}}
            request_path = base / "request.json"
            request_path.write_text(json.dumps(request))
            result = subprocess.run([sys.executable, str(Path(__file__).parent / "worker.py"),
                                     "--request", str(request_path)], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            events = [json.loads(line) for line in result.stdout.splitlines()]
            self.assertEqual(events[-1]["type"], "complete")
            self.assertEqual((events[-1]["frames"], events[-1]["sample_rate"], events[-1]["channels"]), (2, 44100, 2))
            self.assertIn("copy", [event.get("stage") for event in events])
            for event in events:
                validator.validate(event)

    def test_fractional_speech_blends_source_with_transformed_model_output(self):
        import numpy as np
        import soundfile as sf
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory).resolve()
            bundle_fixture(base)
            audio = np.linspace(-0.8, 0.8, 16014, dtype="float32").reshape(-1, 2)
            source = base / "input.wav"
            sf.write(source, audio, 8000, subtype="FLOAT")
            request = {**ProtocolTests().request(), "input_path":str(source),
                       "output_dir":str(base / "result"), "model_manifest":str(base / "manifest.json"),
                       "parameters":{"wet_mix":0.25}}
            calls = []
            def infer(samples):
                calls.append(samples.shape)
                return samples * -0.5 + np.float32(0.125)
            with patch("worker.build_engine", return_value=(8000, infer)), \
                 patch("worker.importlib.metadata.version", return_value="1"), \
                 patch("worker.deny_network"), patch("worker.CORE_SECONDS", 1), patch.dict(os.environ):
                process(request, lambda *args, **kwargs: None)
            output, rate = sf.read(base / "result/speech.wav", dtype="float32", always_2d=True)
            np.testing.assert_array_equal(output, audio * 0.75 + (audio * -0.5 + np.float32(0.125)) * 0.25)
            self.assertEqual(rate, 8000)
            self.assertEqual(len(calls), 2)
            report = json.loads((base / "result/result.json").read_text())
            self.assertFalse(report["processing"]["bypass"])
            self.assertTrue(report["processing"]["model_executed"])

    def test_dry_speech_copies_bounded_pcm_without_model_dependencies(self):
        import numpy as np
        import soundfile as sf
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory).resolve()
            bundle_fixture(base)
            source = base / "input.wav"
            for channels, rate in ((1, 8000), (2, 44100)):
                audio = np.linspace(-1.25, 1.25, (rate + 7) * channels, dtype="float32").reshape(-1, channels)
                audio[1, 0] = np.float32(-0.0)
                sf.write(source, audio, rate, subtype="FLOAT")
                destination = base / str(channels)
                request = {**ProtocolTests().request(), "input_path":str(source),
                           "output_dir":str(destination), "model_manifest":str(base / "manifest.json"),
                           "device":"cuda", "parameters":{"wet_mix":0}}
                events = []
                with patch("worker.check_packages", side_effect=AssertionError("packages loaded")), \
                     patch("worker.build_engine", side_effect=AssertionError("model loaded")), \
                     patch("worker.resample_exact", side_effect=AssertionError("resampled")), \
                     patch("worker.importlib.metadata.version", side_effect=AssertionError("dependency queried")), \
                     patch("worker.deny_network"), patch("worker.CORE_SECONDS", 1), patch.dict(os.environ), \
                     patch.object(sf.SoundFile, "read", autospec=True, side_effect=sf.SoundFile.read) as reads:
                    process(request, lambda kind, **fields: events.append((kind, fields)))
                output, output_rate = sf.read(destination / "speech.wav", dtype="float32", always_2d=True)
                np.testing.assert_array_equal(output.view("uint32"), audio.view("uint32"))
                self.assertEqual([call.args[1] for call in reads.call_args_list], [rate, 7])
                self.assertEqual(output_rate, rate)
                self.assertEqual(events[-1][0], "complete")
                self.assertEqual(events[-1][1]["frames"], len(audio))
                self.assertNotIn("load", [fields.get("stage") for _, fields in events])
                report = json.loads((destination / "result.json").read_text())
                self.assertTrue(report["processing"]["bypass"])
                self.assertFalse(report["processing"]["model_executed"])
                self.assertIsNone(report["processing"]["resampling"])
                self.assertEqual(report["processing"]["context_seconds"], 0)
                self.assertEqual(len(report["metrics"]["block_seconds"]), 2)
                self.assertEqual(report["frames"], len(audio))
                self.assertEqual(report["metrics"]["output_peaks"]["speech"], 1.25)
                expected_rms = float(np.sqrt(np.mean(np.square(audio.astype("float64")))))
                self.assertAlmostEqual(report["metrics"]["input_rms"], expected_rms, places=12)
                self.assertEqual(report["metrics"]["input_rms"], report["metrics"]["target_rms"])
                for metric in ("load_seconds", "inference_seconds", "resampling_seconds"):
                    self.assertEqual(report["metrics"][metric], 0)

    def test_dry_speech_still_rejects_model_controls_artifacts_and_bad_samples(self):
        import numpy as np
        import soundfile as sf
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory).resolve()
            bundle_fixture(base)
            source = base / "input.wav"
            sf.write(source, np.zeros((8, 1), dtype="float32"), 8000, subtype="FLOAT")
            destination = base / "result"
            request = {**ProtocolTests().request(), "input_path":str(source),
                       "output_dir":str(destination), "model_manifest":str(base / "manifest.json"),
                       "parameters":{"wet_mix":0, "post_filter":False}}
            with self.assertRaisesRegex(WorkerError, "post_filter"):
                process(request, lambda *args, **kwargs: None)
            request["parameters"] = {"wet_mix":0}
            sf.write(source, np.full((8, 1), np.nan, dtype="float32"), 8000, subtype="FLOAT")
            with patch("worker.deny_network"), patch.dict(os.environ):
                with self.assertRaisesRegex(WorkerError, "nonfinite"):
                    process(request, lambda *args, **kwargs: None)
            self.assertFalse(destination.exists())
            (base / "weight-license.txt").write_text("tampered")
            with self.assertRaisesRegex(WorkerError, "SHA256 mismatch"):
                process(request, lambda *args, **kwargs: None)
            self.assertFalse(destination.exists())

    def test_separation_uses_riff_when_each_stem_fits_even_if_total_exceeds_four_gib(self):
        import numpy as np
        import soundfile as sf
        from types import SimpleNamespace
        from unittest.mock import patch

        class InspectedOutputs(Exception):
            pass

        scratch = ROOT / ".cache/neural/tests"
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as directory:
            base = Path(directory)
            source = base / "input.wav"
            sf.write(source, np.zeros((32 * 48000, 2), dtype="float32"), 48000, subtype="FLOAT")
            manifest = base / "manifest.json"
            bundle_fixture(base, "separate", "htdemucs")
            destination = base / "result"
            request = {"version":1,"task_id":"riff-stems","operation":"separate","input_path":str(source),
                       "output_dir":str(destination),"model_manifest":str(manifest)}

            def inspect_outputs(_audio):
                for stem in ("vocals", "accompaniment"):
                    with (destination / (stem + ".wav")).open("rb") as stream:
                        self.assertEqual(stream.read(4), b"RIFF")
                raise InspectedOutputs

            # Exercise real output writers before inference. Advertise a long
            # source only at admission, avoiding multi-GiB fixtures/model loads.
            info = SimpleNamespace(format="WAV", subtype="FLOAT", samplerate=48000, channels=2,
                                   frames=2**28 + 1)
            with patch("worker.build_engine", return_value=(48000, inspect_outputs)), \
                 patch("worker.deny_network"), patch.dict(os.environ), \
                 patch("worker.importlib.metadata.version", return_value="1"), \
                 patch.object(sf, "info", return_value=info), \
                 patch("worker.shutil.disk_usage", return_value=SimpleNamespace(free=16 * 1024**3)):
                with self.assertRaises(InspectedOutputs):
                    process(request, lambda *args, **kwargs: None)
            self.assertFalse(destination.exists())

    def test_large_report_does_not_expand_completion_and_preserves_audio(self):
        import io
        import sys
        import numpy as np
        import soundfile as sf
        import worker
        from contextlib import redirect_stdout
        from unittest.mock import patch

        scratch = ROOT / ".cache/neural/tests"
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as directory:
            base = Path(directory).resolve()
            source = base / "input.wav"
            audio = np.full((8001, 1), 0.25, dtype="float32")
            sf.write(source, audio, 8000, subtype="FLOAT")
            manifest = base / "manifest.json"
            bundle_fixture(base, provenance={"record":"x" * 70000})
            destination = base / "result"
            request = {**ProtocolTests().request(), "input_path":str(source),
                       "output_dir":str(destination), "model_manifest":str(manifest)}
            request_path = base / "request.json"
            request_path.write_text(json.dumps(request))
            stdout = io.StringIO()
            # Only inference is synthetic: admission, WAV I/O, reporting and
            # the actual main() JSONL serializer execute unchanged.
            with patch.object(worker, "build_engine", return_value=(8000, lambda samples: samples * 0.5)), \
                 patch.object(worker, "deny_network"), patch.object(worker, "CORE_SECONDS", 1), \
                 patch("worker.importlib.metadata.version", return_value="1"), \
                 patch.dict(os.environ), patch.object(sys, "argv", ["worker.py", "--request", str(request_path)]), \
                 redirect_stdout(stdout):
                self.assertEqual(worker.main(), 0)
            lines = stdout.getvalue().splitlines()
            event = json.loads(lines[-1])
            self.assertEqual(event["type"], "complete")
            self.assertEqual(event["version"], 2)
            self.assertLess(len(lines[-1].encode("utf-8")) + 1, 65536)
            report_path = destination / "result.json"
            self.assertGreater(report_path.stat().st_size, 65536)
            report = json.loads(report_path.read_text())
            self.assertEqual(report["model"]["provenance"]["record"], "x" * 70000)
            self.assertEqual(len(report["metrics"]["block_seconds"]), 2)
            self.assertEqual(report["source_sha256"], digest(source))
            output, rate = sf.read(destination / event["outputs"]["speech"], dtype="float32", always_2d=True)
            self.assertEqual(rate, 8000)
            np.testing.assert_array_equal(output, audio * 0.5)


@unittest.skipUnless(os.environ.get("RIVU_SPEECH_MANIFEST"),"explicit prepared real speech model required")
class RealSpeechTests(unittest.TestCase):
    def test_real_model_alignment_wet_bypass_and_no_independent_scaling(self):
        import numpy as np
        import soundfile as sf
        scratch = ROOT / ".cache/neural/tests"
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as directory:
            base = Path(directory)
            rate = 44100
            audio = np.random.default_rng(123).normal(0,0.1,(rate+37,2)).astype("float32")
            audio[100,0] = 1.5
            audio[101,0] = np.float32(-0.0)
            source = base / "input.wav"
            sf.write(source,audio,rate,subtype="FLOAT")
            request = {"version":1,"task_id":"real-bypass","operation":"speech","input_path":str(source),
                       "output_dir":str(base/"result"),"model_manifest":os.environ["RIVU_SPEECH_MANIFEST"],
                       "threads":1,"parameters":{"wet_mix":0.0,"attenuation_limit_db":80}}
            events = []
            process(request,lambda kind,**fields:events.append((kind,fields)))
            self.assertEqual(events[-1][0],"complete")
            output, output_rate = sf.read(base / "result" / events[-1][1]["outputs"]["speech"],dtype="float32",always_2d=True)
            self.assertEqual(output_rate,rate)
            np.testing.assert_array_equal(output.view("uint32"),audio.view("uint32"))
            self.assertGreater(float(output.max()),1)
            with self.assertRaisesRegex(WorkerError,"already exist"):
                process(request,lambda *args,**kwargs:None)

    def test_real_model_rejects_multichannel_before_inference(self):
        import numpy as np
        import soundfile as sf
        scratch = ROOT / ".cache/neural/tests"
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as directory:
            base = Path(directory)
            source = base/"input.wav"
            sf.write(source,np.zeros((100,3),dtype="float32"),48000,subtype="FLOAT")
            request = {"version":1,"task_id":"multichannel","operation":"speech","input_path":str(source),
                       "output_dir":str(base/"result"),"model_manifest":os.environ["RIVU_SPEECH_MANIFEST"]}
            with self.assertRaisesRegex(WorkerError,"multichannel downmix"):
                process(request,lambda *args,**kwargs:None)
            self.assertFalse((base/"result").exists())

    def test_full_strength_silence_exact_length_and_source_unchanged(self):
        import subprocess
        import sys
        import numpy as np
        import soundfile as sf
        scratch = ROOT / ".cache/neural/tests"
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as directory:
            base = Path(directory)
            source = base/"silence.wav"
            audio = np.zeros((44100+37,2),dtype="float32")
            sf.write(source,audio,44100,subtype="FLOAT")
            original_hash = digest(source)
            request = {"version":1,"task_id":"full-strength-silence","operation":"speech","input_path":str(source),
                       "output_dir":str(base/"result"),"model_manifest":os.environ["RIVU_SPEECH_MANIFEST"],
                       "threads":1,"parameters":{"wet_mix":1.0,"attenuation_limit_db":80}}
            request_path = base/"request.json"
            request_path.write_text(json.dumps(request))
            completed = subprocess.run([sys.executable,str(Path(__file__).with_name("worker.py")),"--request",str(request_path)],
                                       capture_output=True,text=True,check=True)
            event = json.loads(completed.stdout.splitlines()[-1])
            self.assertEqual(event["type"],"complete")
            output,rate = sf.read(base / "result" / event["outputs"]["speech"],dtype="float32",always_2d=True)
            self.assertEqual(rate,44100)
            np.testing.assert_array_equal(output,audio)
            self.assertEqual(digest(source),original_hash)

class RealSeparationTests(unittest.TestCase):
    def check_model(self, variable, model_id):
        import subprocess
        import sys
        import numpy as np
        import soundfile as sf
        manifest = os.environ.get(variable)
        if not manifest:
            self.skipTest("explicit prepared real separation model required: " + variable)
        scratch = ROOT / ".cache/neural/tests"
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as directory:
            base = Path(directory)
            rate = 48000
            # Distinct stereo signals, a partial final block, and impulses on
            # both sides of the outer core boundary exercise source-rate trim.
            time = np.arange(24*rate+37, dtype="float64") / rate
            audio = np.column_stack((0.1*np.sin(2*np.pi*233*time),
                                     0.07*np.sin(2*np.pi*617*time))).astype("float32")
            audio[24*rate-1, 0] = 1.5
            audio[24*rate+1, 1] = -1.25
            source = base / "input.wav"
            sf.write(source, audio, rate, subtype="FLOAT")
            source_hash = digest(source)
            request = {"version":1, "task_id":"real-separation", "operation":"separate",
                       "input_path":str(source), "output_dir":str(base/"result"),
                       "model_manifest":manifest, "threads":1, "parameters":{"overlap":0.75}}
            request_path = base / "request.json"
            request_path.write_text(json.dumps(request))
            completed = subprocess.run([sys.executable, str(Path(__file__).with_name("worker.py")),
                                        "--request", str(request_path)],
                                       capture_output=True, text=True, check=True)
            event = json.loads(completed.stdout.splitlines()[-1])
            self.assertEqual(event["type"], "complete")
            self.assertEqual(event["model"]["id"], model_id)
            self.assertEqual((event["sample_rate"], event["channels"], event["frames"]),
                             (rate, 2, len(audio)))
            stems = {}
            for name in ("vocals", "accompaniment"):
                stems[name], output_rate = sf.read(base / "result" / event["outputs"][name], dtype="float32", always_2d=True)
                self.assertEqual(output_rate, rate)
                self.assertEqual(stems[name].shape, audio.shape)
                self.assertTrue(np.isfinite(stems[name]).all())
            np.testing.assert_array_equal(stems["accompaniment"].view("uint32"),
                                          (audio-stems["vocals"]).view("uint32"))
            self.assertEqual(digest(source), source_hash)
            report = json.loads((base / "result/result.json").read_text())
            self.assertEqual(len(report["metrics"]["block_seconds"]), 2)
            for metric, samples in (("input_rms", audio), ("target_rms", stems["vocals"])):
                expected_rms = float(np.sqrt(np.mean(np.square(samples.astype("float64")))))
                self.assertAlmostEqual(report["metrics"][metric], expected_rms, places=12)

    def test_kim_source_rate_stereo_boundary_and_exact_complement(self):
        self.check_model("RIVU_KIM_MANIFEST", "melband_roformer_kim")

    def test_htdemucs_source_rate_stereo_boundary_and_exact_complement(self):
        self.check_model("RIVU_HTDEMUCS_MANIFEST", "htdemucs")

if __name__ == "__main__":
    unittest.main()
