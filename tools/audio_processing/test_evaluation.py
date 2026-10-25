"""Deterministic evaluation contracts; no model, worker or download required."""
import copy
import hashlib
import json
import math
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import numpy as np

if __package__:
    from . import benchmark, evaluate
    from ..audio_quality import evaluate as quality, prepare_music, run_classical
    from ..audio_quality.metric_core import score_difference, score_summary
    from ..audio_quality.music_manifest import music_records
else:
    import benchmark
    import evaluate
    from audio_quality import evaluate as quality, prepare_music, run_classical
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


class TimingBoundaryTests(unittest.TestCase):
    def run_tool(self, module, root):
        clock = [0.0]
        audio = np.ones((8, 2), dtype="float32")
        def read_audio(*args, **kwargs):
            clock[0] += 2
            return audio.copy(), 8000
        def digest(path):
            clock[0] += 5
            return "a"*64
        def metrics(*args):
            clock[0] += 3
            return {"si_sdr_db":[1.0, "infinity"]}
        def worker(command, **kwargs):
            request = json.loads(Path(command[-1]).read_text())
            destination = Path(request["output_dir"])
            destination.mkdir()
            result = {"outputs":{"vocals":str(destination/"vocals.wav"),
                                 "accompaniment":str(destination/"accompaniment.wav")},
                      "processing":{"core_seconds":24}, "metrics":{"real_time_factor":1.0}}
            (destination/"result.json").write_text(json.dumps(result))
            clock[0] += 7
            event = {"version":2, "type":"complete", "task_id":request["task_id"]}
            return subprocess.CompletedProcess(command, 0, json.dumps(event)+"\n", "")
        manifest = root/"manifest.json"
        if module is benchmark:
            manifest.write_text("{}")
            request_path = root/"request.json"
            request_path.write_text(json.dumps({"output_dir":str(root/"worker"), "input_path":"input.wav",
                                                "operation":"separate", "task_id":"sample"}))
            report_path = root/"report.json"
            argv = ["benchmark.py", "--request", str(request_path), "--reference", "reference.wav",
                    "--corpus-manifest", str(manifest), "--upstream-reference", "upstream.wav",
                    "--accompaniment-reference", "backing.wav", "--repeats", "1", "--report", str(report_path)]
        else:
            (root/"track-00").mkdir()
            manifest.write_text(json.dumps({"schema":2,"records":[{"directory":"track-00", "source":"original.mp4",
                                                                    "sha256":"a"*64, "diagnostic_only":False}]}))
            model = root/"model.json"
            model.write_text(json.dumps({"operation":"separate"}))
            report_path = root/"evaluation/report.json"
            argv = ["evaluate.py", "--kind", "music", "--corpus-root", str(root),
                    "--model-manifest", str(model), "--output-dir", str(root/"evaluation")]
        with patch.object(sys, "argv", argv), patch("soundfile.read", side_effect=read_audio), \
                patch.object(module, "digest", side_effect=digest), patch.object(module, "metrics", side_effect=metrics), \
                patch.object(module.subprocess, "run", side_effect=worker), \
                patch.object(module.time, "perf_counter", side_effect=lambda:clock[0]), patch("builtins.print"):
            module.main()
        return json.loads(report_path.read_text())

    def test_both_tools_include_last_score_but_not_setup_in_evaluation(self):
        for module, expected in ((benchmark, 41), (evaluate, 41)):
            with self.subTest(module=module.__name__), tempfile.TemporaryDirectory() as temporary:
                report = self.run_tool(module, Path(temporary))
                run = report["runs"][0]
                self.assertEqual(run["worker_subprocess_wall_seconds"], 7)
                self.assertEqual(run["evaluation_wall_seconds"], expected)
                for field in ("worker_subprocess_wall_seconds", "evaluation_wall_seconds"):
                    self.assertEqual(report["timing_summary"][field]["median"], run[field])
                self.assertEqual(run["si_sdr_gain_db"], [0, None])
                json.dumps(report, allow_nan=False)


if __name__ == "__main__":
    unittest.main()
