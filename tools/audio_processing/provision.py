#!/usr/bin/env python3
"""Provision an explicit standalone local worker/model bundle without any network.

Run with the already prepared interpreter, not system Python:
  /abs/.venv/bin/python tools/audio_processing/provision.py \
    --manifest /abs/moss/manifest.json --directory /abs/new-moss-bundle

The new bundle contains immutable model artifacts, worker sidecar sources/schema,
exact installed dependency identities and their supplied code/license notices.
Large existing model artifacts are hard-linked on the same filesystem, copied
otherwise. The old manifest/artifacts are never modified. This is local asset
provisioning, not app installation or permission to redistribute third-party
weights. Explicitly configure all three emitted absolute paths in Rivu. Preserve
interpreter's venv symlink path: resolving it to the base Python loses the venv.
"""
import argparse
import importlib.metadata
import json
import os
from pathlib import Path
import shutil
import sys
if __package__:
    from .runtime.worker import RUNTIME_FILES, check_packages, digest, read_json, validate_artifacts, validate_manifest
else:
    from runtime.worker import RUNTIME_FILES, check_packages, digest, read_json, validate_artifacts, validate_manifest


def populate(base, manifest):
    runtime=base/"runtime"
    runtime.mkdir()
    source=Path(__file__).resolve().parent / "runtime"
    for name in RUNTIME_FILES:
        shutil.copyfile(source/name,runtime/name)
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
            shutil.copyfile(origin,target)
            records.append(str(target.relative_to(base)))
        dependencies[name]={"version":distribution.version,"license_expression":distribution.metadata.get("License-Expression"),
                            "license_classifiers":[value for value in distribution.metadata.get_all("Classifier",[]) if value.startswith("License ::")],
                            "notices":records,"notice_status":"bundled distribution notices" if records else "no license file supplied by this distribution; metadata is not a substitute for upstream permission"}
    worker_notice=base/"resources/licenses/RIVU-WORKER-NOTICE.txt"
    worker_notice.parent.mkdir(parents=True,exist_ok=True)
    worker_notice.write_text("Rivu local audio-processing worker source snapshot. This local provisioning command makes no new or separate redistribution license grant; consult the Rivu project's own terms. Third-party code and pretrained weights retain their respective recorded licenses and notices; source-code permission alone does not establish pretrained-weight permission. Models are never implicitly downloaded by inference.\n")
    manifest["dependencies"]=dependencies
    manifest["runtime"]={"worker":str((runtime/"worker.py").resolve()),"interpreter":os.path.abspath(sys.executable),
                         "interpreter_sha256":digest(sys.executable),"model_manifest":str((base/"manifest.json").resolve()),
                         "worker_notice":str(worker_notice.relative_to(base)),"cwd_independent":True,
                         "provisioning":"explicit local bundle; installed application alone does not include Python dependencies, models or this worker"}
    manifest["files"]={str(path.relative_to(base)):digest(path) for path in sorted(base.rglob("*"))
                       if path.is_file() and path.name!="manifest.json" and "__pycache__" not in path.parts}
    (base/"manifest.json").write_text(json.dumps(manifest,indent=2)+"\n")
    validate_manifest(base/"manifest.json", manifest["operation"])
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


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest",type=Path,required=True)
    parser.add_argument("--directory",type=Path,required=True)
    parser.add_argument("--populate-new",action="store_true",help=argparse.SUPPRESS)
    args=parser.parse_args()
    original=args.manifest.resolve()
    raw=read_json(original)
    # Provisioning consumes verified construction artifacts, not inference admission.
    manifest = validate_artifacts(original, raw["operation"])
    check_packages(manifest)
    target=args.directory.resolve()
    if args.populate_new:
        if target!=original.parent or (target/"runtime").exists():
            parser.error("internal populate requires newly prepared directory without runtime")
        print(json.dumps(populate(target,manifest)))
        return
    if target.exists() or not target.parent.is_dir():
        parser.error("directory must not exist and parent must already exist")
    target.mkdir(mode=0o700)
    try:
        old=original.parent
        for name in manifest["files"]:
            if name.startswith(("runtime/","resources/licenses/")):
                continue
            origin=old/name
            destination=target/name
            destination.parent.mkdir(parents=True,exist_ok=True)
            if name=="registry.json":
                destination.write_text(json.dumps(relocate(read_json(origin),old,target),indent=2)+"\n")
            elif origin.stat().st_dev==target.stat().st_dev:
                os.link(origin,destination)
            else:
                shutil.copyfile(origin,destination)
        manifest.pop("runtime",None)
        manifest.pop("dependencies",None)
        print(json.dumps(populate(target,manifest)))
    except BaseException:
        shutil.rmtree(target)
        raise

if __name__=="__main__":
    main()
