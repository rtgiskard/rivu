//! Spectrum envelopes, per-panel bin mapping, geometry, and GPU publications.
use super::{
    FrequencyState, HeatSample, axis_frequency, axis_label_margin, clipped_range, db_height,
    db_label, finite_level, heat_sample, plot_bounds, stop_color,
};
use crate::{
    VisualParameters,
    labels::{paint_aligned_label, paint_label},
    palette::SPECTRUM_GPU_PALETTE_SIZE,
};
use gpui::{
    App, Bounds, GpuFrame, GpuLimits, GpuPathParams, GpuRectangle, GpuRectangleParams, GpuSource,
    PathBuilder, Pixels, Point, SharedString, Window, fill, point, px, rgb, size,
};
use rivu_format::config::{SpectrumBarLayout, SpectrumStyle};
use std::{collections::HashMap, ops::Range, sync::Arc, time::Duration};

pub(super) const MAX_SPECTRUM_BARS: usize = 2000;

pub(super) struct Spectrum {
    pub(super) labels: [SharedString; 2],
    pub(super) db_labels: [SharedString; 4],
    pub(super) peaks: Vec<f32>,
    pub(super) peak_deadlines: Vec<Duration>,
    pub(super) bar_levels: Vec<f32>,
    pub(super) bar_deadlines: Vec<Duration>,
    pub(super) smoothed_levels: Vec<f32>,
    pub(super) visible: bool,
    pub(super) envelope_initialized: bool,
    pub(super) points: Vec<Point<Pixels>>,
    pub(super) rectangles: Vec<GpuRectangle>,
    pub(super) mappings: HashMap<u64, SpectrumMapping>,
    pub(super) presentations: HashMap<u64, SpectrumPresentation>,
    pub(super) last_update: Option<Duration>,
    pub(super) top_db: f32,
    pub(super) db_range: f32,
    pub(super) bar_layout: SpectrumBarLayout,
    pub(super) interpolate: bool,
    pub(super) style: SpectrumStyle,
    pub(super) gap: f32,
    pub(super) show_peaks: bool,
    pub(super) peak_hold_ms: u32,
    pub(super) peak_gravity: f32,
    pub(super) bar_hold_ms: u32,
    pub(super) bar_gravity: f32,
    pub(super) smoothing_ms: u32,
}

impl Spectrum {
    pub(super) fn new(config: &VisualParameters) -> Self {
        Self {
            labels: ["20 Hz".into(), "20 kHz".into()],
            db_labels: std::array::from_fn(|step| {
                db_label(-config.spectrum_db_range() * step as f32 / 3.0)
            }),
            peaks: Vec::new(),
            peak_deadlines: Vec::new(),
            bar_levels: Vec::new(),
            bar_deadlines: Vec::new(),
            smoothed_levels: Vec::new(),
            visible: true,
            envelope_initialized: false,
            points: Vec::new(),
            rectangles: Vec::new(),
            mappings: HashMap::new(),
            presentations: HashMap::new(),
            last_update: None,
            top_db: 0.0,
            db_range: config.spectrum_db_range(),
            bar_layout: config.spectrum_bar_layout,
            interpolate: config.spectrum_interpolate,
            style: config.spectrum_style,
            gap: config.spectrum_gap,
            show_peaks: config.spectrum_peaks,
            peak_hold_ms: config.spectrum_peak_hold_ms,
            peak_gravity: config.spectrum_peak_gravity,
            bar_hold_ms: config.spectrum_bar_hold_ms,
            bar_gravity: config.spectrum_bar_gravity,
            smoothing_ms: config.spectrum_smoothing_ms,
        }
    }
}

#[derive(PartialEq)]
pub(super) struct SpectrumGroupingKey {
    pub(super) bars: usize,
    pub(super) generation: u64,
    pub(super) low: f32,
    pub(super) high: f32,
}

pub(super) struct SpectrumMapping {
    pub(super) grouping_key: SpectrumGroupingKey,
    pub(super) bins: Vec<Range<usize>>,
    pub(super) samples: Vec<Option<HeatSample>>,
}

pub(super) struct SpectrumPresentation {
    pub(super) body: Option<Arc<GpuFrame>>,
    pub(super) peaks: Vec<(Bounds<f32>, u32)>,
    pub(super) labels: [SharedString; 2],
    pub(super) db_labels: [SharedString; 4],
}

impl FrequencyState {
    pub(super) fn advance_spectrum(&mut self, now: Duration) -> bool {
        if !self.spectrum.visible || !self.spectrum.envelope_initialized {
            return false;
        }
        let last = self.spectrum.last_update.unwrap_or(now);
        if now <= last {
            return false;
        }
        self.spectrum.last_update = Some(now);
        let decay = smoothing_decay(now - last, self.spectrum.smoothing_ms);
        let mut changed = false;
        for index in 0..self.spectrum.peaks.len() {
            let before = self.display_level(index);
            let peak_before = self.spectrum.peaks[index];
            let value = finite_level(self.latest_levels[index]);
            // Only accepted input can renew hold deadlines.
            self.spectrum.peaks[index] = advance_envelope(
                self.spectrum.peaks[index],
                self.spectrum.peak_deadlines[index],
                value,
                last,
                now,
                self.spectrum.peak_hold_ms,
                self.spectrum.peak_gravity,
            )
            .0;
            self.spectrum.bar_levels[index] = advance_envelope(
                self.spectrum.bar_levels[index],
                self.spectrum.bar_deadlines[index],
                value,
                last,
                now,
                self.spectrum.bar_hold_ms,
                self.spectrum.bar_gravity,
            )
            .0;
            self.spectrum.smoothed_levels[index] =
                smooth_release(self.spectrum.smoothed_levels[index], value, decay);
            changed |=
                before != self.display_level(index) || peak_before != self.spectrum.peaks[index];
        }
        changed
    }

    pub(super) fn display_level(&self, index: usize) -> f32 {
        let level = finite_level(self.spectrum.bar_levels[index]);
        if self.spectrum.bar_gravity == 0.0 {
            level
        } else {
            level.max(finite_level(self.spectrum.smoothed_levels[index]))
        }
    }

    pub(super) fn interpolate_level(&self, sample: Option<HeatSample>, peaks: bool) -> f32 {
        let Some(HeatSample {
            left,
            right,
            fraction,
        }) = sample
        else {
            return f32::NEG_INFINITY;
        };
        let level = |index| {
            if peaks {
                finite_level(self.spectrum.peaks[index])
            } else {
                self.display_level(index)
            }
        };
        let a = level(left);
        let b = level(right);
        if left == right {
            return b;
        }
        let floor = self.spectrum.top_db - self.spectrum.db_range;
        a.max(floor) + (b.max(floor) - a.max(floor)) * fraction
    }

    pub(super) fn release_spectrum(&mut self) {
        self.spectrum.peaks = Vec::new();
        self.spectrum.peak_deadlines = Vec::new();
        self.spectrum.bar_levels = Vec::new();
        self.spectrum.bar_deadlines = Vec::new();
        self.spectrum.smoothed_levels = Vec::new();
        self.spectrum.points = Vec::new();
        self.spectrum.rectangles = Vec::new();
        self.spectrum.mappings = HashMap::new();
        self.spectrum.envelope_initialized = false;
        self.spectrum.last_update = None;
    }

    pub(super) fn spectrum_range(&self) -> Option<(usize, usize, f32, f32)> {
        clipped_range(
            &self.frequencies,
            self.sample_rate,
            16.0,
            self.sample_rate as f32 * 0.5,
        )
    }

    pub(super) fn refresh_db_labels(&mut self) {
        self.spectrum.db_labels = std::array::from_fn(|step| {
            db_label(self.spectrum.top_db - self.spectrum.db_range * step as f32 / 3.0)
        });
    }

    pub(super) fn spectrum_render_range(&self) -> Option<(usize, usize, f32, f32)> {
        // Hidden views keep history current but do not advance envelopes. A
        // restored view must wait for a real frame before indexing those arrays.
        if !self.spectrum.visible
            || !self.spectrum.envelope_initialized
            || self.spectrum.bar_levels.len() != self.frequencies.len()
            || self.spectrum.smoothed_levels.len() != self.frequencies.len()
            || self.spectrum.peaks.len() != self.frequencies.len()
        {
            return None;
        }
        self.spectrum_range()
    }

    pub(super) fn prepare_spectrum_bins(
        &mut self,
        panel_id: u64,
        bars: usize,
        low: f32,
        high: f32,
    ) {
        let key = SpectrumGroupingKey {
            bars,
            generation: self.stream_generation,
            low,
            high,
        };
        let mapping = self
            .spectrum
            .mappings
            .entry(panel_id)
            .or_insert_with(|| SpectrumMapping {
                grouping_key: SpectrumGroupingKey {
                    bars: 0,
                    generation: 0,
                    low: 0.0,
                    high: 0.0,
                },
                bins: Vec::new(),
                samples: Vec::new(),
            });
        if mapping.grouping_key != key {
            group_bands(&self.frequencies, low, high, bars, true, &mut mapping.bins);
            mapping.samples.clear();
            mapping.grouping_key = key;
        }
        if self.spectrum.interpolate && mapping.samples.len() != bars {
            spectrum_samples(&self.frequencies, low, high, bars, &mut mapping.samples);
        }
    }

    pub(super) fn spectrum_bin(&self, panel_id: u64, bar: usize) -> Range<usize> {
        self.spectrum.mappings[&panel_id].bins[bar].clone()
    }

    pub(super) fn spectrum_sample(&self, panel_id: u64, bar: usize) -> Option<HeatSample> {
        self.spectrum.mappings[&panel_id].samples[bar]
    }

    pub(super) fn prepare_spectrum_body(
        &mut self,
        panel_id: u64,
        plot: Bounds<Pixels>,
        bars: usize,
        low: f32,
        high: f32,
    ) -> Result<Option<Arc<GpuFrame>>, String> {
        self.prepare_spectrum_bins(panel_id, bars, low, high);
        self.spectrum.points.clear();
        self.spectrum.rectangles.clear();
        let style = self.spectrum.style;
        let bottom = plot.bottom();
        let top = plot.top();
        let continuous = matches!(style, SpectrumStyle::Line | SpectrumStyle::Solid);
        let rectangles = !continuous;
        let rectangle_capacity = if style == SpectrumStyle::Outline {
            bars * 4
        } else {
            bars
        };
        if continuous {
            // Reserve once before the point loop; clear retains capacity for later frames.
            self.spectrum.points.reserve(bars);
        }
        if rectangles {
            self.spectrum.rectangles.reserve(rectangle_capacity);
        }
        // Fill contours share clockwise winding. NonZero keeps overlapping
        // body regions filled instead of cutting even-odd holes.
        let mut shape_path = if rectangles {
            None
        } else if style == SpectrumStyle::Line {
            Some(PathBuilder::stroke(px(2.0)))
        } else {
            Some(PathBuilder::fill().with_style(gpui::PathStyle::Fill(
                gpui::FillOptions::default().with_fill_rule(gpui::FillRule::NonZero),
            )))
        };
        let mut any_visible = false;
        for bar in 0..bars {
            let left_t = bar as f32 / bars as f32;
            let right_t = (bar + 1) as f32 / bars as f32;
            let range = self.spectrum_bin(panel_id, bar);
            let mut level = f32::NEG_INFINITY;
            for index in range.clone() {
                level = level.max(self.display_level(index));
            }
            if self.spectrum.interpolate {
                // Interpolation fills undersampled slots, but never averages
                // away a transient already max-pooled into this slot.
                level =
                    level.max(self.interpolate_level(self.spectrum_sample(panel_id, bar), false));
            }
            let slot_width = plot.size.width * (right_t - left_t);
            let slot_gap = px(self.spectrum.gap)
                .max(px(0.0))
                .min((slot_width - px(1.0)).max(px(0.0)));
            let level_fraction = db_height(level, self.spectrum.top_db, self.spectrum.db_range);
            let bar_height = (plot.size.height * level_fraction).clamp(px(0.0), plot.size.height);
            any_visible |= bar_height > px(0.0);
            let (x, width) = match style {
                SpectrumStyle::Line | SpectrumStyle::Solid => {
                    (plot.left() + plot.size.width * left_t, slot_width)
                }
                _ => (
                    plot.left() + plot.size.width * left_t + slot_gap * 0.5,
                    (slot_width - slot_gap).max(px(1.0)),
                ),
            };
            if (style == SpectrumStyle::Line && level.is_finite())
                || (style != SpectrumStyle::Line && (continuous || bar_height > px(0.0)))
            {
                match style {
                    SpectrumStyle::Bars | SpectrumStyle::Led => {
                        let height = if style == SpectrumStyle::Bars {
                            bar_height.max(px(1.0)).min(plot.size.height)
                        } else {
                            bar_height
                        };
                        let normalized_height = height / plot.size.height;
                        let normalized_x = ((x - plot.left()) / plot.size.width).clamp(0.0, 1.0);
                        // Match the existing plot mask at the right edge and avoid
                        // rejecting a last-slot rounding overshoot at publication.
                        let normalized_width = (width / plot.size.width).min(1.0 - normalized_x);
                        self.spectrum.rectangles.push(GpuRectangle {
                            bounds: [
                                normalized_x,
                                1.0 - normalized_height,
                                normalized_width,
                                normalized_height,
                            ],
                            palette_range: [0.0, normalized_height],
                        });
                    }
                    SpectrumStyle::Outline => {
                        let height = bar_height.max(px(1.0)).min(plot.size.height);
                        let edge = px(1.0).min(width * 0.5).min(height * 0.5);
                        append_spectrum_rectangle(
                            &mut self.spectrum.rectangles,
                            plot,
                            Bounds::new(point(x, bottom - height), size(width, edge)),
                        );
                        if height > edge {
                            append_spectrum_rectangle(
                                &mut self.spectrum.rectangles,
                                plot,
                                Bounds::new(point(x, bottom - edge), size(width, edge)),
                            );
                        }
                        if height > edge * 2.0 {
                            append_spectrum_rectangle(
                                &mut self.spectrum.rectangles,
                                plot,
                                Bounds::new(
                                    point(x, bottom - height + edge),
                                    size(edge, height - edge * 2.0),
                                ),
                            );
                            append_spectrum_rectangle(
                                &mut self.spectrum.rectangles,
                                plot,
                                Bounds::new(
                                    point(x + width - edge, bottom - height + edge),
                                    size(edge, height - edge * 2.0),
                                ),
                            );
                        }
                    }
                    SpectrumStyle::Line | SpectrumStyle::Solid => {
                        // Only the stroked line needs clearance from the plot edge.
                        // Filled areas retain zero and subpixel signal heights.
                        let y = if style == SpectrumStyle::Line {
                            (bottom - bar_height).clamp(top + px(1.0), bottom - px(1.0))
                        } else {
                            bottom - bar_height
                        };
                        self.spectrum.points.push(point(x + width * 0.5, y));
                    }
                }
            }
        }
        if continuous && any_visible {
            append_spectrum_shape(
                shape_path.as_mut().unwrap(),
                &self.spectrum.points,
                plot,
                style == SpectrumStyle::Solid,
                style == SpectrumStyle::Line || self.spectrum.interpolate,
            );
        }
        let body = if !any_visible {
            None
        } else if rectangles {
            let params = if style == SpectrumStyle::Led {
                GpuRectangleParams {
                    cell_height: 3.0,
                    cell_gap: 2.0,
                }
            } else {
                GpuRectangleParams::default()
            };
            let frame = GpuFrame::rectangles(
                &self.spectrum.rectangles,
                self.palette_lut.spectrum_gpu_palette()?,
                params,
                GpuLimits {
                    max_rectangles: rectangle_capacity,
                    max_palette_colors: SPECTRUM_GPU_PALETTE_SIZE,
                    ..GpuLimits::default()
                },
            )
            .map_err(|error| format!("Cannot create spectrum GPU rectangles: {error}"))?;
            Some(frame)
        } else {
            let mut path = shape_path
                .unwrap()
                .build()
                .map_err(|error| format!("Cannot tessellate spectrum: {error}"))?;
            // Geometry and palette stay in the full plot domain, not the body's bounds.
            path.bounds = plot;
            // The existing builder consumes at most MAX_SPECTRUM_BARS points;
            // publish its complete tessellation without padding or truncation.
            let path_vertices = path.vertices.len();
            Some(
                GpuFrame::path(
                    Arc::new(path),
                    self.palette_lut.spectrum_gpu_palette()?,
                    GpuPathParams::default(),
                    GpuLimits {
                        max_path_vertices: path_vertices,
                        max_palette_colors: SPECTRUM_GPU_PALETTE_SIZE,
                        ..GpuLimits::default()
                    },
                )
                .map_err(|error| format!("Cannot create spectrum GPU path: {error}"))?,
            )
        };
        self.spectrum.rectangles.clear();
        Ok(body)
    }

    pub(super) fn prepare_spectrum_presentation(
        &mut self,
        panel_id: u64,
        plot: Bounds<Pixels>,
    ) -> Result<(), String> {
        let Some((_, _, low, high)) = self.spectrum_render_range() else {
            return Ok(());
        };
        let bars = spectrum_bar_count(
            plot.size.width / px(1.0),
            self.spectrum.style == SpectrumStyle::Solid,
            self.spectrum.bar_layout,
            self.spectrum.gap,
        );
        let body = self.prepare_spectrum_body(panel_id, plot, bars, low, high)?;
        let mut peaks = self
            .spectrum
            .presentations
            .get_mut(&panel_id)
            .map(|presentation| std::mem::take(&mut presentation.peaks))
            .unwrap_or_default();
        peaks.clear();
        if self.spectrum.show_peaks {
            for bar in 0..bars {
                let left_t = bar as f32 / bars as f32;
                let right_t = (bar + 1) as f32 / bars as f32;
                let mut peak = f32::NEG_INFINITY;
                for index in self.spectrum_bin(panel_id, bar) {
                    peak = peak.max(finite_level(self.spectrum.peaks[index]));
                }
                if self.spectrum.interpolate {
                    peak =
                        peak.max(self.interpolate_level(self.spectrum_sample(panel_id, bar), true));
                }
                let fraction = db_height(peak, self.spectrum.top_db, self.spectrum.db_range);
                if fraction <= 0.0 {
                    continue;
                }
                let slot_width = plot.size.width * (right_t - left_t);
                let gap = px(self.spectrum.gap)
                    .max(px(0.0))
                    .min((slot_width - px(1.0)).max(px(0.0)));
                let (x, width) = match self.spectrum.style {
                    SpectrumStyle::Line | SpectrumStyle::Solid => {
                        (plot.size.width * left_t, slot_width)
                    }
                    _ => (
                        plot.size.width * left_t + gap * 0.5,
                        (slot_width - gap).max(px(1.0)),
                    ),
                };
                let peak_width = match self.spectrum.style {
                    SpectrumStyle::Solid => px(1.0),
                    SpectrumStyle::Line => (slot_width - gap).max(px(1.0)),
                    _ => width,
                };
                let peak_x = (x + (width - peak_width) * 0.5)
                    .clamp(px(0.0), (plot.size.width - peak_width).max(px(0.0)));
                let peak_y = (plot.size.height * (1.0 - fraction))
                    .clamp(px(0.0), plot.size.height - px(1.0));
                peaks.push((
                    Bounds::new(
                        point(peak_x / plot.size.width, peak_y / plot.size.height),
                        size(peak_width / plot.size.width, px(1.0) / plot.size.height),
                    ),
                    stop_color(self.palette_lut.lookup_mapped_rgb(fraction)),
                ));
            }
        }
        self.spectrum.presentations.insert(
            panel_id,
            SpectrumPresentation {
                body,
                peaks,
                labels: self.spectrum.labels.clone(),
                db_labels: self.spectrum.db_labels.clone(),
            },
        );
        Ok(())
    }

    pub(super) fn paint_spectrum(
        &mut self,
        panel_id: u64,
        bounds: Bounds<Pixels>,
        options: rivu_format::config::AudioVisualizerOptions,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<(), String> {
        let show_labels = spectrum_labels_visible(options.labels, bounds.size.height);
        let labels = self
            .spectrum
            .presentations
            .get(&panel_id)
            .map(|presentation| &presentation.labels)
            .unwrap_or(&self.spectrum.labels);
        let left_margin = axis_label_margin(show_labels, labels);
        let bottom_margin = if show_labels { 22.0 } else { 8.0 };
        let plot = Bounds::new(
            point(bounds.left() + px(left_margin), bounds.top() + px(8.0)),
            size(
                (bounds.size.width - px(left_margin + 8.0)).max(px(1.0)),
                (bounds.size.height - px(bottom_margin + 8.0)).max(px(1.0)),
            ),
        );
        if plot.size.width <= px(2.0) || plot.size.height <= px(2.0) {
            return Ok(());
        }
        let prepared = self.prepare_spectrum_presentation(panel_id, plot);
        let Some(presentation) = self.spectrum.presentations.get(&panel_id) else {
            return prepared;
        };
        let mut grid_path = PathBuilder::stroke(px(1.0));
        for (step, label) in presentation.db_labels.iter().enumerate() {
            let fraction = step as f32 / (presentation.db_labels.len() - 1) as f32;
            let y = plot.top() + plot.size.height * fraction;
            if options.reference_lines {
                grid_path.move_to(point(plot.left(), y));
                grid_path.line_to(point(plot.right(), y));
            }
            if show_labels {
                paint_aligned_label(
                    label.clone(),
                    point(plot.left() - px(5.0), y - px(5.0)),
                    1.0,
                    window,
                    cx,
                );
            }
        }
        if options.reference_lines {
            let path = grid_path
                .build()
                .map_err(|error| format!("Cannot tessellate spectrum grid: {error}"))?;
            window.paint_path(path, rgb(0x14161b));
        }
        window.with_content_mask(Some(gpui::ContentMask { bounds: plot }), |window| {
            if let Some(body) = &presentation.body {
                window
                    .paint_gpu(plot, GpuSource::Fixed(body.clone()))
                    .map_err(|error| format!("Cannot paint spectrum GPU body: {error}"))?;
            }
            for (bounds, color) in &presentation.peaks {
                window.paint_quad(fill(plot_bounds(plot, *bounds), rgb(*color)));
            }
            Ok::<(), String>(())
        })?;
        if show_labels {
            let y = plot.bottom() + px(3.0);
            paint_label(
                presentation.labels[0].clone(),
                point(plot.left(), y),
                0xb1bbc5,
                window,
                cx,
            );
            paint_aligned_label(
                presentation.labels[1].clone(),
                point(plot.right(), y),
                1.0,
                window,
                cx,
            );
        }
        prepared
    }
}

pub(super) fn spectrum_labels_visible(configured: bool, height: Pixels) -> bool {
    // Four 10px dB labels need separation as well as the bottom note labels.
    configured && height >= px(78.0)
}

/// Ballistic fall in dB/s², integrating only the portion beyond the hold.
/// Zero gravity means an immediate snap after hold, not a suspended peak.
pub(super) fn advance_envelope(
    level: f32,
    deadline: Duration,
    value: f32,
    last: Duration,
    now: Duration,
    hold_ms: u32,
    gravity: f32,
) -> (f32, Duration) {
    if value >= level {
        return (value, now + Duration::from_millis(hold_ms as u64));
    }
    if now < deadline || now <= last {
        return (level, deadline);
    }
    if gravity == 0.0 {
        return (value, deadline);
    }
    let before = last.saturating_sub(deadline).as_secs_f64();
    let after = now.saturating_sub(deadline).as_secs_f64();
    let fallen = level - (0.5 * gravity as f64 * (after * after - before * before)) as f32;
    if fallen <= value {
        (value, now + Duration::from_millis(hold_ms as u64))
    } else {
        (fallen, deadline)
    }
}

pub(super) fn smoothing_decay(elapsed: Duration, smoothing_ms: u32) -> f32 {
    if smoothing_ms == 0 {
        0.0
    } else {
        (-elapsed.as_secs_f32() / (smoothing_ms as f32 * 0.001)).exp()
    }
}

pub(super) fn smooth_release(previous: f32, value: f32, decay: f32) -> f32 {
    if value >= previous || decay == 0.0 {
        return value;
    }
    if decay == 1.0 {
        return previous;
    }
    let floor = -240.0;
    value.max(floor) + (previous.max(floor) - value.max(floor)) * decay
}

pub(super) fn spectrum_bar_count(
    width: f32,
    solid: bool,
    layout: SpectrumBarLayout,
    gap: f32,
) -> usize {
    let drawable = (width.floor() as usize).max(1);
    let count = if solid {
        drawable
    } else {
        match layout {
            SpectrumBarLayout::Automatic { width: bar_width } => {
                (width / (bar_width + gap).max(1.0)).floor() as usize
            }
            SpectrumBarLayout::Fixed { count } => count as usize,
        }
    };
    count.clamp(1, MAX_SPECTRUM_BARS).min(drawable)
}

pub(super) fn shape_y(points: &[Point<Pixels>], x: Pixels, interpolate: bool) -> Pixels {
    let right = points
        .partition_point(|point| point.x < x)
        .min(points.len() - 1);
    let left = right.saturating_sub(1);
    let a = points[left];
    let b = points[right];
    if a.x == b.x {
        return b.y;
    }
    if !interpolate {
        return if x < (a.x + b.x) * 0.5 { a.y } else { b.y };
    }
    let t = ((x - a.x) / (b.x - a.x)).clamp(0.0, 1.0);
    a.y + (b.y - a.y) * t
}

/// Append the continuous body outline without connecting separate Peak markers.
pub(super) fn append_spectrum_shape(
    builder: &mut PathBuilder,
    points: &[Point<Pixels>],
    plot: Bounds<Pixels>,
    solid: bool,
    interpolate: bool,
) {
    if points.is_empty() {
        return;
    }
    if solid {
        builder.move_to(point(plot.left(), plot.bottom()));
        builder.line_to(point(
            plot.left(),
            shape_y(points, plot.left(), interpolate),
        ));
    } else {
        builder.move_to(point(
            plot.left(),
            shape_y(points, plot.left(), interpolate),
        ));
    }
    if interpolate {
        for &vertex in points {
            if vertex.x > plot.left() && vertex.x < plot.right() {
                builder.line_to(vertex);
            }
        }
    } else {
        for pair in points.windows(2) {
            let x = (pair[0].x + pair[1].x) * 0.5;
            if x > plot.left() && x < plot.right() {
                builder.line_to(point(x, pair[0].y));
                builder.line_to(point(x, pair[1].y));
            }
        }
    }
    builder.line_to(point(
        plot.right(),
        shape_y(points, plot.right(), interpolate),
    ));
    if solid {
        builder.line_to(point(plot.right(), plot.bottom()));
        builder.close();
    }
}

pub(super) fn append_spectrum_rectangle(
    rectangles: &mut Vec<GpuRectangle>,
    plot: Bounds<Pixels>,
    bounds: Bounds<Pixels>,
) {
    let x = ((bounds.left() - plot.left()) / plot.size.width).clamp(0.0, 1.0);
    let y = ((bounds.top() - plot.top()) / plot.size.height).clamp(0.0, 1.0);
    let height = (bounds.size.height / plot.size.height).min(1.0 - y);
    rectangles.push(GpuRectangle {
        bounds: [
            x,
            y,
            (bounds.size.width / plot.size.width).min(1.0 - x),
            height,
        ],
        palette_range: [1.0 - y - height, 1.0 - y],
    });
}

pub(super) fn spectrum_samples(
    frequencies: &[f32],
    low: f32,
    high: f32,
    bars: usize,
    samples: &mut Vec<Option<HeatSample>>,
) {
    samples.clear();
    samples.extend((0..bars).map(|bar| {
        let left = bar as f32 / bars as f32;
        let right = (bar + 1) as f32 / bars as f32;
        heat_sample(
            frequencies,
            axis_frequency(low, high, (left + right) * 0.5, true),
            low,
            high,
        )
    }));
}

pub(super) fn group_bands(
    frequencies: &[f32],
    low: f32,
    high: f32,
    count: usize,
    logarithmic: bool,
    groups: &mut Vec<Range<usize>>,
) {
    groups.clear();
    let log_low = low.ln();
    let log_span = high.ln() - log_low;
    let mut start = frequencies.partition_point(|hz| *hz < low);
    for index in 0..count {
        let end = if index + 1 == count {
            frequencies.partition_point(|hz| *hz <= high)
        } else {
            let fraction = (index + 1) as f32 / count as f32;
            let edge = if logarithmic {
                (log_low + log_span * fraction).exp()
            } else {
                low + (high - low) * fraction
            };
            frequencies.partition_point(|hz| *hz < edge)
        };
        groups.push(start..end);
        start = end;
    }
}

pub(super) fn note_label(frequency: f32) -> SharedString {
    const NAMES: [&str; 12] = [
        "C", "C♯", "D", "D♯", "E", "F", "F♯", "G", "G♯", "A", "A♯", "B",
    ];
    let midi = (12.0 * (frequency / 440.0).log2() + 69.0).round() as i32;
    let octave = midi.div_euclid(12) - 1;
    format!("{}{octave}", NAMES[midi.rem_euclid(12) as usize]).into()
}
