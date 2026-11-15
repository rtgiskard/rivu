"""Apache-2.0 MossFormer2 SE 48K, using pinned publisher architecture and weights.

Only explicit prepare downloads. Inference uses strict state loading, upstream
Kaldi features/deltas and unnormalized spectral-mask synthesis (no peak rescale).
"""
import importlib.util
import sys


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
    state = torch.load(base / "model.pt", map_location="cpu", weights_only=True)
    model.load_state_dict(state, strict=True)
    return model.to(device).eval()

def _features(audio, padded_length, device):
    """Build Kaldi features while keeping only waveform/features alive for inference."""
    import numpy as np
    import torch
    import torchaudio
    # np.pad owns a copy even when no padding is needed; never scale caller storage.
    padded = np.pad(audio, (0, padded_length-len(audio)))
    waveform = torch.from_numpy(padded).mul_(32768.0)
    fbank = torchaudio.compliance.kaldi.fbank(waveform.unsqueeze(0), dither=1.0,
        frame_length=40.0, frame_shift=8.0, num_mel_bins=60, sample_frequency=48000, window_type="hamming")
    first = torchaudio.functional.compute_deltas(fbank.T)
    second = torchaudio.functional.compute_deltas(first)
    features = torch.cat((fbank, first.T, second.T), dim=1).unsqueeze(0).to(device)
    return waveform, features


def _synthesize(waveform, mask, window, count, gain_floor):
    """Consume a private CPU mask and synthesize without a second spectral buffer."""
    import torch
    # Pinned MaskNet's final non-inplace ReLU owns the output, not model state.
    mask.clamp_min_(gain_floor)
    spectrum = torch.stft(waveform, n_fft=1920, hop_length=384, win_length=1920,
                          window=window, center=False, return_complex=True)
    spectrum.mul_(mask)
    enhanced = torch.istft(spectrum, n_fft=1920, hop_length=384, win_length=1920,
                           window=window, center=False, length=len(waveform))
    enhanced[:count].div_(32768.0)
    return enhanced[:count].numpy()


def load_engine(manifest, base, device, controls):
    import numpy as np
    import torch
    model = load_model(base,device)
    window_size, hop = 1920, 384
    window = torch.hamming_window(window_size,periodic=False)
    gain_floor = 10**(-controls.get("attenuation_limit_db",80)/20)
    def run(audio):
        count, channels = audio.shape
        # The reference uses center=False. Pad the end to a complete frame so source tails
        # are inferred instead of silently zero-filled by istft(length=original_length).
        padded_length = max(window_size, ((max(0,count-window_size)+hop-1)//hop)*hop+window_size)
        result = np.empty_like(audio)
        for channel in range(channels):
            # Equal-channel inputs get equal dither; there is still no stereo-linked mask.
            torch.manual_seed(0)
            waveform, features = _features(audio[:,channel], padded_length, device)
            with torch.inference_mode():
                mask = model(features)[-1].squeeze(0).T.detach().cpu()
                del features
                result[:,channel] = _synthesize(waveform, mask, window, count, gain_floor)
            del waveform, mask
        return result
    return 48000, run
