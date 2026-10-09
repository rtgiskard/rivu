//! Shared, accepted-audio-clocked generative light views. No simulation in paint.
use super::{fractal_beams, fractal_flow};
use crate::{
    analysis::{AnalysisFrame, AnalysisSettings, VisualizerMode},
    config::{Config, RadialSpectrumStyle},
};
use gpui::{
    AnyElement, Bounds, PathBuilder, Pixels, Window, canvas, div, fill, point, prelude::*, px, rgb,
};
use std::{cell::RefCell, rc::Rc, time::Duration};

#[derive(Clone, Copy, Default)]
pub(super) struct Drive {
    pub bands: [f32; 3],
    pub energy: f32,
    pub onset: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct LightSegment {
    pub from: [f32; 2],
    pub to: [f32; 2],
    pub strength: f32,
    pub tone: usize,
}

pub(super) fn fractal_style(style: RadialSpectrumStyle) -> bool {
    matches!(
        style,
        RadialSpectrumStyle::FractalBeams | RadialSpectrumStyle::FractalFlow
    )
}

enum Scene {
    Beams(fractal_beams::Scene),
    Flow(fractal_flow::Scene),
}

impl Scene {
    fn new(style: RadialSpectrumStyle) -> Option<Self> {
        match style {
            RadialSpectrumStyle::FractalBeams => Some(Self::Beams(fractal_beams::Scene::new())),
            RadialSpectrumStyle::FractalFlow => Some(Self::Flow(fractal_flow::Scene::new())),
            _ => None,
        }
    }

    fn reset(&mut self) {
        match self {
            Self::Beams(scene) => scene.reset(),
            Self::Flow(scene) => scene.reset(),
        }
    }

    fn advance(&mut self, dt: f32, drive: Drive) {
        match self {
            Self::Beams(scene) => scene.advance(dt, drive),
            Self::Flow(scene) => scene.advance(dt, drive),
        }
    }

    fn segments(&self) -> &[LightSegment] {
        match self {
            Self::Beams(scene) => scene.segments(),
            Self::Flow(scene) => scene.segments(),
        }
    }
}

#[derive(Clone)]
pub(super) struct FractalVisualizer {
    data: Rc<RefCell<Data>>,
}

struct Data {
    style: RadialSpectrumStyle,
    visible: bool,
    background: u32,
    colors: [u32; 2],
    sensitivity: f32,
    db_range: f32,
    bloom: f32,
    settings: AnalysisSettings,
    source: Option<(u64, u32)>,
    last_time: Option<Duration>,
    drive: Drive,
    previous_energy: f32,
    scene: Option<Scene>,
}

impl FractalVisualizer {
    pub(super) fn new() -> Self {
        let config = Config::default();
        Self {
            data: Rc::new(RefCell::new(Data {
                style: config.radial_spectrum_style,
                visible: true,
                background: config.visual_background.rgb(),
                colors: [
                    config.radial_spectrum_primary_color.rgb(),
                    config.radial_spectrum_secondary_color.rgb(),
                ],
                sensitivity: config.radial_spectrum_sensitivity,
                db_range: config.radial_spectrum_db_range(),
                bloom: config.radial_spectrum_bloom_intensity,
                settings: AnalysisSettings::from(&config),
                source: None,
                last_time: None,
                drive: Drive::default(),
                previous_energy: 0.0,
                scene: Scene::new(config.radial_spectrum_style),
            })),
        }
    }

    pub(super) fn configure(&self, config: &Config) -> bool {
        let mut data = self.data.borrow_mut();
        let settings = AnalysisSettings::from(config);
        let style_changed = data.style != config.radial_spectrum_style;
        let reset = style_changed
            || data.settings.fft_size != settings.fft_size
            || data.settings.window != settings.window;
        let colors = [
            config.radial_spectrum_primary_color.rgb(),
            config.radial_spectrum_secondary_color.rgb(),
        ];
        let changed = reset
            || data.colors != colors
            || data.background != config.visual_background.rgb()
            || data.sensitivity != config.radial_spectrum_sensitivity
            || data.db_range != config.radial_spectrum_db_range()
            || data.bloom != config.radial_spectrum_bloom_intensity;
        data.style = config.radial_spectrum_style;
        data.settings = settings;
        data.colors = colors;
        data.background = config.visual_background.rgb();
        data.sensitivity = config.radial_spectrum_sensitivity;
        data.db_range = config.radial_spectrum_db_range();
        data.bloom = config.radial_spectrum_bloom_intensity;
        if style_changed {
            data.scene = Scene::new(data.style);
        }
        if reset {
            data.reset();
        }
        changed
    }

    pub(super) fn set_visible(&self, visible: bool) {
        let mut data = self.data.borrow_mut();
        if data.visible != visible {
            data.visible = visible;
            data.reset();
        }
    }

    pub(super) fn update(&self, frame: &AnalysisFrame) {
        self.data.borrow_mut().update(frame);
    }

    pub(super) fn view(&self, panel_id: u64) -> AnyElement {
        let data = Rc::clone(&self.data);
        div()
            .id(("fractal-visualizer", panel_id))
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| data.borrow().paint(bounds, window),
                )
                .size_full(),
            )
            .into_any_element()
    }
}

impl Data {
    fn reset(&mut self) {
        self.source = None;
        self.last_time = None;
        self.drive = Drive::default();
        self.previous_energy = 0.0;
        if let Some(scene) = &mut self.scene {
            scene.reset();
        }
    }

    fn segments(&self) -> &[LightSegment] {
        self.scene.as_ref().map(Scene::segments).unwrap_or(&[])
    }

    fn update(&mut self, frame: &AnalysisFrame) {
        if !self.visible || !fractal_style(self.style) {
            return;
        }
        if self.source != Some((frame.generation, frame.sample_rate)) {
            self.reset();
            self.source = Some((frame.generation, frame.sample_rate));
        }
        // Never consume an old mode/window while reconfiguration is in flight.
        if frame.visualizer.mode != VisualizerMode::Generative
            || !frame.settings.is_some_and(|s| {
                s.fft_size == self.settings.fft_size && s.window == self.settings.window
            })
        {
            return;
        }
        if self.last_time == Some(frame.sample_time) {
            return;
        }
        let mut elapsed = self
            .last_time
            .map(|last| frame.sample_time.saturating_sub(last).as_secs_f32())
            .unwrap_or(1.0 / self.settings.fps.max(1) as f32);
        if self.last_time.is_some_and(|last| frame.sample_time < last) || elapsed > 0.5 {
            self.reset();
            self.source = Some((frame.generation, frame.sample_rate));
            elapsed = 1.0 / self.settings.fps.max(1) as f32;
        }
        let target = Drive {
            bands: frame
                .visualizer
                .band_rms
                .map(|v| response(v, self.db_range, self.sensitivity)),
            energy: response(
                ((frame.rms_left * frame.rms_left + frame.rms_right * frame.rms_right) * 0.5)
                    .sqrt(),
                self.db_range,
                self.sensitivity,
            ),
            onset: 0.0,
        };
        let onset = ((target.energy - self.previous_energy).max(0.0) * 3.0).clamp(0.0, 1.0);
        self.previous_energy = target.energy;
        // Flow needs bounded particle-integration steps; beams only deform geometry.
        let step_rate = if self.style == RadialSpectrumStyle::FractalFlow {
            120.0
        } else {
            10.0
        };
        let steps = (elapsed * step_rate).ceil().clamp(1.0, 60.0) as usize;
        let dt = elapsed / steps as f32;
        for step in 0..steps {
            for (value, target) in self.drive.bands.iter_mut().zip(target.bands) {
                *value = smooth(*value, target, dt);
            }
            self.drive.energy = smooth(self.drive.energy, target.energy, dt);
            self.drive.onset = if step == 0 { onset } else { 0.0 };
            if let Some(scene) = &mut self.scene {
                scene.advance(dt, self.drive);
            }
        }
        self.last_time = Some(frame.sample_time);
    }

    fn paint(&self, bounds: Bounds<Pixels>, window: &mut Window) {
        window.paint_quad(fill(bounds, rgb(self.background)));
        if !self.visible || !fractal_style(self.style) {
            return;
        }
        let side = (bounds.size.width / px(1.0)).min(bounds.size.height / px(1.0));
        if side < 24.0 {
            return;
        }
        let center = point(
            bounds.left() + bounds.size.width * 0.5,
            bounds.top() + bounds.size.height * 0.5,
        );
        let scale = side * 0.47;
        // Three color groups and three intensity buckets, independent of branch count.
        // Wide translucent strokes approximate a halo without image feedback buffers.
        for pass in 0..3 {
            if pass < 2 && self.bloom <= 0.0 {
                continue;
            }
            let (width, alpha) = match pass {
                0 => (9.0, 0.045 * self.bloom),
                1 => (4.0, 0.15 * self.bloom),
                _ => (1.0, 0.9),
            };
            let mut paths: [PathBuilder; 9] =
                std::array::from_fn(|_| PathBuilder::stroke(px(width)));
            let mut used = [false; 9];
            for segment in self.segments() {
                if segment.strength < 0.015 {
                    continue;
                }
                let bucket = ((segment.strength * 3.0) as usize).min(2);
                let index = segment.tone * 3 + bucket;
                let path = &mut paths[index];
                path.move_to(point(
                    center.x + px(segment.from[0] * scale),
                    center.y + px(segment.from[1] * scale),
                ));
                path.line_to(point(
                    center.x + px(segment.to[0] * scale),
                    center.y + px(segment.to[1] * scale),
                ));
                used[index] = true;
            }
            for (index, path) in paths.into_iter().enumerate() {
                if used[index] {
                    let color = blend(self.colors[0], self.colors[1], (index / 3) as f32 * 0.5);
                    if let Ok(path) = path.build() {
                        window.paint_path(
                            path,
                            rgb(color).alpha(alpha * ((index % 3) as f32 + 0.5) / 3.0),
                        );
                    }
                }
            }
        }
    }
}

fn response(rms: f32, db_range: f32, sensitivity: f32) -> f32 {
    if !rms.is_finite() || rms <= 0.0 {
        return 0.0;
    }
    let level = ((20.0 * rms.log10() + db_range) / db_range).clamp(0.0, 1.0);
    (level * level * sensitivity).clamp(0.0, 1.0)
}

fn smooth(value: f32, target: f32, dt: f32) -> f32 {
    let tau = if target > value { 0.06 } else { 0.25 };
    target + (value - target) * (-dt / tau).exp()
}

fn blend(from: u32, to: u32, t: f32) -> u32 {
    let channel = |shift: u32| {
        let a = ((from >> shift) & 0xff) as f32;
        let b = ((to >> shift) & 0xff) as f32;
        (a + (b - a) * t).round() as u32
    };
    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::VisualizerFrame;

    fn plot(style: RadialSpectrumStyle) -> FractalVisualizer {
        let plot = FractalVisualizer::new();
        plot.configure(&Config {
            radial_spectrum_style: style,
            ..Config::default()
        });
        plot
    }

    fn frame(time: u64) -> AnalysisFrame {
        AnalysisFrame {
            sample_time: Duration::from_millis(time),
            generation: 1,
            sample_rate: 48000,
            settings: Some(AnalysisSettings::from(&Config::default())),
            rms_left: 0.3,
            rms_right: 0.3,
            visualizer: VisualizerFrame {
                mode: VisualizerMode::Generative,
                band_rms: [0.2; 3],
                ..VisualizerFrame::default()
            },
            ..AnalysisFrame::default()
        }
    }

    #[test]
    fn paused_publications_do_not_advance_and_silence_extinguishes_light() {
        for style in [
            RadialSpectrumStyle::FractalBeams,
            RadialSpectrumStyle::FractalFlow,
        ] {
            let visual = plot(style);
            for t in (50..=1000).step_by(50) {
                visual.update(&frame(t));
            }
            let before = visual.data.borrow().segments().to_vec();
            assert!(before.iter().any(|s| s.strength > 0.05));
            let mut repeated = frame(1000);
            repeated.sequence += 20;
            repeated.visualizer.band_rms = [1.0; 3];
            visual.update(&repeated);
            assert_eq!(visual.data.borrow().segments(), &before);
            let mut silence = frame(1000);
            silence.visualizer.band_rms = [0.0; 3];
            silence.rms_left = 0.0;
            silence.rms_right = 0.0;
            for t in (1050..=11000).step_by(50) {
                silence.sample_time = Duration::from_millis(t);
                visual.update(&silence);
            }
            assert!(
                visual
                    .data
                    .borrow()
                    .segments()
                    .iter()
                    .all(|s| s.strength < 0.015)
            );
        }
    }

    #[test]
    fn discontinuities_and_stale_configuration_never_bridge_scenes() {
        for style in [
            RadialSpectrumStyle::FractalBeams,
            RadialSpectrumStyle::FractalFlow,
        ] {
            let visual = plot(style);
            for t in (50..=1000).step_by(50) {
                visual.update(&frame(t));
            }
            let mut stale = frame(1050);
            stale.generation += 1;
            stale.visualizer.mode = VisualizerMode::StereoDensity;
            visual.update(&stale);
            assert!(
                visual
                    .data
                    .borrow()
                    .segments()
                    .iter()
                    .all(|s| s.strength < 0.015)
            );
            let fresh = plot(style);
            stale.visualizer.mode = VisualizerMode::Generative;
            visual.update(&stale);
            fresh.update(&stale);
            assert_eq!(
                visual.data.borrow().segments(),
                fresh.data.borrow().segments()
            );
            visual.set_visible(false);
            visual.update(&frame(1100));
            assert!(visual.data.borrow().last_time.is_none());
            visual.set_visible(true);
            visual.configure(&Config {
                radial_spectrum_style: style,
                spectrum_fft_size: 4096,
                ..Config::default()
            });
            visual.update(&frame(1150));
            assert!(visual.data.borrow().last_time.is_none());
        }
    }

    #[test]
    fn changing_fractal_style_clears_old_motion_even_with_same_analysis_mode() {
        let visual = plot(RadialSpectrumStyle::FractalBeams);
        for t in (50..=1000).step_by(50) {
            visual.update(&frame(t));
        }
        visual.configure(&Config {
            radial_spectrum_style: RadialSpectrumStyle::FractalFlow,
            ..Config::default()
        });
        assert!(visual.data.borrow().segments().is_empty());
        visual.update(&frame(1000));
        let fresh = plot(RadialSpectrumStyle::FractalFlow);
        fresh.update(&frame(1000));
        assert_eq!(
            visual.data.borrow().segments(),
            fresh.data.borrow().segments()
        );
        visual.configure(&Config {
            radial_spectrum_style: RadialSpectrumStyle::BarsRings,
            ..Config::default()
        });
        assert!(visual.data.borrow().scene.is_none());
    }
}
