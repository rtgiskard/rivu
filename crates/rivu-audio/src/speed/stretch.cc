// Minimal C ABI over the vendored, header-only Signalsmith Stretch.
// Construction allocates; reset, process and flush reuse configured buffers.
#include "signalsmith-stretch.h"

#include <memory>
#include <random>

namespace {
// A fixed seed keeps rendering deterministic for identical input.
using Stretch = signalsmith::stretch::SignalsmithStretch<float, std::minstd_rand>;

template <class T> struct Interleaved {
	T  *data;
	int channels;
	struct Channel {
		T  *data;
		int channel;
		int channels;
		T  &operator[](int frame) const { return data[frame * channels + channel]; }
	};
	Channel operator[](int channel) const { return {data, channel, channels}; }
};
} // namespace

extern "C" {
void *rivu_stretch_new(int channels, float sample_rate) {
	try {
		// Own the instance until configuration succeeds; a throwing preset frees it.
		std::unique_ptr<Stretch> stretch(new Stretch(0));
		stretch->presetDefault(channels, sample_rate);
		return stretch.release();
	} catch (...) {
		return nullptr;
	}
}

void rivu_stretch_free(void *stretch) { delete static_cast<Stretch *>(stretch); }

void rivu_stretch_reset(void *stretch) { static_cast<Stretch *>(stretch)->reset(); }

int rivu_stretch_input_latency(const void *stretch) {
	return static_cast<const Stretch *>(stretch)->inputLatency();
}

int rivu_stretch_output_latency(const void *stretch) {
	return static_cast<const Stretch *>(stretch)->outputLatency();
}

void rivu_stretch_process(void *stretch, const float *input, int input_frames, float *output,
                          int output_frames, int channels) {
	static_cast<Stretch *>(stretch)->process(Interleaved<const float>{input, channels},
	                                         input_frames, Interleaved<float>{output, channels},
	                                         output_frames);
}

void rivu_stretch_flush(void *stretch, float *output, int output_frames, int channels,
                        float playback_rate) {
	static_cast<Stretch *>(stretch)->flush(Interleaved<float>{output, channels}, output_frames,
	                                       playback_rate);
}
}
