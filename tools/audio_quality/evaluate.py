#!/usr/bin/env python3
"""Paired audio quality evidence, not a substitute for a listening review.

Requires numpy and soundfile in the explicitly selected evaluation environment.
No automatic alignment, level matching, resampling, clipping, or source permutation.
"""
import argparse
import hashlib
import json
from pathlib import Path
import platform
import sys

import numpy as np
import soundfile as sf

MAX_FRAMES = 12_000_000
MAX_CHANNELS = 8


def identity(path):
    path = Path(path).resolve(strict=True)
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while block := stream.read(1024 * 1024):
            digest.update(block)
    return {"path": str(path), "sha256": digest.hexdigest(), "bytes": path.stat().st_size}


def load(path):
    info = sf.info(path)
    if not 0 < info.frames <= MAX_FRAMES or not 0 < info.channels <= MAX_CHANNELS:
        raise ValueError(f"evaluation bound exceeded: {info.frames} frames/{info.channels} channels")
    data, rate = sf.read(path, dtype="float64", always_2d=True)
    if not np.isfinite(data).all():
        raise ValueError(f"non-finite audio: {path}")
    return data, rate


def db_ratio(numerator, denominator):
    if numerator == 0:
        return None
    if denominator == 0:
        return "infinity"
    return float(10 * np.log10(numerator / denominator))


def channel_metrics(reference, estimate):
    ref = reference - reference.mean()
    est = estimate - estimate.mean()
    energy = float(np.dot(ref, ref))
    alpha = float(np.dot(ref, est) / energy) if energy else None
    target = alpha * ref if alpha is not None else np.zeros_like(ref)
    error = estimate - reference
    return {
        "reference_rms": float(np.sqrt(np.mean(reference ** 2))),
        "estimate_rms": float(np.sqrt(np.mean(estimate ** 2))),
        "reference_dc": float(reference.mean()),
        "estimate_dc": float(estimate.mean()),
        "peak_abs": float(np.max(np.abs(estimate))),
        "peak_error_abs": float(np.max(np.abs(error))),
        "snr_db": db_ratio(float(np.dot(reference, reference)), float(np.dot(error, error))),
        "si_sdr_db": db_ratio(float(np.dot(target, target)), float(np.dot(est - target, est - target))),
        "least_squares_reference_gain": alpha,
        "overs_count": int(np.count_nonzero(np.abs(estimate) > 1)),
    }


def stereo_metrics(data):
    if data.shape[1] != 2:
        return None
    left, right = data.T
    mid, side = (left + right) / 2, (left - right) / 2
    denominator = float(np.linalg.norm(left) * np.linalg.norm(right))
    return {
        "channel_correlation": float(np.dot(left, right) / denominator) if denominator else None,
        "mid_rms": float(np.sqrt(np.mean(mid ** 2))),
        "side_rms": float(np.sqrt(np.mean(side ** 2))),
    }


def evaluate(args):
    reference, rate = load(args.reference)
    estimate, estimate_rate = load(args.estimate)
    if rate != estimate_rate or reference.shape != estimate.shape:
        raise ValueError(f"alignment contract violated: reference={rate}/{reference.shape}, estimate={estimate_rate}/{estimate.shape}; not silently compensating")
    report = {
        "schema": 1,
        "reference": identity(args.reference),
        "estimate": identity(args.estimate),
        "sample_rate": rate,
        "frames": reference.shape[0],
        "channels": reference.shape[1],
        "metrics_by_channel": [channel_metrics(a, b) for a, b in zip(reference.T, estimate.T)],
        "reference_stereo": stereo_metrics(reference),
        "estimate_stereo": stereo_metrics(estimate),
        "provenance": args.provenance,
        "implementation_identity": args.implementation,
        "environment": {"python": sys.version, "numpy": np.__version__, "soundfile": sf.__version__, "platform": platform.platform()},
        "limits": {"alignment": "exact frame/rate/channel equality required, no automatic lag correction", "metrics": "SI-SDR is scale invariant; inspect gain and ordinary SNR too. Silent references give undefined scores. These metrics do not establish absence of perceptual artifacts.", "listening": "not performed by this program"},
    }
    if args.noisy:
        noisy, noisy_rate = load(args.noisy)
        if noisy_rate != rate or noisy.shape != reference.shape:
            raise ValueError("noisy baseline alignment mismatch")
        report["noisy_baseline"] = identity(args.noisy)
        report["baseline_metrics_by_channel"] = [channel_metrics(a, b) for a, b in zip(reference.T, noisy.T)]
    if args.complement:
        complement, complement_rate = load(args.complement)
        if complement_rate != rate or complement.shape != reference.shape:
            raise ValueError("complement alignment mismatch")
        if not args.mixture:
            raise ValueError("--complement requires --mixture")
        mixture, mixture_rate = load(args.mixture)
        if mixture_rate != rate or mixture.shape != reference.shape:
            raise ValueError("mixture alignment mismatch")
        residual = estimate + complement - mixture
        report["reconstruction"] = {
            "mixture": identity(args.mixture), "complement": identity(args.complement),
            "residual_rms": float(np.sqrt(np.mean(residual ** 2))),
            "residual_peak": float(np.max(np.abs(residual))),
            "warning": "A complement defined as mixture minus target guarantees reconstruction, not target quality.",
        }
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", required=True)
    parser.add_argument("--estimate", required=True)
    parser.add_argument("--noisy")
    parser.add_argument("--mixture")
    parser.add_argument("--complement")
    parser.add_argument("--provenance", required=True, help="corpus source/license and mixture recipe")
    parser.add_argument("--implementation", required=True, help="revision/build/model/config identity")
    args = parser.parse_args()
    print(json.dumps(evaluate(args), indent=2, allow_nan=False))


if __name__ == "__main__":
    main()
