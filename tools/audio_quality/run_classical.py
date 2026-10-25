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
from types import SimpleNamespace

import soundfile as sf
if __package__:
    from .evaluate import evaluate
    from .metric_core import score_difference, score_summary
else:
    from evaluate import evaluate
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
    for corpus_name in args.corpus:
        corpus = Path(corpus_name).resolve(strict=True)
        manifest = json.loads((corpus / "manifest.json").read_text())
        reference = Path(manifest["reference"])
        clean, rate = sf.read(reference, dtype="float64")
        cases = [{"path": str(reference), "name": "clean", "snr_db": None}, *manifest["cases"]]
        for case in cases:
            source = Path(case["path"])
            snr = case["snr_db"]
            for mode in ("stationary", "profile", "adaptive"):
                stem = f"{corpus.name}-{source.stem}-{mode}"
                config = {"mode": mode}
                if mode == "stationary":
                    config["stationary_noise_dbfs"] = -90.0 if snr is None else 20 * math.log10(manifest["shared_reference_rms"]) - snr
                config_path = output / f"{stem}.json"
                config_path.write_text(json.dumps(config) + "\n")
                profile_args = []
                if mode == "profile":
                    noisy, noisy_rate = sf.read(source, dtype="float64")
                    if rate != noisy_rate or clean.shape != noisy.shape:
                        raise ValueError("noise-profile reference alignment mismatch")
                    profile = output / f"{stem}-noise.wav"
                    sf.write(profile, noisy - clean, rate, subtype="FLOAT")
                    profile_args.append(str(profile))
                timings = []
                estimate = None
                for repeat in range(args.repeats):
                    estimate = output / f"{stem}-{repeat}.wav"
                    command = [str(binary), str(source), str(estimate), str(config_path), *profile_args]
                    result = subprocess.run(command, check=True, text=True, capture_output=True)
                    timings.append(json.loads(result.stdout))
                metrics = evaluate(SimpleNamespace(reference=str(reference), estimate=str(estimate), noisy=str(source), complement=None, mixture=None, provenance=manifest["provenance"], implementation=f"rivu-dsp classical {mode}; config={config}; synthetic calibrated controls disclosed"))
                metrics.update({"speaker": corpus.name, "noise_kind": case["name"], "input_snr_db": snr, "mode": mode, "timings": timings})
                reports.append(metrics)
    groups = summarize_reports(reports)
    report = {"schema": 1, "summary": groups, "reports": reports, "limits": "Synthetic noise, four short source utterances; no perceptual or listening claim. Profile uses oracle synthetic noise. Timings exclude WAV I/O and profile capture; no cross-mode timing comparison made."}
    (output / "report.json").write_text(json.dumps(report, indent=2, allow_nan=False) + "\n")
    print(json.dumps({"report": str(output / "report.json"), "summary": groups}, indent=2, allow_nan=False))


if __name__ == "__main__":
    main()
