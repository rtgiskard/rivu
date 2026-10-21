"""Apache-2.0 MossFormer2 SE 48K, using pinned publisher architecture and weights.

Only explicit prepare downloads. Inference uses strict state loading, upstream
Kaldi features/deltas and unnormalized spectral-mask synthesis (no peak rescale).
"""
import importlib.util
import math
from pathlib import Path
import shutil
import sys

CODE_REVISION = "6b3774dc79c46ae8bed2a4fa5f706f0ac8c75c61"
CODE_BASE = "https://raw.githubusercontent.com/modelscope/ClearerVoice-Studio/" + CODE_REVISION + "/"
MODEL_REVISION = "873142431e608e0249bea2c3adab330f8954c936"
WEIGHT_URL = "https://modelscope.cn/models/iic/ClearerVoice-Studio/resolve/" + MODEL_REVISION + "/MossFormer2_SE_48K/last_best_checkpoint.pt"
WEIGHT_SHA = "03692b9f773bbd6bb43b9c5a41f96b1e28affd66e13796b7bec66ad3d8b227c6"
LICENSE_CARD = "https://www.modelscope.cn/models/iic/ClearerVoice-Studio.md"
SOURCE_FILES = ("__init__.py", "conv_module.py", "fsmn.py", "layer_norm.py", "mossformer2.py", "mossformer2_block.py", "mossformer2_se_wrapper.py")

def prepare(args, target, download):
    from worker import digest
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
        download(WEIGHT_URL, checkpoint, 221552019)
    else:
        shutil.copyfile(args.weights, checkpoint)
    if checkpoint.stat().st_size != 221552019 or digest(checkpoint) != WEIGHT_SHA:
        raise ValueError("MossFormer2 SE48K checkpoint differs from pinned publisher artifact")
    return {"version":1,"operation":"speech","id":"MossFormer2_SE_48K",
            "packages":{"torch":"2.11.0","torchaudio":"2.11.0","numpy":"2.2.6","scipy":"1.15.3",
                        "soundfile":"0.13.1","einops":"0.8.1","rotary-embedding-torch":"0.8.3","torchinfo":"1.8.0"},
            "provenance":{"weights":WEIGHT_URL,"code_revision":CODE_REVISION,"model_revision":MODEL_REVISION,
                          "weight_license":"Apache-2.0","code_license":"Apache-2.0","license_source":LICENSE_CARD},
            "geometry":{"core_seconds":8,"context_seconds":2,"window_samples":1920,"hop_samples":384,
                        "mel_bands":60,"fbank_dither":1.0,"seed":0}}

def load_model(base, device):
    import torch
    package_name = "_rivu_mossformer2_se"
    spec = importlib.util.spec_from_file_location(package_name, base / "architecture/__init__.py",
                                                submodule_search_locations=[str(base / "architecture")])
    module = importlib.util.module_from_spec(spec)
    sys.modules[package_name] = module
    spec.loader.exec_module(module)
    wrapper = importlib.import_module(package_name + ".mossformer2_se_wrapper")
    model = wrapper.MossFormer2_SE_48K(None).model
    checkpoint = torch.load(base / "model.pt", map_location="cpu", weights_only=True)
    state = checkpoint["model"] if "model" in checkpoint else checkpoint
    state = {key.removeprefix("module."):value for key,value in state.items()}
    model.load_state_dict(state, strict=True)
    return model.to(device).eval()

def load_engine(manifest, base, device, controls):
    import numpy as np
    import torch
    import torchaudio
    model = load_model(base,device)
    window_size, hop = 1920, 384
    window = torch.hamming_window(window_size,periodic=False)
    def run(audio):
        count, channels = audio.shape
        # The reference uses center=False. Pad the end to a complete frame so source tails
        # are inferred instead of silently zero-filled by istft(length=original_length).
        padded_length = max(window_size, math.ceil(max(0,count-window_size)/hop)*hop+window_size)
        result = np.empty_like(audio)
        for channel in range(channels):
            # Equal-channel inputs get equal dither; there is still no stereo-linked mask.
            torch.manual_seed(0)
            padded = np.pad(audio[:,channel],(0,padded_length-count))
            waveform = torch.from_numpy(padded)*32768.0  # model's documented training units
            fbank = torchaudio.compliance.kaldi.fbank(waveform.unsqueeze(0),dither=1.0,
                frame_length=40.0,frame_shift=8.0,num_mel_bins=60,sample_frequency=48000,window_type="hamming")
            first = torchaudio.functional.compute_deltas(fbank.T)
            second = torchaudio.functional.compute_deltas(first)
            features = torch.cat((fbank,first.T,second.T),dim=1).unsqueeze(0).to(device)
            with torch.inference_mode():
                mask = model(features)[-1].squeeze(0).T.detach().cpu()
                mask = mask.clamp_min(10**(-controls.get("attenuation_limit_db",80)/20))
                spectrum = torch.stft(waveform,n_fft=window_size,hop_length=hop,win_length=window_size,
                                      window=window,center=False,return_complex=True)
                enhanced = torch.istft(spectrum*mask,n_fft=window_size,hop_length=hop,win_length=window_size,
                                       window=window,center=False,length=padded_length)
            result[:,channel] = enhanced[:count].numpy()/32768.0
        return result
    return 48000, run
