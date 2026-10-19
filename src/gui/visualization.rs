mod analysis_visualizer;
mod audio_visualizer;
mod fractal_beams;
mod fractal_flow;
mod fractal_visualizer;
mod visuals;
mod waveform;

pub(super) use audio_visualizer::AudioVisualizer;
pub(super) use visuals::Visuals;
pub(super) use waveform::{Waveform, WaveformEvent};

use crate::{config::AudioVisualizerStyle, i18n::Localizer};
use gpui::{Div, SharedString, div, prelude::*, rgb};

fn visualization_status(message: impl Into<SharedString>) -> Div {
    div()
        .w_full()
        .p_2()
        .text_sm()
        .text_color(rgb(0xb1bbc5))
        .child(message.into())
}

pub(super) fn audio_visualizer_style_id(style: AudioVisualizerStyle) -> &'static str {
    match style {
        AudioVisualizerStyle::Bars => "settings-bars",
        AudioVisualizerStyle::Rings => "settings-rings",
        AudioVisualizerStyle::BarsRings => "settings-bars-rings",
        AudioVisualizerStyle::StereoDensity => "settings-stereo-density",
        AudioVisualizerStyle::SpatialSpectrum => "settings-spatial-spectrum",
        AudioVisualizerStyle::FractalBeams => "settings-fractal-beams",
        AudioVisualizerStyle::FractalFlow => "settings-fractal-flow",
    }
}

pub(super) fn audio_visualizer_style_label(i18n: &Localizer, style: AudioVisualizerStyle) -> &str {
    i18n.text(audio_visualizer_style_id(style))
}
