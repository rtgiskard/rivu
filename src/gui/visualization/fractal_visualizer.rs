//! Shared generative light views with accepted targets and display-clocked simulation.
use super::{fractal_beams, fractal_flow};
use crate::config::{AudioVisualizerOptions, AudioVisualizerStyle, Config};
use crate::i18n::Localizer;
use gpui::{
    AnyElement, App, Bounds, GpuFrame, GpuLimits, GpuPalette, GpuSegment, GpuSource,
    GpuStrokeLayer, Pixels, TextAlign, Window, canvas, div, fill, point, prelude::*, px, rgb, size,
};
use rivu_audio::analysis::{AnalysisFrame, AnalysisSettings};
use std::{cell::RefCell, rc::Rc, sync::Arc, time::Duration};

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

pub(super) fn fractal_style(style: AudioVisualizerStyle) -> bool {
    matches!(
        style,
        AudioVisualizerStyle::FractalBeams | AudioVisualizerStyle::FractalFlow
    )
}

enum Scene {
    Beams(fractal_beams::Scene),
    Flow(fractal_flow::Scene),
}

impl Scene {
    fn new(style: AudioVisualizerStyle) -> Option<Self> {
        match style {
            AudioVisualizerStyle::FractalBeams => Some(Self::Beams(fractal_beams::Scene::new())),
            AudioVisualizerStyle::FractalFlow => Some(Self::Flow(fractal_flow::Scene::new())),
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
    guide_labels: Option<[gpui::SharedString; 7]>,
    style: AudioVisualizerStyle,
    visible: bool,
    background: u32,
    colors: [u32; 2],
    sensitivity: f32,
    db_range: f32,
    bloom: f32,
    settings: AnalysisSettings,
    source: Option<(u64, u32)>,
    last_time: Option<Duration>,
    last_presentation_time: Option<Duration>,
    target: Drive,
    pending_onset: f32,
    drive: Drive,
    previous_energy: f32,
    scene: Option<Scene>,
    gpu_frame: Option<Arc<GpuFrame>>,
    gpu_palette: Option<Arc<GpuPalette>>,
    gpu_segments: Vec<GpuSegment>,
    gpu_error_reported: bool,
    retained: bool,
}

impl FractalVisualizer {
    pub(super) fn new(style: AudioVisualizerStyle) -> Self {
        assert!(fractal_style(style));
        let config = Config::default();
        Self {
            data: Rc::new(RefCell::new(Data {
                guide_labels: None,
                style,
                visible: true,
                background: config.visual_background.rgb(),
                colors: [
                    config.audio_visualizer_primary_color.rgb(),
                    config.audio_visualizer_secondary_color.rgb(),
                ],
                sensitivity: config.audio_visualizer_sensitivity,
                db_range: config.audio_visualizer_db_range(),
                bloom: config.audio_visualizer_bloom_intensity,
                settings: AnalysisSettings::from(&config),
                source: None,
                last_time: None,
                last_presentation_time: None,
                target: Drive::default(),
                pending_onset: 0.0,
                drive: Drive::default(),
                previous_energy: 0.0,
                scene: None,
                gpu_frame: None,
                gpu_palette: None,
                gpu_segments: Vec::new(),
                gpu_error_reported: false,
                retained: false,
            })),
        }
    }

    pub(super) fn configure(&self, config: &Config) -> bool {
        let mut data = self.data.borrow_mut();
        let settings = AnalysisSettings::from(config);
        let reset = data.settings.fft_size != settings.fft_size
            || data.settings.window != settings.window
            || data.settings.signal != settings.signal;
        let colors = [
            config.audio_visualizer_primary_color.rgb(),
            config.audio_visualizer_secondary_color.rgb(),
        ];
        let appearance_changed =
            data.colors != colors || data.bloom != config.audio_visualizer_bloom_intensity;
        let changed = reset
            || appearance_changed
            || data.background != config.visual_background.rgb()
            || data.sensitivity != config.audio_visualizer_sensitivity
            || data.db_range != config.audio_visualizer_db_range();
        if appearance_changed {
            data.gpu_palette = None;
        }
        data.settings = settings;
        data.colors = colors;
        data.background = config.visual_background.rgb();
        data.sensitivity = config.audio_visualizer_sensitivity;
        data.db_range = config.audio_visualizer_db_range();
        data.bloom = config.audio_visualizer_bloom_intensity;
        if reset {
            data.reset();
        } else if appearance_changed {
            // Background only affects paint; response settings apply to future audio.
            data.publish();
        }
        changed
    }

    pub(super) fn set_visible(&self, visible: bool) -> bool {
        let mut data = self.data.borrow_mut();
        let restored = visible && !data.visible;
        if data.visible != visible {
            data.visible = visible;
            if !visible {
                data.retire();
            }
        }
        restored
    }

    pub(super) fn clear(&self) {
        self.data.borrow_mut().reset();
    }

    pub(super) fn update(&self, frame: &AnalysisFrame, presentation_time: Duration) {
        self.data.borrow_mut().update(frame, presentation_time);
    }

    pub(super) fn animate(&self, presentation_time: Duration) -> bool {
        self.data.borrow_mut().animate(presentation_time)
    }

    pub(super) fn view(
        &self,
        i18n: &Arc<Localizer>,
        panel_id: u64,
        options: AudioVisualizerOptions,
    ) -> AnyElement {
        let data = Rc::clone(&self.data);
        self.data.borrow_mut().guide_labels.get_or_insert_with(|| {
            [
                i18n.text("visual-scene"),
                "x −1",
                "x 0",
                "x +1",
                "y −1",
                "y 0",
                "y +1",
            ]
            .map(|text| text.to_owned().into())
        });
        div()
            .id(("fractal-visualizer", panel_id))
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, cx| data.borrow().paint(bounds, options, window, cx),
                )
                .size_full(),
            )
            .into_any_element()
    }
}

impl Data {
    fn retire(&mut self) {
        self.scene = None;
        self.gpu_segments = Vec::new();
        self.gpu_palette = None;
        self.last_time = None;
        self.last_presentation_time = None;
        self.target = Drive::default();
        self.pending_onset = 0.0;
        self.drive = Drive::default();
        self.previous_energy = 0.0;
        self.retained = true;
    }

    fn reset(&mut self) {
        self.gpu_frame = None;
        self.retained = false;
        self.gpu_segments.clear();
        self.source = None;
        self.last_time = None;
        self.last_presentation_time = None;
        self.target = Drive::default();
        self.pending_onset = 0.0;
        self.drive = Drive::default();
        self.previous_energy = 0.0;
        if let Some(scene) = &mut self.scene {
            scene.reset();
        }
    }

    #[cfg(test)]
    fn segments(&self) -> &[LightSegment] {
        self.scene.as_ref().map(Scene::segments).unwrap_or(&[])
    }

    fn update(&mut self, frame: &AnalysisFrame, presentation_time: Duration) {
        if !self.visible {
            return;
        }
        // Reject stale settings and undemanded payloads before any future tick
        // can animate a previous source's drive.
        if !frame.visualizer.demand.band_energy
            || !frame.settings.is_some_and(|s| {
                s.fft_size == self.settings.fft_size
                    && s.window == self.settings.window
                    && s.signal == self.settings.signal
            })
        {
            if !self.retained
                || frame.settings != Some(self.settings)
                || self.source != Some((frame.generation, frame.sample_rate))
            {
                self.reset();
            }
            return;
        }
        if self.source != Some((frame.generation, frame.sample_rate))
            || self.last_time.is_some_and(|last| {
                frame.sample_time < last
                    || frame.sample_time.saturating_sub(last) > Duration::from_millis(500)
            })
        {
            self.reset();
            self.source = Some((frame.generation, frame.sample_rate));
        }
        if self.last_time == Some(frame.sample_time) {
            return;
        }
        if self.scene.is_none() {
            self.scene = Scene::new(self.style);
        }
        self.target = Drive {
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
        let onset = ((self.target.energy - self.previous_energy).max(0.0) * 3.0).clamp(0.0, 1.0);
        self.pending_onset = self.pending_onset.max(onset);
        self.previous_energy = self.target.energy;
        self.last_time = Some(frame.sample_time);
        if self.last_presentation_time.is_none() {
            // Seed visible geometry immediately, independent of analysis FPS.
            // All subsequent simulation belongs exclusively to native ticks.
            self.last_presentation_time = Some(presentation_time);
            self.advance(1.0 / 60.0);
        }
    }

    fn animate(&mut self, presentation_time: Duration) -> bool {
        if !self.visible || self.last_time.is_none() {
            return false;
        }
        let Some(last) = self.last_presentation_time else {
            return false;
        };
        if presentation_time <= last {
            return false;
        }
        // The display owner bounds this clock to accepted audio. Bound work as
        // well when a native callback is delayed; do not reset a held drive.
        let elapsed = presentation_time
            .saturating_sub(last)
            .as_secs_f32()
            .min(0.5);
        self.last_presentation_time = Some(presentation_time);
        let had_light = self
            .scene
            .as_ref()
            .is_some_and(|scene| !scene.segments().is_empty());
        self.advance(elapsed);
        had_light
            || self
                .scene
                .as_ref()
                .is_some_and(|scene| !scene.segments().is_empty())
    }

    fn advance(&mut self, elapsed: f32) {
        let step_rate = if self.style == AudioVisualizerStyle::FractalFlow {
            120.0
        } else {
            10.0
        };
        let steps = (elapsed * step_rate).ceil().clamp(1.0, 60.0) as usize;
        let dt = elapsed / steps as f32;
        let onset = std::mem::take(&mut self.pending_onset);
        for step in 0..steps {
            for (value, target) in self.drive.bands.iter_mut().zip(self.target.bands) {
                *value = smooth(*value, target, dt);
            }
            self.drive.energy = smooth(self.drive.energy, self.target.energy, dt);
            self.drive.onset = if step == 0 { onset } else { 0.0 };
            if let Some(scene) = &mut self.scene {
                scene.advance(dt, self.drive);
            }
        }
        self.drive.onset = 0.0;
        match &mut self.scene {
            Some(Scene::Beams(scene)) => scene.rebuild_segments(),
            Some(Scene::Flow(scene)) => scene.rebuild_segments(),
            None => {}
        }
        self.publish();
    }

    fn publish(&mut self) {
        if !self.visible || self.last_time.is_none() || self.scene.is_none() {
            if !self.retained {
                self.gpu_frame = None;
            }
            return;
        }
        let result = self.publication();
        match result {
            Ok(frame) => {
                self.gpu_frame = Some(frame);
                self.retained = false;
                self.gpu_error_reported = false;
            }
            Err(error) => {
                if !self.retained {
                    self.gpu_frame = None;
                }
                if !self.gpu_error_reported {
                    eprintln!("rivu fractal GPU publication: {error:#}");
                    self.gpu_error_reported = true;
                }
            }
        }
    }

    fn publication(&mut self) -> anyhow::Result<Arc<GpuFrame>> {
        // Flow has at most 96 * 23 lines; Beams has 3 * 63 * 4.
        const MAX_SEGMENTS: usize = 96 * 23;
        let palette = if let Some(palette) = &self.gpu_palette {
            palette.clone()
        } else {
            let colors: [[f32; 4]; 27] = std::array::from_fn(|index| {
                let pass = index / 9;
                let group = index % 9;
                let mut color = blend(self.colors[0], self.colors[1], (group / 3) as f32 * 0.5);
                if self.style != AudioVisualizerStyle::FractalFlow && pass == 2 {
                    color = blend(color, 0xffffff, [0.0, 0.28, 0.62][group % 3]);
                }
                let alpha = [0.045 * self.bloom, 0.15 * self.bloom, 0.9][pass]
                    * ((group % 3) as f32 + 0.5)
                    / 3.0;
                [
                    ((color >> 16) & 255) as f32 / 255.0,
                    ((color >> 8) & 255) as f32 / 255.0,
                    (color & 255) as f32 / 255.0,
                    alpha,
                ]
            });
            let palette = GpuPalette::new(&colors, 27)?;
            self.gpu_palette = Some(palette.clone());
            palette
        };
        let segments = self.scene.as_ref().expect("published scene").segments();
        let group = |s: &LightSegment| s.tone * 3 + ((s.strength * 3.0) as usize).min(2);
        let mut counts = [0usize; 9];
        for segment in segments.iter().filter(|s| s.strength >= 0.015) {
            counts[group(segment)] += 1;
        }
        let count: usize = counts.iter().sum();
        anyhow::ensure!(count <= MAX_SEGMENTS, "Fractal segment capacity exceeded");
        self.gpu_segments.resize(
            count,
            GpuSegment {
                endpoints: [0.0; 4],
                color: 0,
            },
        );
        let mut offsets = [0usize; 9];
        for index in 1..9 {
            offsets[index] = offsets[index - 1] + counts[index - 1];
        }
        for segment in segments.iter().filter(|s| s.strength >= 0.015) {
            let index = group(segment);
            self.gpu_segments[offsets[index]] = GpuSegment {
                endpoints: [
                    segment.from[0],
                    segment.from[1],
                    segment.to[0],
                    segment.to[1],
                ]
                .map(|v| 0.5 + v * 0.47),
                color: index as u32,
            };
            offsets[index] += 1;
        }
        let layers = [
            GpuStrokeLayer {
                width: 9.0,
                palette_offset: 0,
            },
            GpuStrokeLayer {
                width: 4.0,
                palette_offset: 9,
            },
            GpuStrokeLayer {
                width: 1.0,
                palette_offset: 18,
            },
        ];
        GpuFrame::segments(
            &self.gpu_segments,
            if self.bloom > 0.0 {
                &layers
            } else {
                &layers[2..]
            },
            palette,
            GpuLimits {
                max_segments: MAX_SEGMENTS,
                max_stroke_layers: 3,
                max_palette_colors: 27,
                ..Default::default()
            },
        )
    }

    fn paint(
        &self,
        bounds: Bounds<Pixels>,
        options: AudioVisualizerOptions,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.paint_quad(fill(bounds, rgb(self.background)));
        if !self.visible {
            return;
        }
        let side = (bounds.size.width / px(1.0)).min(bounds.size.height / px(1.0));
        if side < 24.0 {
            return;
        }
        if let Some(frame) = &self.gpu_frame {
            let scene_bounds = Bounds::new(
                point(
                    bounds.left() + (bounds.size.width - px(side)) * 0.5,
                    bounds.top() + (bounds.size.height - px(side)) * 0.5,
                ),
                size(px(side), px(side)),
            );
            if let Err(error) = window.paint_gpu(scene_bounds, GpuSource::Fixed(frame.clone())) {
                eprintln!("rivu fractal GPU paint: {error:#}");
            }
        }
        paint_scene_guides(
            self.guide_labels.as_ref().expect("localized guide labels"),
            bounds,
            options,
            self.background,
            window,
            cx,
        );
    }
}

// Both simulations use centered scene coordinates with +y down. These fixed
// spatial references describe the drawing, never audio level or frequency.
fn paint_scene_guides(
    labels: &[gpui::SharedString; 7],
    bounds: Bounds<Pixels>,
    options: AudioVisualizerOptions,
    background: u32,
    window: &mut Window,
    cx: &mut App,
) {
    if !options.reference_lines && !options.labels {
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
    let scale = px(side * 0.47);
    if options.reference_lines {
        for coordinate in [-1.0, -0.5, 0.0, 0.5, 1.0] {
            let color = rgb(0xb1bbc5).alpha(if coordinate == 0.0 { 0.30 } else { 0.14 });
            window.paint_quad(fill(
                Bounds::new(
                    point(center.x + scale * coordinate, center.y - scale),
                    size(px(1.0), scale * 2.0),
                ),
                color,
            ));
            window.paint_quad(fill(
                Bounds::new(
                    point(center.x - scale, center.y + scale * coordinate),
                    size(scale * 2.0, px(1.0)),
                ),
                color,
            ));
        }
    }
    if !options.labels || side < 180.0 {
        return;
    }
    // Static text and the exact same transform as the scene; no per-frame format.
    for (text, x, y, align) in [
        (labels[0].clone(), 0.0, -1.0, 0.5),
        (labels[1].clone(), -1.0, 0.0, 0.0),
        (labels[2].clone(), 0.0, 0.0, 0.5),
        (labels[3].clone(), 1.0, 0.0, 1.0),
        (labels[4].clone(), -1.0, -1.0, 0.0),
        (labels[5].clone(), -1.0, 0.0, 0.0),
        (labels[6].clone(), -1.0, 1.0, 0.0),
    ] {
        let mut style = window.text_style();
        style.color = rgb(0xb1bbc5).into();
        let run = style.to_run(text.len());
        let line = window
            .text_system()
            .shape_line(text.clone(), px(10.0), &[run], None);
        let label_y = if text.starts_with('x') {
            center.y + px(3.0)
        } else if text.starts_with('y') {
            (center.y + scale * y - px(15.0)).max(bounds.top() + px(18.0))
        } else {
            center.y - scale
        };
        let origin = point(center.x + scale * x - line.width() * align, label_y);
        if origin.x < bounds.left() + px(2.0)
            || origin.x + line.width() > bounds.right() - px(2.0)
            || origin.y + px(14.0) > bounds.bottom() - px(2.0)
        {
            continue;
        }
        window.paint_quad(fill(
            Bounds::new(
                point(origin.x - px(2.0), origin.y),
                size(line.width() + px(4.0), px(14.0)),
            ),
            rgb(background).alpha(0.85),
        ));
        if let Err(error) = line.paint(origin, px(14.0), TextAlign::Left, None, window, cx) {
            eprintln!("rivu scene coordinate label: {error}");
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
    fn accept(visual: &FractalVisualizer, frame: &AnalysisFrame) {
        visual.update(frame, frame.sample_time);
        visual.animate(frame.sample_time);
    }

    #[test]
    fn five_hz_inputs_keep_native_motion_without_repeating_onsets() {
        for style in [
            AudioVisualizerStyle::FractalBeams,
            AudioVisualizerStyle::FractalFlow,
        ] {
            let visual = plot(style);
            let reference = plot(style);
            let start = Duration::from_millis(1000);
            let first = frame(1000);
            visual.update(&first, start);
            reference.update(&first, start);
            assert!(
                visual
                    .data
                    .borrow()
                    .segments()
                    .iter()
                    .any(|s| s.strength > 0.001)
            );
            let mut next = frame(1200);
            next.rms_left = 0.7;
            next.rms_right = 0.7;
            visual.update(&next, start);
            reference.update(&next, start);
            let energy = visual.data.borrow().previous_energy;
            for tick in 1..=12 {
                let now = start + Duration::from_micros(16_000 * tick);
                let before = visual.data.borrow().segments().to_vec();
                assert!(visual.animate(now));
                assert!(reference.animate(now));
                let snapshot = publication(&visual);
                assert_ne!(visual.data.borrow().segments(), before);
                assert_eq!(
                    visual.data.borrow().segments(),
                    reference.data.borrow().segments()
                );
                assert!(!visual.animate(now));
                assert!(Arc::ptr_eq(&snapshot, &publication(&visual)));
                assert_eq!(visual.data.borrow().last_time, Some(next.sample_time));
                assert_eq!(visual.data.borrow().pending_onset, 0.0);
                assert_eq!(visual.data.borrow().drive.onset, 0.0);
                // A material refresh and repeated accepted payload must neither
                // reset motion nor treat changed payload values as another onset.
                if tick == 4 {
                    visual.configure(&Config {
                        audio_visualizer_primary_color: "#ff0000".parse().unwrap(),
                        ..Config::default()
                    });
                    next.rms_left = 1.0;
                    next.rms_right = 1.0;
                    visual.update(&next, now);
                    assert_eq!(visual.data.borrow().previous_energy, energy);
                    assert_eq!(visual.data.borrow().pending_onset, 0.0);
                }
            }
        }
    }

    #[test]
    fn invalid_and_hidden_scenes_cannot_animate_previous_drive() {
        for style in [
            AudioVisualizerStyle::FractalBeams,
            AudioVisualizerStyle::FractalFlow,
        ] {
            for invalid in 0..3 {
                let visual = plot(style);
                let mut audio = frame(1000);
                visual.update(&audio, audio.sample_time);
                match invalid {
                    0 => audio.visualizer.demand.band_energy = false,
                    1 => audio.settings = None,
                    _ => {
                        visual.set_visible(false);
                    }
                }
                visual.update(&audio, audio.sample_time);
                assert!(!visual.animate(audio.sample_time + Duration::from_millis(16)));
                assert_eq!(visual.data.borrow().gpu_frame.is_some(), invalid == 2);
                assert!(visual.data.borrow().last_time.is_none());
            }
        }
    }
    use rivu_audio::analysis::{VisualizerDemand, VisualizerFrame};

    fn plot(style: AudioVisualizerStyle) -> FractalVisualizer {
        let plot = FractalVisualizer::new(style);
        plot.configure(&Config::default());
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
                demand: VisualizerDemand {
                    band_energy: true,
                    ..VisualizerDemand::default()
                },
                band_rms: [0.2; 3],
                ..VisualizerFrame::default()
            },
            ..AnalysisFrame::default()
        }
    }

    fn publication(visual: &FractalVisualizer) -> Arc<GpuFrame> {
        Arc::clone(visual.data.borrow().gpu_frame.as_ref().unwrap())
    }

    fn published_segments(frame: &GpuFrame) -> Vec<([f32; 4], u32)> {
        let gpui::GpuContent::Segments { segments, .. } = frame.content() else {
            panic!("expected fractal segments");
        };
        segments.iter().map(|s| (s.endpoints, s.color)).collect()
    }

    #[test]
    fn response_and_background_refresh_do_not_inject_audio_or_animation() {
        for style in [
            AudioVisualizerStyle::FractalBeams,
            AudioVisualizerStyle::FractalFlow,
        ] {
            for (config, changes_drive) in [
                (
                    Config {
                        visual_background: "#123456".parse().unwrap(),
                        ..Config::default()
                    },
                    false,
                ),
                (
                    Config {
                        audio_visualizer_sensitivity: 0.25,
                        ..Config::default()
                    },
                    true,
                ),
                (
                    Config {
                        audio_visualizer_db_range: Some(40.0),
                        ..Config::default()
                    },
                    true,
                ),
            ] {
                let visual = plot(style);
                let reference = plot(style);
                for t in (50..=1000).step_by(50) {
                    accept(&visual, &frame(t));
                    accept(&reference, &frame(t));
                }
                let before = publication(&visual);
                let geometry = visual.data.borrow().segments().to_vec();
                let drive = visual.data.borrow().drive;
                let previous_energy = visual.data.borrow().previous_energy;
                assert!(visual.configure(&config));
                assert!(!visual.configure(&config));
                let mut paused = frame(1000);
                paused.sequence += 1;
                paused.visualizer.band_rms = [1.0; 3];
                accept(&visual, &paused);
                assert!(Arc::ptr_eq(&before, &publication(&visual)));
                {
                    let data = visual.data.borrow();
                    assert_eq!(data.background, config.visual_background.rgb());
                    assert_eq!(data.segments(), geometry);
                    assert_eq!(data.last_time, Some(paused.sample_time));
                    assert_eq!(data.drive.bands, drive.bands);
                    assert_eq!(data.drive.energy, drive.energy);
                    assert_eq!(data.drive.onset, drive.onset);
                    assert_eq!(data.previous_energy, previous_energy);
                }
                accept(&visual, &frame(1050));
                accept(&reference, &frame(1050));
                let after = publication(&visual);
                assert!(!Arc::ptr_eq(&before, &after));
                assert!(Arc::ptr_eq(before.palette(), after.palette()));
                if changes_drive {
                    assert!(
                        visual.data.borrow().drive.energy < reference.data.borrow().drive.energy
                    );
                    assert_ne!(
                        visual.data.borrow().segments(),
                        reference.data.borrow().segments()
                    );
                } else {
                    assert_eq!(
                        visual.data.borrow().segments(),
                        reference.data.borrow().segments()
                    );
                }
            }
        }
    }

    #[test]
    fn color_and_bloom_configuration_republishes_material_without_advancing_audio() {
        for style in [
            AudioVisualizerStyle::FractalBeams,
            AudioVisualizerStyle::FractalFlow,
        ] {
            let visual = plot(style);
            for t in (50..=1000).step_by(50) {
                accept(&visual, &frame(t));
            }
            let geometry = visual.data.borrow().segments().to_vec();
            let drive = visual.data.borrow().drive;
            let mut config = Config::default();
            for change in 0..4 {
                let before = publication(&visual);
                match change {
                    0 => config.audio_visualizer_primary_color = "#ff0000".parse().unwrap(),
                    1 => config.audio_visualizer_secondary_color = "#0000ff".parse().unwrap(),
                    2 => config.audio_visualizer_bloom_intensity = 0.9,
                    _ => config.audio_visualizer_bloom_intensity = 0.0,
                }
                assert!(visual.configure(&config));
                let after = publication(&visual);
                assert!(!Arc::ptr_eq(&before, &after));
                assert!(!Arc::ptr_eq(before.palette(), after.palette()));
                assert_ne!(before.palette().colors(), after.palette().colors());
                assert_eq!(published_segments(&before), published_segments(&after));
                if change >= 2 {
                    let gpui::GpuContent::Segments { layers, .. } = after.content() else {
                        panic!("expected fractal segments");
                    };
                    if config.audio_visualizer_bloom_intensity == 0.0 {
                        assert_eq!(layers.len(), 1);
                        assert_eq!(layers[0].width, 1.0);
                        assert_eq!(layers[0].palette_offset, 18);
                        assert!(after.palette().colors()[..18].iter().all(|c| c[3] == 0.0));
                    } else {
                        assert_eq!(layers.len(), 3);
                        assert!(after.palette().colors()[0][3] > before.palette().colors()[0][3]);
                    }
                }
                assert!(!visual.configure(&config));
                accept(&visual, &frame(1000));
                assert!(Arc::ptr_eq(&after, &publication(&visual)));
                let data = visual.data.borrow();
                assert_eq!(data.segments(), geometry);
                assert_eq!(data.last_time, Some(Duration::from_millis(1000)));
                assert_eq!(data.drive.bands, drive.bands);
                assert_eq!(data.drive.energy, drive.energy);
                assert_eq!(data.drive.onset, drive.onset);
            }
        }
    }

    #[test]
    fn fft_and_window_reset_drop_publication_until_matching_audio_is_accepted() {
        for style in [
            AudioVisualizerStyle::FractalBeams,
            AudioVisualizerStyle::FractalFlow,
        ] {
            for mut config in [
                Config {
                    analysis_fft_size: 4096,
                    ..Config::default()
                },
                Config {
                    analysis_window: crate::config::AnalysisWindow::Hann,
                    ..Config::default()
                },
            ] {
                let visual = plot(style);
                accept(&visual, &frame(50));
                let old = publication(&visual);
                config.audio_visualizer_primary_color = "#ff0000".parse().unwrap();
                assert!(visual.configure(&config));
                assert!(visual.data.borrow().gpu_frame.is_none());
                assert!(visual.data.borrow().segments().is_empty());
                accept(&visual, &frame(50));
                assert!(visual.data.borrow().last_time.is_none());
                config.audio_visualizer_secondary_color = "#0000ff".parse().unwrap();
                assert!(visual.configure(&config));
                assert!(visual.data.borrow().gpu_frame.is_none());
                let mut accepted = frame(50);
                accepted.settings = Some(AnalysisSettings::from(&config));
                accept(&visual, &accepted);
                let fresh = plot(style);
                fresh.configure(&config);
                accept(&fresh, &accepted);
                assert!(!Arc::ptr_eq(&old, &publication(&visual)));
                assert_eq!(
                    visual.data.borrow().segments(),
                    fresh.data.borrow().segments()
                );
                assert_eq!(
                    published_segments(&publication(&visual)),
                    published_segments(&publication(&fresh))
                );
            }
        }
    }

    #[test]
    fn flow_refresh_preserves_state_and_discontinuities_reset() {
        let visual = plot(AudioVisualizerStyle::FractalFlow);
        accept(&visual, &frame(50));
        // 500 ms requires all 60 bounded integration steps, but one publication.
        accept(&visual, &frame(550));
        let before = visual.data.borrow().segments().to_vec();
        let accepted_time = visual.data.borrow().last_time;
        let config = Config {
            audio_visualizer_primary_color: "#ff0000".parse().unwrap(),
            ..Config::default()
        };
        assert!(visual.configure(&config));
        accept(&visual, &frame(550));
        assert_eq!(visual.data.borrow().last_time, accepted_time);
        assert_eq!(visual.data.borrow().segments(), before);
        let mut gap = frame(1100);
        accept(&visual, &gap);
        let fresh = plot(AudioVisualizerStyle::FractalFlow);
        fresh.configure(&config);
        accept(&fresh, &gap);
        assert_eq!(
            visual.data.borrow().segments(),
            fresh.data.borrow().segments()
        );
        gap.sample_time = Duration::from_millis(100);
        accept(&visual, &gap);
        let rewound = plot(AudioVisualizerStyle::FractalFlow);
        rewound.configure(&config);
        accept(&rewound, &gap);
        assert_eq!(
            visual.data.borrow().segments(),
            rewound.data.borrow().segments()
        );
    }

    #[test]
    fn signal_switch_restarts_scene_and_rejects_old_drive() {
        for style in [
            AudioVisualizerStyle::FractalBeams,
            AudioVisualizerStyle::FractalFlow,
        ] {
            let visual = plot(style);
            accept(&visual, &frame(50));
            accept(&visual, &frame(100));
            let config = Config {
                visualization_signal: crate::config::VisualizationSignal::Before,
                ..Config::default()
            };
            visual.configure(&config);
            accept(&visual, &frame(150));
            assert_eq!(visual.data.borrow().last_time, None);
            assert!(visual.data.borrow().gpu_frame.is_none());
            let mut selected = frame(200);
            selected.settings.as_mut().unwrap().signal = crate::config::VisualizationSignal::Before;
            accept(&visual, &selected);
            let fresh = plot(style);
            fresh.configure(&config);
            accept(&fresh, &selected);
            assert_eq!(visual.data.borrow().last_time, Some(selected.sample_time));
            assert_eq!(
                visual.data.borrow().segments(),
                fresh.data.borrow().segments()
            );
        }
    }

    #[test]
    fn paused_publications_do_not_advance_and_silence_extinguishes_light() {
        for style in [
            AudioVisualizerStyle::FractalBeams,
            AudioVisualizerStyle::FractalFlow,
        ] {
            let visual = plot(style);
            for t in (50..=1000).step_by(50) {
                accept(&visual, &frame(t));
            }
            let before = visual.data.borrow().segments().to_vec();
            assert!(before.iter().any(|s| s.strength > 0.05));
            let mut repeated = frame(1000);
            repeated.sequence += 20;
            repeated.visualizer.band_rms = [1.0; 3];
            accept(&visual, &repeated);
            assert_eq!(visual.data.borrow().segments(), &before);
            let mut silence = frame(1000);
            silence.visualizer.band_rms = [0.0; 3];
            silence.rms_left = 0.0;
            silence.rms_right = 0.0;
            for t in (1050..=11000).step_by(50) {
                silence.sample_time = Duration::from_millis(t);
                accept(&visual, &silence);
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
    fn generation_rate_rewind_and_gap_restart_from_fresh_audio_state() {
        for style in [
            AudioVisualizerStyle::FractalBeams,
            AudioVisualizerStyle::FractalFlow,
        ] {
            for discontinuity in 0..4 {
                let visual = plot(style);
                for t in (50..=1000).step_by(50) {
                    accept(&visual, &frame(t));
                }
                let old = publication(&visual);
                let mut next = frame(1000);
                match discontinuity {
                    0 => next.generation += 1,
                    1 => next.sample_rate = 44100,
                    2 => next.sample_time = Duration::from_millis(950),
                    _ => next.sample_time = Duration::from_millis(1550),
                }
                if discontinuity < 2 {
                    let mut undemanded = next.clone();
                    undemanded.visualizer.demand.band_energy = false;
                    accept(&visual, &undemanded);
                    assert!(visual.data.borrow().gpu_frame.is_none());
                    assert!(visual.data.borrow().segments().is_empty());
                    // Material changes cannot resurrect a reset scene before audio arrives.
                    visual.configure(&Config {
                        audio_visualizer_bloom_intensity: 0.9,
                        ..Config::default()
                    });
                    assert!(visual.data.borrow().gpu_frame.is_none());
                    visual.configure(&Config::default());
                }
                accept(&visual, &next);
                let fresh = plot(style);
                accept(&fresh, &next);
                assert!(!Arc::ptr_eq(&old, &publication(&visual)));
                assert_eq!(
                    visual.data.borrow().segments(),
                    fresh.data.borrow().segments()
                );
                assert_eq!(
                    published_segments(&publication(&visual)),
                    published_segments(&publication(&fresh))
                );
            }
        }
    }

    #[test]
    fn discontinuities_and_stale_configuration_never_bridge_scenes() {
        for style in [
            AudioVisualizerStyle::FractalBeams,
            AudioVisualizerStyle::FractalFlow,
        ] {
            let visual = plot(style);
            for t in (50..=1000).step_by(50) {
                accept(&visual, &frame(t));
            }
            let mut stale = frame(1050);
            stale.generation += 1;
            stale.visualizer.demand.band_energy = false;
            accept(&visual, &stale);
            assert!(visual.data.borrow().gpu_frame.is_none());
            assert!(
                visual
                    .data
                    .borrow()
                    .segments()
                    .iter()
                    .all(|s| s.strength < 0.015)
            );
            let fresh = plot(style);
            stale.visualizer.demand.band_energy = true;
            accept(&visual, &stale);
            accept(&fresh, &stale);
            assert_eq!(
                visual.data.borrow().segments(),
                fresh.data.borrow().segments()
            );
            visual.set_visible(false);
            accept(&visual, &frame(1100));
            assert!(visual.data.borrow().last_time.is_none());
            visual.set_visible(true);
            visual.configure(&Config {
                audio_visualizer_style: style,
                analysis_fft_size: 4096,
                ..Config::default()
            });
            accept(&visual, &frame(1150));
            assert!(visual.data.borrow().last_time.is_none());
        }
    }

    #[test]
    fn concurrent_scenes_ignore_global_style_and_hiding_one_preserves_the_other() {
        let beams = plot(AudioVisualizerStyle::FractalBeams);
        let flow = plot(AudioVisualizerStyle::FractalFlow);
        let mut audio = frame(50);
        audio.visualizer.demand.stereo_density = true;
        audio.visualizer.demand.spatial_spectrum = true;
        for t in (50..=1000).step_by(50) {
            audio.sample_time = Duration::from_millis(t);
            accept(&beams, &audio);
            accept(&flow, &audio);
        }
        let beams_before = beams.data.borrow().segments().to_vec();
        let flow_before = flow.data.borrow().segments().to_vec();
        assert!(beams_before.iter().any(|s| s.strength > 0.05));
        assert!(flow_before.iter().any(|s| s.strength > 0.05));
        assert_ne!(beams_before, flow_before);
        let config = Config {
            audio_visualizer_style: AudioVisualizerStyle::StereoDensity,
            ..Config::default()
        };
        assert!(!beams.configure(&config));
        assert!(!flow.configure(&config));
        assert_eq!(beams.data.borrow().segments(), &beams_before);
        assert_eq!(flow.data.borrow().segments(), &flow_before);
        beams.set_visible(false);
        assert!(beams.data.borrow().scene.is_none());
        assert_eq!(flow.data.borrow().segments(), &flow_before);
        audio.sample_time += Duration::from_millis(50);
        accept(&beams, &audio);
        accept(&flow, &audio);
        assert!(beams.data.borrow().scene.is_none());
        assert_ne!(flow.data.borrow().segments(), &flow_before);
        beams.set_visible(true);
        accept(&beams, &audio);
        let fresh = plot(AudioVisualizerStyle::FractalBeams);
        accept(&fresh, &audio);
        assert_eq!(
            beams.data.borrow().segments(),
            fresh.data.borrow().segments()
        );
    }

    #[test]
    fn restored_scene_holds_frame_until_fresh_material_is_drawable() {
        for style in [
            AudioVisualizerStyle::FractalBeams,
            AudioVisualizerStyle::FractalFlow,
        ] {
            let visual = plot(style);
            accept(&visual, &frame(50));
            let retained = publication(&visual);
            visual.set_visible(false);
            visual.set_visible(true);
            visual.data.borrow_mut().bloom = f32::NAN;
            accept(&visual, &frame(100));
            assert!(Arc::ptr_eq(&retained, &publication(&visual)));
            assert!(visual.data.borrow().gpu_error_reported);
            visual.data.borrow_mut().bloom = 0.5;
            assert!(visual.animate(Duration::from_millis(116)));
            assert!(!Arc::ptr_eq(&retained, &publication(&visual)));
            assert!(!visual.data.borrow().retained);
        }
    }

    #[test]
    fn hidden_scenes_release_storage_and_only_valid_audio_recreates_them() {
        for style in [
            AudioVisualizerStyle::FractalBeams,
            AudioVisualizerStyle::FractalFlow,
        ] {
            let visual = plot(style);
            accept(&visual, &frame(50));
            let retained = publication(&visual);
            assert!(visual.data.borrow().scene.is_some());
            visual.set_visible(false);
            assert!(Arc::ptr_eq(&retained, &publication(&visual)));
            assert!(visual.data.borrow().scene.is_none());
            assert_eq!(visual.data.borrow().gpu_segments.capacity(), 0);
            assert!(visual.data.borrow().gpu_palette.is_none());
            accept(&visual, &frame(100));
            assert!(visual.data.borrow().scene.is_none());
            visual.set_visible(true);
            assert!(visual.data.borrow().scene.is_none());
            assert!(Arc::ptr_eq(&retained, &publication(&visual)));
            assert!(!visual.animate(Duration::from_secs(100)));
            let mut stale = frame(150);
            stale.visualizer.demand.band_energy = false;
            accept(&visual, &stale);
            assert!(visual.data.borrow().scene.is_none());
            assert!(Arc::ptr_eq(&retained, &publication(&visual)));
            accept(&visual, &frame(200));
            let fresh = plot(style);
            accept(&fresh, &frame(200));
            assert_eq!(
                visual.data.borrow().segments(),
                fresh.data.borrow().segments()
            );
            assert!(!Arc::ptr_eq(&retained, &publication(&visual)));
            visual.set_visible(false);
            visual.clear();
            assert!(visual.data.borrow().gpu_frame.is_none());
        }
    }
}
