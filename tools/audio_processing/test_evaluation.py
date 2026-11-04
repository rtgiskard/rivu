"""Deterministic evaluation contracts; no model, worker or download required."""
import copy
import hashlib
import json
import math
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import numpy as np
import soundfile as sf

if __package__:
    from . import benchmark, evaluate
    from ..audio_quality import evaluate as quality, compare_separation, prepare_music, run_classical
    from ..audio_quality.metric_core import score_difference, score_summary
    from ..audio_quality.music_manifest import music_records
else:
    import benchmark
    import evaluate
    from audio_quality import evaluate as quality, compare_separation, prepare_music, run_classical
    from audio_quality.metric_core import score_difference, score_summary
    from audio_quality.music_manifest import music_records


class MetricSemanticsTests(unittest.TestCase):
    def scores(self, reference, estimate):
        ref = np.asarray(reference, dtype="float64")
        est = np.asarray(estimate, dtype="float64")
        processing = benchmark.metrics(ref[:, None], est[:, None], 8000)
        paired = quality.channel_metrics(ref, est)
        self.assertEqual(processing["absolute_snr_db"], paired["snr_db"])
        self.assertEqual(processing["si_sdr_db"][0], paired["si_sdr_db"])
        self.assertEqual(processing["reference_gain"][0], paired["least_squares_reference_gain"])
        json.dumps([processing, paired], allow_nan=False)
        return paired

    def test_silence_and_constant_reference_are_not_zero_scores(self):
        for estimate in ([0, 0, 0, 0], [1, -1, 1, -1]):
            scores = self.scores([0, 0, 0, 0], estimate)
            self.assertIsNone(scores["snr_db"])
            self.assertIsNone(scores["si_sdr_db"])
            self.assertIsNone(scores["least_squares_reference_gain"])
        scores = self.scores([1, 1, 1, 1], [1, 1, 1, 1])
        self.assertEqual(scores["snr_db"], "infinity")
        self.assertIsNone(scores["si_sdr_db"])
        self.assertIsNone(scores["least_squares_reference_gain"])

    def test_exact_reconstruction_and_zero_output(self):
        reference = [-1, 1, -1, 1]
        exact = self.scores(reference, reference)
        self.assertEqual(exact["snr_db"], "infinity")
        self.assertEqual(exact["si_sdr_db"], "infinity")
        self.assertEqual(exact["least_squares_reference_gain"], 1)
        silent = self.scores(reference, [0, 0, 0, 0])
        self.assertEqual(silent["snr_db"], 0)
        self.assertIsNone(silent["si_sdr_db"])
        self.assertEqual(silent["least_squares_reference_gain"], 0)

    def test_finite_formula_and_extreme_finite_ratio(self):
        scores = self.scores([-1, 1, -1, 1], [-0.5, 1.5, -1.5, 0.5])
        self.assertAlmostEqual(scores["snr_db"], 10 * math.log10(4))
        self.assertAlmostEqual(scores["si_sdr_db"], 10 * math.log10(4))
        self.assertEqual(scores["least_squares_reference_gain"], 1)
        self.assertEqual(quality.db_ratio(1e300, 1e-300), 6000)

    def test_extended_scores_and_aggregation_stay_json_safe(self):
        for output, baseline, expected in (
            (4, 1, 3), (None, 1, None), (1, None, None),
            ("infinity", "infinity", None), ("infinity", 1, "infinity"),
            (1, "infinity", "-infinity"),
        ):
            self.assertEqual(score_difference(output, baseline), expected)
        summary = score_summary([None, "-infinity", 2, "infinity"])
        self.assertEqual(summary["median"], 2)
        self.assertEqual(summary["min"], "-infinity")
        self.assertEqual(summary["max"], "infinity")
        self.assertEqual(summary["undefined_count"], 1)
        mixed = score_summary(["-infinity", "infinity"])
        self.assertIsNone(mixed["median"])
        self.assertIsNone(score_summary([None])["median"])
        json.dumps([summary, mixed], allow_nan=False)


class MusicIdentityTests(unittest.TestCase):
    def prepare(self, root, inputs, diagnostic):
        argv = ["prepare_music.py", *map(str, inputs), "--output", str(root),
                "--provenance", "deterministic test", "--diagnostic-source", str(diagnostic)]
        with patch.object(sys, "argv", argv), patch.object(prepare_music.subprocess, "run"), patch("builtins.print"):
            prepare_music.main()
        return json.loads((root / "manifest.json").read_text())

    def test_input_reordering_and_content_rename_preserve_membership(self):
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            diagnostic = base / "known-erratum.stem.mp4"
            ordinary = base / "ordinary.stem.mp4"
            renamed = base / "renamed.stem.mp4"
            diagnostic.write_bytes(b"diagnostic content")
            renamed.write_bytes(diagnostic.read_bytes())
            ordinary.write_bytes(b"ordinary content")
            before = self.prepare(base / "before", [diagnostic, ordinary, renamed], diagnostic)
            after = self.prepare(base / "after", [ordinary, renamed, diagnostic], renamed)
            def membership(manifest, root):
                return {record["sha256"]:record["diagnostic_only"] for _, record in music_records(manifest, root)}
            self.assertEqual(membership(before, base / "before"), membership(after, base / "after"))
            self.assertFalse(after["records"][0]["diagnostic_only"])
            primary = [record[-1]["sha256"] for record in evaluate.cases("music", base / "after", set()) if not record[-2]]
            self.assertEqual(primary, [hashlib.sha256(ordinary.read_bytes()).hexdigest()])

    def test_missing_or_conflicting_identity_is_not_assumed_clean(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "track-00").mkdir()
            record = {"source":"renamable.stem.mp4", "sha256":"a"*64,
                      "directory":"track-00", "diagnostic_only":False}
            valid = {"schema":2, "records":[record]}
            music_records(valid, root)
            for field in ("source", "sha256", "diagnostic_only"):
                invalid = copy.deepcopy(valid)
                del invalid["records"][0][field]
                with self.assertRaises(ValueError):
                    music_records(invalid, root)
            with self.assertRaisesRegex(ValueError, "re-prepare"):
                music_records({"schema":1, "records":[record]}, root)
            (root / "track-01").mkdir()
            conflict = {**record, "directory":"track-01", "diagnostic_only":True}
            with self.assertRaisesRegex(ValueError, "conflicting"):
                music_records({"schema":2, "records":[record, conflict]}, root)


class ClassicalSummaryTests(unittest.TestCase):
    def row(self, output, baseline, mode="stationary", noise_kind="white_0dB"):
        return {"mode":mode, "noise_kind":noise_kind,
                "metrics_by_channel":[{"si_sdr_db":output}],
                "baseline_metrics_by_channel":[{"si_sdr_db":baseline}]}

    def test_finite_infinite_and_undefined_improvements(self):
        rows = [self.row(4, 1), self.row(1, 4), self.row(None, 1),
                self.row(1, None), self.row("infinity", "infinity"),
                self.row("infinity", 1), self.row(1, "infinity"),
                self.row(-100, 0, noise_kind="clean")]
        summary = run_classical.summarize_reports(rows)
        stationary = summary["stationary"]
        self.assertEqual(stationary["cases"], 7)
        self.assertEqual(stationary["degraded_cases"], 2)
        self.assertEqual(stationary["median_si_sdr_improvement_db"], 0)
        self.assertEqual(stationary["min_si_sdr_improvement_db"], "-infinity")
        self.assertEqual(stationary["max_si_sdr_improvement_db"], "infinity")
        self.assertEqual(stationary["si_sdr_improvement_counts"], {
            "defined_count":4, "undefined_count":3,
            "positive_infinity_count":1, "negative_infinity_count":1})
        self.assertEqual(summary["profile"]["cases"], 0)
        self.assertIsNone(summary["profile"]["median_si_sdr_improvement_db"])
        json.dumps(summary, allow_nan=False)

    def test_all_undefined_and_opposite_infinite_medians(self):
        summary = run_classical.summarize_reports([
            self.row(None, "infinity"),
            self.row("infinity", 1, mode="adaptive"),
            self.row(1, "infinity", mode="adaptive")])
        self.assertEqual(summary["stationary"]["degraded_cases"], 0)
        self.assertIsNone(summary["stationary"]["median_si_sdr_improvement_db"])
        self.assertEqual(summary["stationary"]["si_sdr_improvement_counts"]["undefined_count"], 1)
        self.assertIsNone(summary["adaptive"]["median_si_sdr_improvement_db"])
        self.assertEqual(summary["adaptive"]["degraded_cases"], 1)
        json.dumps(summary, allow_nan=False)


class EvaluationIdentityTests(unittest.TestCase):
    def music_corpus(self, root):
        folder = root / "track-00"
        folder.mkdir()
        audio = np.sin(np.arange(512, dtype="float32")[:, None] * 0.125) * np.array([[0.25, 0.125]], dtype="float32")
        for name, data in (("vocals", audio), ("mixture", audio * 1.5), ("accompaniment", audio * 0.5)):
            sf.write(folder / (name + ".wav"), data, 8000, subtype="FLOAT")
        (root / "manifest.json").write_text(json.dumps({"schema":2, "records":[{
            "directory":"track-00", "source":"original.mp4", "sha256":"a"*64, "diagnostic_only":False}]}))
        model = root / "model.json"
        model.write_text(json.dumps({"operation":"separate"}))
        argv = ["evaluate.py", "--kind", "music", "--corpus-root", str(root),
                "--model-manifest", str(model), "--output-dir", str(root / "evaluation")]
        return folder, audio, argv

    def test_each_loaded_corpus_pcm_rejects_replacement_during_decode(self):
        read_audio = sf.read
        for stem in ("mixture", "vocals", "accompaniment"):
            with self.subTest(stem=stem), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                folder, _, argv = self.music_corpus(root)
                target = folder / (stem + ".wav")
                def replace_after_read(path, *args, **kwargs):
                    data, rate = read_audio(path, *args, **kwargs)
                    if Path(path) == target:
                        sf.write(path, data * 0.75, rate, subtype="FLOAT")
                    return data, rate
                with patch.object(sys, "argv", argv), patch.object(sf, "read", side_effect=replace_after_read):
                    with self.assertRaisesRegex(ValueError, "audio changed while loading"):
                        evaluate.main()
                self.assertFalse((root / "evaluation" / "track-00-request.json").exists())

    def test_separation_comparison_rejects_changed_reference_or_mixture(self):
        read_audio = sf.read
        for stem in ("mixture", "vocals"):
            with self.subTest(stem=stem), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                folder, _, _ = self.music_corpus(root)
                target = folder / (stem + ".wav")
                args = SimpleNamespace(mixture=folder / "mixture.wav", reference=folder / "vocals.wav",
                                       provenance="test corpus", result=[])
                def replace_after_read(path, *args, **kwargs):
                    data, rate = read_audio(path, *args, **kwargs)
                    if Path(path) == target:
                        sf.write(path, data * 0.75, rate, subtype="FLOAT")
                    return data, rate
                with patch.object(sf, "read", side_effect=replace_after_read):
                    with self.assertRaisesRegex(ValueError, "audio changed while loading"):
                        compare_separation.compare(args)

    def test_missing_or_different_worker_source_is_rejected_before_output_scoring(self):
        for source_hash in (None, "b"*64):
            with self.subTest(source_hash=source_hash), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                _, _, argv = self.music_corpus(root)
                def worker(command, **kwargs):
                    request = json.loads(Path(command[-1]).read_text())
                    destination = Path(request["output_dir"])
                    destination.mkdir()
                    (destination / "result.json").write_text(json.dumps({"source_sha256":source_hash}))
                    event = {"version":evaluate.EVENT_VERSION, "type":"complete", "task_id":request["task_id"]}
                    return subprocess.CompletedProcess(command, 0, json.dumps(event) + "\n", "")
                with patch.object(sys, "argv", argv), patch.object(evaluate.subprocess, "run", side_effect=worker):
                    with self.assertRaisesRegex(ValueError, "worker input does not match"):
                        evaluate.main()

    def test_music_scores_the_identity_bound_pair_and_backing(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            folder, audio, argv = self.music_corpus(root)
            identities = {name:quality.identity(folder / (name + ".wav"))["sha256"]
                          for name in ("mixture", "vocals", "accompaniment")}
            def worker(command, **kwargs):
                request = json.loads(Path(command[-1]).read_text())
                destination = Path(request["output_dir"])
                destination.mkdir()
                outputs = {}
                for name, data in (("vocals", audio), ("accompaniment", audio * 0.5)):
                    path = destination / (name + ".wav")
                    sf.write(path, data, 8000, subtype="FLOAT")
                    outputs[name] = str(path)
                # Later reference changes must not relabel the snapshot already loaded.
                sf.write(folder / "vocals.wav", audio * 0.25, 8000, subtype="FLOAT")
                sf.write(folder / "accompaniment.wav", audio * 0.25, 8000, subtype="FLOAT")
                result = {"source_sha256":identities["mixture"], "outputs":outputs,
                          "processing":{"core_seconds":24}, "metrics":{"real_time_factor":1.0}}
                (destination / "result.json").write_text(json.dumps(result))
                event = {"version":evaluate.EVENT_VERSION, "type":"complete", "task_id":request["task_id"]}
                return subprocess.CompletedProcess(command, 0, json.dumps(event) + "\n", "")
            with patch.object(sys, "argv", argv), patch.object(evaluate.subprocess, "run", side_effect=worker), patch("builtins.print"):
                evaluate.main()
            report = json.loads((root / "evaluation" / "report.json").read_text())
            run = report["runs"][0]
            self.assertEqual(run["input_sha256"], identities["mixture"])
            self.assertEqual(run["reference_sha256"], identities["vocals"])
            self.assertEqual(run["accompaniment_reference_sha256"], identities["accompaniment"])
            self.assertEqual(run["output_quality"]["rms_error"], 0)
            self.assertEqual(run["accompaniment_quality"]["rms_error"], 0)
            for field in ("worker_subprocess_wall_seconds", "evaluation_wall_seconds"):
                self.assertGreaterEqual(run[field], 0)
                self.assertEqual(report["timing_summary"][field]["median"], run[field])
            json.dumps(report, allow_nan=False)


class PreparedQualityTests(unittest.TestCase):
    def test_case_snapshots_keep_identity_and_score_each_output_independently(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            reference = np.array([[-0.25], [0.25], [-0.125], [0.125]], dtype="float64")
            noisy = reference + np.array([[0.0625], [0.0625], [-0.0625], [-0.0625]])
            ref_path, noisy_path = root / "clean.wav", root / "noisy.wav"
            sf.write(ref_path, reference, 8000, subtype="FLOAT")
            sf.write(noisy_path, noisy, 8000, subtype="FLOAT")
            prepared = quality.ReferenceEvaluation(ref_path)
            baseline = prepared.baseline(noisy_path)
            reference_identity, noisy_identity = quality.identity(ref_path), quality.identity(noisy_path)
            sf.write(ref_path, reference * 0.5, 8000, subtype="FLOAT")
            sf.write(noisy_path, noisy * 0.5, 8000, subtype="FLOAT")
            for index, output in enumerate((reference, noisy, np.zeros_like(reference))):
                path = root / f"output-{index}.wav"
                sf.write(path, output, 8000, subtype="FLOAT")
                report = prepared.score(path, "test corpus", "test implementation", baseline=baseline)
                self.assertEqual(report["reference"], reference_identity)
                self.assertEqual(report["noisy_baseline"], noisy_identity)
                self.assertEqual(report["estimate"], quality.identity(path))
                self.assertEqual(report["metrics_by_channel"], [quality.channel_metrics(reference[:, 0], output[:, 0])])
                self.assertEqual(report["baseline_metrics_by_channel"], [quality.channel_metrics(reference[:, 0], noisy[:, 0])])
                json.dumps(report, allow_nan=False)
            fresh = quality.ReferenceEvaluation(ref_path)
            self.assertNotEqual(fresh.audio.identity, prepared.audio.identity)
            with self.assertRaisesRegex(ValueError, "another reference"):
                fresh.score(root / "output-0.wav", "test", "test", baseline=baseline)

    def test_standalone_api_preserves_reconstruction_and_exact_output_contract(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            reference = np.array([[-0.25, 0.25], [0.25, -0.25]], dtype="float64")
            for name, data in (("reference", reference), ("estimate", reference),
                               ("noisy", reference * 1.5), ("mixture", reference * 1.5),
                               ("complement", reference * 0.5)):
                sf.write(root / (name + ".wav"), data, 8000, subtype="FLOAT")
            args = SimpleNamespace(**{name:str(root / (name + ".wav")) for name in
                                   ("reference", "estimate", "noisy", "mixture", "complement")},
                                   provenance="test corpus", implementation="test implementation")
            report = quality.evaluate(args)
            self.assertEqual(report["reconstruction"]["residual_peak"], 0)
            self.assertEqual(report["metrics_by_channel"][0]["si_sdr_db"], "infinity")
            self.assertEqual(report["reconstruction"]["mixture"], quality.identity(args.mixture))
            for data, rate, message in ((reference[:1], 8000, "alignment"),
                                        (reference[:, :1], 8000, "alignment"),
                                        (reference, 16000, "alignment"),
                                        (reference * np.nan, 8000, "non-finite")):
                sf.write(args.estimate, data, rate, subtype="FLOAT")
                with self.subTest(shape=data.shape, rate=rate, message=message):
                    with self.assertRaisesRegex(ValueError, message):
                        quality.evaluate(args)

    def test_classical_modes_share_the_pair_and_score_each_final_output(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            corpus = root / "speech-00"
            corpus.mkdir()
            binary = root / "renderer"
            binary.write_bytes(b"test renderer identity")
            reference = np.array([-0.25, 0.25, -0.125, 0.125], dtype="float64")
            noisy = reference + np.array([0.0625, 0.0625, -0.0625, -0.0625])
            ref_path, noisy_path = corpus / "clean.wav", corpus / "white_+0dB.wav"
            sf.write(ref_path, reference, 8000, subtype="FLOAT")
            sf.write(noisy_path, noisy, 8000, subtype="FLOAT")
            (corpus / "manifest.json").write_text(json.dumps({"reference":str(ref_path),
                "provenance":"test corpus", "shared_reference_rms":0.08,
                "cases":[{"path":str(noisy_path), "name":"white_+0dB", "snr_db":0}]}))
            def render(command, **kwargs):
                _, source, estimate, config_path, *profile = command
                config = json.loads(Path(config_path).read_text())
                input_audio, rate = sf.read(source, always_2d=True)
                if config["mode"] == "profile":
                    profile_audio, profile_rate = sf.read(profile[0], always_2d=True)
                    self.assertEqual(profile_rate, rate)
                    np.testing.assert_array_equal(profile_audio, input_audio - reference[:, None])
                repeat = int(Path(estimate).stem.rsplit("-", 1)[1])
                factor = {"stationary":1.0, "profile":0.75, "adaptive":0.5}[config["mode"]] - repeat * 0.125
                sf.write(estimate, input_audio * factor, rate, subtype="FLOAT")
                if Path(source) == noisy_path:
                    # Scoring and the oracle profile must use the same loaded reference,
                    # not a later file version with a newly attached identity.
                    sf.write(ref_path, reference * 0.5, rate, subtype="FLOAT")
                return subprocess.CompletedProcess(command, 0, json.dumps({"core_seconds":repeat + 1}), "")
            argv = ["run_classical.py", "--binary", str(binary), "--corpus", str(corpus),
                    "--output", str(root / "results"), "--repeats", "2"]
            with patch.object(sys, "argv", argv), patch.object(run_classical.subprocess, "run", side_effect=render), patch("builtins.print"):
                run_classical.main()
            report = json.loads((root / "results" / "report.json").read_text())
            self.assertEqual(len(report["reports"]), 6)
            for row in report["reports"]:
                baseline = reference if row["noise_kind"] == "clean" else noisy
                self.assertEqual(row["timings"], [{"core_seconds":1}, {"core_seconds":2}])
                output, _ = sf.read(row["estimate"]["path"])
                self.assertEqual(row["metrics_by_channel"], [quality.channel_metrics(reference, output)])
                self.assertEqual(row["baseline_metrics_by_channel"], [quality.channel_metrics(reference, baseline)])
                self.assertEqual(row["estimate"], quality.identity(row["estimate"]["path"]))
            json.dumps(report, allow_nan=False)


class BenchmarkRepeatTests(unittest.TestCase):
    def benchmark_inputs(self, root):
        audio = np.array([[-0.25], [0.25], [-0.125], [0.125]], dtype="float32")
        paths = {name:root / (name + ".wav") for name in ("reference", "input", "upstream", "backing")}
        for path in paths.values():
            sf.write(path, audio, 8000, subtype="FLOAT")
        manifest = root / "manifest.json"
        manifest.write_text("{}")
        request_path = root / "request.json"
        request_path.write_text(json.dumps({"output_dir":str(root / "worker"), "input_path":str(paths["input"]),
                                            "operation":"separate", "task_id":"identity"}))
        argv = ["benchmark.py", "--request", str(request_path), "--reference", str(paths["reference"]),
                "--upstream-reference", str(paths["upstream"]), "--accompaniment-reference", str(paths["backing"]),
                "--corpus-manifest", str(manifest), "--report", str(root / "report.json")]
        return paths, argv

    def test_changed_fixed_input_reference_upstream_or_backing_is_not_scored(self):
        read_audio = sf.read
        for name in ("reference", "input", "upstream", "backing"):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                paths, argv = self.benchmark_inputs(root)
                def replace_after_read(path, *args, **kwargs):
                    data, rate = read_audio(path, *args, **kwargs)
                    if Path(path) == paths[name]:
                        sf.write(path, data * 0.5, rate, subtype="FLOAT")
                    return data, rate
                with patch.object(sys, "argv", argv), patch.object(sf, "read", side_effect=replace_after_read):
                    with self.assertRaisesRegex(ValueError, "audio changed while loading"):
                        benchmark.main()
                self.assertFalse((root / "worker-request-0.json").exists())

    def test_missing_or_changed_worker_input_hash_is_rejected_before_scoring(self):
        for source_hash in (None, "b"*64):
            with self.subTest(source_hash=source_hash), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                _, argv = self.benchmark_inputs(root)
                def worker(command, **kwargs):
                    request = json.loads(Path(command[-1]).read_text())
                    destination = Path(request["output_dir"])
                    destination.mkdir()
                    (destination / "result.json").write_text(json.dumps({"source_sha256":source_hash}))
                    event = {"version":benchmark.EVENT_VERSION, "type":"complete", "task_id":"identity"}
                    return subprocess.CompletedProcess(command, 0, json.dumps(event) + "\n", "")
                with patch.object(sys, "argv", argv), patch.object(benchmark.subprocess, "run", side_effect=worker):
                    with self.assertRaisesRegex(ValueError, "worker input does not match"):
                        benchmark.main()

    def test_each_repeat_scores_its_own_output_against_fixed_inputs(self):
        import soundfile as sf
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            rate = 8000
            reference = np.sin(np.arange(512, dtype="float32")[:, None] * 0.125) * 0.25
            noisy = reference * 0.5
            reference_path, input_path = root/"reference.wav", root/"input.wav"
            sf.write(reference_path, reference, rate, subtype="FLOAT")
            sf.write(input_path, noisy, rate, subtype="FLOAT")
            manifest = root/"manifest.json"
            manifest.write_text("{}")
            request = {"output_dir":str(root/"worker"), "input_path":str(input_path),
                       "operation":"speech", "task_id":"repeat"}
            request_path = root/"request.json"
            request_path.write_text(json.dumps(request))
            requests = []
            outputs = [reference, reference*0.75, np.zeros_like(reference)]
            def worker(command, **kwargs):
                current = json.loads(Path(command[-1]).read_text())
                destination = Path(current["output_dir"])
                destination.mkdir()
                output_path = destination/"speech.wav"
                sf.write(output_path, outputs[len(requests)], rate, subtype="FLOAT")
                requests.append(current)
                result = {"outputs":{"speech":str(output_path)}, "processing":{"core_seconds":24},
                          "source_sha256":benchmark.digest(input_path)}
                (destination/"result.json").write_text(json.dumps(result))
                event = {"version":benchmark.EVENT_VERSION, "type":"complete", "task_id":"repeat"}
                return subprocess.CompletedProcess(command, 0, json.dumps(event)+"\n", "")
            report_path = root/"report.json"
            argv = ["benchmark.py", "--request", str(request_path), "--reference", str(reference_path),
                    "--corpus-manifest", str(manifest), "--repeats", "3", "--report", str(report_path)]
            with patch.object(sys, "argv", argv), patch.object(benchmark.subprocess, "run", side_effect=worker), \
                    patch("builtins.print"):
                benchmark.main()
            report = json.loads(report_path.read_text())
            self.assertEqual(report["input_quality"], benchmark.metrics(reference, noisy, rate))
            self.assertEqual(len({current["output_dir"] for current in requests}), 3)
            for current in requests:
                self.assertEqual({key:value for key,value in current.items() if key != "output_dir"},
                                 {key:value for key,value in request.items() if key != "output_dir"})
            for run, output in zip(report["runs"], outputs):
                self.assertEqual(run["reference_quality"], benchmark.metrics(reference, output, rate))
            self.assertEqual(report["runs"][0]["reference_quality"]["rms_error"], 0)
            self.assertGreater(report["runs"][1]["reference_quality"]["rms_error"], 0)
            self.assertGreater(report["runs"][2]["reference_quality"]["rms_error"],
                               report["runs"][1]["reference_quality"]["rms_error"])


if __name__ == "__main__":
    unittest.main()
