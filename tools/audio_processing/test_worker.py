"""Behavioral checks; optional real-model tests require RIVU_SPEECH_MANIFEST.

Run with the prepared speech interpreter: python -m unittest discover -s tools/audio_processing -p 'test_*.py'
"""
import json
import os
from pathlib import Path
import tempfile
import unittest
from worker import WorkerError, canonical_digest, deny_network, digest, process, resample_exact, validate_manifest, validate_request

ROOT = Path(__file__).resolve().parents[2]

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
            artifact.write_text("explicit test fixture, not a weight")
            manifest = {"version":1,"operation":"speech","id":"nsnet2","files":{"weight-license.txt":digest(artifact)},
                        "license_evidence":"weight-license.txt"}
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
            evidence = base / "weight-license.txt"
            evidence.write_text("Test provenance record: license unknown; no permission grant.")
            for operation, models in (("separate", ("melband_roformer_kim", "htdemucs")),
                                      ("speech", ("MossFormer2_SE_48K", "nsnet2", "DeepFilterNet3"))):
                for model in models:
                    with self.subTest(model=model):
                        manifest = {"version":1,"operation":operation,"id":model,
                                    "files":{"weight-license.txt":digest(evidence)},
                                    "license_evidence":"weight-license.txt"}
                        path = base / "manifest.json"
                        path.write_text(json.dumps(manifest))
                        self.assertEqual(validate_manifest(path,operation)["id"],model)
                        with self.assertRaises(WorkerError):
                            validate_manifest(path,"speech" if operation=="separate" else "separate")
            manifest["id"] = "unknown-engine"
            path.write_text(json.dumps(manifest))
            with self.assertRaisesRegex(WorkerError,"unsupported model"):
                validate_manifest(path,operation)

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
            output, output_rate = sf.read(events[-1][1]["outputs"]["speech"],dtype="float32",always_2d=True)
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
            output,rate = sf.read(event["outputs"]["speech"],dtype="float32",always_2d=True)
            self.assertEqual(rate,44100)
            np.testing.assert_array_equal(output,audio)
            self.assertEqual(digest(source),original_hash)

if __name__ == "__main__":
    unittest.main()
