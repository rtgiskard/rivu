#!/usr/bin/env python3
"""Compare real separation worker outputs on the exact same paired excerpt.

Example (with the prepared separation interpreter):
  python tools/audio_quality/compare_separation.py --mixture /abs/mixture.wav \
    --reference /abs/vocals.wav --result /abs/kim-output/result.json \
    --result /abs/htdemucs-output/result.json --provenance 'corpus/revision/recipe'

No inference, downloads, resampling, level matching, automatic alignment or
winner selection. Objective scores on one excerpt are not a latest/best claim.
Each --result is the actual worker's completed result.json, never an input model
name standing in for output. Results from different input contents are rejected.
"""
import argparse
import json
from pathlib import Path

import numpy as np
if __package__:
    from .evaluate import channel_metrics, identity, load, stereo_metrics
else:
    from evaluate import channel_metrics, identity, load, stereo_metrics


def compare(args):
    mixture, rate = load(args.mixture)
    reference, reference_rate = load(args.reference)
    if mixture.shape != reference.shape or reference_rate != rate or mixture.shape[1] != 2:
        raise ValueError("aligned stereo mixture/vocal references required")
    mixture_identity = identity(args.mixture)
    report = {"schema":1,"mixture":mixture_identity,"reference":identity(args.reference),
              "sample_rate":rate,"frames":len(mixture),"channels":2,"provenance":args.provenance,
              "baseline_metrics_by_channel":[channel_metrics(a,b) for a,b in zip(reference.T,mixture.T)],
              "models":[],"limits":"Same excerpt only; no listening review or full-corpus/model ranking. SI-SDR is scale invariant; inspect ordinary SNR, gain, stereo and peaks too. Mixture-minus-vocals reconstruction does not establish separation quality."}
    for path in args.result:
        path = Path(path).resolve(strict=True)
        if path.stat().st_size > 1024**2:
            raise ValueError("worker result JSON exceeds 1 MiB")
        result = json.loads(path.read_text())
        if result["source_sha256"] != mixture_identity["sha256"]:
            raise ValueError("worker input content differs from selected mixture: " + str(path))
        if (result["sample_rate"],result["frames"],result["channels"]) != (rate,len(mixture),2):
            raise ValueError("worker timeline differs from paired references: " + str(path))
        vocals, vocal_rate = load(result["outputs"]["vocals"])
        backing, backing_rate = load(result["outputs"]["accompaniment"])
        if vocals.shape != mixture.shape or backing.shape != mixture.shape or vocal_rate != rate or backing_rate != rate:
            raise ValueError("worker outputs violate alignment contract: " + str(path))
        residual = vocals + backing - mixture
        report["models"].append({
            "model":result["model"],"worker_result":identity(path),
            "request_sha256":result["request_sha256"],"worker_sha256":result["worker_sha256"],
            "packages":result["packages"],"device":result["device"],"threads":result["threads"],
            "processing":result["processing"],"performance":result["metrics"],
            "vocals":identity(result["outputs"]["vocals"]),
            "accompaniment":identity(result["outputs"]["accompaniment"]),
            "vocal_metrics_by_channel":[channel_metrics(a,b) for a,b in zip(reference.T,vocals.T)],
            "vocal_stereo":stereo_metrics(vocals),
            "reconstruction":{"residual_rms":float(np.sqrt(np.mean(residual**2))),
                              "residual_peak":float(np.max(np.abs(residual)))},
        })
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mixture", required=True)
    parser.add_argument("--reference", required=True)
    parser.add_argument("--result", action="append", required=True)
    parser.add_argument("--provenance", required=True)
    args = parser.parse_args()
    if not 2 <= len(args.result) <= 8:
        parser.error("provide 2..8 real completed model results for the same excerpt")
    print(json.dumps(compare(args),indent=2,allow_nan=False))


if __name__ == "__main__":
    main()
