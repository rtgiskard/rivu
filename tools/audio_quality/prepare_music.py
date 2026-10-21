#!/usr/bin/env python3
"""Decode official MUSDB STEMS excerpts to unclipped float evaluation references.

Stream order follows sigsep-mus-db mus.yaml: mixture, drums, bass, other, vocals.
AAC mixture and separately encoded stems need not reconstruct bit-exactly.
Research evaluation only; this script does not grant rights to redistribute data.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("inputs", nargs="+")
    parser.add_argument("--output", required=True)
    parser.add_argument("--provenance", required=True)
    args = parser.parse_args()
    if not 1 <= len(args.inputs) <= 32:
        raise ValueError("select between 1 and 32 bounded reference excerpts")
    root = Path(args.output).resolve()
    root.mkdir(parents=True, exist_ok=False)
    records = []
    for index, name in enumerate(args.inputs):
        source = Path(name).resolve(strict=True)
        if source.stat().st_size > 20_000_000:
            raise ValueError("select a small reference excerpt, not an unbounded track")
        target = root / f"track-{index:02d}"
        target.mkdir()
        command = ["ffmpeg", "-nostdin", "-v", "error", "-n", "-threads", "1", "-i", str(source),
                   "-filter_complex", "[0:a:1][0:a:2][0:a:3]amix=inputs=3:normalize=0[accompaniment]",
                   "-map", "0:a:0", "-c:a", "pcm_f32le", str(target / "mixture.wav"),
                   "-map", "0:a:4", "-c:a", "pcm_f32le", str(target / "vocals.wav"),
                   "-map", "[accompaniment]", "-c:a", "pcm_f32le", str(target / "accompaniment.wav")]
        subprocess.run(command, check=True)
        records.append({"source": str(source), "sha256": hashlib.sha256(source.read_bytes()).hexdigest(),
                        "directory": str(target), "command": command})
    report = {"schema": 1, "provenance": args.provenance, "records": records,
              "caveat": "Lossy encoded excerpts, small subset, not a full-corpus quality claim; inspect accompaniment as well as vocals."}
    (root / "manifest.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
