"""Microsoft NSNet2 full-band ONNX inference; code MIT, weights CC-BY-4.0.

Reference: microsoft/DNS-Challenge commit cb4076255c2172d82fac1aefaa3eb5fb9b231a09,
NSNet2-baseline/enhance_onnx.py. Padding compensates the reference's unwindowed
edge loss; the neural log-power features, periodic sqrt-Hann and mask are unchanged.
"""
import hashlib
from pathlib import Path
import shutil

REVISION = "cb4076255c2172d82fac1aefaa3eb5fb9b231a09"
BASE = "https://raw.githubusercontent.com/microsoft/DNS-Challenge/" + REVISION + "/"
WEIGHT_PATH = "NSNet2-baseline/nsnet2-20ms-48k-baseline.onnx"
WEIGHT_SIZE = 24674693
WEIGHT_GIT_BLOB = "ea70dc67f9414441d82add290d0b70c38afc3eb8"

def prepare(args, target, download):
    checkpoint = target / "model.onnx"
    if args.download:
        download(BASE + WEIGHT_PATH, checkpoint, WEIGHT_SIZE)
    else:
        shutil.copyfile(args.weights, checkpoint)
    raw = checkpoint.read_bytes()
    blob = hashlib.sha1(b"blob " + str(len(raw)).encode() + b"\0" + raw).hexdigest()
    if len(raw) != WEIGHT_SIZE or blob != WEIGHT_GIT_BLOB:
        raise ValueError("NSNet2 artifact differs from pinned Microsoft Git blob")
    # Carry the actual primary legal notices and full licenses, not aggregator labels.
    for filename in ("README.md", "LICENSE", "LICENSE-CODE"):
        download(BASE + filename, target / ("upstream-" + filename.replace(".md", ".txt")), 65536)
    legal = (target / "upstream-README.txt").read_text()
    if "documentation and other content" not in legal or "Creative Commons Attribution 4.0" not in legal:
        raise ValueError("primary Microsoft content license declaration is missing")
    (target / "weight-license.txt").write_text(
        "NSNet2 pretrained ONNX weights are other repository content under the explicit Microsoft Legal Notices, CC-BY-4.0.\n"
        "Attribution: Microsoft Corporation; Noise Suppression Net 2 (NSNet2), Sebastian Braun and Ivan Tashev, 2020.\n"
        + BASE + WEIGHT_PATH + "\n" + legal[legal.index("# Legal Notices"):])
    return {"version":1,"operation":"speech","id":"nsnet2",
            "packages":{"onnxruntime":"1.23.2","numpy":"1.26.4","scipy":"1.11.4","soundfile":"0.12.1"},
            "provenance":{"weights":BASE+WEIGHT_PATH,"weight_git_blob":WEIGHT_GIT_BLOB,
                          "weight_license":"CC-BY-4.0","code_license":"MIT",
                          "license_source":BASE+"README.md","reference":BASE+"NSNet2-baseline/enhance_onnx.py"}}

def _log_power(spectrum):
    import numpy as np
    power = np.abs(spectrum)
    np.square(power, out=power)
    np.maximum(power, 1e-12, out=power)
    np.log10(power, out=power)
    return power.astype("float32", copy=False)


def _synthesize(spectrum, window, window_power, hop, count):
    """Overlap-add masked frames with bounded inverse-transform scratch storage."""
    import numpy as np
    if __package__:
        from .worker import require
    else:
        from worker import require
    window_size = len(window)
    fft_size = (spectrum.shape[-1] - 1) * 2
    padded_length = count + 2 * hop + (-count) % hop
    output = np.zeros((padded_length, spectrum.shape[0]), dtype="float64")
    weight = np.zeros(padded_length, dtype="float64")
    windowed = None
    # Transform bounded batches; preserve frame order and NumPy's transform dtype.
    for first in range(0, spectrum.shape[1], 64):
        reconstructed = np.fft.irfft(spectrum[:, first:first+64], n=fft_size, axis=-1)[..., :window_size].transpose(1, 0, 2)
        if windowed is None:
            windowed = np.empty((window_size, spectrum.shape[0]), dtype=reconstructed.dtype)
        for index, frame in enumerate(reconstructed):
            begin = (first + index) * hop
            np.multiply(frame.T, window[:, None], out=windowed)
            output[begin:begin+window_size] += windowed
            weight[begin:begin+window_size] += window_power
        # The last frame is a view; release it before allocating the next batch.
        del frame, reconstructed
    output = output[hop:hop+count]
    weight = weight[hop:hop+count]
    require(np.all(weight > 0), "alignment", "zero overlap-add weight")
    output /= weight[:, None]
    return output.astype("float32")


def load_engine(manifest, base, device, controls):
    import os
    import numpy as np
    import onnxruntime as ort
    if __package__:
        from .worker import require
    else:
        from worker import require
    require(device == "cpu", "device_unavailable", "pinned NSNet2 environment supports CPU only; no device fallback")
    options = ort.SessionOptions()
    options.intra_op_num_threads = int(os.environ["OMP_NUM_THREADS"])
    options.inter_op_num_threads = 1
    session = ort.InferenceSession(str(base / "model.onnx"), sess_options=options, providers=["CPUExecutionProvider"])
    name = session.get_inputs()[0].name
    window_size, hop, fft_size = 960, 512, 1024
    window = np.sqrt(0.5 - 0.5*np.cos(2*np.pi*np.arange(window_size)/window_size)).astype("float32")
    window_power = window**2
    gain_floor = 10**(-controls.get("attenuation_limit_db",80)/20)
    def run(audio):
        count, channels = audio.shape
        # One leading hop and enough trailing zeros restore both signal edges without latency.
        padded = np.pad(audio, ((hop, hop + (-count) % hop), (0, 0)))
        frames = np.lib.stride_tricks.sliding_window_view(padded, window_size, axis=0)[::hop]
        # [time, channels, window] -> [channels, time, frequency]
        spectrum = np.fft.rfft(frames*window, n=fft_size, axis=-1).transpose(1, 0, 2)
        features = _log_power(spectrum)
        # The graph initializes recurrent states per batch member: no cross-channel mixing.
        mask = session.run(None, {name:features})[0]
        require(mask.shape == features.shape and np.isfinite(mask).all(), "inference_output", "invalid NSNet2 spectral mask")
        # A gain floor limits suppression in dB; this is not waveform clipping/rescaling.
        np.maximum(mask, gain_floor, out=mask)
        spectrum *= mask
        return _synthesize(spectrum, window, window_power, hop, count)
    return 48000, run
