#!/usr/bin/env python3
"""Paired audio quality evidence, not a substitute for a listening review.

Requires numpy and soundfile in the explicitly selected evaluation environment.
No automatic alignment, level matching, resampling, clipping, or source permutation.
"""
import argparse
import hashlib
from dataclasses import dataclass
import json
from pathlib import Path
import platform
import sys

import numpy as np
import soundfile as sf

if __package__:
    from .metric_core import db_ratio, PreparedProjection, reconstruction_metrics
else:
    from metric_core import db_ratio, PreparedProjection, reconstruction_metrics

MAX_FRAMES = 12_000_000
MAX_CHANNELS = 8


def identity(path):
    path = Path(path).resolve(strict=True)
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while block := stream.read(1024 * 1024):
            digest.update(block)
    return {"path": str(path), "sha256": digest.hexdigest(), "bytes": path.stat().st_size}


def load(path, dtype="float64", *, max_frames=MAX_FRAMES):
    info = sf.info(path)
    if info.frames <= 0 or (max_frames is not None and info.frames > max_frames) or not 0 < info.channels <= MAX_CHANNELS:
        raise ValueError(f"evaluation bound exceeded: {info.frames} frames/{info.channels} channels")
    data, rate = sf.read(path, dtype=dtype, always_2d=True)
    if data.shape != (info.frames, info.channels) or rate != info.samplerate:
        raise ValueError(f"audio changed while loading: {path}")
    if not np.isfinite(data).all():
        raise ValueError(f"non-finite audio: {path}")
    return data, rate

@dataclass(frozen=True)
class IdentifiedAudio:
    data: np.ndarray
    rate: int
    identity: dict


def load_identified(path, dtype="float64", *, max_frames=MAX_FRAMES):
    before = identity(path)
    data, rate = load(path, dtype=dtype, max_frames=max_frames)
    if identity(path) != before:
        raise ValueError(f"audio changed while loading: {path}")
    data.flags.writeable = False
    return IdentifiedAudio(data, rate, before)


def require_alignment(reference, other, label):
    if reference.rate != other.rate or reference.data.shape != other.data.shape:
        raise ValueError(f"{label} alignment contract violated: reference={reference.rate}/{reference.data.shape}, other={other.rate}/{other.data.shape}; not silently compensating")


class PreparedChannel:
    """Bounded reference statistics shared by all estimates for one channel."""

    def __init__(self, reference):
        self.projection = PreparedProjection(reference)
        self.energy = 0.0
        for start in range(0, len(reference), 65536):
            ref = reference[start:start+65536].astype("float64", copy=False)
            self.energy += float(np.dot(ref, ref))
        self.dc = self.projection.mean


def channel_metrics(reference, estimate):
    si_sdr, alpha = reference.projection.score(estimate)
    reference_energy = reference.energy
    reference_dc = reference.dc
    reference = reference.projection.reference
    estimate_energy = error_energy = 0.0
    peak_abs = peak_error = 0.0
    overs_count = 0
    for start in range(0, len(reference), 65536):
        ref = reference[start:start+65536].astype("float64", copy=False)
        est = estimate[start:start+65536].astype("float64", copy=False)
        error = est-ref
        estimate_energy += float(np.dot(est, est))
        error_energy += float(np.dot(error, error))
        peak_abs = max(peak_abs, float(np.max(np.abs(est))))
        peak_error = max(peak_error, float(np.max(np.abs(error))))
        overs_count += int(np.count_nonzero(np.abs(est) > 1))
    return {
        "reference_rms": float(np.sqrt(reference_energy/len(reference))),
        "estimate_rms": float(np.sqrt(estimate_energy/len(estimate))),
        "reference_dc": reference_dc,
        "estimate_dc": float(estimate.mean()),
        "peak_abs": peak_abs,
        "peak_error_abs": peak_error,
        "snr_db": db_ratio(reference_energy, error_energy),
        "si_sdr_db": si_sdr,
        "least_squares_reference_gain": alpha,
        "overs_count": overs_count,
    }


def stereo_metrics(data):
    if data.shape[1] != 2:
        return None
    left_energy = right_energy = product = mid_energy = side_energy = 0.0
    for start in range(0, len(data), 65536):
        left, right = data[start:start+65536].T
        mid, side = (left+right)/2, (left-right)/2
        left_energy += float(np.dot(left, left))
        right_energy += float(np.dot(right, right))
        product += float(np.dot(left, right))
        mid_energy += float(np.dot(mid, mid))
        side_energy += float(np.dot(side, side))
    denominator = np.sqrt(left_energy)*np.sqrt(right_energy)
    return {
        "channel_correlation": float(product/denominator) if denominator else None,
        "mid_rms": float(np.sqrt(mid_energy/len(data))),
        "side_rms": float(np.sqrt(side_energy/len(data))),
    }


@dataclass(frozen=True)
class PreparedBaseline:
    audio: IdentifiedAudio
    metrics_by_channel: list
    reference: IdentifiedAudio


class ReferenceEvaluation:
    """One caller-owned reference; baseline lifetime is one independent case."""

    def __init__(self, path):
        self.audio = load_identified(path)
        self.channels = [PreparedChannel(channel) for channel in self.audio.data.T]
        self.stereo = stereo_metrics(self.audio.data)

    def baseline(self, path):
        audio = self.audio if Path(path).resolve(strict=True) == Path(self.audio.identity["path"]) else load_identified(path)
        require_alignment(self.audio, audio, "noisy baseline")
        return PreparedBaseline(audio, [channel_metrics(a, b) for a, b in zip(self.channels, audio.data.T)], self.audio)

    def score(self, estimate_path, provenance, implementation, *, baseline=None, complement=None, mixture=None):
        estimate = load_identified(estimate_path)
        require_alignment(self.audio, estimate, "estimate")
        reference = self.audio.data
        report = {
            "schema": 1,
            "reference": self.audio.identity,
            "estimate": estimate.identity,
            "sample_rate": self.audio.rate,
            "frames": reference.shape[0],
            "channels": reference.shape[1],
            "metrics_by_channel": [channel_metrics(a, b) for a, b in zip(self.channels, estimate.data.T)],
            "reference_stereo": self.stereo,
            "estimate_stereo": stereo_metrics(estimate.data),
            "provenance": provenance,
            "implementation_identity": implementation,
            "environment": {"python": sys.version, "numpy": np.__version__, "soundfile": sf.__version__, "platform": platform.platform()},
            "limits": {"alignment": "exact frame/rate/channel equality required, no automatic lag correction", "metrics": "SI-SDR is scale invariant; inspect gain and ordinary SNR too. Silent references give undefined scores. These metrics do not establish absence of perceptual artifacts.", "listening": "not performed by this program"},
        }
        if baseline is not None:
            if baseline.reference is not self.audio:
                raise ValueError("noisy baseline belongs to another reference")
            require_alignment(self.audio, baseline.audio, "noisy baseline")
            report["noisy_baseline"] = baseline.audio.identity
            report["baseline_metrics_by_channel"] = baseline.metrics_by_channel
        if complement:
            if not mixture:
                raise ValueError("--complement requires --mixture")
            complement_audio = load_identified(complement)
            mixture_audio = load_identified(mixture)
            require_alignment(self.audio, complement_audio, "complement")
            require_alignment(self.audio, mixture_audio, "mixture")
            residual_rms, residual_peak = reconstruction_metrics(mixture_audio.data, estimate.data, complement_audio.data)
            report["reconstruction"] = {
                "mixture": mixture_audio.identity, "complement": complement_audio.identity,
                "residual_rms": residual_rms,
                "residual_peak": residual_peak,
                "warning": "A complement defined as mixture minus target guarantees reconstruction, not target quality.",
            }
        return report


def evaluate(args):
    reference = ReferenceEvaluation(args.reference)
    baseline = reference.baseline(args.noisy) if args.noisy else None
    return reference.score(args.estimate, args.provenance, args.implementation,
                           baseline=baseline, complement=args.complement, mixture=args.mixture)


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
