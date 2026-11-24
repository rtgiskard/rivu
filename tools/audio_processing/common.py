"""Shared offline bundle integrity, dependency and error contracts."""
import hashlib
import importlib.metadata
import json
import re
from pathlib import Path

RUNTIME_FILES = ("worker.py", "mossformer.py", "request.schema.json", "event.schema.json")
MAX_JSON_BYTES = 1024**2
MAX_REPORT_BYTES = 8 * 1024**2

class WorkerError(Exception):
    def __init__(self, code, message):
        super().__init__(message)
        self.code = code

def require(condition, code, message):
    if not condition:
        raise WorkerError(code, message)

def digest_stream(stream):
    h = hashlib.sha256()
    while block := stream.read(256 * 1024):
        h.update(block)
    return h.hexdigest()

def digest(path):
    with Path(path).open("rb") as stream:
        return digest_stream(stream)

def canonical_digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()

def absolute_path(value, field):
    require(isinstance(value, str) and Path(value).is_absolute(), "invalid_request", field + " must be an absolute local path")
    return Path(value).resolve()

def _json_bytes(path):
    with path.open("rb") as stream:
        data = stream.read(MAX_JSON_BYTES + 1)
    require(len(data) <= MAX_JSON_BYTES, "invalid_request", "JSON exceeds 1 MiB")
    return data

def read_json(path):
    return json.loads(_json_bytes(path).decode("utf-8"))

def read_json_with_digest(path):
    data = _json_bytes(path)
    return json.loads(data.decode("utf-8")), hashlib.sha256(data).hexdigest()

def read_export_report(path, source_sha256, *, operation, request=None, mismatch):
    """Return the validated report and the bounded bytes from which it was parsed."""
    with path.open("rb") as stream:
        data = stream.read(MAX_REPORT_BYTES + 1)
    if len(data) > MAX_REPORT_BYTES:
        raise ValueError("worker result JSON exceeds 8 MiB")
    report = json.loads(data.decode("utf-8"))
    if not isinstance(report, dict) or report.get("source_sha256") != source_sha256:
        raise ValueError(mismatch)
    request_digest = report.get("export_request_sha256")
    if not isinstance(request_digest, str) or not re.fullmatch(r"[0-9a-f]{64}", request_digest):
        raise ValueError("missing or invalid export request SHA256; re-export the audio")
    if request is not None and request_digest != canonical_digest(request):
        raise ValueError("worker export request does not match the submitted request")
    stems = ("vocals", "accompaniment") if operation == "separate" else ("speech",)
    outputs = report.get("outputs")
    if not isinstance(outputs, dict) or set(outputs) != set(stems) or not all(
            isinstance(p, str) and Path(p).is_absolute() for p in outputs.values()):
        raise ValueError("invalid worker output paths or stems")
    if request is not None:
        directory = Path(request["output_dir"]).resolve()
        if outputs != {stem:str(directory / (stem + ".wav")) for stem in stems}:
            raise ValueError("worker output paths do not match the submitted export directory")
    hashes = report.get("output_sha256")
    if (not isinstance(hashes, dict) or set(hashes) != set(stems) or not all(
            isinstance(h, str) and re.fullmatch(r"[0-9a-f]{64}", h) for h in hashes.values())):
        raise ValueError("missing or invalid exported WAV SHA256; re-export the audio")
    return report, data

def _validate_artifact_metadata(manifest, operation):
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
    require(manifest.get("license_evidence") in manifest["files"], "model_license", "hashed factual weight-license/provenance record required; not a permission gate")


def _artifact_path(base, name):
    require(isinstance(name, str) and name and not Path(name).is_absolute() and ".." not in Path(name).parts,
            "model_manifest", "artifact names must be bundle-relative")
    candidate = (base / name).resolve()
    require(candidate.is_relative_to(base) and candidate.is_file(), "model_missing", "missing/nonlocal model artifact: " + name)
    return candidate


def validate_artifact_manifest(path, manifest, operation):
    _validate_artifact_metadata(manifest, operation)
    base = path.parent.resolve()
    for name, checksum in manifest["files"].items():
        candidate = _artifact_path(base, name)
        require(isinstance(checksum, str) and digest(candidate) == checksum, "model_hash", "SHA256 mismatch: " + name)
    return manifest


def validate_artifacts(path, operation):
    """Validate model construction inputs, never authorize inference."""
    return validate_artifact_manifest(path, read_json(path), operation)


def _validate_runtime_metadata(path, manifest):
    base = path.parent.resolve()
    runtime = manifest.get("runtime")
    require(isinstance(runtime, dict), "model_runtime", "provisioned runtime metadata required; run prepare.py")
    require(runtime.get("worker") == str(base / "runtime/worker.py") and runtime.get("model_manifest") == str(path.resolve())
            and runtime.get("cwd_independent") is True, "model_runtime", "runtime paths must identify this bundle")
    interpreter = runtime.get("interpreter")
    require(isinstance(interpreter, str) and Path(interpreter).is_absolute() and Path(interpreter).is_file(),
            "model_runtime", "provisioned interpreter missing")
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
    return interpreter


def _validate_manifest(path, manifest, operation):
    validate_artifact_manifest(path, manifest, operation)
    interpreter = _validate_runtime_metadata(path, manifest)
    require(manifest["runtime"].get("interpreter_sha256") == digest(interpreter), "model_runtime", "provisioned interpreter SHA256 mismatch")
    return manifest


def validate_manifest(path, operation):
    """Admit a provisioned bundle for the requested operation."""
    return _validate_manifest(path, read_json(path), operation)


def read_validated_manifest_with_digest(path):
    """Use the bundle's operation and identify the exact bytes validated."""
    manifest, checksum = read_json_with_digest(path)
    operation = manifest.get("operation") if isinstance(manifest, dict) else None
    require(operation in ("speech", "separate"), "model_manifest", "unsupported manifest operation")
    return _validate_manifest(path, manifest, operation), checksum


def hash_bundle_files(base, names):
    return {name: digest(_artifact_path(base, name)) for name in names}


def write_bundle_manifest(path, manifest):
    """Validate and write a manifest with its final artifact hashes."""
    manifest["files"] = dict(sorted(manifest["files"].items()))
    _validate_artifact_metadata(manifest, manifest["operation"])
    if "runtime" in manifest:
        interpreter = _validate_runtime_metadata(path, manifest)
        manifest["runtime"]["interpreter_sha256"] = digest(interpreter)
    data = (json.dumps(manifest, indent=2) + "\n").encode("utf-8")
    require(len(data) <= MAX_JSON_BYTES, "invalid_request", "JSON exceeds 1 MiB")
    path.write_bytes(data)

def check_packages(manifest):
    for name, expected in manifest["packages"].items():
        try:
            actual = importlib.metadata.version(name)
        except importlib.metadata.PackageNotFoundError:
            raise WorkerError("dependency_missing", f"install {name}=={expected} using prepare.py") from None
        require(actual.split("+")[0] == expected.split("+")[0], "dependency_version", f"{name}: expected {expected}, found {actual}")
