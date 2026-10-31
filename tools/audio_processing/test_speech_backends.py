"""Numerical speech-backend boundaries; real weights require RIVU_SPEECH_MANIFEST."""
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import unittest

import numpy as np

@unittest.skipUnless(importlib.util.find_spec("torch") and importlib.util.find_spec("torchaudio"),
                     "Torch and torchaudio required for real Kaldi/spectral numerics")
class MossFormerNumericalTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        from runtime import mossformer
        import torch
        import torchaudio
        cls.backend, cls.torch, cls.torchaudio = mossformer, torch, torchaudio
        cls.addClassCleanup(torch.set_num_threads, torch.get_num_threads())
        torch.set_num_threads(1)
        cls.enterClassContext(torch.random.fork_rng(devices=[]))
        cls.window = torch.hamming_window(1920, periodic=False)

    def test_exact_features_synthesis_and_caller_storage_at_boundaries(self):
        torch, torchaudio = self.torch, self.torchaudio
        for count in (1, 383, 384, 385, 1919, 1920, 1921, 2303, 2304, 2305, 48037):
            source = np.random.default_rng(count).normal(0, 0.1, (count, 4)).astype("float32")
            source[0, 0], source[-1, 2] = 1.5, -1.25
            for layout, audio in (("mono", source[:, :1]), ("stereo_strided", source[:, ::2]),
                                  ("stereo_equal", np.repeat(source[:, :1], 2, axis=1))):
                with self.subTest(count=count, layout=layout):
                    original = audio.copy()
                    audio.setflags(write=False)
                    # Independent full-frame padding formula, including the no-pad boundary.
                    length = 1920 + ((max(0, count-1920)+383)//384)*384
                    channel_features, channel_outputs = [], []
                    for channel in range(audio.shape[1]):
                        torch.random.default_generator.manual_seed(0)
                        expected_waveform = torch.from_numpy(np.pad(audio[:, channel], (0, length-count)))*32768.0
                        fbank = torchaudio.compliance.kaldi.fbank(expected_waveform.unsqueeze(0), dither=1.0,
                            frame_length=40.0, frame_shift=8.0, num_mel_bins=60,
                            sample_frequency=48000, window_type="hamming")
                        first = torchaudio.functional.compute_deltas(fbank.T)
                        second = torchaudio.functional.compute_deltas(first)
                        expected_features = torch.cat((fbank, first.T, second.T), dim=1).unsqueeze(0)
                        torch.random.default_generator.manual_seed(0)
                        waveform, features = self.backend._features(audio[:, channel], length, "cpu")
                        np.testing.assert_array_equal(features.numpy(), expected_features.numpy())
                        np.testing.assert_array_equal(waveform.numpy(), expected_waveform.numpy())
                        self.assertFalse(np.shares_memory(waveform.numpy(), audio))
                        channel_features.append(features)
                        # Nontrivial frequency/time gains test DSP, never substitute a model.
                        mask = torch.linspace(-0.1, 1.25, 961*fbank.shape[0]).reshape(961, fbank.shape[0])
                        for gain_floor in (1.0, 0.1, 0.0001):
                            with self.subTest(channel=channel, gain_floor=gain_floor), torch.inference_mode():
                                spectrum = torch.stft(expected_waveform, n_fft=1920, hop_length=384,
                                    win_length=1920, window=self.window, center=False, return_complex=True)
                                enhanced = torch.istft(spectrum*mask.clamp_min(gain_floor), n_fft=1920,
                                    hop_length=384, win_length=1920, window=self.window, center=False, length=length)
                                expected = enhanced[:count].numpy()/32768.0
                                actual = self.backend._synthesize(waveform, mask.clone(), self.window, count, gain_floor)
                                self.assertEqual(actual.shape, (count,))
                                self.assertEqual(actual.dtype, np.float32)
                                np.testing.assert_array_equal(actual, expected)
                        channel_outputs.append(actual)
                    if layout == "stereo_equal":
                        np.testing.assert_array_equal(channel_features[0].numpy(), channel_features[1].numpy())
                        np.testing.assert_array_equal(channel_outputs[0], channel_outputs[1])
                    np.testing.assert_array_equal(audio, original)

    def test_synthesis_preserves_other_speaker_in_transposed_relu_mask(self):
        torch = self.torch
        with torch.inference_mode():
            waveform = torch.linspace(-1000, 1000, 2688)
            source = torch.linspace(-0.1, 1.1, 2*961*3).reshape(1, 2, 961, 3)
            relu_output = torch.nn.ReLU()(source)
            untouched_speaker = relu_output[:, 1].clone()
            # Pinned final activation/view geometry: selected speaker shares storage.
            mask = relu_output.transpose(0, 1)[0].transpose(1, 2).squeeze(0).T.detach().cpu()
            spectrum = torch.stft(waveform, n_fft=1920, hop_length=384, win_length=1920,
                window=self.window, center=False, return_complex=True)
            expected = torch.istft(spectrum*mask.clamp_min(0.0001), n_fft=1920, hop_length=384,
                win_length=1920, window=self.window, center=False, length=len(waveform))[:2305].numpy()/32768.0
            output = self.backend._synthesize(waveform, mask, self.window, 2305, 0.0001)
            np.testing.assert_array_equal(output, expected)
            np.testing.assert_array_equal(relu_output[:, 1].numpy(), untouched_speaker.numpy())


@unittest.skipUnless(os.environ.get("RIVU_SPEECH_MANIFEST"), "explicit prepared real speech model required")
class RealSpeechBackendTests(unittest.TestCase):
    def test_real_model_short_tails_and_state_reset(self):
        # Match the worker's one-request process lifetime; torch thread/config state is global.
        script = """
import os
from pathlib import Path
import numpy as np
from runtime.worker import build_engine, check_packages, deny_network, validate_manifest
path = Path(os.environ["RIVU_SPEECH_MANIFEST"]).resolve()
manifest = validate_manifest(path, "speech")
check_packages(manifest)
os.environ.setdefault("OMP_NUM_THREADS", "1")
deny_network()
_, engine = build_engine(manifest, path.parent, "cpu", {})
for count in (1, 383, 384, 385, 511, 512, 513, 1919, 1920, 1921, 48037):
    mono = np.random.default_rng(count).normal(0, 0.1, (count, 1)).astype("float32")
    audio = np.repeat(mono, 2, axis=1)
    original = audio.copy()
    first = engine(audio)
    assert first.shape == audio.shape, (count, first.shape)
    assert np.isfinite(first).all(), count
    np.testing.assert_allclose(first[:, 0], first[:, 1], rtol=1e-5, atol=1e-6)
    np.testing.assert_array_equal(audio, original)
    engine(np.zeros((1921, 1), dtype="float32"))
    np.testing.assert_allclose(engine(audio), first, rtol=1e-5, atol=1e-6)
"""
        completed = subprocess.run([sys.executable, "-B", "-c", script], cwd=Path(__file__).resolve().parent,
                                   capture_output=True, text=True, timeout=600)
        self.assertEqual(completed.returncode, 0, completed.stderr + completed.stdout)



if __name__ == "__main__":
    unittest.main()
