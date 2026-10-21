#!/usr/bin/env python3
"""Explicit isolated CPU dependency installation; never invoked by the worker.

From the repository root, with uv already explicitly installed:
  python tools/audio_processing/install.py moss
  python tools/audio_processing/install.py separate
  python tools/audio_processing/install.py df3

The separate environment supports both Kim Mel-band RoFormer and HTDemucs;
prepare.py selects and pins their distinct weights/architecture per manifest.
These prepare independent pinned Python environments below .cache/neural.
Pinned CPU PyTorch environments and checkpoints require approximately5-8GiB
combined, plus corpus/results. The command requires10GiB free reserve. Download
caches and temporary files are directed below .cache/neural. No global Python
packages are changed. CUDA deployments must explicitly prepare matching pinned
PyTorch/torchaudio versions themselves; the worker never silently falls back.

Optional explicit baseline: install.py speech (NSNet2).
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
import sys

ROOT = Path(__file__).resolve().parents[2]
PINS = {
    "speech": ("3.11", [], ["onnxruntime==1.23.2", "numpy==1.26.4", "scipy==1.11.4", "soundfile==0.12.1"]),
    "moss": ("3.12.15", ["torch==2.11.0+cpu", "torchaudio==2.11.0+cpu"], [
        "numpy==2.2.6", "scipy==1.15.3", "soundfile==0.13.1", "einops==0.8.1",
        "rotary-embedding-torch==0.8.3", "torchinfo==1.8.0", "pystoi==0.4.1"]),
    "df3": ("3.11", ["torch==2.1.2+cpu", "torchaudio==2.1.2+cpu"], [
        "deepfilternet==0.5.6", "deepfilterlib==0.5.6", "numpy==1.26.4",
        "scipy==1.11.4", "soundfile==0.12.1", "packaging==23.2",
        "appdirs==1.4.4", "loguru==0.7.3", "requests==2.32.5", "sympy==1.13.3"]),
    "separate": ("3.12.15", ["torch==2.13.0+cpu", "torchaudio==2.11.0+cpu"], [
        "unblend==1.0.0", "numpy==2.2.6", "scipy==1.15.3", "soundfile==0.13.1",
        "torchcodec==0.15.0", "safetensors==0.8.0", "typer==0.24.1", "click==8.3.1",
        "rich==14.3.3", "httpx==0.28.1", "filelock==3.20.3", "pyyaml==6.0.3"]),
}

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=PINS)
    args = parser.parse_args()
    root = ROOT / ".cache/neural"
    root.mkdir(parents=True, exist_ok=True)
    if shutil.disk_usage(root).free < 10 * 1024**3:
        parser.error("reserve at least 10 GiB free before installing CPU environments")
    uv = shutil.which("uv")
    if not uv:
        parser.error("uv is required; install uv explicitly before this installer")
    env = os.environ.copy()
    for key, suffix in {"TMPDIR":"tmp", "UV_CACHE_DIR":"uv-cache", "PIP_CACHE_DIR":"pip-cache",
                        "XDG_CACHE_HOME":"xdg-cache", "TORCH_HOME":"torch", "UV_PYTHON_INSTALL_DIR":"python",
                        "UV_PYTHON_BIN_DIR":"python-bin"}.items():
        directory = root / suffix
        directory.mkdir(exist_ok=True)
        env[key] = str(directory)
    version, torch_pins, pins = PINS[args.operation]
    target = root / (args.operation + "-venv")
    if not target.exists():
        if not any((root / "python").glob("cpython-" + version + "-*")):
            subprocess.run([uv, "python", "install", version], env=env, check=True)
        subprocess.run([uv, "venv", "--managed-python", "--python", version, str(target)], env=env, check=True)
    python = str(target / "bin/python")
    if torch_pins:
        subprocess.run([uv, "pip", "install", "--python", python, "--index-url",
                        "https://download.pytorch.org/whl/cpu", *torch_pins], env=env, check=True)
    subprocess.run([uv, "pip", "install", "--python", python, *pins], env=env, check=True)
    frozen = subprocess.check_output([uv, "pip", "freeze", "--python", python], env=env, text=True)
    (root / (args.operation + "-installed.txt")).write_text(frozen)
    print(json.dumps({"operation":args.operation,"interpreter":python,"installed_lock":str(root / (args.operation + "-installed.txt"))}))

if __name__ == "__main__":
    main()
