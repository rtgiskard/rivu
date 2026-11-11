#!/usr/bin/env python3
"""Render actual classical DSP on prepared corpora and report paired metrics.

Stationary uses known synthetic RMS, and profile uses the explicitly available
synthetic noise-only reference. These are calibrated controls, not blind-mode
claims. Adaptive never receives clean-reference or noise-only data.
"""
import argparse
import json
import math
from pathlib import Path
import subprocess

import soundfile as sf
if __package__:
    from .evaluate import ReferenceEvaluation, identity
    from .metric_core import score_difference, score_summary
else:
    from evaluate import ReferenceEvaluation, identity
    from metric_core import score_difference, score_summary


def summarize_reports(reports):
    groups = {}
    for mode in ("stationary", "profile", "adaptive"):
        rows = [r for r in reports if r["mode"] == mode and r["noise_kind"] != "clean"]
        gains = [score_difference(r["metrics_by_channel"][0]["si_sdr_db"],
                                  r["baseline_metrics_by_channel"][0]["si_sdr_db"]) for r in rows]
        summary = score_summary(gains)
        groups[mode] = {
            "cases": len(rows),
            "median_si_sdr_improvement_db": summary["median"],
            "min_si_sdr_improvement_db": summary["min"],
            "max_si_sdr_improvement_db": summary["max"],
            "si_sdr_improvement_counts": {key:value for key,value in summary.items() if key.endswith("_count")},
            "degraded_cases": sum(float(gain) < 0 for gain in gains if gain is not None),
        }
    return groups


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--corpus", action="append", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--repeats", type=int, default=1)
    args = parser.parse_args()
    if not 1 <= args.repeats <= 20:
        raise ValueError("repeat bound is 1..20")
    binary = Path(args.binary).resolve(strict=True)
    output = Path(args.output).resolve()
    output.mkdir(parents=True, exist_ok=False)
    reports = []
    for corpus_index, corpus_name in enumerate(args.corpus):
        corpus = Path(corpus_name).resolve(strict=True)
        corpus_output = output / f"corpus-{corpus_index}"
        corpus_output.mkdir(exist_ok=False)
        manifest = json.loads((corpus / "manifest.json").read_text())
        reference = ReferenceEvaluation(manifest["reference"])
        clean, rate = reference.audio.data, reference.audio.rate
        cases = [{"path": reference.audio.identity["path"], "name": "clean", "snr_db": None}, *manifest["cases"]]
        for case_index, case in enumerate(cases):
            source = Path(case["path"])
            case_output = corpus_output / f"case-{case_index}"
            case_output.mkdir(exist_ok=False)
            baseline = reference.baseline(source)
            snr = case["snr_db"]
            for mode in ("stationary", "profile", "adaptive"):
                config = {"mode": mode}
                if mode == "stationary":
                    config["stationary_noise_dbfs"] = -90.0 if snr is None else 20 * math.log10(manifest["shared_reference_rms"]) - snr
                config_path = case_output / f"{mode}.json"
                with config_path.open("x") as stream:
                    stream.write(json.dumps(config) + "\n")
                profile_args = []
                if mode == "profile":
                    noisy = baseline.audio.data
                    profile = case_output / "profile-noise.wav"
                    with profile.open("xb") as stream:
                        sf.write(stream, noisy - clean, rate, format="WAV", subtype="FLOAT")
                    profile_args.append(str(profile))
                timings = []
                estimate = None
                for repeat in range(args.repeats):
                    estimate = case_output / f"{mode}-{repeat}.wav"
                    command = [str(binary), str(source), str(estimate), str(config_path), *profile_args]
                    if identity(source) != baseline.audio.identity:
                        raise ValueError("classical input changed before rendering: " + str(source))
                    result = subprocess.run(command, check=True, text=True, capture_output=True)
                    if identity(source) != baseline.audio.identity:
                        raise ValueError("classical input changed while rendering: " + str(source))
                    timings.append(json.loads(result.stdout))
                metrics = reference.score(estimate, manifest["provenance"],
                    f"rivu-dsp classical {mode}; config={config}; synthetic calibrated controls disclosed",
                    baseline=baseline)
                metrics.update({"speaker": corpus.name, "noise_kind": case["name"], "input_snr_db": snr, "mode": mode, "timings": timings})
                reports.append(metrics)
    groups = summarize_reports(reports)
    report = {"schema": 1, "summary": groups, "reports": reports, "limits": "Synthetic noise, four short source utterances; no perceptual or listening claim. Profile uses oracle synthetic noise. Timings exclude WAV I/O and profile capture; no cross-mode timing comparison made."}
    (output / "report.json").write_text(json.dumps(report, indent=2, allow_nan=False) + "\n")
    print(json.dumps({"report": str(output / "report.json"), "summary": groups}, indent=2, allow_nan=False))


if __name__ == "__main__":
    main()
