#!/usr/bin/env python3
"""Explicit isolated CPU dependency installation; never invoked by the worker.

From the repository root, with uv already explicitly installed:
  python tools/audio_processing/install.py
  python tools/audio_processing/install.py df3

The default .venv supports MossFormer2 and Kim Mel-band RoFormer.
DeepFilterNet3 remains separate because it requires NumPy below 2.
Dependency ranges express compatibility; the installed versions are recorded
below .cache/neural. Model preparation records those exact installed identities.
CPU environments and checkpoints require approximately 5-8 GiB combined, plus
corpus/results. The command requires 10 GiB free reserve. uv uses its default
cache and Python directories; large temporary files stay below .cache/tmp.
No global Python packages are changed.
CUDA environments must be prepared explicitly; the worker never falls back.

DeepFilterNet3: install.py df3, then prepare.py df3 --download or --weights.
Its public pretrained weight-license status is unresolved and recorded as such;
candidate preparation does not imply permission to redistribute the weights.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[2]
ENVIRONMENTS = {
    "default": ("3.12", ["torch>=2.13", "torchaudio>=2.11"], [
        "unblend>=1.0", "numpy>=2", "scipy>=1.15", "soundfile>=0.13",
        "einops>=0.8", "rotary-embedding-torch>=0.8", "torchinfo>=1.8", "pystoi>=0.4"]),
    "df3": ("3.11", ["torch==2.1.2+cpu", "torchaudio==2.1.2+cpu"], [
        "deepfilternet==0.5.6", "deepfilterlib==0.5.6", "numpy==1.26.4",
        "scipy==1.11.4", "soundfile==0.12.1", "packaging==23.2",
        "appdirs==1.4.4", "loguru==0.7.3", "requests==2.32.5", "sympy==1.13.3"]),
}

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", nargs="?", choices=ENVIRONMENTS, default="default")
    args = parser.parse_args()
    root = ROOT / ".cache/neural"
    root.mkdir(parents=True, exist_ok=True)
    if shutil.disk_usage(root).free < 10 * 1024**3:
        parser.error("reserve at least 10 GiB free before installing CPU environments")
    uv = shutil.which("uv")
    if not uv:
        parser.error("uv is required; install uv explicitly before this installer")
    env = os.environ.copy()
    temporary = ROOT / ".cache/tmp"
    temporary.mkdir(parents=True, exist_ok=True)
    env["TMPDIR"] = str(temporary)
    version, torch_requirements, requirements = ENVIRONMENTS[args.operation]
    target = root / (".venv" if args.operation == "default" else args.operation + "-venv")
    if not target.exists():
        subprocess.run([uv, "venv", "--managed-python", "--python", version, str(target)], env=env, check=True)
    python = str(target / "bin/python")
    if torch_requirements:
        subprocess.run([uv, "pip", "install", "--python", python, "--index-url",
                        "https://download.pytorch.org/whl/cpu", *torch_requirements], env=env, check=True)
    subprocess.run([uv, "pip", "install", "--python", python, *requirements], env=env, check=True)
    subprocess.run([uv, "pip", "check", "--python", python], env=env, check=True)
    frozen = subprocess.check_output([uv, "pip", "freeze", "--python", python], env=env, text=True)
    lock = root / ("installed.txt" if args.operation == "default" else args.operation + "-installed.txt")
    lock.write_text(frozen)
    print(json.dumps({"operation":args.operation,"interpreter":python,"installed_lock":str(lock)}))

if __name__ == "__main__":
    main()
