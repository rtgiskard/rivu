//! Noctalia v5 Fancy Audio Visualizer reference implementation.
//!
//! Rivu's RadialSpectrum follows the Noctalia v5 control's Bars/Rings concept
//! and parameter model, using GPUI-native paths for the local rendering backend.

use crate::{
    analysis::{AnalysisFrame, AnalysisSettings},
    config::{Config, RadialSpectrumStyle},
};
use gpui::{
    AnyElement, Bounds, PathBuilder, Rgba, Window, canvas, div, point, prelude::*, px, rgb,
};
use std::{
    cell::RefCell,
    f32::consts::PI,
    rc::Rc,
    time::{Duration, Instant},
};

const FANCY_BANDS: usize = 32;
const SMOOTHING_MS: f32 = 60.0;
const TWO_PI: f32 = PI * 2.0;
const IDLE_FADE_DURATION: Duration = Duration::from_secs(2);
const BAR_GLOW_PASSES: [(f32, f32, f32); 4] = [
    (0.05, 3.5, 0.18),
    (0.025, 2.0, 0.32),
    (0.012, 1.5, 0.42),
    (0.006, 1.2, 0.55),
];

pub(super) struct RadialSpectrum {
    data: Rc<RefCell<VisualizerData>>,
}

struct VisualizerData {
    background: u32,
    style: RadialSpectrumStyle,
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
    values: Vec<f32>,
    visible: bool,
    displayed: Vec<f32>,
    last_sample_time: Option<Duration>,
    source_generation: Option<u64>,
    expected_settings: AnalysisSettings,
    time: f32,
    fade_started: Option<Instant>,
}
impl RadialSpectrum {
    pub(super) fn new() -> Self {
        Self {
            data: Rc::new(RefCell::new(VisualizerData {
                background: 0x08090c,
                style: RadialSpectrumStyle::BarsRings,
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
                values: Vec::with_capacity(FANCY_BANDS),
                visible: true,
                displayed: Vec::with_capacity(FANCY_BANDS),
                last_sample_time: None,
                source_generation: None,
                expected_settings: AnalysisSettings::from(&Config::default()),
                time: 0.0,
                fade_started: None,
            })),
        }
    }

    pub(super) fn configure(&mut self, config: &Config) -> bool {
        let mut data = self.data.borrow_mut();
        let db_range = config.radial_spectrum_db_range();
        let expected_settings = AnalysisSettings::from(config);
        let changed = data.background != config.visual_background.rgb()
            || data.style != config.radial_spectrum_style
            || data.sensitivity != config.radial_spectrum_sensitivity
            || data.rotation_speed != config.radial_spectrum_rotation_speed
            || data.bar_width != config.radial_spectrum_bar_width
            || data.bar_glow_layers != config.radial_spectrum_bar_glow_layers
            || data.ring_opacity != config.radial_spectrum_ring_opacity
            || data.bloom_intensity != config.radial_spectrum_bloom_intensity
            || data.inner_diameter != config.radial_spectrum_inner_diameter
            || data.fade_when_idle != config.radial_spectrum_fade_when_idle
            || data.primary_color != config.radial_spectrum_primary_color.rgb()
            || data.secondary_color != config.radial_spectrum_secondary_color.rgb()
            || data.db_range != db_range
            || data.expected_settings.fft_size != expected_settings.fft_size
            || data.expected_settings.window != expected_settings.window;
        if !changed {
            return false;
        }
        data.background = config.visual_background.rgb();
        data.style = config.radial_spectrum_style;
        data.sensitivity = config.radial_spectrum_sensitivity;
        data.rotation_speed = config.radial_spectrum_rotation_speed;
        data.bar_width = config.radial_spectrum_bar_width;
        data.bar_glow_layers = config.radial_spectrum_bar_glow_layers;
        data.ring_opacity = config.radial_spectrum_ring_opacity;
        data.bloom_intensity = config.radial_spectrum_bloom_intensity;
        data.inner_diameter = config.radial_spectrum_inner_diameter;
        data.fade_when_idle = config.radial_spectrum_fade_when_idle;
        data.primary_color = config.radial_spectrum_primary_color.rgb();
        data.secondary_color = config.radial_spectrum_secondary_color.rgb();
        data.db_range = db_range;
        data.expected_settings = expected_settings;
        data.values.clear();
        data.displayed.clear();
        data.last_sample_time = None;
        data.time = 0.0;
        true
    }

    pub(super) fn set_visible(&self, visible: bool) -> bool {
        let mut data = self.data.borrow_mut();
        let restored = visible && !data.visible;
        if data.visible != visible {
            data.visible = visible;
            data.displayed.clear();
            data.values.clear();
            data.last_sample_time = None;
            data.fade_started = None;
        }
        restored
    }

    pub(super) fn update(&mut self, frame: &AnalysisFrame) {
        if frame.spectrum_db.is_empty() || frame.frequencies_hz.is_empty() {
            return;
        }
        let mut data = self.data.borrow_mut();
        if !data.visible || frame.spectrum_db.len() != frame.frequencies_hz.len()
            || !frame.settings.is_some_and(|settings| {
                settings.fft_size == data.expected_settings.fft_size
                    && settings.window == data.expected_settings.window
            }) {
            return;
        }
        if data.source_generation != Some(frame.generation) {
            data.source_generation = Some(frame.generation);
            data.displayed.clear();
            data.last_sample_time = None;
            data.fade_started = None;
        }
        let count = FANCY_BANDS.min(frame.spectrum_db.len()).max(1);
        let low = 20.0_f32.max(*frame.frequencies_hz.first().unwrap_or(&20.0));
        let high = 20_000.0_f32
            .min(frame.sample_rate as f32 * 0.5)
            .min(*frame.frequencies_hz.last().unwrap_or(&20_000.0));
        if low >= high {
            return;
        }
        let db_range = data.db_range;
        let values = std::mem::take(&mut data.values);
        let mut values = values;
        values.resize(count, 0.0);
        let log_low = low.ln();
        let log_span = high.ln() - log_low;
        for (band, value) in values.iter_mut().enumerate() {
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
            *value = ((level + db_range) / db_range).clamp(0.0, 1.0);
        }
        let elapsed = data
            .last_sample_time
            .map(|last| frame.sample_time.saturating_sub(last).as_secs_f32() * 1000.0)
            .unwrap_or(SMOOTHING_MS);
        let alpha = if elapsed <= 0.0 {
            1.0
        } else {
            1.0 - (-elapsed / SMOOTHING_MS).exp()
        };
        if data.displayed.len() != values.len() {
            data.displayed.clone_from(&values);
        } else {
            for (display, target) in data.displayed.iter_mut().zip(&values) {
                *display += (*target - *display) * alpha;
            }
        }
        data.values = values;
        data.last_sample_time = Some(frame.sample_time);
        data.time = (frame.sample_time.as_secs_f32() * data.rotation_speed).rem_euclid(TWO_PI);
    }

    pub(super) fn view(&self, panel_id: u64, playing: bool) -> AnyElement {
        let data = Rc::clone(&self.data);
        div()
            .id(("radial-spectrum", panel_id))
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| {
                        let mut data = data.borrow_mut();
                        window.paint_quad(gpui::fill(bounds, rgb(data.background)));
                        if data.displayed.is_empty() {
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
                        match data.style {
                            RadialSpectrumStyle::Bars => paint_bars(bounds, &data, window),
                            RadialSpectrumStyle::Rings => paint_rings(bounds, &data, window),
                            RadialSpectrumStyle::BarsRings => {
                                paint_rings(bounds, &data, window);
                                paint_bars(bounds, &data, window);
                            }
                        }
                        if fade_alpha < 1.0 {
                            window.paint_quad(gpui::fill(
                                bounds,
                                rgb(data.background).alpha(1.0 - fade_alpha),
                            ));
                        }
                    },
                )
                .size_full(),
            )
            .into_any_element()
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
    let base = 0.35 * scale;
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
        let end = base + value * scale * 0.5;
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

    #[test]
    fn same_rate_new_generation_resets_radial_smoothing() {
        let mut radial = RadialSpectrum::new();
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
        for config in [Config { spectrum_fft_size: 16384, ..Config::default() },
                       Config { spectrum_window: crate::config::SpectrumWindow::Hann, ..Config::default() }] {
            let mut radial = RadialSpectrum::new();
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
        let mut radial = RadialSpectrum::new();
        radial.update(&frame());
        radial.set_visible(false);
        radial.update(&frame());
        assert!(radial.data.borrow().displayed.is_empty());
        radial.set_visible(true);
        let mut latest = frame();
        latest.spectrum_db.fill(-70.0);
        radial.update(&latest);
        assert_eq!(radial.data.borrow().displayed, vec![0.0; 4]);
    }

    #[test]
    fn other_view_overrides_do_not_reset_radial_display() {
        let mut radial = RadialSpectrum::new();
        radial.update(&frame());
        let mut config = Config::default();
        config.spectrum_db_range = Some(40.0);
        config.spectrogram_db_range = Some(100.0);
        radial.configure(&config);
        let data = radial.data.borrow();
        assert_eq!(data.values, vec![0.5; 4]);
        assert_eq!(data.displayed, vec![0.5; 4]);
        assert_eq!(data.last_sample_time, Some(Duration::from_secs(1)));
    }

    #[test]
    fn common_range_changes_reset_only_inherited_radial_display() {
        for range_override in [None, Some(70.0)] {
            let mut radial = RadialSpectrum::new();
            let mut config = Config {
                radial_spectrum_db_range: range_override,
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
                    assert!(data.values.is_empty());
                    assert!(data.displayed.is_empty());
                    assert!(data.last_sample_time.is_none());
                }
            }
            radial.update(&frame());
            let data = radial.data.borrow();
            let expected = if range_override.is_some() { 0.5 } else { 0.65 };
            assert_eq!(data.values, vec![expected; 4]);
            assert_eq!(data.displayed, vec![expected; 4]);
        }
    }

    #[test]
    fn radial_override_uses_its_own_range_and_reset() {
        let mut radial = RadialSpectrum::new();
        radial.update(&frame());
        radial.configure(&Config {
            visualization_db_range: 100.0,
            spectrum_db_range: Some(40.0),
            radial_spectrum_db_range: Some(50.0),
            ..Config::default()
        });
        assert!(radial.data.borrow().displayed.is_empty());
        radial.update(&frame());
        let data = radial.data.borrow();
        assert_eq!(data.values, vec![0.3; 4]);
        assert_eq!(data.displayed, vec![0.3; 4]);
    }
}
