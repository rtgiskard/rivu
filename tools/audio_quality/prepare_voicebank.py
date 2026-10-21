#!/usr/bin/env python3
"""Prepare fixed native-rate VoiceBank-DEMAND test pairs; never normalize.

Cassia Valentini-Botinhao (2017), University of Edinburgh, DOI10.7488/ds/2117,
CC BY4.0. Inputs are the publisher's clean_testset_wav.zip and
noisy_testset_wav.zip. This fixed 20-utterance subset is not a full-corpus score.
"""
import argparse
import hashlib
import io
import json
from pathlib import Path
import zipfile

import numpy as np
import soundfile as sf
from evaluate import identity

UTTERANCES = [
    *[f"p232_{n:03}.wav" for n in (1, 2, 3, 5, 6, 7, 9, 10, 11, 12)],
    *[f"p257_{n:03}.wav" for n in (1, 2, 3, 4, 6, 7, 8, 9, 10, 11)],
]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--clean-zip", required=True)
    parser.add_argument("--noisy-zip", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    root = Path(args.output).resolve()
    root.mkdir(parents=True, exist_ok=False)
    (root / "clean").mkdir()
    (root / "noisy").mkdir()
    records = []
    with zipfile.ZipFile(args.clean_zip) as clean_zip, zipfile.ZipFile(args.noisy_zip) as noisy_zip:
        for name in UTTERANCES:
            pair = {}
            for kind, archive in (("clean", clean_zip), ("noisy", noisy_zip)):
                member = f"{kind}_testset_wav/{name}"
                info = archive.getinfo(member)
                if info.file_size > 4_000_000:
                    raise ValueError(f"oversized input member: {member}")
                original = archive.read(info)
                audio, rate = sf.read(io.BytesIO(original), dtype="float64", always_2d=True)
                if rate != 48_000 or audio.shape[1] != 1 or not np.isfinite(audio).all():
                    raise ValueError(f"unexpected source format: {member}")
                destination = root / kind / name
                sf.write(destination, audio, rate, subtype="FLOAT")
                pair[kind] = {"member": member, "member_sha256": hashlib.sha256(original).hexdigest(), "float": identity(destination), "frames": len(audio), "sample_rate": rate}
            if pair["clean"]["frames"] != pair["noisy"]["frames"]:
                raise ValueError(f"unaligned pair: {name}")
            records.append({"utterance": name, **pair})
    report = {"schema": 1, "provenance": "Cassia Valentini-Botinhao (2017), University of Edinburgh DOI10.7488/ds/2117, CC BY4.0", "clean_archive": identity(args.clean_zip), "noisy_archive": identity(args.noisy_zip), "processing": "native48kHz mono, PCM converted to float32; no level changes, clipping, denoising or resampling", "selection": "fixed first10 available filenames from each held-out speaker; chosen before inference", "records": records}
    (root / "manifest.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"manifest": str(root / "manifest.json"), "pairs": len(records)}, indent=2))


if __name__ == "__main__":
    main()
