"""Pinned official streaming ONNX bundles; preparation only, never inference.

GTCRN uses sqrt(periodic Hann); UL-UNAS uses periodic Hann. Both consume
16 kHz, FFT512/hop256 centered spectra and carry independent per-channel state.
Native synthesis completes the final hop by reflecting against original length
and crops after overlap-add; it does not reproduce an undercovered partial tail.
The original repository MIT notices are retained verbatim. No separate weight
license declaration is invented from those source notices.
"""
import hashlib
import shutil

GTCRN_REV = "502ebfab64da7c4a9af78dcb9c6ceef1ebb01c73"
ULUNAS_REV = "00f7c700da43d38347f30a6ccebd86fcbc798e07"
# local name, upstream path, exact bytes, SHA256. All sources are immutable.
GTCRN_FILES = (
    ("model.onnx", "stream/onnx_models/gtcrn_simple.onnx", 535190, "b4718df6228e7bdf1a8a435cf98f838636eb2fd331acabf86ba87c5192ebcb87"),
    ("weight-license.txt", "LICENSE", 1069, "c467165e5860b4a7494ef4a6e2788e4115d95b5b8989bb5f86287089adddf794"),
    ("reference-infer.py", "infer.py", 782, "1061bb6a9a6f1a27423f7603871e683aaf0cd3ffd40a75efa9f9ba0965330d7b"),
    ("reference-model.rs", "ladspa/src/model.rs", 8423, "a9b6ae067585b17295c70ea8e455e1942bb26cbf9df4ddc54a6392c8b0eebe0a"),
    ("reference-stft.rs", "ladspa/src/stft.rs", 6239, "1314c4d07d9c66d82bdff37119aa1784fa6f838e267f93ed2090b2fb8ad6b80d"),
)
ULUNAS_FILES = (
    ("model.onnx", "ulunas_onnx/onnx_models/ulunas_stream_simple.onnx", 788967, "f2e804d54d6a88f4f82f44d86c9f1cf646db2509bfca935cfbfc5fcd8cbfac3b"),
    ("weight-license.txt", "LICENSE", 1069, "94daad6ad643a2f79d4a111015c3415fe20c6f630b908e884edf466923724184"),
    ("reference-stream.txt", "ulunas_onnx/README.md", 2142, "e148039b6b1f61240d7ea1113e21b5c48997ad9a6c2bf1257500e16afc48fcb9"),
)


def prepare(args, target, download):
    """Return a package-free native manifest using the shared preparation API."""
    if args.model == "gtcrn":
        model_id, repo, revision, artifacts = "GTCRN", "gtcrn", GTCRN_REV, GTCRN_FILES
    elif args.model == "ul_unas":
        model_id, repo, revision, artifacts = "UL_UNAS", "ul-unas", ULUNAS_REV, ULUNAS_FILES
    else:
        raise ValueError("unsupported native speech model")
    base = f"https://raw.githubusercontent.com/Xiaobin-Rong/{repo}/{revision}/"
    files = {}
    for name, source, size, expected in artifacts:
        destination = target / name
        if name == "model.onnx" and not args.download:
            if args.weights.stat().st_size != size:
                raise ValueError(f"{model_id} supplied ONNX differs from pinned official size")
            shutil.copyfile(args.weights, destination)
        else:
            download(base + source, destination, size)
        actual = hashlib.sha256(destination.read_bytes()).hexdigest()
        if destination.stat().st_size != size or actual != expected:
            raise ValueError(f"{model_id} {name} differs from pinned official size/SHA256")
        files[name] = actual
    return {
        "version": 1, "operation": "speech", "id": model_id, "backend": "onnx",
        "model": "model.onnx", "files": files, "license_evidence": "weight-license.txt",
        "provenance": {
            "weights": base + artifacts[0][1], "weights_sha256": artifacts[0][3],
            "weights_size_bytes": artifacts[0][2], "revision": revision,
            "repository": f"https://github.com/Xiaobin-Rong/{repo}/tree/{revision}",
            "license_source": base + "LICENSE", "code_license": "MIT",
            "weights_license": "repository MIT notice retained; no separate weight-specific grant identified",
            "selection": "explicit preparation of exact official ONNX; no model substitution",
        },
        "geometry": {"sample_rate": 16000, "fft_size": 512, "hop_size": 256,
                     "window": "sqrt_hann_periodic" if model_id == "GTCRN" else "hann_periodic",
                     "center": True, "pad_mode": "reflect", "channels": "independent recurrent state",
                     "synthesis": "complete final reflected hop; ceil(N/256)+1 spectra (minimum two), crop to N"},
    }
