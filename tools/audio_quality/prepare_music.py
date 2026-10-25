#!/usr/bin/env python3
"""Decode official MUSDB STEMS excerpts to unclipped float evaluation references.

Stream order follows sigsep-mus-db mus.yaml: mixture, drums, bass, other, vocals.
AAC mixture and separately encoded stems need not reconstruct bit-exactly.
Research evaluation only; this script does not grant rights to redistribute data.
Use --diagnostic-source for each known reference-erratum source (for example the
documented Al James excerpt). Selection is resolved and bound to SHA256, so input
order and identical-content renames do not change diagnostic membership.
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
    parser.add_argument("--diagnostic-source", action="append", default=[],
                        help="input path with a known reference erratum; repeat for each diagnostic source")
    args = parser.parse_args()
    if not 1 <= len(args.inputs) <= 32:
        raise ValueError("select between 1 and 32 bounded reference excerpts")
    sources = [Path(name).resolve(strict=True) for name in args.inputs]
    diagnostic_sources = {Path(name).resolve(strict=True) for name in args.diagnostic_source}
    if not diagnostic_sources.issubset(set(sources)):
        raise ValueError("every --diagnostic-source must select an input source")
    hashes = {}
    for source in sources:
        if source.stat().st_size > 20_000_000:
            raise ValueError("select a small reference excerpt, not an unbounded track")
        hashes[source] = hashlib.sha256(source.read_bytes()).hexdigest()
    diagnostic_hashes = {hashes[source] for source in diagnostic_sources}
    root = Path(args.output).resolve()
    root.mkdir(parents=True, exist_ok=False)
    records = []
    for index, source in enumerate(sources):
        target = root / f"track-{index:02d}"
        target.mkdir()
        command = ["ffmpeg", "-nostdin", "-v", "error", "-n", "-threads", "1", "-i", str(source),
                   "-filter_complex", "[0:a:1][0:a:2][0:a:3]amix=inputs=3:normalize=0[accompaniment]",
                   "-map", "0:a:0", "-c:a", "pcm_f32le", str(target / "mixture.wav"),
                   "-map", "0:a:4", "-c:a", "pcm_f32le", str(target / "vocals.wav"),
                   "-map", "[accompaniment]", "-c:a", "pcm_f32le", str(target / "accompaniment.wav")]
        subprocess.run(command, check=True)
        records.append({"source": str(source), "sha256": hashes[source],
                        "diagnostic_only": hashes[source] in diagnostic_hashes,
                        "directory": target.name, "command": command})
    report = {"schema": 2, "provenance": args.provenance, "records": records,
              "caveat": "Lossy encoded excerpts, small subset, not a full-corpus quality claim; inspect accompaniment as well as vocals."}
    (root / "manifest.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
