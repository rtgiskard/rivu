"""Synthetic model bundle with real runtime sidecars for admission and transport tests."""
import json
import os
from pathlib import Path
import sys

from common import RUNTIME_FILES, canonical_digest, digest


def bundle_fixture(base, operation="speech", model="MossFormer2_SE_48K", provenance=None):
    runtime = base / "runtime"
    runtime.mkdir(exist_ok=True)
    for name in RUNTIME_FILES:
        (runtime / name).write_bytes((Path(__file__).parent / "runtime" / name).read_bytes())
    shared = base / "common.py"
    shared.write_bytes((Path(__file__).parent / "common.py").read_bytes())
    evidence = base / "weight-license.txt"
    evidence.write_text("Factual test provenance; no permission grant.")
    manifest = {"version":1, "operation":operation, "id":model, "packages":{"fixture":"1"},
                "license_evidence":"weight-license.txt", "provenance":provenance or {},
                "runtime":{"worker":str(runtime / "worker.py"), "interpreter":os.path.abspath(sys.executable),
                           "interpreter_sha256":digest(sys.executable), "model_manifest":str(base / "manifest.json"),
                           "worker_notice":"weight-license.txt", "cwd_independent":True},
                "dependencies":{"fixture":{"version":"1", "notices":["weight-license.txt"],
                    "license_expression":None, "license_classifiers":[], "notice_status":"test evidence"}},
                "files":{str(path.relative_to(base)):digest(path) for path in [evidence, shared, *(runtime / name for name in RUNTIME_FILES)]}}
    (base / "manifest.json").write_text(json.dumps(manifest))
    return manifest


if __name__ == "__main__":
    # Rust drives the real dry worker using this same admitted bundle and interpreter.
    manifest = bundle_fixture(Path(sys.argv[1]))
    print(canonical_digest(manifest))
    print(manifest["runtime"]["interpreter"])
