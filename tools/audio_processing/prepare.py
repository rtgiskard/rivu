#!/usr/bin/env python3
"""Explicit model preparation; only this separate command downloads artifacts.

Run from repository root after installing the desired interpreter:
  python tools/audio_processing/prepare.py moss --download --directory .cache/neural/moss \
    --interpreter "$PWD/.cache/neural/moss-venv/bin/python"
  python tools/audio_processing/prepare.py separate --download --directory .cache/neural/kim \
    --interpreter "$PWD/.cache/neural/separate-venv/bin/python"
  python tools/audio_processing/prepare.py df3 --download --directory .cache/neural/df3 \
    --interpreter "$PWD/.cache/neural/df3-venv/bin/python"

MossFormer2 SE48K checkpoint221552019bytes is pinned to the primary ModelScope
publisher revision and SHA256; actual model card grants Apache2.0. Kim's
912885624byte safetensors conversion is pinned to unblend's immutable revision
and SHA256; its MIT declaration is recorded from the original trainer's pinned
model card, not inferred from the converter's code license. Primary snapshots,
code revisions, supplied notices and all artifact hashes are retained.
--hf-host hf-mirror.com explicitly selects a mirror for pinned Kim artifacts;
the expected checkpoint hash and original trainer card revision do not change.

--weights supplies a local checkpoint instead of downloading it; preparation
still fetches pinned factual provenance snapshots and architecture metadata.
Directories must not already exist; failed preparation removes only its new
directory. No operation touches media sources. Worker.py performs no download.

Preparation emits absolute worker/interpreter/model_manifest paths. Explicitly
configure all three in the app: installing the Rivu binary alone does not ship
Python environments, models or this worker. The prepared runtime/ contains all
worker sidecar sources/schema and works independently of app CWD/repository.
Dependency versions and supplied license notices are recorded under resources/.
Keep the interpreter venv symlink path; resolving it to base Python loses its
site-packages. An existing verified model can be bundled without network using
its prepared interpreter and provision.py --manifest OLD --directory NEW.

DF3 weight-license grants are not established here: licensing is recorded
factually, not used as a gate on explicit user candidate preparation.
Unknown terms are never represented as redistribution permission. --proxy may
explicitly select a local HTTP/SOCKS proxy via curl; inference stays offline.
"""
import argparse
from functools import partial
import json
from pathlib import Path
import shutil
import subprocess
import urllib.request
from runtime.worker import digest

KIM_SHA = "03bfc70b43df1f3e92ed1e582e8b18983f8d4adcd1a7d2133ada3b2f371b3095"
KIM_URL = "https://huggingface.co/Ryan5453/unblend/resolve/9ba75778acf1b64fc40a9ea05137a2c6af9cd1d5/melband_roformer_kim.safetensors"
KIM_CARD = "https://huggingface.co/KimberleyJSN/melbandroformer/raw/ac9b0614ab3cd7f77219e18ba494dfd93956c348/README.md"
KIM_CONFIG = {"dim":384,"depth":6,"stereo":True,"num_stems":1,"time_transformer_depth":1,
              "freq_transformer_depth":1,"num_bands":60,"dim_head":64,"heads":8,"sample_rate":44100,
              "stft_n_fft":2048,"stft_hop_length":441,"stft_win_length":2048,"mask_estimator_depth":2,
              "mlp_expansion_factor":4}

def download(url, target, maximum, proxy=None):
    size = 0
    if proxy:
        if not shutil.which("curl"):
            raise ValueError("explicit --proxy transport requires curl")
        # stdout is streamed and bounded; do not echo proxy URLs or credentials.
        with subprocess.Popen(["curl", "--fail", "--silent", "--location", "--max-time", "600",
                               "--proxy", proxy, url], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL) as process:
            try:
                with target.open("xb") as stream:
                    while data := process.stdout.read(1024 * 1024):
                        size += len(data)
                        if size > maximum:
                            raise ValueError("download exceeds declared artifact bound")
                        stream.write(data)
                if process.wait():
                    raise ValueError("explicit proxy download failed; no direct transport fallback")
            except BaseException:
                process.kill()
                process.wait()
                raise
    else:
        request = urllib.request.Request(url, headers={"User-Agent":"Rivu explicit model preparation"})
        with urllib.request.urlopen(request, timeout=60) as response, target.open("xb") as stream:
            while data := response.read(1024 * 1024):
                size += len(data)
                if size > maximum:
                    raise ValueError("download exceeds declared artifact bound")
                stream.write(data)
    return target

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("model", choices=("separate", "df3", "moss"))
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--interpreter", type=Path, required=True, help="absolute prepared venv Python path; preserve its symlink, do not resolve to base Python")
    parser.add_argument("--weights", type=Path)
    parser.add_argument("--download", action="store_true")
    parser.add_argument("--hf-host", choices=("huggingface.co", "hf-mirror.com"), default="huggingface.co")
    parser.add_argument("--license-evidence", type=Path, help="DF3 only: optional supplied weight-license evidence; no permission is inferred")
    parser.add_argument("--proxy", help="explicit preparation-only curl proxy (e.g. socks5h://[::1]:22224); never used by inference")
    args = parser.parse_args()
    fetch = partial(download, proxy=args.proxy)
    if not args.interpreter.is_absolute() or not args.interpreter.is_file():
        parser.error("--interpreter must name an existing absolute prepared Python executable")
    target = args.directory.resolve()
    if target.exists():
        parser.error("directory must not exist (preparation never overwrites)")
    if shutil.disk_usage(target.parent).free < 2 * 1024**3:
        parser.error("reserve 2 GiB free before preparing models")
    if not args.download and not args.weights:
        parser.error("supply --weights or explicitly opt into --download")
    if args.download and args.weights:
        parser.error("choose exactly one of --download or --weights")
    target.mkdir(mode=0o700)
    try:
        if args.model == "separate":
            checkpoint = target / "model.safetensors"
            if args.download:
                fetch(KIM_URL.replace("huggingface.co", args.hf_host) + "?download=true", checkpoint, 912885624)
            else:
                shutil.copyfile(args.weights, checkpoint)
            if checkpoint.stat().st_size != 912885624 or digest(checkpoint) != KIM_SHA:
                raise ValueError("Kim safetensors differs from pinned unblend 1.0.0 checkpoint")
            # Always retain the real pinned primary card, including for supplied
            # weights. Never synthesize a license declaration as evidence.
            fetch(KIM_CARD.replace("huggingface.co", args.hf_host), target / "weight-license.txt", 65536)
            registry = {"version":1,"models":{"melband_roformer_kim":{
                "architecture":"mel_band_roformer", "license":"MIT", "sources":["vocals","other"],
                "samplerate":44100,"segment_samples":352800,"config":KIM_CONFIG,
                "checkpoint":{"format":"safetensors","path":str(checkpoint),"sha256":KIM_SHA,"size_bytes":912885624}}}}
            (target / "registry.json").write_text(json.dumps(registry, indent=2)+"\n")
            manifest = {"version":1,"operation":"separate","id":"melband_roformer_kim",
                        "packages":{"unblend":"1.0.0","torch":"2.13.0","numpy":"2.2.6","scipy":"1.15.3","soundfile":"0.13.1"},
                        "provenance":{"weights":KIM_URL,"weight_license":KIM_CARD,"transport_host":args.hf_host,
                                      "code":"https://github.com/Ryan5453/unblend/tree/64fd0216b9f0309c9d055457002d6910dc245950",
                                      "code_license":"MIT", "weights_license":"MIT"}}
        elif args.model == "df3":
            from df3 import prepare
            manifest = prepare(args, target, fetch)
        elif args.model == "moss":
            from runtime.mossformer import prepare
            manifest = prepare(args, target, fetch)
        manifest["license_evidence"] = "weight-license.txt"
        manifest["files"] = {str(path.relative_to(target)):digest(path) for path in sorted(target.rglob("*")) if path.is_file()}
        (target / "manifest.json").write_text(json.dumps(manifest, indent=2)+"\n")
        provisioned = subprocess.run([str(args.interpreter), str(Path(__file__).with_name("provision.py").resolve()),
                                     "--manifest", str(target / "manifest.json"), "--directory", str(target), "--populate-new"],
                                    stdout=subprocess.PIPE, text=True, check=True)
        print(provisioned.stdout.strip())
    except BaseException:
        shutil.rmtree(target)
        raise

if __name__ == "__main__":
    main()
