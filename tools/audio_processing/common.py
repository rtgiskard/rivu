"""Shared offline bundle integrity, dependency and error contracts."""
import hashlib
import importlib.metadata
import json
from pathlib import Path

RUNTIME_FILES = ("worker.py", "mossformer.py", "request.schema.json", "event.schema.json")

class WorkerError(Exception):
    def __init__(self, code, message):
        super().__init__(message)
        self.code = code

def require(condition, code, message):
    if not condition:
        raise WorkerError(code, message)

def digest(path):
    h = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()

def canonical_digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()

def absolute_path(value, field):
    require(isinstance(value, str) and Path(value).is_absolute(), "invalid_request", field + " must be an absolute local path")
    return Path(value).resolve()

def read_json(path):
    require(path.stat().st_size <= 1024 * 1024, "invalid_request", "JSON exceeds 1 MiB")
    return json.loads(path.read_text())

def validate_artifacts(path, operation):
    """Validate model construction inputs, never authorize inference."""
    manifest = read_json(path)
    require(isinstance(manifest, dict) and manifest.get("version") == 1 and manifest.get("operation") == operation,
            "model_manifest", "manifest version/operation mismatch")
    native = manifest.get("backend") == "onnx"
    allowed_models = (("GTCRN", "UL_UNAS") if native and operation == "speech" else
                      ("melband_roformer_kim",) if operation == "separate" else ("DeepFilterNet3", "MossFormer2_SE_48K"))
    require(manifest.get("id") in allowed_models, "model_manifest", "unsupported model id for operation")
    packages = manifest.get("packages")
    require((native and packages is None) or (isinstance(packages, dict) and packages and all(
            isinstance(name, str) and name and isinstance(version, str) and version for name, version in packages.items())),
            "model_manifest", "exact model dependency pins required")
    require(isinstance(manifest.get("files"), dict) and manifest["files"], "model_manifest", "manifest has no hashed files")
    base = path.parent.resolve()
    for name, checksum in manifest["files"].items():
        require(isinstance(name, str) and name and not Path(name).is_absolute() and ".." not in Path(name).parts,
                "model_manifest", "artifact names must be bundle-relative")
        candidate = (base / name).resolve()
        require(candidate.is_relative_to(base) and candidate.is_file(), "model_missing", "missing/nonlocal model artifact: " + name)
        require(isinstance(checksum, str) and digest(candidate) == checksum, "model_hash", "SHA256 mismatch: " + name)
    require(manifest.get("license_evidence") in manifest["files"], "model_license", "hashed factual weight-license/provenance record required; not a permission gate")
    return manifest

def validate_manifest(path, operation):
    """Admit only a complete current provisioned bundle to inference."""
    manifest = validate_artifacts(path, operation)
    base = path.parent.resolve()
    runtime = manifest.get("runtime")
    require(isinstance(runtime, dict), "model_runtime", "provisioned runtime metadata required; run prepare.py")
    require(runtime.get("worker") == str(base / "runtime/worker.py") and runtime.get("model_manifest") == str(path.resolve())
            and runtime.get("cwd_independent") is True, "model_runtime", "runtime paths must identify this bundle")
    interpreter = runtime.get("interpreter")
    require(isinstance(interpreter, str) and Path(interpreter).is_absolute() and Path(interpreter).is_file(),
            "model_runtime", "provisioned interpreter missing")
    require(runtime.get("interpreter_sha256") == digest(interpreter), "model_runtime", "provisioned interpreter SHA256 mismatch")
    require("common.py" in manifest["files"], "model_runtime", "hashed shared bundle module required: common.py")
    for name in RUNTIME_FILES:
        require("runtime/" + name in manifest["files"], "model_runtime", "hashed runtime sidecar required: " + name)
    require(runtime.get("worker_notice") in manifest["files"], "model_runtime", "hashed worker notice required")
    dependencies = manifest.get("dependencies")
    require(isinstance(dependencies, dict) and dependencies, "model_dependencies", "provisioned dependency metadata required")
    normalized = {}
    for name, record in dependencies.items():
        require(isinstance(name, str) and name and isinstance(record, dict) and isinstance(record.get("version"), str)
                and record["version"] and isinstance(record.get("notices"), list)
                and isinstance(record.get("notice_status"), str) and record["notice_status"]
                and isinstance(record.get("license_classifiers"), list) and "license_expression" in record,
                "model_dependencies", "incomplete dependency metadata")
        require(all(isinstance(notice, str) and notice in manifest["files"] for notice in record["notices"]),
                "model_dependencies", "dependency notices must be hashed bundle artifacts")
        normalized[name.lower().replace("_", "-").replace(".", "-")] = record["version"]
    for name, version in manifest["packages"].items():
        recorded = normalized.get(name.lower().replace("_", "-").replace(".", "-"))
        require(recorded is not None and recorded.split("+")[0] == version.split("+")[0],
                "model_dependencies", "dependency metadata does not match model pin: " + name)
    return manifest

def check_packages(manifest):
    for name, expected in manifest["packages"].items():
        try:
            actual = importlib.metadata.version(name)
        except importlib.metadata.PackageNotFoundError:
            raise WorkerError("dependency_missing", f"install {name}=={expected} using prepare.py") from None
        require(actual.split("+")[0] == expected.split("+")[0], "dependency_version", f"{name}: expected {expected}, found {actual}")
