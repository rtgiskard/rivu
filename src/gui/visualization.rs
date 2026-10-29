pub(super) use rivu_visuals::{
    FrequencyViews, StyleCoordinator, VisualLabels, VisualParameters, Waveform, WaveformEvent,
    WaveformScanParameters,
};

use crate::{config::AudioVisualizerStyle, i18n::Localizer};
use std::sync::Arc;

pub(super) const AUDIO_VISUALIZER_STYLES: [AudioVisualizerStyle; 11] = [
    AudioVisualizerStyle::Waveform,
    AudioVisualizerStyle::Spectrum,
    AudioVisualizerStyle::Spectrogram,
    AudioVisualizerStyle::Bars,
    AudioVisualizerStyle::Rings,
    AudioVisualizerStyle::BarsRings,
    AudioVisualizerStyle::StereoDensity,
    AudioVisualizerStyle::SpatialSpectrum,
    AudioVisualizerStyle::FractalBeams,
    AudioVisualizerStyle::FractalFlow,
    AudioVisualizerStyle::Oscilloscope,
];

pub(super) fn audio_visualizer_style_id(style: AudioVisualizerStyle) -> &'static str {
    match style {
        AudioVisualizerStyle::Waveform => "settings-waveform",
        AudioVisualizerStyle::Spectrum => "settings-spectrum",
        AudioVisualizerStyle::Spectrogram => "settings-spectrogram",
        AudioVisualizerStyle::Bars => "settings-bars",
        AudioVisualizerStyle::Rings => "settings-rings",
        AudioVisualizerStyle::BarsRings => "settings-bars-rings",
        AudioVisualizerStyle::StereoDensity => "settings-stereo-density",
        AudioVisualizerStyle::SpatialSpectrum => "settings-spatial-spectrum",
        AudioVisualizerStyle::FractalBeams => "settings-fractal-beams",
        AudioVisualizerStyle::FractalFlow => "settings-fractal-flow",
        AudioVisualizerStyle::Oscilloscope => "settings-oscilloscope",
    }
}

pub(super) fn audio_visualizer_style_label(i18n: &Localizer, style: AudioVisualizerStyle) -> &str {
    i18n.text(audio_visualizer_style_id(style))
}

pub(super) fn visual_labels(i18n: &Localizer) -> Arc<VisualLabels> {
    let text = |id| i18n.text(id).to_owned().into();
    Arc::new(VisualLabels {
        density_title: text("visual-density-title"),
        spatial_title: text("visual-spatial-title"),
        mid: text("visual-mid"),
        side: text("visual-side"),
        level_zero: text("visual-level-zero"),
        level_half: text("visual-level-half"),
        level_full: text("visual-level-full"),
        scene: text("visual-scene"),
        oscilloscope_scale: i18n
            .format(
                "visual-oscilloscope-scale",
                &[
                    ("amplitude", "{amplitude}".into()),
                    ("span", "{span}".into()),
                ],
            )
            .into(),
        no_spectrum: text("visual-no-spectrum"),
        no_spectrogram: text("visual-no-spectrogram"),
        waveform_loading: text("waveform-loading"),
        visual_error: i18n
            .format("visual-error", &[("error", "{error}".into())])
            .into(),
        waveform_error: i18n
            .format("waveform-error", &[("error", "{error}".into())])
            .into(),
        density_range: i18n
            .format("visual-density-range", &[("range", "{range}".into())])
            .into(),
        power_range: i18n
            .format("visual-power-range", &[("range", "{range}".into())])
            .into(),
        stereo_readout: i18n
            .format(
                "visual-stereo-readout",
                &[
                    ("correlation", "{correlation}".into()),
                    ("balance", "{balance}".into()),
                    ("side", "{side}".into()),
                ],
            )
            .into(),
    })
}

#[cfg(test)]
mod integration_tests;
