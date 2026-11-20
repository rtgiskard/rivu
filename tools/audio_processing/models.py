"""Explicit bounded downloads and preparation of every supported model.

Inference never imports this module or downloads artifacts. Pinned publisher
snapshots, original notices and factual weight-license status are retained.
"""
import json
import shutil
import subprocess
import urllib.request
import zipfile

if __package__:
    from .common import digest
else:
    from common import digest

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
CODE_REVISION = "6b3774dc79c46ae8bed2a4fa5f706f0ac8c75c61"
CODE_BASE = "https://raw.githubusercontent.com/modelscope/ClearerVoice-Studio/" + CODE_REVISION + "/"
MODEL_REVISION = "873142431e608e0249bea2c3adab330f8954c936"
MOSS_WEIGHT_URL = "https://modelscope.cn/models/iic/ClearerVoice-Studio/resolve/" + MODEL_REVISION + "/MossFormer2_SE_48K/last_best_checkpoint.pt"
WEIGHT_SHA = "03692b9f773bbd6bb43b9c5a41f96b1e28affd66e13796b7bec66ad3d8b227c6"
LICENSE_CARD = "https://www.modelscope.cn/models/iic/ClearerVoice-Studio.md"
SOURCE_FILES = ("__init__.py", "conv_module.py", "fsmn.py", "layer_norm.py", "mossformer2.py", "mossformer2_block.py", "mossformer2_se_wrapper.py")

def prepare_moss(args, target, download):
    download(LICENSE_CARD, target / "weight-license.txt", 65536)
    if 'license: "Apache License 2.0"' not in (target / "weight-license.txt").read_text():
        raise ValueError("primary model publisher card does not grant Apache2.0")
    download(CODE_BASE + "LICENSE", target / "LICENSE-APACHE-2.0", 65536)
    architecture = target / "architecture"
    architecture.mkdir()
    for filename in SOURCE_FILES:
        download(CODE_BASE + "clearvoice/clearvoice/models/mossformer2_se/" + filename, architecture / filename, 1024**2)
    # Retain original reference decode/helpers for independently executable comparisons.
    for filename in ("decode.py", "misc.py"):
        download(CODE_BASE + "clearvoice/clearvoice/utils/" + filename, target / ("reference_" + filename), 1024**2)
    checkpoint = target / "model.pt"
    if args.download:
        download(MOSS_WEIGHT_URL, checkpoint, 221552019)
    else:
        shutil.copyfile(args.weights, checkpoint)
    if checkpoint.stat().st_size != 221552019:
        raise ValueError("MossFormer2 SE48K checkpoint differs from pinned publisher artifact")
    return {"version":1,"operation":"speech","id":"MossFormer2_SE_48K",
            "packages":["torch", "torchaudio", "numpy", "scipy", "soundfile", "einops",
                        "rotary-embedding-torch", "torchinfo"],
            "provenance":{"weights":MOSS_WEIGHT_URL,"code_revision":CODE_REVISION,"model_revision":MODEL_REVISION,
                          "weight_license":"Apache-2.0","code_license":"Apache-2.0","license_source":LICENSE_CARD},
            "geometry":{"core_seconds":8,"context_seconds":2,"window_samples":1920,"hop_samples":384,
                        "mel_bands":60,"fbank_dither":1.0,"seed":0}}, {checkpoint.name: WEIGHT_SHA}
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


def prepare_onnx(args, target, download):
    """Return a package-free native manifest using the shared preparation API."""
    if args.model == "gtcrn":
        model_id, repo, revision, artifacts = "GTCRN", "gtcrn", GTCRN_REV, GTCRN_FILES
    elif args.model == "ul_unas":
        model_id, repo, revision, artifacts = "UL_UNAS", "ul-unas", ULUNAS_REV, ULUNAS_FILES
    else:
        raise ValueError("unsupported native speech model")
    base = f"https://raw.githubusercontent.com/Xiaobin-Rong/{repo}/{revision}/"
    pins = {}
    for name, source, size, expected in artifacts:
        destination = target / name
        if name == "model.onnx" and not args.download:
            if args.weights.stat().st_size != size:
                raise ValueError(f"{model_id} supplied ONNX differs from pinned official size")
            shutil.copyfile(args.weights, destination)
        else:
            download(base + source, destination, size)
        if destination.stat().st_size != size:
            raise ValueError(f"{model_id} {name} differs from pinned official size")
        pins[name] = expected
    return {
        "version": 1, "operation": "speech", "id": model_id, "backend": "onnx",
        "model": "model.onnx", "license_evidence": "weight-license.txt",
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
    }, pins
REVISION = "978576aa8400552a4ce9730838c635aa30db5e61"
BASE = f"https://raw.githubusercontent.com/Rikorose/DeepFilterNet/{REVISION}/"
WEIGHT_URL = BASE + "models/DeepFilterNet3.zip"
WEIGHT_SIZE = 7986207
WEIGHT_GIT_BLOB = "ea53bac00d064378451d0201d573f44f05f1d746"
WEIGHT_SHA256 = "49c52edc8947ae1f9bf50d81530beaf3a2c3245aeaf34b6f31ff535cd22284d2"


def prepare_df3(args, target, download):
    archive_path = target / "upstream-model.zip"
    if args.download:
        download(WEIGHT_URL, archive_path, WEIGHT_SIZE)
    else:
        if args.weights.stat().st_size > 64 * 1024**2:
            raise ValueError("DF3 supplied archive exceeds 64 MiB")
        shutil.copyfile(args.weights, archive_path)
    archive_digest = digest(archive_path)
    if args.download and (archive_path.stat().st_size != WEIGHT_SIZE or archive_digest != WEIGHT_SHA256):
        raise ValueError("DF3 artifact differs from pinned upstream archive SHA256")
    with zipfile.ZipFile(archive_path) as archive:
        if sum(member.file_size for member in archive.infolist()) > 64 * 1024**2:
            raise ValueError("DF3 archive expands beyond 64 MiB")
        for member in archive.infolist():
            name = Path(member.filename)
            if name.is_absolute() or ".." in name.parts or (member.external_attr >> 16) & 0o170000 == 0o120000:
                raise ValueError("unsafe archive entry")
            if not name.parts or name.parts[0] != "DeepFilterNet3":
                raise ValueError("DF3 archive must contain only DeepFilterNet3/ model assets")
        archive.extractall(target)
    if not (target / "DeepFilterNet3/config.ini").is_file():
        raise ValueError("expected DeepFilterNet3/config.ini in archive")
    for name in ("README.md", "LICENSE", "LICENSE-MIT", "LICENSE-APACHE"):
        download(BASE + name, target / ("upstream-" + name.replace(".md", ".txt")), 65536)
    if args.license_evidence:
        shutil.copyfile(args.license_evidence, target / "supplied-weight-license.txt")
    (target / "weight-license.txt").write_text(
        "DeepFilterNet3 pretrained weight license: unresolved; no weight grant inferred from source-code licenses.\n"
        "Upstream source is MIT/Apache-2.0; original notices are retained separately.\n"
        "https://github.com/Rikorose/DeepFilterNet/issues/697\n"
        "This is explicit user-authorized preparation of a supplied/public candidate, not a redistribution grant.\n"
        + ("Additional user-supplied evidence is retained verbatim in supplied-weight-license.txt.\n" if args.license_evidence else ""))
    return {"version":1,"operation":"speech","id":"DeepFilterNet3",
            "packages":{"deepfilternet":"0.5.6","torch":"2.1.2","numpy":"1.26.4","scipy":"1.11.4","soundfile":"0.12.1"},
            "provenance":{"weights":WEIGHT_URL if args.download else "user-supplied local archive",
                          "weights_sha256":archive_digest,"upstream_weight_git_blob":WEIGHT_GIT_BLOB if args.download else None,
                          "code":f"https://github.com/Rikorose/DeepFilterNet/tree/{REVISION}","code_license":"MIT OR Apache-2.0",
                          "weights_license":"unresolved; no grant inferred", "license_source":BASE + "README.md",
                          "public_weight_license_status":"unresolved https://github.com/Rikorose/DeepFilterNet/issues/697",
                          "selection":"explicit user preparation; license metadata is not a candidate gate"}}


def prepare_kim(args, target, download):
    checkpoint = target / "model.safetensors"
    if args.download:
        download(KIM_URL.replace("huggingface.co", args.hf_host) + "?download=true", checkpoint, 912885624)
    else:
        shutil.copyfile(args.weights, checkpoint)
    if checkpoint.stat().st_size != 912885624:
        raise ValueError("Kim safetensors differs from pinned unblend 1.0.0 checkpoint")
    # The original trainer's immutable card, not the converter's code license.
    download(KIM_CARD.replace("huggingface.co", args.hf_host), target / "weight-license.txt", 65536)
    registry = {"version":1,"models":{"melband_roformer_kim":{
        "architecture":"mel_band_roformer", "license":"MIT", "sources":["vocals","other"],
        "samplerate":44100,"segment_samples":352800,"config":KIM_CONFIG,
        "checkpoint":{"format":"safetensors","path":str(checkpoint),"sha256":KIM_SHA,"size_bytes":912885624}}}}
    (target / "registry.json").write_text(json.dumps(registry, indent=2) + "\n")
    return {"version":1,"operation":"separate","id":"melband_roformer_kim",
            "packages":["unblend", "torch", "torchaudio", "torchcodec", "numpy", "scipy", "soundfile"],
            "provenance":{"weights":KIM_URL,"weight_license":KIM_CARD,"transport_host":args.hf_host,
                          "code":"https://github.com/Ryan5453/unblend/tree/64fd0216b9f0309c9d055457002d6910dc245950",
                          "code_license":"MIT", "weights_license":"MIT"}}, {checkpoint.name: KIM_SHA}


def prepare_model(args, target, download):
    if args.model == "moss":
        return prepare_moss(args, target, download)
    if args.model == "separate":
        return prepare_kim(args, target, download)
    if args.model in ("ul_unas", "gtcrn"):
        return prepare_onnx(args, target, download)
    return prepare_df3(args, target, download), {}
