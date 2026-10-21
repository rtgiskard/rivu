#!/usr/bin/env python3
"""Generate reproducible known-SNR speech mixtures from an attributed clean file.

Synthetic noise establishes controlled signal tests; it does not replace real
noisy recordings or listening. Inputs/outputs are never clipped or normalized
independently. Output directory must not exist.
"""
import argparse
import hashlib
import json
from pathlib import Path

import numpy as np
import soundfile as sf


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--provenance", required=True)
    parser.add_argument("--seed", type=int, default=271828)
    args = parser.parse_args()
    source = Path(args.source).resolve(strict=True)
    info = sf.info(source)
    if info.channels != 1 or not 0 < info.frames <= 30 * info.samplerate:
        raise ValueError("select a mono clean utterance of at most 30 seconds")
    clean, rate = sf.read(source, dtype="float64")
    if not np.isfinite(clean).all():
        raise ValueError("nonfinite reference")
    rms = float(np.sqrt(np.mean(clean ** 2)))
    if rms < 1e-6:
        raise ValueError("reference is silent")
    # One shared reference scaling leaves safe mixing room without hiding overs.
    clean *= 0.08 / rms
    output = Path(args.output).resolve()
    output.mkdir(parents=True, exist_ok=False)
    sf.write(output / "clean.wav", clean, rate, subtype="FLOAT")
    rng = np.random.default_rng(args.seed)
    time = np.arange(clean.size, dtype=np.float64) / rate
    white = rng.standard_normal(clean.size)
    fan = 0.6 * np.sin(2 * np.pi * 120 * time) + 0.3 * np.sin(2 * np.pi * 240 * time) + 0.1 * white
    nonstationary = white * (0.1 + 0.9 * (np.sin(2 * np.pi * 0.7 * time) > 0))
    impulses = np.zeros_like(clean)
    for start in range(rate // 3, clean.size, max(1, rate // 2)):
        length = min(rate // 100, clean.size - start)
        impulses[start:start + length] += rng.standard_normal(length) * np.exp(-np.arange(length) / max(1, rate / 1000))
    noises = {"white": white, "fan": fan, "varying": nonstationary, "impulsive": impulses + 0.03 * white}
    cases = []
    clean_power = float(np.mean(clean ** 2))
    for name, noise in noises.items():
        noise_power = float(np.mean(noise ** 2))
        for snr in [-5, 0, 5, 15]:
            scaled = noise * np.sqrt(clean_power / (10 ** (snr / 10) * noise_power))
            path = output / f"{name}_{snr:+d}dB.wav"
            sf.write(path, clean + scaled, rate, subtype="FLOAT")
            cases.append({"name": name, "snr_db": snr, "path": str(path), "peak": float(np.max(np.abs(clean + scaled)))})
    manifest = {
        "schema": 1, "source": str(source), "source_sha256": hashlib.sha256(source.read_bytes()).hexdigest(),
        "provenance": args.provenance, "seed": args.seed, "rate": rate, "frames": clean.size,
        "reference": str(output / "clean.wav"), "shared_reference_rms": 0.08,
        "noise_kind": "deterministic synthetic; no claim of real-world noise coverage", "cases": cases,
    }
    (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(json.dumps(manifest, indent=2))


if __name__ == "__main__":
    main()
