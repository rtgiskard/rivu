#!/usr/bin/env python3
"""Explicit environment, model preparation and offline bundle provisioning.

  python3 prepare.py moss --download --directory /abs/bundle
  python3 prepare.py separate --weights /abs/model.safetensors --directory /abs/bundle --interpreter /abs/.venv/bin/python
  python3 prepare.py --manifest /abs/old/manifest.json --directory /abs/new

Python models install an isolated CPU environment with uv unless --interpreter
selects an existing environment. Native ONNX models never install Python packages.
Offline reprovisioning reuses the original bundle interpreter by default, keeping
its venv symlink identity. Only explicit preparation downloads; local --weights
still fetches factual publisher provenance and architecture snapshots. --hf-host
selects the pinned Kim mirror; --proxy selects curl without transport fallback.
New bundles are CWD-independent, hash every source/notice and record installed
dependency identities. Unknown weight-license terms are not redistribution grants.
Preparation never overwrites bundles. Failed model preparation removes its new
bundle but retains the dependency environment for reuse.
"""
import argparse
from functools import partial
import importlib.metadata
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

sys.dont_write_bytecode = True
if __package__:
    from . import models
    from .common import RUNTIME_FILES, check_packages, hash_bundle_files, read_json, require, validate_artifact_manifest, validate_artifacts, write_bundle_manifest
else:
    import models
    from common import RUNTIME_FILES, check_packages, hash_bundle_files, read_json, require, validate_artifact_manifest, validate_artifacts, write_bundle_manifest

ENVIRONMENTS = {
    "default": ("3.12", ["torch>=2.13", "torchaudio>=2.11"], [
        "unblend>=1.0", "numpy>=2", "scipy>=1.15", "soundfile>=0.13",
        "einops>=0.8", "rotary-embedding-torch>=0.8", "torchinfo>=1.8", "pystoi>=0.4"]),
    "df3": ("3.11", ["torch==2.1.2+cpu", "torchaudio==2.1.2+cpu"], [
        "deepfilternet==0.5.6", "deepfilterlib==0.5.6", "numpy==1.26.4",
        "scipy==1.11.4", "soundfile==0.12.1", "packaging==23.2",
        "appdirs==1.4.4", "loguru==0.7.3", "requests==2.32.5", "sympy==1.13.3"]),
}
def populate(base, manifest, *, reuse_verified=False):
    written = set()

    def copy_artifact(source, target):
        shutil.copyfile(source, target)
        name = str(target.relative_to(base))
        written.add(name)
        return name

    runtime=base/"runtime"
    runtime.mkdir()
    source=Path(__file__).resolve().parent / "runtime"
    for name in RUNTIME_FILES:
        copy_artifact(source / name, runtime / name)
    copy_artifact(Path(__file__).with_name("common.py"), base / "common.py")
    notices=base/"resources/licenses/dependencies"
    dependencies={}
    for distribution in sorted(importlib.metadata.distributions(),key=lambda item:item.metadata["Name"].lower()):
        name=distribution.metadata["Name"]
        records=[]
        for member in distribution.files or []:
            parts=Path(str(member)).parts
            info_index=next((index for index,part in enumerate(parts) if part.endswith((".dist-info",".egg-info"))),None)
            if info_index is None:
                continue
            relative=Path(*parts[info_index+1:])
            if ".." in relative.parts or not any(word in str(relative).lower() for word in ("license","copying","notice","copyright")):
                continue
            origin=Path(distribution.locate_file(member))
            if not origin.is_file():
                continue
            target=notices/name/relative
            target.parent.mkdir(parents=True,exist_ok=True)
            records.append(copy_artifact(origin, target))
        dependencies[name]={"version":distribution.version,"license_expression":distribution.metadata.get("License-Expression"),
                            "license_classifiers":[value for value in distribution.metadata.get_all("Classifier",[]) if value.startswith("License ::")],
                            "notices":records,"notice_status":"bundled distribution notices" if records else "no license file supplied by this distribution; metadata is not a substitute for upstream permission"}
    worker_notice=base/"resources/licenses/RIVU-WORKER-NOTICE.txt"
    worker_notice.parent.mkdir(parents=True,exist_ok=True)
    worker_notice.write_text("Rivu local audio-processing worker source snapshot. This local provisioning command makes no new or separate redistribution license grant; consult the Rivu project's own terms. Third-party code and pretrained weights retain their respective recorded licenses and notices; source-code permission alone does not establish pretrained-weight permission. Models are never implicitly downloaded by inference.\n")
    notice_name = str(worker_notice.relative_to(base))
    written.add(notice_name)
    manifest["dependencies"]=dependencies
    manifest["runtime"]={"worker":str((runtime/"worker.py").resolve()),"interpreter":os.path.abspath(sys.executable),
                         "model_manifest":str((base/"manifest.json").resolve()),
                         "worker_notice":notice_name,"cwd_independent":True,
                         "provisioning":"explicit local bundle; installed application alone does not include Python dependencies, models or this worker"}
    names = {str(path.relative_to(base)) for path in base.rglob("*")
             if path.is_file() and path.name != "manifest.json" and "__pycache__" not in path.parts}
    verified = manifest["files"] if reuse_verified else {}
    files = {name: verified[name] for name in names - written if name in verified}
    files.update(hash_bundle_files(base, names - files.keys()))
    manifest["files"] = files
    write_bundle_manifest(base / "manifest.json", manifest)
    return {"worker":manifest["runtime"]["worker"],"interpreter":manifest["runtime"]["interpreter"],
            "model_manifest":manifest["runtime"]["model_manifest"],"model":manifest["id"]}


def relocate(value, old, new):
    if isinstance(value,dict):
        return {key:relocate(item,old,new) for key,item in value.items()}
    if isinstance(value,list):
        return [relocate(item,old,new) for item in value]
    if isinstance(value,str) and value.startswith(str(old)+os.sep):
        return str(new/Path(value).relative_to(old))
    return value


def install_environment(model, directory, temporary):
    """Install isolated CPU dependencies; retain environments for reuse on failure."""
    directory.mkdir(parents=True, exist_ok=True)
    if shutil.disk_usage(directory).free < 10 * 1024**3:
        raise ValueError("reserve at least 10 GiB free before installing CPU environments")
    uv = shutil.which("uv")
    if not uv:
        raise ValueError("uv is required; install uv explicitly before preparation")
    temporary.mkdir(parents=True, exist_ok=True)
    env = {**os.environ, "TMPDIR": str(temporary)}
    operation = "df3" if model == "df3" else "default"
    version, torch_requirements, requirements = ENVIRONMENTS[operation]
    target = directory / ("df3-venv" if operation == "df3" else ".venv")
    if not target.exists():
        subprocess.run([uv, "venv", "--managed-python", "--python", version, str(target)],
                       env=env, stdout=sys.stderr, check=True)
    python = str(target / "bin/python")
    subprocess.run([uv, "pip", "install", "--python", python, "--index-url",
                    "https://download.pytorch.org/whl/cpu", *torch_requirements],
                   env=env, stdout=sys.stderr, check=True)
    subprocess.run([uv, "pip", "install", "--python", python, *requirements],
                   env=env, stdout=sys.stderr, check=True)
    subprocess.run([uv, "pip", "check", "--python", python], env=env, stdout=sys.stderr, check=True)
    frozen = subprocess.check_output([uv, "pip", "freeze", "--python", python], env=env, text=True)
    lock = directory / ("df3-installed.txt" if operation == "df3" else "installed.txt")
    lock.write_text(frozen)
    return Path(python), lock


def reprovision(original, target, manifest):
    """Copy verified local artifacts without downloading or changing the source."""
    old = original.parent
    target.mkdir(mode=0o700)
    try:
        target_device = target.stat().st_dev
        for name in manifest["files"]:
            if name == "common.py" or name.startswith(("runtime/", "resources/licenses/")):
                continue
            origin, destination = old / name, target / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            if name == "registry.json":
                destination.write_text(json.dumps(relocate(read_json(origin), old, target), indent=2) + "\n")
            elif origin.stat().st_dev == target_device:
                os.link(origin, destination)
            else:
                shutil.copyfile(origin, destination)
        manifest.pop("runtime", None)
        manifest.pop("dependencies", None)
        if manifest.get("backend") == "onnx":
            (target / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
            validate_artifacts(target / "manifest.json", manifest["operation"])
            return {"model_manifest": str(target / "manifest.json"), "backend": "onnx"}
        return populate(target, manifest)
    except BaseException:
        shutil.rmtree(target)
        raise


def provision(args, parser):
    original = args.manifest.resolve()
    raw = read_json(original)
    target = args.directory.resolve()
    native = raw.get("backend") == "onnx"
    if not native and not args.populate_new and not args.provision_in_process:
        interpreter = args.interpreter or Path(raw.get("runtime", {}).get("interpreter", sys.executable))
        if not interpreter.is_absolute() or not interpreter.is_file():
            parser.error("provisioning requires an existing absolute Python interpreter")
        if os.path.abspath(sys.executable) != str(interpreter):
            return json.loads(subprocess.check_output([
                str(interpreter), str(Path(__file__).resolve()), "--manifest", str(original),
                "--directory", str(target), "--provision-in-process"], text=True))
    manifest = validate_artifact_manifest(original, raw, raw["operation"])
    if not native:
        check_packages(manifest)
    if args.populate_new:
        if native or target != original.parent or (target / "runtime").exists():
            parser.error("internal populate requires newly prepared Python directory without runtime")
        # The parent owns this directory and rolls it back if population fails.
        return populate(target, manifest, reuse_verified=True)
    if target.exists() or not target.parent.is_dir():
        parser.error("directory must not exist and parent must already exist")
    return reprovision(original, target, manifest)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("model", nargs="?", choices=("moss", "separate", "df3", "ul_unas", "gtcrn"))
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--manifest", type=Path, help="offline reprovisioning of verified local model artifacts")
    parser.add_argument("--interpreter", type=Path, help="existing absolute Python executable; preserve its venv symlink")
    parser.add_argument("--environment-directory", type=Path, help="isolated dependency root; default: BUNDLE.environment")
    parser.add_argument("--temporary-directory", type=Path, help="dependency extraction/build cache; default: BUNDLE_PARENT/.cache/neural")
    parser.add_argument("--download", action="store_true")
    parser.add_argument("--weights", type=Path)
    parser.add_argument("--hf-host", choices=("huggingface.co", "hf-mirror.com"), default="huggingface.co")
    parser.add_argument("--license-evidence", type=Path, help="DF3 only: retained supplied weight-license evidence")
    parser.add_argument("--proxy", help="explicit preparation-only curl proxy; no direct transport fallback")
    parser.add_argument("--populate-new", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--provision-in-process", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.manifest:
        if args.model or args.download or args.weights or args.environment_directory:
            parser.error("--manifest is offline reprovisioning, not model installation/preparation")
        result = provision(args, parser)
        print(json.dumps({"environment_directory": None, "installed_lock": None, **result}))
        return
    if not args.model or args.populate_new or args.provision_in_process:
        parser.error("choose a model or --manifest")
    if args.download == bool(args.weights):
        parser.error("choose exactly one of --download or --weights")
    target = args.directory.resolve()
    if target.exists() or not target.parent.is_dir():
        parser.error("directory must not exist and parent must already exist")
    if shutil.disk_usage(target.parent).free < 2 * 1024**3:
        parser.error("reserve 2 GiB free before preparing models")
    native = args.model in ("ul_unas", "gtcrn")
    if not native and args.interpreter and (not args.interpreter.is_absolute() or not args.interpreter.is_file()):
        parser.error("--interpreter must name an existing absolute Python executable")
    environment = lock = None
    target.mkdir(mode=0o700)
    try:
        if not native and args.interpreter is None:
            environment = Path(os.path.abspath(args.environment_directory or target.with_name(target.name + ".environment")))
            if environment == target or environment.is_relative_to(target):
                raise ValueError("environment directory must be outside the immutable model bundle")
            temporary = args.temporary_directory or target.parent / ".cache/neural"
            args.interpreter, lock = install_environment(args.model, environment, Path(os.path.abspath(temporary)))
        fetch = partial(models.download, proxy=args.proxy)
        manifest, pinned_hashes = models.prepare_model(args, target, fetch)
        if args.model in ("moss", "separate"):
            manifest["packages"] = json.loads(subprocess.check_output([
                str(args.interpreter), "-c",
                "import importlib.metadata as m, json, sys; print(json.dumps({name:m.version(name) for name in sys.argv[1:]}))",
                *manifest["packages"],
            ], text=True))
        manifest["license_evidence"] = "weight-license.txt"
        names = (str(path.relative_to(target)) for path in target.rglob("*") if path.is_file())
        files = hash_bundle_files(target, names)
        for name, expected in pinned_hashes.items():
            require(files.get(name) == expected, "model_hash",
                    "Bundle file differs from pinned publisher artifact: " + name)
        manifest["files"] = files
        write_bundle_manifest(target / "manifest.json", manifest)
        if native:
            result = {"model_manifest": str(target / "manifest.json"), "backend": "onnx"}
        else:
            result = json.loads(subprocess.check_output([
                str(args.interpreter), str(Path(__file__).resolve()), "--manifest", str(target / "manifest.json"),
                "--directory", str(target), "--populate-new"], text=True))
        print(json.dumps({**result, "environment_directory": str(environment) if environment else None,
                          "installed_lock": str(lock) if lock else None}))
    except BaseException:
        if target.exists():
            shutil.rmtree(target)
        raise


if __name__ == "__main__":
    main()
