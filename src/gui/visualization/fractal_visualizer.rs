//! Shared, accepted-audio-clocked generative light views. No simulation in paint.
use super::{fractal_beams, fractal_flow};
use crate::i18n::Localizer;
use crate::{
    analysis::{AnalysisFrame, AnalysisSettings},
    config::{AudioVisualizerOptions, AudioVisualizerStyle, Config},
};
use gpui::{
    AnyElement, App, Bounds, GpuFrame, GpuLimits, GpuPalette, GpuSegment, GpuSource,
    GpuStrokeLayer, Pixels, TextAlign, Window, canvas, div, fill, point, prelude::*, px, rgb, size,
};
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
    drive: Drive,
    previous_energy: f32,
    scene: Option<Scene>,
    gpu_frame: Option<Arc<GpuFrame>>,
    gpu_palette: Option<Arc<GpuPalette>>,
    gpu_segments: Vec<GpuSegment>,
    gpu_error_reported: bool,
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
                drive: Drive::default(),
                previous_energy: 0.0,
                scene: None,
                gpu_frame: None,
                gpu_palette: None,
                gpu_segments: Vec::new(),
                gpu_error_reported: false,
            })),
        }
    }

    pub(super) fn configure(&self, config: &Config) -> bool {
        let mut data = self.data.borrow_mut();
        let settings = AnalysisSettings::from(config);
        let reset =
            data.settings.fft_size != settings.fft_size || data.settings.window != settings.window;
        let colors = [
            config.audio_visualizer_primary_color.rgb(),
            config.audio_visualizer_secondary_color.rgb(),
        ];
        let changed = reset
            || data.colors != colors
            || data.background != config.visual_background.rgb()
            || data.sensitivity != config.audio_visualizer_sensitivity
            || data.db_range != config.audio_visualizer_db_range()
            || data.bloom != config.audio_visualizer_bloom_intensity;
        if data.colors != colors || data.bloom != config.audio_visualizer_bloom_intensity {
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
        }
        if changed && !reset {
            data.publish();
        }
        changed
    }

    pub(super) fn set_visible(&self, visible: bool) -> bool {
        let mut data = self.data.borrow_mut();
        let restored = visible && !data.visible;
        if data.visible != visible {
            data.visible = visible;
            data.scene = None;
            data.gpu_segments = Vec::new();
            data.gpu_palette = None;
            data.reset();
        }
        restored
    }

    pub(super) fn update(&self, frame: &AnalysisFrame) {
        self.data.borrow_mut().update(frame);
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
    fn reset(&mut self) {
        self.gpu_frame = None;
        self.gpu_segments.clear();
        self.source = None;
        self.last_time = None;
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

    fn update(&mut self, frame: &AnalysisFrame) {
        if !self.visible {
            return;
        }
        if self.source != Some((frame.generation, frame.sample_rate)) {
            self.reset();
            self.source = Some((frame.generation, frame.sample_rate));
        }
        // Never consume an undemanded payload or an old window during reconfiguration.
        if !frame.visualizer.demand.band_energy
            || !frame.settings.is_some_and(|s| {
                s.fft_size == self.settings.fft_size && s.window == self.settings.window
            })
        {
            return;
        }
        if self.last_time == Some(frame.sample_time) {
            return;
        }
        if self.scene.is_none() {
            self.scene = Scene::new(self.style);
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
        let step_rate = if self.style == AudioVisualizerStyle::FractalFlow {
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
        match &mut self.scene {
            Some(Scene::Beams(scene)) => scene.rebuild_segments(),
            Some(Scene::Flow(scene)) => scene.rebuild_segments(),
            None => {}
        }
        self.last_time = Some(frame.sample_time);
        self.publish();
    }

    fn publish(&mut self) {
        if !self.visible || self.scene.is_none() {
            self.gpu_frame = None;
            return;
        }
        let result = self.publication();
        match result {
            Ok(frame) => {
                self.gpu_frame = Some(frame);
                self.gpu_error_reported = false;
            }
            Err(error) => {
                self.gpu_frame = None;
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
    use crate::analysis::{VisualizerDemand, VisualizerFrame};

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

    #[test]
    fn flow_refresh_preserves_state_and_discontinuities_reset() {
        let visual = plot(AudioVisualizerStyle::FractalFlow);
        visual.update(&frame(50));
        // 500 ms requires all 60 bounded integration steps, but one publication.
        visual.update(&frame(550));
        let before = visual.data.borrow().segments().to_vec();
        let accepted_time = visual.data.borrow().last_time;
        let config = Config {
            audio_visualizer_primary_color: "#ff0000".parse().unwrap(),
            ..Config::default()
        };
        assert!(visual.configure(&config));
        visual.update(&frame(550));
        assert_eq!(visual.data.borrow().last_time, accepted_time);
        assert_eq!(visual.data.borrow().segments(), before);
        let mut gap = frame(1100);
        visual.update(&gap);
        let fresh = plot(AudioVisualizerStyle::FractalFlow);
        fresh.configure(&config);
        fresh.update(&gap);
        assert_eq!(
            visual.data.borrow().segments(),
            fresh.data.borrow().segments()
        );
        gap.sample_time = Duration::from_millis(100);
        visual.update(&gap);
        let rewound = plot(AudioVisualizerStyle::FractalFlow);
        rewound.configure(&config);
        rewound.update(&gap);
        assert_eq!(
            visual.data.borrow().segments(),
            rewound.data.borrow().segments()
        );
    }

    #[test]
    fn paused_publications_do_not_advance_and_silence_extinguishes_light() {
        for style in [
            AudioVisualizerStyle::FractalBeams,
            AudioVisualizerStyle::FractalFlow,
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
            AudioVisualizerStyle::FractalBeams,
            AudioVisualizerStyle::FractalFlow,
        ] {
            let visual = plot(style);
            for t in (50..=1000).step_by(50) {
                visual.update(&frame(t));
            }
            let mut stale = frame(1050);
            stale.generation += 1;
            stale.visualizer.demand.band_energy = false;
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
            stale.visualizer.demand.band_energy = true;
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
                audio_visualizer_style: style,
                analysis_fft_size: 4096,
                ..Config::default()
            });
            visual.update(&frame(1150));
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
            beams.update(&audio);
            flow.update(&audio);
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
        beams.update(&audio);
        flow.update(&audio);
        assert!(beams.data.borrow().scene.is_none());
        assert_ne!(flow.data.borrow().segments(), &flow_before);
        beams.set_visible(true);
        beams.update(&audio);
        let fresh = plot(AudioVisualizerStyle::FractalBeams);
        fresh.update(&audio);
        assert_eq!(
            beams.data.borrow().segments(),
            fresh.data.borrow().segments()
        );
    }

    #[test]
    fn hidden_scenes_release_storage_and_only_valid_audio_recreates_them() {
        for style in [
            AudioVisualizerStyle::FractalBeams,
            AudioVisualizerStyle::FractalFlow,
        ] {
            let visual = plot(style);
            visual.update(&frame(50));
            assert!(visual.data.borrow().scene.is_some());
            visual.set_visible(false);
            assert!(visual.data.borrow().scene.is_none());
            visual.update(&frame(100));
            assert!(visual.data.borrow().scene.is_none());
            visual.set_visible(true);
            assert!(visual.data.borrow().scene.is_none());
            let mut stale = frame(150);
            stale.visualizer.demand.band_energy = false;
            visual.update(&stale);
            assert!(visual.data.borrow().scene.is_none());
            visual.update(&frame(200));
            let fresh = plot(style);
            fresh.update(&frame(200));
            assert_eq!(
                visual.data.borrow().segments(),
                fresh.data.borrow().segments()
            );
        }
    }
}
