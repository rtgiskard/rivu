//! AudioVisualizer coordinates all seven audio visualization styles.
//!
//! Its radial Bars, Rings, and Bars + Rings renderers follow the Noctalia v5
//! control's concept and parameter model using GPUI-native paths. Analysis and
//! fractal styles are delegated to their respective rendering backends.

use super::super::layout;
use super::{analysis_visualizer::AnalysisVisualizer, fractal_visualizer::FractalVisualizer};
use crate::{
    analysis::{AnalysisFrame, AnalysisSettings},
    config::{AudioVisualizerOptions, AudioVisualizerStyle, Config},
};
use gpui::{
    AnyElement, App, Bounds, PathBuilder, Rgba, TextAlign, Window, canvas, div, point, prelude::*,
    px, rgb,
};
use std::{
    cell::RefCell,
    f32::consts::PI,
    ops::Range,
    rc::Rc,
    time::{Duration, Instant},
};

const FANCY_BANDS: usize = 32;
const SMOOTHING_MS: f32 = 60.0;
const MAX_RADIAL_AXIS: usize = 32768 / 2 + 1;

// All radial styles and panels consume the same <=32 groups. Keep only the
// current axis, including its exact float bits so NaNs do not rebuild forever.
#[derive(Default)]
struct RadialMapping {
    sample_rate: u32,
    fft_size: u32,
    frequencies: Vec<u32>,
    ranges: Vec<Range<usize>>,
    #[cfg(test)]
    rebuilds: usize,
}

impl RadialMapping {
    fn prepare(&mut self, sample_rate: u32, fft_size: u32, frequencies: &[f32]) -> bool {
        let low = 20.0_f32.max(*frequencies.first().unwrap_or(&20.0));
        let high = 20_000.0_f32
            .min(sample_rate as f32 * 0.5)
            .min(*frequencies.last().unwrap_or(&20_000.0));
        if frequencies.is_empty() || low >= high {
            return false;
        }
        if self.sample_rate == sample_rate
            && self.fft_size == fft_size
            && self.frequencies.len() == frequencies.len()
            && self
                .frequencies
                .iter()
                .zip(frequencies)
                .all(|(cached, frequency)| *cached == frequency.to_bits())
        {
            return true;
        }
        self.sample_rate = sample_rate;
        self.fft_size = fft_size;
        self.frequencies.clear();
        // Backend axes fit this bound. An oversized axis keeps the old numeric
        // behavior, rebuilding the <=32 ranges rather than retaining that axis.
        if frequencies.len() <= MAX_RADIAL_AXIS {
            self.frequencies
                .extend(frequencies.iter().map(|frequency| frequency.to_bits()));
        }
        self.ranges.clear();
        let count = FANCY_BANDS.min(frequencies.len());
        let log_low = low.ln();
        let log_span = high.ln() - log_low;
        self.ranges.extend((0..count).map(|band| {
            let start = (log_low + log_span * band as f32 / count as f32).exp();
            let end = (log_low + log_span * (band + 1) as f32 / count as f32).exp();
            let first = frequencies.partition_point(|hz| *hz < start);
            let last = frequencies.partition_point(|hz| *hz < end);
            let end = last
                .max(first + usize::from(first < frequencies.len()))
                .min(frequencies.len());
            first.min(end.saturating_sub(1))..end
        }));
        #[cfg(test)]
        {
            self.rebuilds += 1;
        }
        true
    }
}
const TWO_PI: f32 = PI * 2.0;
const IDLE_FADE_DURATION: Duration = Duration::from_secs(2);
const BAR_GLOW_PASSES: [(f32, f32, f32); 4] = [
    (0.05, 3.5, 0.18),
    (0.025, 2.0, 0.32),
    (0.012, 1.5, 0.42),
    (0.006, 1.2, 0.55),
];

const ANALYSIS_STYLES: [AudioVisualizerStyle; 2] = [
    AudioVisualizerStyle::StereoDensity,
    AudioVisualizerStyle::SpatialSpectrum,
];
const FRACTAL_STYLES: [AudioVisualizerStyle; 2] = [
    AudioVisualizerStyle::FractalBeams,
    AudioVisualizerStyle::FractalFlow,
];

#[derive(Clone)]
pub(in crate::gui) struct AudioVisualizer {
    data: Rc<RefCell<VisualizerData>>,
    analysis: [AnalysisVisualizer; 2],
    fractal: [FractalVisualizer; 2],
}

struct VisualizerData {
    background: u32,
    sensitivity: f32,
    rotation_speed: f32,
    bar_width: f32,
    bar_glow_layers: u32,
    ring_opacity: f32,
    bloom_intensity: f32,
    inner_diameter: f32,
    fade_when_idle: bool,
    primary_color: u32,
    secondary_color: u32,
    db_range: f32,
    visible: bool,
    displayed: Vec<f32>,
    mapping: RadialMapping,
    last_sample_time: Option<Duration>,
    source_generation: Option<u64>,
    expected_settings: AnalysisSettings,
    time: f32,
    fade_started: Option<Instant>,
}
impl AudioVisualizer {
    pub(in crate::gui) fn new() -> Self {
        Self {
            analysis: ANALYSIS_STYLES.map(AnalysisVisualizer::new),
            fractal: FRACTAL_STYLES.map(FractalVisualizer::new),
            data: Rc::new(RefCell::new(VisualizerData {
                background: 0x08090c,
                sensitivity: 1.5,
                rotation_speed: 0.5,
                bar_width: 0.6,
                bar_glow_layers: 2,
                ring_opacity: 0.8,
                bloom_intensity: 0.5,
                inner_diameter: 0.7,
                fade_when_idle: false,
                primary_color: 0x7aa2f7,
                secondary_color: 0xbb9af7,
                db_range: 70.0,
                visible: true,
                displayed: Vec::with_capacity(FANCY_BANDS),
                mapping: RadialMapping::default(),
                last_sample_time: None,
                source_generation: None,
                expected_settings: AnalysisSettings::from(&Config::default()),
                time: 0.0,
                fade_started: None,
            })),
        }
    }

    pub(in crate::gui) fn sync(
        &mut self,
        config: &Config,
        panels: &[&layout::Panel],
        window_visible: bool,
    ) -> bool {
        let mut radial_visible = false;
        let mut analysis_visible = [false; 2];
        let mut fractal_visible = [false; 2];
        if window_visible {
            for panel in panels {
                if panel.kind != "audio_visualizer" {
                    continue;
                }
                let options = panel.audio_visualizer.unwrap_or_else(|| config.into());
                match options.style {
                    AudioVisualizerStyle::Bars
                    | AudioVisualizerStyle::Rings
                    | AudioVisualizerStyle::BarsRings => radial_visible = true,
                    AudioVisualizerStyle::StereoDensity => analysis_visible[0] = true,
                    AudioVisualizerStyle::SpatialSpectrum => analysis_visible[1] = true,
                    AudioVisualizerStyle::FractalBeams => fractal_visible[0] = true,
                    AudioVisualizerStyle::FractalFlow => fractal_visible[1] = true,
                }
            }
        }
        let mut changed = self.set_visible(radial_visible);
        for (renderer, visible) in self.analysis.iter().zip(analysis_visible) {
            changed |= renderer.set_visible(visible);
            changed |= renderer.configure(config);
        }
        for (renderer, visible) in self.fractal.iter().zip(fractal_visible) {
            changed |= renderer.set_visible(visible);
            changed |= renderer.configure(config);
        }
        changed | self.configure(config)
    }

    fn configure(&mut self, config: &Config) -> bool {
        let mut data = self.data.borrow_mut();
        let db_range = config.audio_visualizer_db_range();
        let expected_settings = AnalysisSettings::from(config);
        let changed = data.background != config.visual_background.rgb()
            || data.sensitivity != config.audio_visualizer_sensitivity
            || data.rotation_speed != config.audio_visualizer_rotation_speed
            || data.bar_width != config.audio_visualizer_bar_width
            || data.bar_glow_layers != config.audio_visualizer_bar_glow_layers
            || data.ring_opacity != config.audio_visualizer_ring_opacity
            || data.bloom_intensity != config.audio_visualizer_bloom_intensity
            || data.inner_diameter != config.audio_visualizer_inner_diameter
            || data.fade_when_idle != config.audio_visualizer_fade_when_idle
            || data.primary_color != config.audio_visualizer_primary_color.rgb()
            || data.secondary_color != config.audio_visualizer_secondary_color.rgb()
            || data.db_range != db_range
            || data.expected_settings.fft_size != expected_settings.fft_size
            || data.expected_settings.window != expected_settings.window;
        if !changed {
            return false;
        }
        data.background = config.visual_background.rgb();
        data.sensitivity = config.audio_visualizer_sensitivity;
        data.rotation_speed = config.audio_visualizer_rotation_speed;
        data.bar_width = config.audio_visualizer_bar_width;
        data.bar_glow_layers = config.audio_visualizer_bar_glow_layers;
        data.ring_opacity = config.audio_visualizer_ring_opacity;
        data.bloom_intensity = config.audio_visualizer_bloom_intensity;
        data.inner_diameter = config.audio_visualizer_inner_diameter;
        data.fade_when_idle = config.audio_visualizer_fade_when_idle;
        data.primary_color = config.audio_visualizer_primary_color.rgb();
        data.secondary_color = config.audio_visualizer_secondary_color.rgb();
        data.db_range = db_range;
        data.expected_settings = expected_settings;
        data.displayed.clear();
        data.last_sample_time = None;
        data.time = 0.0;
        true
    }

    fn set_visible(&self, visible: bool) -> bool {
        let mut data = self.data.borrow_mut();
        let restored = visible && !data.visible;
        if data.visible != visible {
            data.visible = visible;
            data.displayed = Vec::new();
            data.mapping = RadialMapping::default();
            data.last_sample_time = None;
            data.fade_started = None;
        }
        restored
    }

    pub(in crate::gui) fn update(&mut self, frame: &AnalysisFrame) {
        for renderer in &self.analysis {
            renderer.update(frame);
        }
        for renderer in &self.fractal {
            renderer.update(frame);
        }
        if frame.spectrum_db.is_empty() || frame.frequencies_hz.is_empty() {
            return;
        }
        let mut data = self.data.borrow_mut();
        if !data.visible
            || frame.spectrum_db.len() != frame.frequencies_hz.len()
            || !frame.settings.is_some_and(|settings| {
                settings.fft_size == data.expected_settings.fft_size
                    && settings.window == data.expected_settings.window
            })
        {
            return;
        }
        if data.source_generation != Some(frame.generation) {
            data.source_generation = Some(frame.generation);
            data.displayed.clear();
            data.last_sample_time = None;
            data.fade_started = None;
        }
        let fft_size = data.expected_settings.fft_size;
        if !data
            .mapping
            .prepare(frame.sample_rate, fft_size, &frame.frequencies_hz)
        {
            return;
        }
        let count = data.mapping.ranges.len();
        let db_range = data.db_range;
        let elapsed = data
            .last_sample_time
            .map(|last| frame.sample_time.saturating_sub(last).as_secs_f32() * 1000.0)
            .unwrap_or(SMOOTHING_MS);
        let alpha = if data.displayed.len() != count || elapsed <= 0.0 {
            1.0
        } else {
            1.0 - (-elapsed / SMOOTHING_MS).exp()
        };
        data.displayed.resize(count, 0.0);
        let VisualizerData {
            displayed, mapping, ..
        } = &mut *data;
        for (range, display) in mapping.ranges.iter().zip(displayed) {
            let level = frame.spectrum_db[range.clone()]
                .iter()
                .copied()
                .filter(|level| level.is_finite())
                .fold(f32::NEG_INFINITY, f32::max);
            let target = ((level + db_range) / db_range).clamp(0.0, 1.0);
            *display += (target - *display) * alpha;
        }
        data.last_sample_time = Some(frame.sample_time);
        data.time = (frame.sample_time.as_secs_f32() * data.rotation_speed).rem_euclid(TWO_PI);
    }

    pub(in crate::gui) fn release_retired_images(&self, window: &mut Window) {
        for renderer in &self.analysis {
            renderer.release_retired_images(window);
        }
    }

    pub(in crate::gui) fn release_all_images(&self, window: &mut Window) {
        for renderer in &self.analysis {
            renderer.release_all_images(window);
        }
    }

    pub(in crate::gui) fn view(
        &self,
        panel_id: u64,
        playing: bool,
        options: AudioVisualizerOptions,
    ) -> AnyElement {
        match options.style {
            AudioVisualizerStyle::StereoDensity => return self.analysis[0].view(panel_id, options),
            AudioVisualizerStyle::SpatialSpectrum => {
                return self.analysis[1].view(panel_id, options);
            }
            AudioVisualizerStyle::FractalBeams => return self.fractal[0].view(panel_id, options),
            AudioVisualizerStyle::FractalFlow => return self.fractal[1].view(panel_id, options),
            AudioVisualizerStyle::Bars
            | AudioVisualizerStyle::Rings
            | AudioVisualizerStyle::BarsRings => {}
        }
        let data = Rc::clone(&self.data);
        div()
            .id(("audio-visualizer", panel_id))
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, cx| {
                        let mut data = data.borrow_mut();
                        window.paint_quad(gpui::fill(bounds, rgb(data.background)));
                        if data.displayed.is_empty() {
                            paint_guides(bounds, options, data.background, window, cx);
                            return;
                        }
                        let fade_alpha = if playing || !data.fade_when_idle {
                            data.fade_started = None;
                            1.0
                        } else {
                            let started = *data.fade_started.get_or_insert_with(Instant::now);
                            let elapsed = started.elapsed();
                            let alpha = 1.0
                                - (elapsed.as_secs_f32() / IDLE_FADE_DURATION.as_secs_f32())
                                    .clamp(0.0, 1.0);
                            if alpha > 0.0 {
                                window.request_animation_frame();
                            }
                            alpha
                        };
                        match options.style {
                            AudioVisualizerStyle::Bars => paint_bars(bounds, &data, window),
                            AudioVisualizerStyle::Rings => paint_rings(bounds, &data, window),
                            AudioVisualizerStyle::BarsRings => {
                                paint_rings(bounds, &data, window);
                                paint_bars(bounds, &data, window);
                            }
                            AudioVisualizerStyle::StereoDensity
                            | AudioVisualizerStyle::SpatialSpectrum
                            | AudioVisualizerStyle::FractalBeams
                            | AudioVisualizerStyle::FractalFlow => {}
                        }
                        if fade_alpha < 1.0 {
                            window.paint_quad(gpui::fill(
                                bounds,
                                rgb(data.background).alpha(1.0 - fade_alpha),
                            ));
                        }
                        paint_guides(bounds, options, data.background, window, cx);
                    },
                )
                .size_full(),
            )
            .into_any_element()
    }
}

// Normalized post-sensitivity, clipped bar level, not dB or frequency. Decorative
// animated rings do not follow this scale; it is a fixed reference for bar tips.
fn bar_level_radius(scale: f32, level: f32) -> f32 {
    (0.35 + level * 0.5) * scale
}

fn paint_guides(
    bounds: Bounds<gpui::Pixels>,
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
    let scale = side * 0.5;
    for (level, label) in [
        (0.0, "Bar level 0%"),
        (0.5, "Bar level 50%"),
        (1.0, "Bar level 100%"),
    ] {
        let radius = bar_level_radius(scale, level);
        if options.reference_lines {
            paint_arc(
                window,
                center,
                radius,
                0.0,
                TWO_PI,
                px(1.0),
                rgb(0xb1bbc5).alpha(0.24),
            );
        }
        // Keep 18px between labels and omit them rather than overlap tiny panels.
        if !options.labels || side < 144.0 {
            continue;
        }
        let mut style = window.text_style();
        style.color = rgb(0xb1bbc5).into();
        let run = style.to_run(label.len());
        let line = window
            .text_system()
            .shape_line(label.into(), px(10.0), &[run], None);
        let origin = point(center.x - line.width() * 0.5, center.y - px(radius + 7.0));
        if origin.x < bounds.left() + px(2.0) || origin.x + line.width() > bounds.right() - px(2.0)
        {
            continue;
        }
        window.paint_quad(gpui::fill(
            Bounds::new(
                point(origin.x - px(2.0), origin.y),
                gpui::size(line.width() + px(4.0), px(14.0)),
            ),
            rgb(background).alpha(0.85),
        ));
        if let Err(error) = line.paint(origin, px(14.0), TextAlign::Left, None, window, cx) {
            eprintln!("rivu visualization label: {error}");
        }
    }
}

fn paint_bars(bounds: Bounds<gpui::Pixels>, data: &VisualizerData, window: &mut Window) {
    let side = (bounds.size.width / px(1.0)).min(bounds.size.height / px(1.0));
    if side <= 8.0 || data.displayed.is_empty() {
        return;
    }
    let center = point(
        bounds.left() + bounds.size.width * 0.5,
        bounds.top() + bounds.size.height * 0.5,
    );
    let scale = side * 0.5;
    let inner = (data.inner_diameter * 0.5 * scale).max(2.0);
    let base = bar_level_radius(scale, 0.0);
    let count = data.displayed.len() * 2;
    let section = TWO_PI / count as f32;
    let angular_width =
        (data.bar_width * 0.015 * scale / base.max(1.0)).clamp(0.004, section * 0.8);
    for index in 0..count {
        let value_index = if index < data.displayed.len() {
            data.displayed.len() - 1 - index
        } else {
            index - data.displayed.len()
        };
        let value = (data.displayed[value_index] * data.sensitivity).clamp(0.0, 1.0);
        let angle = -PI + section * (index as f32 + 0.5) + data.time * 0.2;
        let end = bar_level_radius(scale, value);
        let color = fancy_bar_color(data, value);
        if data.bloom_intensity > 0.01 && value > 0.005 {
            let glow = data.bloom_intensity.clamp(0.0, 2.0);
            for &(spread, width_scale, alpha_scale) in
                BAR_GLOW_PASSES.iter().take(data.bar_glow_layers as usize)
            {
                let halo = radial_bar(
                    center,
                    (inner - scale * spread).max(1.0),
                    end + scale * spread,
                    angle,
                    angular_width * width_scale,
                );
                if let Ok(halo) = halo.build() {
                    window.paint_path(
                        halo,
                        rgb(color).alpha((0.08 + value * 0.18) * glow * alpha_scale),
                    );
                }
            }
        }
        let path = radial_bar(center, inner, end.max(inner + 1.0), angle, angular_width);
        if let Ok(path) = path.build() {
            window.paint_path(path, rgb(color));
        }
    }
}

fn paint_rings(bounds: Bounds<gpui::Pixels>, data: &VisualizerData, window: &mut Window) {
    let side = (bounds.size.width / px(1.0)).min(bounds.size.height / px(1.0));
    if side <= 8.0 {
        return;
    }
    let center = point(
        bounds.left() + bounds.size.width * 0.5,
        bounds.top() + bounds.size.height * 0.5,
    );
    let scale = side * 0.5;
    let inner = (data.inner_diameter * 0.5 * scale).max(2.0);
    let bass = band_value(data, 0.05);
    let mid = band_value(data, 0.30);
    let high_mid = band_value(data, 0.60);
    let treble = band_value(data, 0.90);
    let opacity = data.ring_opacity.clamp(0.0, 1.0);
    let rotation = data.time;
    let accent_radius = inner * (0.92 + bass * 0.02);
    let outer_radius = inner + bass * scale * 0.05;
    if data.bloom_intensity > 0.01 {
        let glow = data.bloom_intensity.clamp(0.0, 2.0);
        let energy = (0.35 + bass * 0.65).clamp(0.0, 1.0);
        paint_glow_arc(
            window,
            center,
            inner * 0.65,
            0.0,
            TWO_PI,
            data.secondary_color,
            glow,
            opacity * (0.4 + high_mid * 0.6),
        );
        paint_glow_arc(
            window,
            center,
            accent_radius,
            0.0,
            TWO_PI,
            mix_color(
                data.secondary_color,
                data.primary_color,
                (bass * 0.5).clamp(0.0, 1.0),
            ),
            glow,
            opacity * energy,
        );
        paint_glow_arc(
            window,
            center,
            outer_radius,
            0.0,
            TWO_PI,
            data.primary_color,
            glow,
            opacity * energy,
        );
    }

    paint_arc(
        window,
        center,
        inner * 0.65,
        0.0,
        TWO_PI,
        px(1.5),
        rgb(data.secondary_color).alpha(opacity * 0.35),
    );
    paint_segmented_ring(
        window,
        center,
        inner * 0.65,
        8,
        rotation + high_mid * 3.0,
        px(1.5 + high_mid * 2.0),
        rgb(mix_color(data.primary_color, data.secondary_color, 0.5))
            .alpha(opacity * (0.4 + high_mid * 0.6)),
    );
    paint_segmented_ring(
        window,
        center,
        inner * 0.75,
        16,
        rotation * 0.5 + treble * 2.0,
        px(2.0),
        rgb(data.secondary_color).alpha(opacity * (0.5 + treble.min(0.5))),
    );
    paint_segmented_ring(
        window,
        center,
        inner * 0.85,
        24,
        rotation,
        px(1.0),
        rgb(data.primary_color).alpha(opacity * 0.6),
    );
    paint_arc(
        window,
        center,
        accent_radius,
        0.0,
        TWO_PI,
        px(2.0 + bass * 2.0),
        rgb(mix_color(
            data.secondary_color,
            data.primary_color,
            (bass * 0.5).clamp(0.0, 1.0),
        ))
        .alpha(opacity),
    );
    paint_arc(
        window,
        center,
        outer_radius,
        0.0,
        TWO_PI,
        px(1.5),
        rgb(data.primary_color).alpha(opacity),
    );
    let ripple = (mid * 0.8).clamp(0.0, 1.0);
    if ripple > 0.0 {
        paint_arc(
            window,
            center,
            inner * 0.45,
            rotation,
            rotation + PI * ripple,
            px(1.0 + mid * 2.0),
            rgb(data.secondary_color).alpha(opacity * 0.35),
        );
    }
}

fn paint_segmented_ring(
    window: &mut Window,
    center: gpui::Point<gpui::Pixels>,
    radius: f32,
    segments: usize,
    rotation: f32,
    width: gpui::Pixels,
    color: Rgba,
) {
    let segment = TWO_PI / segments as f32;
    let mut path = PathBuilder::stroke(width);
    for index in 0..segments {
        let start = rotation + index as f32 * segment + segment * 0.08;
        append_arc(&mut path, center, radius, start, start + segment * 0.58);
    }
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

fn paint_arc(
    window: &mut Window,
    center: gpui::Point<gpui::Pixels>,
    radius: f32,
    start: f32,
    end: f32,
    width: gpui::Pixels,
    color: Rgba,
) {
    let mut path = PathBuilder::stroke(width);
    append_arc(&mut path, center, radius, start, end);
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

fn append_arc(
    path: &mut PathBuilder,
    center: gpui::Point<gpui::Pixels>,
    radius: f32,
    start: f32,
    end: f32,
) {
    let steps = ((end - start).abs() * radius / 8.0).ceil().max(4.0) as usize;
    for step in 0..=steps {
        let angle = start + (end - start) * step as f32 / steps as f32;
        let position = point(
            center.x + px(radius * angle.cos()),
            center.y + px(radius * angle.sin()),
        );
        if step == 0 {
            path.move_to(position);
        } else {
            path.line_to(position);
        }
    }
}

/// GPUI paths do not expose a blur filter. Three translucent, widening strokes
/// provide a bounded soft halo without allocating an image or adding a post-pass.
fn paint_glow_arc(
    window: &mut Window,
    center: gpui::Point<gpui::Pixels>,
    radius: f32,
    start: f32,
    end: f32,
    color: u32,
    bloom: f32,
    opacity: f32,
) {
    let strength = bloom.clamp(0.0, 2.0) * opacity.clamp(0.0, 1.0);
    if strength <= 0.001 {
        return;
    }
    for (width, alpha) in [(18.0, 0.08), (10.0, 0.13), (5.0, 0.22)] {
        paint_arc(
            window,
            center,
            radius,
            start,
            end,
            px(width + bloom * 4.0),
            rgb(color).alpha(strength * alpha),
        );
    }
}

fn radial_bar(
    center: gpui::Point<gpui::Pixels>,
    inner: f32,
    outer: f32,
    angle: f32,
    width: f32,
) -> PathBuilder {
    let point_at = |radius: f32, theta: f32| {
        point(
            center.x + px(radius * theta.cos()),
            center.y + px(radius * theta.sin()),
        )
    };
    let mut path = PathBuilder::fill();
    path.move_to(point_at(inner, angle - width * 0.5));
    path.line_to(point_at(outer, angle - width * 0.5));
    path.line_to(point_at(outer, angle + width * 0.5));
    path.line_to(point_at(inner, angle + width * 0.5));
    path.close();
    path
}

fn band_value(data: &VisualizerData, fraction: f32) -> f32 {
    if data.displayed.is_empty() {
        return 0.0;
    }
    let index = (fraction.clamp(0.0, 1.0) * (data.displayed.len() - 1) as f32) as usize;
    (data.displayed[index] * data.sensitivity).clamp(0.0, 1.0)
}

fn fancy_bar_color(data: &VisualizerData, value: f32) -> u32 {
    let value = value.clamp(0.0, 1.0);
    let (from, to, fraction) = if value < 0.5 {
        (
            scale_color(data.primary_color, 0.6),
            data.primary_color,
            value * 2.0,
        )
    } else {
        (
            data.primary_color,
            data.secondary_color,
            (value - 0.5) * 2.0,
        )
    };
    mix_color(from, to, fraction)
}

fn scale_color(color: u32, scale: f32) -> u32 {
    let channel = |shift: u32| (((color >> shift) & 0xff) as f32 * scale).round() as u32;
    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

fn mix_color(first: u32, second: u32, fraction: f32) -> u32 {
    let t = fraction.clamp(0.0, 1.0);
    let channel = |shift: u32| {
        let a = ((first >> shift) & 0xff) as f32;
        let b = ((second >> shift) & 0xff) as f32;
        (a + (b - a) * t).round() as u32
    };
    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> AnalysisFrame {
        AnalysisFrame {
            sample_rate: 48_000,
            sample_time: Duration::from_secs(1),
            frequencies_hz: vec![20.0, 200.0, 2000.0, 20_000.0],
            spectrum_db: vec![-35.0; 4],
            settings: Some(AnalysisSettings::from(&Config::default())),
            generation: 1,
            ..AnalysisFrame::default()
        }
    }
    // Original per-frame grouping, independent of the cached range builder.
    fn reference_levels(frame: &AnalysisFrame, db_range: f32) -> Option<Vec<f32>> {
        if frame.spectrum_db.is_empty() || frame.frequencies_hz.is_empty() {
            return None;
        }
        let count = FANCY_BANDS.min(frame.spectrum_db.len()).max(1);
        let low = 20.0_f32.max(*frame.frequencies_hz.first().unwrap_or(&20.0));
        let high = 20_000.0_f32
            .min(frame.sample_rate as f32 * 0.5)
            .min(*frame.frequencies_hz.last().unwrap_or(&20_000.0));
        if low >= high {
            return None;
        }
        let log_low = low.ln();
        let log_span = high.ln() - log_low;
        Some(
            (0..count)
                .map(|band| {
                    let start = (log_low + log_span * band as f32 / count as f32).exp();
                    let end = (log_low + log_span * (band + 1) as f32 / count as f32).exp();
                    let first = frame.frequencies_hz.partition_point(|hz| *hz < start);
                    let last = frame.frequencies_hz.partition_point(|hz| *hz < end);
                    let end = last
                        .max(first + usize::from(first < frame.spectrum_db.len()))
                        .min(frame.spectrum_db.len());
                    let level = frame.spectrum_db[first.min(end.saturating_sub(1))..end]
                        .iter()
                        .copied()
                        .filter(|level| level.is_finite())
                        .fold(f32::NEG_INFINITY, f32::max);
                    ((level + db_range) / db_range).clamp(0.0, 1.0)
                })
                .collect(),
        )
    }

    #[test]
    fn cached_radial_groups_match_original_levels_and_smoothing_at_numeric_edges() {
        for sample_rate in [0, 40, 41, 8_000, 44_100, 48_000, 192_000] {
            let high = (sample_rate as f32 * 0.5).min(20_000.0);
            let mut axes = vec![
                Vec::new(),
                vec![20.0],
                vec![20.0, 21.0],
                vec![20.0, 200.0, 2000.0, 20_000.0],
                vec![f32::NAN, 20.0, 200.0, 2000.0, f32::NAN],
                vec![f32::NEG_INFINITY, -0.0, 20.0, 2000.0, f32::INFINITY],
            ];
            axes.push(
                (0..=256)
                    .map(|bin| bin as f32 * sample_rate as f32 / 512.0)
                    .collect(),
            );
            if high > 20.0 {
                let mut boundaries = Vec::new();
                for band in 0..=FANCY_BANDS {
                    let frequency = (20.0_f32.ln()
                        + (high.ln() - 20.0_f32.ln()) * band as f32 / FANCY_BANDS as f32)
                        .exp();
                    boundaries.extend([-1_i32, 0, 1].map(|offset| {
                        f32::from_bits((frequency.to_bits() as i64 + offset as i64) as u32)
                    }));
                }
                axes.push(boundaries);
            }
            for frequencies_hz in axes {
                let mut radial = AudioVisualizer::new();
                let mut next = frame();
                next.sample_rate = sample_rate;
                next.frequencies_hz = frequencies_hz;
                next.spectrum_db = (0..next.frequencies_hz.len())
                    .map(|bin| {
                        [
                            f32::NAN,
                            f32::NEG_INFINITY,
                            -100.0,
                            -35.0,
                            0.0,
                            10.0,
                            f32::INFINITY,
                        ][bin % 7]
                    })
                    .collect();
                let expected = reference_levels(&next, 70.0);
                radial.update(&next);
                let first: Vec<_> = expected
                    .unwrap_or_default()
                    .iter()
                    .map(|target| 0.0 + (*target - 0.0) * 1.0)
                    .collect();
                assert_eq!(radial.data.borrow().displayed, first);
                next.sample_time += Duration::from_millis(10);
                next.spectrum_db.fill(-20.0);
                let expected = reference_levels(&next, 70.0);
                radial.update(&next);
                let alpha = 1.0 - (-10.0_f32 / SMOOTHING_MS).exp();
                let smoothed: Vec<_> = first
                    .iter()
                    .zip(expected.unwrap_or_default())
                    .map(|(display, target)| *display + (target - *display) * alpha)
                    .collect();
                assert_eq!(radial.data.borrow().displayed, smoothed);
            }
        }
    }

    #[test]
    fn radial_mapping_reuses_axis_and_rebuilds_changed_axes_with_bounded_storage() {
        let mut frequencies: Vec<_> = (0..=4096)
            .map(|bin| bin as f32 * 48_000.0 / 8192.0)
            .collect();
        let mut mapping = RadialMapping::default();
        for _ in 0..120 {
            assert!(mapping.prepare(48_000, 8192, &frequencies));
        }
        assert_eq!(mapping.rebuilds, 1);
        assert_eq!(mapping.ranges.len(), 32);
        frequencies[10] += 1.0;
        mapping.prepare(48_000, 8192, &frequencies);
        assert_eq!(mapping.rebuilds, 2);
        mapping.prepare(8_000, 8192, &frequencies);
        assert_eq!(mapping.rebuilds, 3);
        mapping.prepare(8_000, 16384, &frequencies);
        assert_eq!(mapping.rebuilds, 4);
        frequencies[10] = f32::NAN;
        mapping.prepare(8_000, 16384, &frequencies);
        mapping.prepare(8_000, 16384, &frequencies);
        assert_eq!(mapping.rebuilds, 5);
        frequencies = (0..=MAX_RADIAL_AXIS)
            .map(|bin| bin as f32 * 48_000.0 / (MAX_RADIAL_AXIS - 1) as f32)
            .collect();
        mapping.prepare(48_000, 32768, &frequencies);
        assert!(mapping.frequencies.is_empty());
        assert_eq!(mapping.ranges.len(), FANCY_BANDS);
    }

    #[test]
    fn same_rate_new_generation_resets_radial_smoothing() {
        let mut radial = AudioVisualizer::new();
        radial.update(&frame());
        let mut next = frame();
        next.sample_time += Duration::from_millis(1);
        next.generation = 2;
        next.spectrum_db.fill(-70.0);
        radial.update(&next);
        assert_eq!(radial.data.borrow().displayed, vec![0.0; 4]);
    }

    #[test]
    fn radial_rejects_stale_fft_and_window_configuration() {
        for config in [
            Config {
                analysis_fft_size: 16384,
                ..Config::default()
            },
            Config {
                analysis_window: crate::config::AnalysisWindow::Hann,
                ..Config::default()
            },
        ] {
            let mut radial = AudioVisualizer::new();
            radial.update(&frame());
            radial.configure(&config);
            radial.update(&frame());
            assert!(radial.data.borrow().displayed.is_empty());
            let mut next = frame();
            next.settings = Some(AnalysisSettings::from(&config));
            next.generation = 2;
            radial.update(&next);
            assert_eq!(radial.data.borrow().displayed, vec![0.5; 4]);
        }
    }

    #[test]
    fn hidden_radial_resumes_directly_from_latest_frame() {
        let mut radial = AudioVisualizer::new();
        radial.update(&frame());
        radial.set_visible(false);
        radial.update(&frame());
        assert!(radial.data.borrow().displayed.is_empty());
        assert_eq!(radial.data.borrow().mapping.frequencies.capacity(), 0);
        assert_eq!(radial.data.borrow().mapping.ranges.capacity(), 0);
        radial.set_visible(true);
        let mut latest = frame();
        latest.spectrum_db.fill(-70.0);
        radial.update(&latest);
        assert_eq!(radial.data.borrow().displayed, vec![0.0; 4]);
    }

    #[test]
    fn other_view_overrides_do_not_reset_radial_display() {
        let mut radial = AudioVisualizer::new();
        radial.update(&frame());
        let mut config = Config::default();
        config.spectrum_db_range = Some(40.0);
        config.spectrogram_db_range = Some(100.0);
        radial.configure(&config);
        let data = radial.data.borrow();
        assert_eq!(data.displayed, vec![0.5; 4]);
        assert_eq!(data.last_sample_time, Some(Duration::from_secs(1)));
    }

    #[test]
    fn common_range_changes_reset_only_inherited_radial_display() {
        for range_override in [None, Some(70.0)] {
            let mut radial = AudioVisualizer::new();
            let mut config = Config {
                audio_visualizer_db_range: range_override,
                ..Config::default()
            };
            radial.configure(&config);
            radial.update(&frame());
            config.visualization_db_range = 100.0;
            radial.configure(&config);
            {
                let data = radial.data.borrow();
                if range_override.is_some() {
                    assert_eq!(data.displayed, vec![0.5; 4]);
                    assert_eq!(data.last_sample_time, Some(Duration::from_secs(1)));
                } else {
                    assert!(data.displayed.is_empty());
                    assert!(data.last_sample_time.is_none());
                }
            }
            radial.update(&frame());
            let data = radial.data.borrow();
            let expected = if range_override.is_some() { 0.5 } else { 0.65 };
            assert_eq!(data.displayed, vec![expected; 4]);
        }
    }

    #[test]
    fn radial_override_uses_its_own_range_and_reset() {
        let mut radial = AudioVisualizer::new();
        radial.update(&frame());
        radial.configure(&Config {
            visualization_db_range: 100.0,
            spectrum_db_range: Some(40.0),
            audio_visualizer_db_range: Some(50.0),
            ..Config::default()
        });
        assert!(radial.data.borrow().displayed.is_empty());
        radial.update(&frame());
        let data = radial.data.borrow();
        assert_eq!(data.displayed, vec![0.3; 4]);
    }
}
