//! Owns style dispatch, visibility, and layout retention across concrete renderers.
use crate::{
    VisualLabels, VisualParameters, fractal_renderer::FractalRenderer,
    oscilloscope_renderer::OscilloscopeRenderer, radial_renderer::RadialRenderer,
    stereo_renderer::StereoRenderer,
};
use gpui::{AnyElement, div, prelude::*};
use rivu_audio::analysis::AnalysisFrame;
use rivu_format::config::{AudioVisualizerOptions, AudioVisualizerStyle};
use rivu_protocol::model::PlaybackStatus;
use std::{
    cell::Cell,
    sync::Arc,
    time::{Duration, Instant},
};

const STEREO_STYLES: [AudioVisualizerStyle; 2] = [
    AudioVisualizerStyle::StereoDensity,
    AudioVisualizerStyle::SpatialSpectrum,
];
const FRACTAL_STYLES: [AudioVisualizerStyle; 2] = [
    AudioVisualizerStyle::FractalBeams,
    AudioVisualizerStyle::FractalFlow,
];

pub struct StyleCoordinator {
    pub(super) radial: RadialRenderer,
    stereo: [StereoRenderer; 2],
    fractal: [FractalRenderer; 2],
    oscilloscope: OscilloscopeRenderer,
    // Layout ownership outlives visibility: radial, density, spatial, beams,
    // flow, scope. Keep the six slots inline, without a separate shared owner.
    presentations: Cell<[bool; 6]>,
}

impl Default for StyleCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl StyleCoordinator {
    pub fn new() -> Self {
        Self {
            stereo: STEREO_STYLES.map(StereoRenderer::new),
            fractal: FRACTAL_STYLES.map(FractalRenderer::new),
            oscilloscope: OscilloscopeRenderer::new(),
            radial: RadialRenderer::new(),
            presentations: Cell::new([true; 6]),
        }
    }

    pub fn sync(
        &mut self,
        config: &VisualParameters,
        options: impl IntoIterator<Item = AudioVisualizerOptions>,
        window_visible: bool,
    ) -> bool {
        let mut radial_visible = false;
        let mut stereo_visible = [false; 2];
        let mut fractal_visible = [false; 2];
        let mut oscilloscope_visible = false;
        if window_visible {
            for options in options {
                match options.style {
                    AudioVisualizerStyle::Bars
                    | AudioVisualizerStyle::Rings
                    | AudioVisualizerStyle::BarsRings => radial_visible = true,
                    AudioVisualizerStyle::StereoDensity => stereo_visible[0] = true,
                    AudioVisualizerStyle::SpatialSpectrum => stereo_visible[1] = true,
                    AudioVisualizerStyle::FractalBeams => fractal_visible[0] = true,
                    AudioVisualizerStyle::FractalFlow => fractal_visible[1] = true,
                    AudioVisualizerStyle::Oscilloscope => oscilloscope_visible = true,
                    AudioVisualizerStyle::Waveform
                    | AudioVisualizerStyle::Spectrum
                    | AudioVisualizerStyle::Spectrogram => {}
                }
            }
        }
        let mut changed = self.radial.set_visible(radial_visible);
        for (renderer, visible) in self.stereo.iter().zip(stereo_visible) {
            changed |= renderer.set_visible(visible);
            changed |= renderer.configure(config);
        }
        for (renderer, visible) in self.fractal.iter().zip(fractal_visible) {
            changed |= renderer.set_activity(window_visible, visible);
            changed |= renderer.configure(config);
        }
        changed |= self.oscilloscope.configure(config, oscilloscope_visible);
        changed | self.radial.configure(config)
    }

    pub fn retain_presentations(&self, options: impl IntoIterator<Item = AudioVisualizerOptions>) {
        let mut retained = [false; 6];
        for options in options {
            let index = match options.style {
                AudioVisualizerStyle::Bars
                | AudioVisualizerStyle::Rings
                | AudioVisualizerStyle::BarsRings => 0,
                AudioVisualizerStyle::StereoDensity => 1,
                AudioVisualizerStyle::SpatialSpectrum => 2,
                AudioVisualizerStyle::FractalBeams => 3,
                AudioVisualizerStyle::FractalFlow => 4,
                AudioVisualizerStyle::Oscilloscope => 5,
                AudioVisualizerStyle::Waveform
                | AudioVisualizerStyle::Spectrum
                | AudioVisualizerStyle::Spectrogram => continue,
            };
            retained[index] = true;
        }
        let previous = self.presentations.replace(retained);
        let removed: [bool; 6] = std::array::from_fn(|index| previous[index] && !retained[index]);
        if removed[0] {
            self.radial.clear();
        }
        for (renderer, removed) in self.stereo.iter().zip(&removed[1..3]) {
            if *removed {
                renderer.clear();
            }
        }
        for (renderer, removed) in self.fractal.iter().zip(&removed[3..5]) {
            if *removed {
                renderer.clear();
            }
        }
        if removed[5] {
            self.oscilloscope.clear();
        }
    }

    /// Freeze bounded simulation state until replacement audio reanchors it.
    pub fn suspend(&self) {
        self.radial.suspend();
        for renderer in &self.fractal {
            renderer.suspend();
        }
    }

    pub fn clear(&self) {
        self.radial.clear();
        self.clear_non_radial();
    }

    fn clear_non_radial(&self) {
        for renderer in &self.stereo {
            renderer.clear();
        }
        for renderer in &self.fractal {
            renderer.clear();
        }
        self.oscilloscope.clear();
    }

    /// Stop retires input immediately; only bounded radial drawing survives.
    pub fn stop(&self, now: Instant, fade_when_idle: bool) {
        self.radial.stop(now, fade_when_idle);
        self.clear_non_radial();
    }

    pub fn retire_idle_fade(&self) {
        self.radial.retire_idle_fade();
    }

    pub fn advance_idle_fade(&self, now: Instant) {
        self.radial.advance_idle_fade(now);
    }

    pub fn update(&mut self, frame: &AnalysisFrame, presentation_time: Duration) {
        if frame.settings.is_none() {
            self.clear();
            return;
        }
        for renderer in &self.stereo {
            renderer.update(frame);
        }
        for renderer in &self.fractal {
            renderer.update(frame, presentation_time);
        }
        self.oscilloscope.update(frame);
        self.radial.update(frame, presentation_time);
    }

    pub fn animate(&mut self, presentation_time: Duration) -> bool {
        let mut changed = false;
        for renderer in &self.fractal {
            changed |= renderer.animate(presentation_time);
        }
        changed | self.radial.animate(presentation_time)
    }

    pub fn begin_idle_fade(&self, now: Instant) {
        self.radial.begin_idle_fade(now);
    }

    pub fn idle_fading(&self, now: Instant) -> bool {
        self.radial.idle_fading(now)
    }

    pub fn view(
        &self,
        i18n: &Arc<VisualLabels>,
        panel_id: u64,
        status: PlaybackStatus,
        options: AudioVisualizerOptions,
    ) -> AnyElement {
        match options.style {
            AudioVisualizerStyle::StereoDensity => self.stereo[0].view(i18n, panel_id, options),
            AudioVisualizerStyle::SpatialSpectrum => self.stereo[1].view(i18n, panel_id, options),
            AudioVisualizerStyle::FractalBeams => self.fractal[0].view(i18n, panel_id, options),
            AudioVisualizerStyle::FractalFlow => self.fractal[1].view(i18n, panel_id, options),
            AudioVisualizerStyle::Oscilloscope => self.oscilloscope.view(i18n, panel_id, options),
            AudioVisualizerStyle::Bars
            | AudioVisualizerStyle::Rings
            | AudioVisualizerStyle::BarsRings => self.radial.view(i18n, panel_id, status, options),
            AudioVisualizerStyle::Waveform
            | AudioVisualizerStyle::Spectrum
            | AudioVisualizerStyle::Spectrogram => div().into_any_element(),
        }
    }
}
