//! Renders radial bars and rings with shared, bounded analysis state.
use crate::VisualLabels;
use crate::VisualParameters;

use gpui::{
    AnyElement, App, Bounds, PathBuilder, Rgba, TextAlign, Window, canvas, div, point, prelude::*,
    px, rgb,
};
use rivu_audio::analysis::AnalysisFrame;
#[cfg(test)]
use rivu_audio::analysis::AnalysisSettings;
use rivu_format::config::{
    AnalysisWindow, AudioVisualizerOptions, AudioVisualizerStyle, VisualizationSignal,
};
use rivu_protocol::model::PlaybackStatus;
use std::{
    cell::RefCell,
    f32::consts::PI,
    ops::Range,
    rc::Rc,
    sync::Arc,
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
    // None rejects an unusable axis; Some reports whether the display mapping changed.
    fn prepare(&mut self, sample_rate: u32, fft_size: u32, frequencies: &[f32]) -> Option<bool> {
        let low = 20.0_f32.max(*frequencies.first().unwrap_or(&20.0));
        let high = 20_000.0_f32
            .min(sample_rate as f32 * 0.5)
            .min(*frequencies.last().unwrap_or(&20_000.0));
        if frequencies.is_empty() || low >= high {
            return None;
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
            return Some(false);
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
        let mut changed = self.ranges.len() != FANCY_BANDS.min(frequencies.len());
        let count = FANCY_BANDS.min(frequencies.len());
        self.ranges.reserve(count.saturating_sub(self.ranges.len()));
        let log_low = low.ln();
        let log_span = high.ln() - log_low;
        for band in 0..count {
            let start = (log_low + log_span * band as f32 / count as f32).exp();
            let end = (log_low + log_span * (band + 1) as f32 / count as f32).exp();
            let first = frequencies.partition_point(|hz| *hz < start);
            let last = frequencies.partition_point(|hz| *hz < end);
            let end = last
                .max(first + usize::from(first < frequencies.len()))
                .min(frequencies.len());
            let range = first.min(end.saturating_sub(1))..end;
            changed |= self.ranges.get(band) != Some(&range);
            if let Some(cached) = self.ranges.get_mut(band) {
                *cached = range;
            } else {
                self.ranges.push(range);
            }
        }
        self.ranges.truncate(count);
        #[cfg(test)]
        {
            self.rebuilds += 1;
        }
        Some(changed)
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

pub(super) struct RadialRenderer {
    data: Rc<RefCell<RadialState>>,
}

struct RadialState {
    guide_labels: Option<[gpui::SharedString; 3]>,
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
    targets: Vec<f32>,
    // Unmapped peaks for at most 32 bands; range edits need no new audio or FFT.
    levels_db: [f32; FANCY_BANDS],
    mapping: RadialMapping,
    last_sample_time: Option<Duration>,
    last_presentation_time: Option<Duration>,
    source_sample_rate: u32,
    source_generation: Option<u64>,
    fft_size: u32,
    window: AnalysisWindow,
    signal: VisualizationSignal,
    time: f32,
    fade_started: Option<Instant>,
}
impl RadialRenderer {
    pub(super) fn new() -> Self {
        let config = VisualParameters::default();
        Self {
            data: Rc::new(RefCell::new(RadialState {
                guide_labels: None,
                background: config.visual_background.rgb(),
                sensitivity: config.audio_visualizer_sensitivity,
                rotation_speed: config.audio_visualizer_rotation_speed,
                bar_width: config.audio_visualizer_bar_width,
                bar_glow_layers: config.audio_visualizer_bar_glow_layers,
                ring_opacity: config.audio_visualizer_ring_opacity,
                bloom_intensity: config.audio_visualizer_bloom_intensity,
                inner_diameter: config.audio_visualizer_inner_diameter,
                fade_when_idle: config.audio_visualizer_fade_when_idle,
                primary_color: config.audio_visualizer_primary_color.rgb(),
                secondary_color: config.audio_visualizer_secondary_color.rgb(),
                db_range: config.audio_visualizer_db_range(),
                visible: true,
                displayed: Vec::with_capacity(FANCY_BANDS),
                targets: Vec::with_capacity(FANCY_BANDS),
                levels_db: [f32::NEG_INFINITY; FANCY_BANDS],
                mapping: RadialMapping::default(),
                last_sample_time: None,
                last_presentation_time: None,
                source_sample_rate: 0,
                source_generation: None,
                fft_size: config.analysis_fft_size,
                window: config.analysis_window,
                signal: config.visualization_signal,
                time: 0.0,
                fade_started: None,
            })),
        }
    }

    pub(super) fn configure(&mut self, config: &VisualParameters) -> bool {
        let mut data = self.data.borrow_mut();
        let db_range = config.audio_visualizer_db_range();
        let incompatible = data.fft_size != config.analysis_fft_size
            || data.window != config.analysis_window
            || data.signal != config.visualization_signal;
        let remap = data.db_range != db_range;
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
            || remap
            || incompatible;
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
        data.fft_size = config.analysis_fft_size;
        data.window = config.analysis_window;
        data.signal = config.visualization_signal;
        if !data.fade_when_idle && data.fade_started.is_some() {
            drop(data);
            self.clear();
            return true;
        }
        if incompatible {
            drop(data);
            self.clear();
        } else if remap {
            let count = data.displayed.len();
            for index in 0..count {
                data.displayed[index] =
                    ((data.levels_db[index] + db_range) / db_range).clamp(0.0, 1.0);
            }
            if !data.targets.is_empty() {
                let RadialState {
                    displayed, targets, ..
                } = &mut *data;
                targets.clone_from(displayed);
            }
        }
        true
    }

    pub(super) fn set_visible(&self, visible: bool) -> bool {
        if !visible && self.data.borrow().fade_started.is_some() {
            self.clear();
        }
        let mut data = self.data.borrow_mut();
        let restored = visible && !data.visible;
        if data.visible != visible {
            data.visible = visible;
            data.targets = Vec::new();
            data.last_presentation_time = None;
            data.mapping = RadialMapping::default();
            data.last_sample_time = None;
            data.fade_started = None;
        }
        restored
    }

    pub(super) fn clear(&self) {
        let mut data = self.data.borrow_mut();
        data.displayed = Vec::new();
        data.targets = Vec::new();
        data.levels_db = [f32::NEG_INFINITY; FANCY_BANDS];
        data.mapping = RadialMapping::default();
        data.last_sample_time = None;
        data.last_presentation_time = None;
        data.source_generation = None;
        data.source_sample_rate = 0;
        data.time = 0.0;
        data.fade_started = None;
    }

    /// Freeze bounded simulation state until replacement audio reanchors it.
    pub(super) fn suspend(&self) {
        let mut data = self.data.borrow_mut();
        data.last_sample_time = None;
        data.last_presentation_time = None;
    }

    /// Stop retires input immediately; only bounded radial drawing survives.
    pub(super) fn stop(&self, now: Instant, fade_when_idle: bool) {
        let retired = {
            let mut data = self.data.borrow_mut();
            data.fade_when_idle = fade_when_idle;
            (data.visible && data.fade_when_idle && !data.displayed.is_empty()).then(|| {
                (
                    std::mem::take(&mut data.displayed),
                    data.levels_db,
                    data.time,
                )
            })
        };
        self.clear();
        if let Some((displayed, levels_db, time)) = retired {
            let mut data = self.data.borrow_mut();
            data.displayed = displayed;
            data.levels_db = levels_db;
            data.time = time;
            drop(data);
            self.begin_idle_fade(now);
        }
    }

    pub(super) fn retire_idle_fade(&self) {
        if self.data.borrow().fade_started.is_some() {
            self.clear();
        }
    }

    pub(super) fn advance_idle_fade(&self, now: Instant) {
        if self
            .data
            .borrow()
            .fade_started
            .is_some_and(|start| now.saturating_duration_since(start) >= IDLE_FADE_DURATION)
        {
            self.clear();
        }
    }

    pub(super) fn update(&mut self, frame: &AnalysisFrame, presentation_time: Duration) {
        let mut data = self.data.borrow_mut();
        if frame.spectrum_db.is_empty()
            || frame.spectrum_db.len() != frame.frequencies_hz.len()
            || !frame.settings.is_some_and(|settings| {
                settings.fft_size == data.fft_size
                    && settings.window == data.window
                    && settings.signal == data.signal
            })
        {
            data.displayed.clear();
            data.fade_started = None;
            data.targets.clear();
            data.last_sample_time = None;
            data.last_presentation_time = None;
            return;
        }
        let discontinuity = data.source_generation != Some(frame.generation)
            || data.source_sample_rate != frame.sample_rate
            || data
                .last_sample_time
                .is_some_and(|last| frame.sample_time < last);
        if discontinuity {
            data.source_generation = Some(frame.generation);
            data.source_sample_rate = frame.sample_rate;
            data.displayed.clear();
            data.targets.clear();
            data.last_sample_time = None;
            data.last_presentation_time = None;
            data.fade_started = None;
        }
        if !data.visible {
            return;
        }
        let fft_size = data.fft_size;
        let Some(mapping_changed) =
            data.mapping
                .prepare(frame.sample_rate, fft_size, &frame.frequencies_hz)
        else {
            data.displayed.clear();
            data.fade_started = None;
            data.targets.clear();
            data.last_sample_time = None;
            data.last_presentation_time = None;
            return;
        };
        if !mapping_changed && data.last_sample_time == Some(frame.sample_time) {
            return;
        }
        let count = data.mapping.ranges.len();
        let db_range = data.db_range;
        data.targets.resize(count, 0.0);
        let RadialState {
            targets,
            mapping,
            levels_db,
            ..
        } = &mut *data;
        for ((range, target), retained) in mapping.ranges.iter().zip(targets).zip(levels_db) {
            let level = frame.spectrum_db[range.clone()]
                .iter()
                .copied()
                .filter(|level| level.is_finite())
                .fold(f32::NEG_INFINITY, f32::max);
            *retained = level;
            *target = ((level + db_range) / db_range).clamp(0.0, 1.0);
        }
        if data.displayed.len() != count || mapping_changed || data.last_presentation_time.is_none()
        {
            let RadialState {
                displayed, targets, ..
            } = &mut *data;
            displayed.clone_from(targets);
            data.last_presentation_time = Some(presentation_time);
            data.time = (presentation_time.as_secs_f64() * data.rotation_speed as f64)
                .rem_euclid(TWO_PI as f64) as f32;
        }
        data.last_sample_time = Some(frame.sample_time);
    }

    pub(super) fn animate(&mut self, presentation_time: Duration) -> bool {
        let mut changed = false;
        let mut data = self.data.borrow_mut();
        if !data.visible || data.displayed.is_empty() {
            return changed;
        }
        let Some(last) = data.last_presentation_time else {
            return changed;
        };
        if presentation_time <= last {
            return changed;
        }
        let elapsed = presentation_time.saturating_sub(last).as_secs_f32();
        let alpha = 1.0 - (-elapsed * 1000.0 / SMOOTHING_MS).exp();
        let RadialState {
            displayed, targets, ..
        } = &mut *data;
        for (display, target) in displayed.iter_mut().zip(targets) {
            let next = *display + (*target - *display) * alpha;
            changed |= next != *display;
            *display = next;
        }
        let rotation = (data.time + elapsed * data.rotation_speed).rem_euclid(TWO_PI);
        changed |= rotation != data.time;
        data.time = rotation;
        data.last_presentation_time = Some(presentation_time);
        changed
    }

    pub(super) fn begin_idle_fade(&self, now: Instant) {
        let mut data = self.data.borrow_mut();
        if data.visible && data.fade_when_idle && !data.displayed.is_empty() {
            data.fade_started.get_or_insert(now);
        }
    }

    pub(super) fn idle_fading(&self, _now: Instant) -> bool {
        let data = self.data.borrow();
        // Keep the final cleanup tick scheduled even if its deadline has passed.
        data.visible && data.fade_started.is_some()
    }

    pub(super) fn view(
        &self,
        i18n: &Arc<VisualLabels>,
        panel_id: u64,
        status: PlaybackStatus,
        options: AudioVisualizerOptions,
    ) -> AnyElement {
        let data = Rc::clone(&self.data);
        {
            let mut data = self.data.borrow_mut();
            if data.guide_labels.as_ref().is_none_or(|labels| {
                labels[0] != i18n.level_zero
                    || labels[1] != i18n.level_half
                    || labels[2] != i18n.level_full
            }) {
                data.guide_labels = Some([
                    i18n.level_zero.clone(),
                    i18n.level_half.clone(),
                    i18n.level_full.clone(),
                ]);
            }
        }
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
                        let data = data.borrow();
                        window.paint_quad(gpui::fill(bounds, rgb(data.background)));
                        let fade_alpha = if data.displayed.is_empty() {
                            0.0
                        } else {
                            radial_opacity(&data, status, Instant::now())
                        };
                        if fade_alpha > 0.0 {
                            match options.style {
                                AudioVisualizerStyle::Bars => paint_bars(bounds, &data, window),
                                AudioVisualizerStyle::Rings => paint_rings(bounds, &data, window),
                                AudioVisualizerStyle::BarsRings => {
                                    paint_rings(bounds, &data, window);
                                    paint_bars(bounds, &data, window);
                                }
                                _ => {}
                            }
                            if fade_alpha < 1.0 {
                                window.paint_quad(gpui::fill(
                                    bounds,
                                    rgb(data.background).alpha(1.0 - fade_alpha),
                                ));
                            }
                        }
                        paint_guides(
                            data.guide_labels.as_ref().expect("localized guide labels"),
                            bounds,
                            options,
                            data.background,
                            window,
                            cx,
                        );
                    },
                )
                .size_full(),
            )
            .into_any_element()
    }
}

fn radial_opacity(data: &RadialState, status: PlaybackStatus, now: Instant) -> f32 {
    if data.displayed.is_empty() {
        return 0.0;
    }
    if status != PlaybackStatus::Stopped {
        return 1.0;
    }
    data.fade_started.map_or(0.0, |started| {
        1.0 - (now.saturating_duration_since(started).as_secs_f32()
            / IDLE_FADE_DURATION.as_secs_f32())
        .clamp(0.0, 1.0)
    })
}

// Normalized post-sensitivity, clipped bar level, not dB or frequency. Decorative
// animated rings do not follow this scale; it is a fixed reference for bar tips.
fn bar_level_radius(scale: f32, level: f32) -> f32 {
    (0.35 + level * 0.5) * scale
}

fn paint_guides(
    labels: &[gpui::SharedString; 3],
    bounds: Bounds<gpui::Pixels>,
    options: AudioVisualizerOptions,
    background: u32,
    window: &mut Window,
    cx: &mut App,
) {
    if !options.grid && !options.label {
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
        (0.0, labels[0].clone()),
        (0.5, labels[1].clone()),
        (1.0, labels[2].clone()),
    ] {
        let radius = bar_level_radius(scale, level);
        if options.grid {
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
        if !options.label || side < 144.0 {
            continue;
        }
        let mut style = window.text_style();
        style.color = rgb(0xb1bbc5).into();
        let run = style.to_run(label.len());
        let line = window
            .text_system()
            .shape_line(label, px(10.0), &[run], None);
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

fn paint_bars(bounds: Bounds<gpui::Pixels>, data: &RadialState, window: &mut Window) {
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

fn paint_rings(bounds: Bounds<gpui::Pixels>, data: &RadialState, window: &mut Window) {
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

#[expect(
    clippy::too_many_arguments,
    reason = "The primitive takes arc geometry and paint parameters directly without a one-use wrapper"
)]
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

fn band_value(data: &RadialState, fraction: f32) -> f32 {
    if data.displayed.is_empty() {
        return 0.0;
    }
    let index = (fraction.clamp(0.0, 1.0) * (data.displayed.len() - 1) as f32) as usize;
    (data.displayed[index] * data.sensitivity).clamp(0.0, 1.0)
}

fn fancy_bar_color(data: &RadialState, value: f32) -> u32 {
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
    use rivu_format::config::Config;
    fn accept(radial: &mut RadialRenderer, frame: &AnalysisFrame) {
        radial.update(frame, frame.sample_time);
        radial.animate(frame.sample_time);
    }

    #[test]
    fn appearance_edits_preserve_frozen_levels_and_animation_time() {
        let mut radial = RadialRenderer::new();
        let audio = frame();
        accept(&mut radial, &audio);
        let (displayed, targets, time, sample_time, presentation_time) = {
            let data = radial.data.borrow();
            (
                data.displayed.clone(),
                data.targets.clone(),
                data.time,
                data.last_sample_time,
                data.last_presentation_time,
            )
        };
        radial.configure(&VisualParameters::from(&Config {
            audio_visualizer_primary_color: "#ff0000".parse().unwrap(),
            audio_visualizer_bar_width: 0.9,
            audio_visualizer_sensitivity: 2.0,
            ..Config::default()
        }));
        {
            let data = radial.data.borrow();
            assert_eq!(data.displayed, displayed);
            assert_eq!(data.targets, targets);
            assert_eq!(data.time, time);
            assert_eq!(data.last_sample_time, sample_time);
            assert_eq!(data.last_presentation_time, presentation_time);
        }
        // A narrow range can clip input. Widening must recover the original
        // peak, not attempt to invert the clipped display value.
        for (range, expected) in [(20.0, 0.0), (100.0, 0.65)] {
            radial.configure(&VisualParameters::from(&Config {
                audio_visualizer_db_range: Some(range),
                ..Config::default()
            }));
            let data = radial.data.borrow();
            assert_eq!(data.displayed, vec![expected; 4]);
            assert_eq!(data.time, time);
            assert_eq!(data.last_sample_time, sample_time);
            assert_eq!(data.last_presentation_time, presentation_time);
        }
    }

    #[test]
    fn stop_retires_only_drawing_and_fades_the_painted_opacity() {
        let mut radial = RadialRenderer::new();
        accept(&mut radial, &frame());
        let start = Instant::now();
        radial.stop(start, true);
        assert!(radial.idle_fading(start));
        {
            let data = radial.data.borrow();
            assert_eq!(data.displayed, vec![0.5; 4]);
            assert!(data.targets.is_empty());
            assert!(data.mapping.ranges.is_empty());
            assert!(data.source_generation.is_none());
            assert!(data.last_sample_time.is_none());
            assert!(data.last_presentation_time.is_none());
            // This is the alpha used by the radial paint overlay, not just a timer.
            for (elapsed, expected) in [(0, 1.0), (500, 0.75), (1000, 0.5), (2000, 0.0)] {
                assert_eq!(
                    radial_opacity(
                        &data,
                        PlaybackStatus::Stopped,
                        start + Duration::from_millis(elapsed),
                    ),
                    expected
                );
            }
        }
        assert!(!radial.animate(Duration::from_secs(100)));
        radial.advance_idle_fade(start + Duration::from_secs(1));
        assert!(radial.idle_fading(start + Duration::from_secs(1)));
        // A delayed frame must still wake once to release the expired picture.
        assert!(radial.idle_fading(start + Duration::from_secs(3)));
        radial.advance_idle_fade(start + Duration::from_secs(3));
        assert!(!radial.idle_fading(start + Duration::from_secs(3)));
        assert_eq!(radial.data.borrow().displayed.capacity(), 0);
        radial.begin_idle_fade(start + Duration::from_secs(4));
        assert!(!radial.idle_fading(start + Duration::from_secs(4)));
    }

    #[test]
    fn disabled_fade_resume_hide_and_invalid_signal_discard_retired_drawing() {
        for boundary in 0..5 {
            let mut radial = RadialRenderer::new();
            accept(&mut radial, &frame());
            let start = Instant::now();
            radial.stop(start, boundary != 0);
            match boundary {
                0 => {}
                1 => {
                    radial.configure(&VisualParameters::from(&Config::default()));
                }
                2 => radial.retire_idle_fade(),
                3 => {
                    radial.set_visible(false);
                }
                _ => {
                    let mut invalid = frame();
                    invalid.settings = None;
                    radial.update(&invalid, invalid.sample_time);
                }
            }
            assert!(!radial.idle_fading(start));
            assert!(radial.data.borrow().displayed.is_empty());
            assert!(!radial.animate(Duration::from_secs(100)));
        }
    }

    #[test]
    fn restored_radial_keeps_picture_only_until_validity_is_known() {
        let mut radial = RadialRenderer::new();
        accept(&mut radial, &frame());
        radial.set_visible(false);
        radial.set_visible(true);
        assert_eq!(radial.data.borrow().displayed, vec![0.5; 4]);
        let mut invalid = frame();
        invalid.settings = None;
        radial.update(&invalid, invalid.sample_time);
        assert!(radial.data.borrow().displayed.is_empty());
        assert!(!radial.animate(Duration::from_secs(100)));
    }

    #[test]
    fn native_ticks_smooth_and_rotate_between_five_hz_inputs() {
        let mut radial = RadialRenderer::new();
        let mut audio = frame();
        let start = audio.sample_time;
        radial.update(&audio, start);
        let initial_rotation = radial.data.borrow().time;
        audio.sample_time += Duration::from_millis(200);
        audio.spectrum_db.fill(-70.0);
        radial.update(&audio, start);
        assert_eq!(radial.data.borrow().displayed, vec![0.5; 4]);
        let rebuilds = radial.data.borrow().mapping.rebuilds;
        let capacity = radial.data.borrow().displayed.capacity();
        for tick in 1..=12 {
            let now = start + Duration::from_micros(16_000 * tick);
            let previous = radial.data.borrow().displayed.clone();
            assert!(radial.animate(now));
            let levels = radial.data.borrow().displayed.clone();
            assert!(levels.iter().zip(previous).all(|(next, old)| *next < old));
            assert!(!radial.animate(now));
            assert_eq!(radial.data.borrow().displayed, levels);
            assert_eq!(
                radial.data.borrow().last_sample_time,
                Some(audio.sample_time)
            );
        }
        assert_ne!(radial.data.borrow().time, initial_rotation);
        assert_eq!(radial.data.borrow().mapping.rebuilds, rebuilds);
        assert_eq!(radial.data.borrow().displayed.capacity(), capacity);
        audio.spectrum_db.fill(0.0);
        radial.update(&audio, start + Duration::from_millis(192));
        assert!(
            radial
                .data
                .borrow()
                .targets
                .iter()
                .all(|target| *target == 0.0)
        );
    }

    #[test]
    fn invalid_and_hidden_radial_inputs_retire_animation() {
        let mut radial = RadialRenderer::new();
        let mut audio = frame();
        radial.update(&audio, audio.sample_time);
        audio.spectrum_db.clear();
        radial.update(&audio, audio.sample_time);
        assert!(!radial.animate(audio.sample_time + Duration::from_millis(16)));
        assert!(radial.data.borrow().displayed.is_empty());
        let audio = frame();
        radial.update(&audio, audio.sample_time);
        radial.set_visible(false);
        assert!(!radial.animate(audio.sample_time + Duration::from_millis(16)));
    }

    #[test]
    fn radial_rate_rewind_and_gap_initialize_without_old_smoothing() {
        for discontinuity in 0..3 {
            let mut radial = RadialRenderer::new();
            let mut audio = frame();
            accept(&mut radial, &audio);
            match discontinuity {
                0 => audio.sample_rate = 44_100,
                1 => audio.sample_time -= Duration::from_millis(100),
                _ => {
                    audio.sample_time += Duration::from_millis(501);
                    audio.generation += 1;
                }
            }
            audio.spectrum_db.fill(-70.0);
            radial.update(&audio, audio.sample_time);
            assert_eq!(radial.data.borrow().displayed, vec![0.0; 4]);
            assert!(!radial.animate(audio.sample_time));
        }
    }

    #[test]
    fn slow_radial_publication_does_not_reset_the_displayed_envelope() {
        let mut radial = RadialRenderer::new();
        let mut audio = frame();
        accept(&mut radial, &audio);
        audio.sample_time += Duration::from_secs(1);
        audio.spectrum_db.fill(-70.0);
        radial.update(&audio, audio.sample_time);
        assert_eq!(radial.data.borrow().displayed, vec![0.5; 4]);
        assert_eq!(radial.data.borrow().targets, vec![0.0; 4]);
    }

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
                let mut radial = RadialRenderer::new();
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
                accept(&mut radial, &next);
                let first: Vec<_> = expected
                    .unwrap_or_default()
                    .iter()
                    .map(|target| 0.0 + (*target - 0.0) * 1.0)
                    .collect();
                assert_eq!(radial.data.borrow().displayed, first);
                next.sample_time += Duration::from_millis(10);
                next.spectrum_db.fill(-20.0);
                let expected = reference_levels(&next, 70.0);
                accept(&mut radial, &next);
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
            assert!(mapping.prepare(48_000, 8192, &frequencies).is_some());
        }
        assert_eq!(mapping.rebuilds, 1);
        assert_eq!(mapping.ranges.len(), 32);
        frequencies[10] += 1.0;
        mapping.prepare(48_000, 8192, &frequencies).unwrap();
        assert_eq!(mapping.rebuilds, 2);
        mapping.prepare(8_000, 8192, &frequencies).unwrap();
        assert_eq!(mapping.rebuilds, 3);
        mapping.prepare(8_000, 16384, &frequencies).unwrap();
        assert_eq!(mapping.rebuilds, 4);
        frequencies[10] = f32::NAN;
        mapping.prepare(8_000, 16384, &frequencies).unwrap();
        mapping.prepare(8_000, 16384, &frequencies).unwrap();
        assert_eq!(mapping.rebuilds, 5);
        frequencies = (0..=MAX_RADIAL_AXIS)
            .map(|bin| bin as f32 * 48_000.0 / (MAX_RADIAL_AXIS - 1) as f32)
            .collect();
        mapping.prepare(48_000, 32768, &frequencies).unwrap();
        assert!(mapping.frequencies.is_empty());
        assert_eq!(mapping.ranges.len(), FANCY_BANDS);
    }

    #[test]
    fn duplicate_input_and_display_timestamp_preserve_partial_decay() {
        for oversized in [false, true] {
            let mut radial = RadialRenderer::new();
            let mut audio = frame();
            if oversized {
                audio.frequencies_hz = (0..=MAX_RADIAL_AXIS)
                    .map(|bin| bin as f32 * 48_000.0 / (MAX_RADIAL_AXIS - 1) as f32)
                    .collect();
                audio.spectrum_db = vec![-35.0; audio.frequencies_hz.len()];
            }
            accept(&mut radial, &audio);
            audio.sample_time += Duration::from_millis(10);
            audio.spectrum_db.fill(-70.0);
            accept(&mut radial, &audio);
            let displayed = radial.data.borrow().displayed.clone();
            assert!(displayed.iter().all(|level| *level > 0.0 && *level < 0.5));
            let rotation = radial.data.borrow().time;
            for _ in 0..10 {
                audio.sequence += 1;
                accept(&mut radial, &audio);
                assert_eq!(radial.data.borrow().displayed, displayed);
                assert_eq!(radial.data.borrow().time, rotation);
            }
            audio.sample_time += Duration::from_millis(10);
            accept(&mut radial, &audio);
            assert!(
                radial
                    .data
                    .borrow()
                    .displayed
                    .iter()
                    .zip(&displayed)
                    .all(|(next, previous)| next < previous)
            );
        }
    }

    #[test]
    fn same_time_can_remap_after_configuration_or_axis_change() {
        let mut radial = RadialRenderer::new();
        let mut audio = frame();
        accept(&mut radial, &audio);
        assert!(radial.configure(&VisualParameters::from(&Config {
            audio_visualizer_db_range: Some(50.0),
            ..Config::default()
        })));
        assert_eq!(radial.data.borrow().displayed, vec![0.3; 4]);
        audio.frequencies_hz = vec![20.0, 100.0, 20_000.0];
        audio.spectrum_db = vec![-50.0; 3];
        accept(&mut radial, &audio);
        assert_eq!(radial.data.borrow().displayed, vec![0.0; 3]);
        assert_eq!(
            radial.data.borrow().last_sample_time,
            Some(audio.sample_time)
        );
    }

    #[test]
    fn same_rate_new_generation_resets_radial_smoothing() {
        let mut radial = RadialRenderer::new();
        accept(&mut radial, &frame());
        let mut next = frame();
        // A reset must accept the same time, not just a later publication.
        next.generation = 2;
        next.spectrum_db.fill(-70.0);
        accept(&mut radial, &next);
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
                analysis_window: rivu_format::config::AnalysisWindow::Hann,
                ..Config::default()
            },
            Config {
                visualization_signal: rivu_format::config::VisualizationSignal::Before,
                ..Config::default()
            },
        ] {
            let mut radial = RadialRenderer::new();
            accept(&mut radial, &frame());
            radial.configure(&VisualParameters::from(&config));
            accept(&mut radial, &frame());
            assert!(radial.data.borrow().displayed.is_empty());
            let mut next = frame();
            next.settings = Some(AnalysisSettings::from(&config));
            next.generation = 2;
            accept(&mut radial, &next);
            assert_eq!(radial.data.borrow().displayed, vec![0.5; 4]);
        }
    }

    #[test]
    fn hidden_radial_resumes_directly_from_latest_frame() {
        let mut radial = RadialRenderer::new();
        accept(&mut radial, &frame());
        radial.set_visible(false);
        accept(&mut radial, &frame());
        assert_eq!(radial.data.borrow().displayed, vec![0.5; 4]);
        assert_eq!(radial.data.borrow().mapping.frequencies.capacity(), 0);
        assert_eq!(radial.data.borrow().mapping.ranges.capacity(), 0);
        radial.set_visible(true);
        assert_eq!(radial.data.borrow().displayed, vec![0.5; 4]);
        assert!(radial.data.borrow().targets.is_empty());
        assert_eq!(radial.data.borrow().targets.capacity(), 0);
        assert!(!radial.animate(Duration::from_secs(100)));
        let mut unavailable = frame();
        unavailable.spectrum_db.clear();
        accept(&mut radial, &unavailable);
        assert!(radial.data.borrow().displayed.is_empty());
        let mut latest = frame();
        latest.spectrum_db.fill(-70.0);
        accept(&mut radial, &latest);
        assert_eq!(radial.data.borrow().displayed, vec![0.0; 4]);
    }

    #[test]
    fn layout_pruning_keeps_shared_radial_output_and_active_working_state() {
        let mut coordinator = crate::StyleCoordinator::new();
        accept(&mut coordinator.radial, &frame());
        coordinator.radial.set_visible(false);
        for style in [
            AudioVisualizerStyle::Bars,
            AudioVisualizerStyle::Rings,
            AudioVisualizerStyle::BarsRings,
        ] {
            coordinator.retain_presentations([AudioVisualizerOptions {
                style,
                grid: false,
                label: false,
            }]);
            let data = coordinator.radial.data.borrow();
            assert!(!data.visible);
            assert_eq!(data.displayed, vec![0.5; 4]);
            assert!(data.targets.is_empty());
        }
        coordinator.radial.set_visible(true);
        accept(&mut coordinator.radial, &frame());
        let targets = coordinator.radial.data.borrow().targets.clone();
        let presentation_time = coordinator.radial.data.borrow().last_presentation_time;
        coordinator.retain_presentations([AudioVisualizerOptions {
            style: AudioVisualizerStyle::BarsRings,
            grid: false,
            label: false,
        }]);
        assert_eq!(coordinator.radial.data.borrow().targets, targets);
        assert_eq!(
            coordinator.radial.data.borrow().last_presentation_time,
            presentation_time
        );
        coordinator.retain_presentations([]);
        let data = coordinator.radial.data.borrow();
        assert!(data.visible);
        assert!(data.displayed.is_empty());
        assert!(data.targets.is_empty());
        assert_eq!(data.mapping.frequencies.capacity(), 0);
        assert_eq!(data.mapping.ranges.capacity(), 0);
    }

    #[test]
    fn other_view_overrides_do_not_reset_radial_display() {
        let mut radial = RadialRenderer::new();
        accept(&mut radial, &frame());
        let config = Config {
            spectrum_db_range: Some(40.0),
            spectrogram_db_range: Some(100.0),
            ..Config::default()
        };
        radial.configure(&VisualParameters::from(&config));
        let data = radial.data.borrow();
        assert_eq!(data.displayed, vec![0.5; 4]);
        assert_eq!(data.last_sample_time, Some(Duration::from_secs(1)));
    }

    #[test]
    fn common_range_changes_remap_only_inherited_radial_display() {
        for range_override in [None, Some(70.0)] {
            let mut radial = RadialRenderer::new();
            let mut config = Config {
                audio_visualizer_db_range: range_override,
                ..Config::default()
            };
            radial.configure(&VisualParameters::from(&config));
            accept(&mut radial, &frame());
            config.visualization_db_range = 100.0;
            radial.configure(&VisualParameters::from(&config));
            let data = radial.data.borrow();
            assert_eq!(data.last_sample_time, Some(Duration::from_secs(1)));
            let expected = if range_override.is_some() { 0.5 } else { 0.65 };
            assert_eq!(data.displayed, vec![expected; 4]);
        }
    }

    #[test]
    fn radial_override_remaps_frozen_input_without_new_publication() {
        let mut radial = RadialRenderer::new();
        accept(&mut radial, &frame());
        radial.configure(&VisualParameters::from(&Config {
            visualization_db_range: 100.0,
            spectrum_db_range: Some(40.0),
            audio_visualizer_db_range: Some(50.0),
            ..Config::default()
        }));
        let data = radial.data.borrow();
        assert_eq!(data.displayed, vec![0.3; 4]);
    }
}
