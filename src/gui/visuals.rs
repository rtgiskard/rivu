//! GPU-backed analysis views. The audio worker owns the configurable FFT;
//! these views retain every musical band supplied by `AnalysisFrame`.
//!
//! Immutable GPU images cache at most 180 audio-time buckets. Dense incoming
//! frames are max-pooled so narrow transients survive; Spectrum separately
//! retains the latest frame. The selected history duration determines bucket
//! width, never the publication rate or canvas width. Unavailable history is
//! left blank, including long gaps in publication.
//! Replaced images are explicitly removed from the window's atlas. Hidden
//! spectrograms retain reusable CPU buckets but create no new GPU images.
//! Range and axis changes remap cached levels even while paused; style changes
//! redraw Spectrum in place. Unchanged buckets reuse their GPU image.
//! Spectrum max-pools transients and animates only on the advancing audio clock.

use super::{ERROR, ERROR_BG};
use gpui::{
    AnyElement, App, Bounds, PathBuilder, Pixels, Point, RenderImage, SharedString, TextAlign,
    Window, canvas, div, fill, linear_color_stop, linear_gradient, point, prelude::*, px, rgb,
    size,
};
use std::{cell::RefCell, ops::Range, rc::Rc, sync::Arc, time::Duration};

use crate::{
    analysis::AnalysisFrame,
    config::{Config, SpectrumStyle, SpectrumWindow, VisualizationPalette},
};

const HISTORY_COLUMNS: usize = 180;
const MAX_SPECTRUM_BARS: usize = 512;
const DEAD_BEE_F_STOPS: &[(f32, [u8; 3])] = &[
    (0.0, [32, 58, 138]),
    (0.25, [30, 190, 220]),
    (0.5, [75, 205, 120]),
    (0.72, [240, 215, 65]),
    (0.87, [245, 140, 50]),
    (1.0, [235, 65, 65]),
];
const TOKYO_NIGHT_STOPS: &[(f32, [u8; 3])] = &[
    (0.0, [65, 72, 110]),
    (0.2, [187, 154, 247]),
    (0.42, [125, 207, 255]),
    (0.62, [158, 206, 106]),
    (0.82, [224, 175, 104]),
    (1.0, [247, 118, 142]),
];
const NORD_STOPS: &[(f32, [u8; 3])] = &[
    (0.0, [46, 52, 64]),
    (0.2, [94, 129, 172]),
    (0.42, [136, 192, 208]),
    (0.62, [163, 190, 140]),
    (0.82, [235, 203, 139]),
    (1.0, [191, 97, 106]),
];

fn palette_stops(palette: VisualizationPalette) -> &'static [(f32, [u8; 3])] {
    match palette {
        VisualizationPalette::TokyoNight => TOKYO_NIGHT_STOPS,
        VisualizationPalette::Deadbeef => DEAD_BEE_F_STOPS,
        VisualizationPalette::Nord => NORD_STOPS,
    }
}

#[derive(Default)]
struct Column {
    levels: Vec<f32>,
    sample_time: Option<Duration>,
    bucket_start: Option<Duration>,
    interval_start: Duration,
    image: Option<Arc<RenderImage>>,
}

fn history_fraction(time: Duration, latest: Duration, history: Duration) -> f32 {
    (1.0 - latest.saturating_sub(time).as_secs_f32() / history.as_secs_f32()).clamp(0.0, 1.0)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VisualMode {
    Spectrum,
    Spectrogram,
}

struct VisualData {
    frequencies: Vec<f32>,
    sample_rate: u32,
    frequency_labels: [SharedString; 2],
    spectrum_labels: [SharedString; 2],
    db_labels: [SharedString; 4],
    spectrum_bins: Vec<Range<usize>>,
    heat_rows: Vec<Range<usize>>,
    columns: Vec<Column>,
    latest_levels: Vec<f32>,
    peaks: Vec<f32>,
    peak_deadlines: Vec<Duration>,
    bar_levels: Vec<f32>,
    bar_deadlines: Vec<Duration>,
    smoothed_levels: Vec<f32>,
    peak_markers: Vec<Bounds<Pixels>>,
    spectrum_points: Vec<Point<Pixels>>,
    last_update: Option<Duration>,
    top_db: f32,
    visual_background: u32,
    visual_palette: VisualizationPalette,
    spectrum_min_hz: f32,
    spectrum_max_hz: f32,
    spectrum_db_range: f32,
    spectrum_bars: u32,
    spectrum_bar_width: f32,
    spectrum_interpolate: bool,
    spectrum_log_scale: bool,
    spectrum_show_labels: bool,
    spectrum_fft_size: u32,
    spectrum_window: SpectrumWindow,
    spectrum_bands_per_octave: u32,
    spectrum_style: SpectrumStyle,
    spectrum_gap: f32,
    spectrum_peaks: bool,
    spectrum_peak_hold_ms: u32,
    spectrum_peak_gravity: f32,
    spectrum_bar_hold_ms: u32,
    spectrum_bar_gravity: f32,
    spectrum_smoothing_ms: u32,
    spectrum_grid: bool,
    spectrogram_min_hz: f32,
    spectrogram_max_hz: f32,
    spectrogram_db_range: f32,
    spectrogram_log_scale: bool,
    spectrogram_show_labels: bool,
    spectrogram_history_seconds: u32,
    head: usize,
    len: usize,
    latest: Option<usize>,
    latest_sample_time: Option<Duration>,
    retired: Vec<Arc<RenderImage>>,
    error: Option<String>,
}

pub(super) struct Visuals {
    // GPUI prepaint/paint closures are 'static. Share the UI-thread state rather
    // than copying the full history into each panel's render closure.
    data: Rc<RefCell<VisualData>>,
}

impl Visuals {
    pub(super) fn new() -> Self {
        Self {
            data: Rc::new(RefCell::new(VisualData {
                frequencies: Vec::new(),
                sample_rate: 0,
                frequency_labels: ["8 Hz".into(), "11 kHz".into()],
                spectrum_labels: ["20 Hz".into(), "20 kHz".into()],
                db_labels: std::array::from_fn(|step| db_label(-70.0 * step as f32 / 3.0)),
                spectrum_bins: Vec::new(),
                heat_rows: Vec::new(),
                columns: (0..HISTORY_COLUMNS).map(|_| Column::default()).collect(),
                latest_levels: Vec::new(),
                peaks: Vec::new(),
                peak_deadlines: Vec::new(),
                bar_levels: Vec::new(),
                bar_deadlines: Vec::new(),
                smoothed_levels: Vec::new(),
                peak_markers: Vec::with_capacity(MAX_SPECTRUM_BARS),
                spectrum_points: Vec::with_capacity(MAX_SPECTRUM_BARS),
                last_update: None,
                top_db: 0.0,
                visual_background: 0x08090c,
                visual_palette: VisualizationPalette::Deadbeef,
                spectrum_min_hz: 20.0,
                spectrum_max_hz: 20_000.0,
                spectrum_db_range: 70.0,
                spectrum_bars: 0,
                spectrum_bar_width: 3.0,
                spectrum_interpolate: true,
                spectrum_log_scale: true,
                spectrum_show_labels: true,
                spectrum_fft_size: 8192,
                spectrum_window: SpectrumWindow::BlackmanHarris,
                spectrum_bands_per_octave: 24,
                spectrum_style: SpectrumStyle::Bars,
                spectrum_gap: 1.0,
                spectrum_peaks: true,
                spectrum_peak_hold_ms: 400,
                spectrum_peak_gravity: 50.0,
                spectrum_bar_hold_ms: 0,
                spectrum_bar_gravity: 50.0,
                spectrum_smoothing_ms: 80,
                spectrum_grid: true,
                spectrogram_min_hz: 20.0,
                spectrogram_max_hz: 20_000.0,
                spectrogram_db_range: 70.0,
                spectrogram_log_scale: true,
                spectrogram_show_labels: true,
                spectrogram_history_seconds: 10,
                head: 0,
                len: 0,
                latest: None,
                latest_sample_time: None,
                retired: Vec::with_capacity(HISTORY_COLUMNS),
                error: None,
            })),
        }
    }
    pub(super) fn configure(&mut self, config: &Config) {
        let mut data = self.data.borrow_mut();
        let changed = data.visual_background != config.visual_background.rgb()
            || data.visual_palette != config.visual_palette
            || data.spectrum_min_hz != config.spectrum_min_hz
            || data.spectrum_max_hz != config.spectrum_max_hz
            || data.spectrum_bars != config.spectrum_bars
            || data.spectrum_style != config.spectrum_style
            || data.spectrum_gap != config.spectrum_gap
            || data.spectrum_peaks != config.spectrum_peaks
            || data.spectrum_peak_hold_ms != config.spectrum_peak_hold_ms
            || data.spectrum_peak_gravity != config.spectrum_peak_gravity
            || data.spectrum_bar_hold_ms != config.spectrum_bar_hold_ms
            || data.spectrum_bar_gravity != config.spectrum_bar_gravity
            || data.spectrum_smoothing_ms != config.spectrum_smoothing_ms
            || data.spectrum_bar_width != config.spectrum_bar_width
            || data.spectrum_interpolate != config.spectrum_interpolate
            || data.spectrum_log_scale != config.spectrum_log_scale
            || data.spectrum_show_labels != config.spectrum_labels
            || data.spectrogram_show_labels != config.spectrogram_labels
            || data.spectrum_fft_size != config.spectrum_fft_size
            || data.spectrum_window != config.spectrum_window
            || data.spectrum_bands_per_octave != config.spectrum_bands_per_octave
            || data.spectrum_grid != config.spectrum_grid
            || data.spectrogram_min_hz != config.spectrogram_min_hz
            || data.spectrogram_max_hz != config.spectrogram_max_hz
            || data.spectrogram_db_range != config.spectrogram_db_range
            || data.spectrogram_log_scale != config.spectrogram_log_scale
            || data.spectrogram_history_seconds != config.spectrogram_history_seconds;
        if !changed {
            return;
        }
        let history_changed =
            data.spectrogram_history_seconds != config.spectrogram_history_seconds;
        let analysis_changed = data.spectrum_fft_size != config.spectrum_fft_size
            || data.spectrum_window != config.spectrum_window
            || data.spectrum_bands_per_octave != config.spectrum_bands_per_octave;
        let heat_changed = data.visual_background != config.visual_background.rgb()
            || data.visual_palette != config.visual_palette
            || data.spectrogram_min_hz != config.spectrogram_min_hz
            || data.spectrogram_max_hz != config.spectrogram_max_hz
            || data.spectrogram_db_range != config.spectrogram_db_range
            || data.spectrogram_log_scale != config.spectrogram_log_scale;
        data.visual_background = config.visual_background.rgb();
        data.visual_palette = config.visual_palette;
        data.spectrum_min_hz = config.spectrum_min_hz;
        data.spectrum_max_hz = config.spectrum_max_hz;
        data.spectrum_db_range = config.spectrum_db_range;
        data.spectrum_bars = config.spectrum_bars;
        data.spectrum_style = config.spectrum_style;
        data.spectrum_gap = config.spectrum_gap;
        data.spectrum_peaks = config.spectrum_peaks;
        data.spectrum_peak_hold_ms = config.spectrum_peak_hold_ms;
        data.spectrum_peak_gravity = config.spectrum_peak_gravity;
        data.spectrum_bar_hold_ms = config.spectrum_bar_hold_ms;
        data.spectrum_bar_gravity = config.spectrum_bar_gravity;
        data.spectrum_smoothing_ms = config.spectrum_smoothing_ms;
        data.spectrum_bar_width = config.spectrum_bar_width;
        data.spectrum_interpolate = config.spectrum_interpolate;
        data.spectrum_log_scale = config.spectrum_log_scale;
        data.spectrum_show_labels = config.spectrum_labels;
        data.spectrogram_show_labels = config.spectrogram_labels;
        data.spectrum_fft_size = config.spectrum_fft_size;
        data.spectrum_window = config.spectrum_window;
        data.spectrum_bands_per_octave = config.spectrum_bands_per_octave;
        data.spectrum_grid = config.spectrum_grid;
        data.spectrogram_min_hz = config.spectrogram_min_hz;
        data.spectrogram_max_hz = config.spectrogram_max_hz;
        data.spectrogram_db_range = config.spectrogram_db_range;
        data.spectrogram_log_scale = config.spectrogram_log_scale;
        data.spectrogram_history_seconds = config.spectrogram_history_seconds;
        if history_changed || analysis_changed {
            data.clear_history();
        } else if heat_changed {
            data.invalidate_images();
        }
        if analysis_changed {
            data.latest_levels.clear();
            data.last_update = None;
        }
        let levels = if data.spectrum_peaks {
            &data.peaks
        } else {
            &data.latest_levels
        };
        data.top_db = (levels.iter().copied().fold(0.0, f32::max) / 6.0).ceil() * 6.0;
        data.refresh_frequency_labels();
        data.refresh_db_labels();
    }

    pub(super) fn update(&mut self, frame: &AnalysisFrame) {
        let mut data = self.data.borrow_mut();
        if frame.spectrum_db.len() != frame.frequencies_hz.len() {
            data.report_error("Analysis frequency and level counts differ".into());
            return;
        }
        let sample_rate_changed = data.sample_rate != frame.sample_rate;
        data.sample_rate = frame.sample_rate;
        if sample_rate_changed || data.frequencies != frame.frequencies_hz {
            data.clear_history();
            data.latest_levels.clear();
            data.frequencies.clone_from(&frame.frequencies_hz);
            data.top_db = 0.0;
            data.last_update = None;
            data.refresh_db_labels();
            data.refresh_frequency_labels();
        }
        if frame.spectrum_db.is_empty() {
            return;
        }
        if let Some(previous) = data.latest_sample_time
            && frame.sample_time.saturating_sub(previous)
                > Duration::from_secs(data.spectrogram_history_seconds as u64)
        {
            data.clear_history();
        }
        let now = frame.sample_time;
        if data.last_update.is_some_and(|last| now < last) {
            data.clear_history();
            data.last_update = None;
        }
        let last = data.last_update.replace(now).unwrap_or(now);
        if data.latest_levels.is_empty() || data.peaks.len() != frame.spectrum_db.len() {
            data.peaks.clone_from(&frame.spectrum_db);
            data.bar_levels.clone_from(&frame.spectrum_db);
            data.smoothed_levels.clone_from(&frame.spectrum_db);
            let peak_deadline = now + Duration::from_millis(data.spectrum_peak_hold_ms as u64);
            let bar_deadline = now + Duration::from_millis(data.spectrum_bar_hold_ms as u64);
            data.peak_deadlines.clear();
            data.peak_deadlines
                .resize(frame.spectrum_db.len(), peak_deadline);
            data.bar_deadlines.clear();
            data.bar_deadlines
                .resize(frame.spectrum_db.len(), bar_deadline);
        }
        data.latest_levels.clone_from(&frame.spectrum_db);
        data.push_history(frame.sample_time, &frame.spectrum_db);
        let decay = smoothing_decay(now.saturating_sub(last), data.spectrum_smoothing_ms);
        for index in 0..data.peaks.len() {
            let value = finite_level(frame.spectrum_db[index]);
            let (peak, deadline) = advance_envelope(
                data.peaks[index],
                data.peak_deadlines[index],
                value,
                last,
                now,
                data.spectrum_peak_hold_ms,
                data.spectrum_peak_gravity,
            );
            data.peaks[index] = peak;
            data.peak_deadlines[index] = deadline;
            let (level, deadline) = advance_envelope(
                data.bar_levels[index],
                data.bar_deadlines[index],
                value,
                last,
                now,
                data.spectrum_bar_hold_ms,
                data.spectrum_bar_gravity,
            );
            data.bar_levels[index] = level;
            data.bar_deadlines[index] = deadline;
            data.smoothed_levels[index] = smooth_release(data.smoothed_levels[index], value, decay);
        }
        let top_levels = if data.spectrum_peaks {
            &data.peaks
        } else {
            &data.latest_levels
        };
        let top_db = (top_levels.iter().copied().fold(0.0, f32::max) / 6.0).ceil() * 6.0;
        if top_db != data.top_db {
            data.top_db = top_db;
            data.refresh_db_labels();
        }
        data.latest_sample_time = Some(frame.sample_time);
    }

    pub(super) fn spectrum(&self, panel_id: u64) -> AnyElement {
        self.view(panel_id, VisualMode::Spectrum)
    }

    pub(super) fn spectrogram(&self, panel_id: u64) -> AnyElement {
        self.view(panel_id, VisualMode::Spectrogram)
    }

    fn view(&self, panel_id: u64, mode: VisualMode) -> AnyElement {
        let data = Rc::clone(&self.data);
        let message: Option<SharedString> = {
            let data = self.data.borrow();
            if data.error.is_some() {
                None
            } else if data.latest_levels.is_empty() {
                Some("Analysis waiting for playback".into())
            } else {
                match mode {
                    VisualMode::Spectrum if data.spectrum_range().is_none() => {
                        Some("No audible spectrum data".into())
                    }
                    VisualMode::Spectrogram if data.heat_rows.is_empty() => Some(
                        format!(
                            "No spectrogram data in {}–{}",
                            frequency_label(data.spectrogram_min_hz),
                            frequency_label(data.spectrogram_max_hz),
                        )
                        .into(),
                    ),
                    _ => None,
                }
            }
        };
        let showing_status = message.is_some();
        let kind = match mode {
            VisualMode::Spectrum => "spectrum",
            VisualMode::Spectrogram => "spectrogram",
        };
        div()
            .id((kind, panel_id))
            .relative()
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .bg(rgb(self.data.borrow().visual_background))
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, cx| {
                        let mut data = data.borrow_mut();
                        data.release_retired(window);
                        window.paint_quad(fill(bounds, rgb(data.visual_background)));
                        if bounds.size.width <= px(64.0) || bounds.size.height <= px(40.0) {
                            return;
                        }
                        if !showing_status {
                            let result = match mode {
                                VisualMode::Spectrum => data.paint_spectrum(bounds, window, cx),
                                VisualMode::Spectrogram => {
                                    data.paint_spectrogram(bounds, window, cx)
                                }
                            };
                            if let Err(error) = result {
                                data.report_error(error);
                            }
                        }
                        if let Some(error) = &data.error {
                            window.paint_quad(fill(
                                Bounds::new(bounds.origin, size(bounds.size.width, px(22.0))),
                                rgb(ERROR_BG),
                            ));
                            paint_label(
                                format!("Visualization error: {error}").into(),
                                point(bounds.left() + px(6.0), bounds.top() + px(3.0)),
                                ERROR,
                                window,
                                cx,
                            );
                        }
                    },
                )
                .size_full(),
            )
            .children(message.map(|message| {
                super::components::visualization_status(message)
                    .absolute()
                    .top_0()
                    .left_0()
            }))
            .into_any_element()
    }
}

impl VisualData {
    fn display_level(&self, index: usize) -> f32 {
        let level = finite_level(self.bar_levels[index]);
        if self.spectrum_bar_gravity == 0.0 {
            level
        } else {
            level.max(finite_level(self.smoothed_levels[index]))
        }
    }

    fn interpolate_level(&self, hz: f32, low: f32, high: f32, peaks: bool) -> f32 {
        let first = self.frequencies.partition_point(|value| *value < low);
        let end = self.frequencies.partition_point(|value| *value <= high);
        if first == end {
            return f32::NEG_INFINITY;
        }
        let right = self
            .frequencies
            .partition_point(|value| *value < hz)
            .clamp(first, end - 1);
        let left = right.saturating_sub(1).max(first);
        let level = |index| {
            if peaks {
                finite_level(self.peaks[index])
            } else {
                self.display_level(index)
            }
        };
        let a = level(left);
        let b = level(right);
        if left == right {
            return b;
        }
        let coordinate = |value: f32| {
            if self.spectrum_log_scale {
                value.ln()
            } else {
                value
            }
        };
        let fraction = ((coordinate(hz) - coordinate(self.frequencies[left]))
            / (coordinate(self.frequencies[right]) - coordinate(self.frequencies[left])))
        .clamp(0.0, 1.0);
        let floor = self.top_db - self.spectrum_db_range;
        a.max(floor) + (b.max(floor) - a.max(floor)) * fraction
    }

    fn push_history(&mut self, time: Duration, levels: &[f32]) {
        // One extra slot covers the partial bucket at the left edge.
        let width = Duration::from_secs_f64(
            self.spectrogram_history_seconds as f64 / (HISTORY_COLUMNS - 1) as f64,
        );
        let bucket =
            Duration::from_nanos(((time.as_nanos() / width.as_nanos()) * width.as_nanos()) as u64);
        let merge = self
            .latest
            .filter(|&index| self.columns[index].bucket_start == Some(bucket));
        let index = merge.unwrap_or(self.head);
        let mut changed = merge.is_none();
        if merge.is_some() {
            for (old, new) in self.columns[index].levels.iter_mut().zip(levels) {
                let pooled = old.max(finite_level(*new));
                changed |= pooled != *old;
                *old = pooled;
            }
        } else {
            self.columns[index].levels.clear();
            self.columns[index].levels.extend_from_slice(levels);
            self.columns[index].bucket_start = Some(bucket);
            self.columns[index].interval_start = self
                .latest_sample_time
                .unwrap_or(time)
                .max(time.saturating_sub(Duration::from_millis(250)));
            self.head = (index + 1) % HISTORY_COLUMNS;
            self.len = (self.len + 1).min(HISTORY_COLUMNS);
        }
        self.columns[index].sample_time = Some(time);
        if changed && let Some(image) = self.columns[index].image.take() {
            self.retired.push(image);
        }
        self.latest = Some(index);
    }
    fn invalidate_images(&mut self) {
        for column in &mut self.columns {
            if let Some(image) = column.image.take() {
                self.retired.push(image);
            }
        }
    }
    fn clear_history(&mut self) {
        self.invalidate_images();
        for column in &mut self.columns {
            column.levels.clear();
            column.sample_time = None;
            column.bucket_start = None;
        }
        self.head = 0;
        self.len = 0;
        self.latest = None;
        self.latest_sample_time = None;
    }

    fn visible_range(&self) -> Option<(usize, usize, f32, f32)> {
        clipped_range(
            &self.frequencies,
            self.sample_rate,
            self.spectrogram_min_hz,
            self.spectrogram_max_hz,
        )
    }

    fn spectrum_range(&self) -> Option<(usize, usize, f32, f32)> {
        clipped_range(
            &self.frequencies,
            self.sample_rate,
            self.spectrum_min_hz,
            self.spectrum_max_hz,
        )
    }

    fn refresh_db_labels(&mut self) {
        self.db_labels = std::array::from_fn(|step| {
            db_label(self.top_db - self.spectrum_db_range * step as f32 / 3.0)
        });
    }

    fn refresh_frequency_labels(&mut self) {
        self.heat_rows.clear();
        self.spectrum_bins.clear();
        if let Some((first, last, low, high)) = self.visible_range() {
            group_bands(
                &self.frequencies,
                low,
                high,
                last - first + 1,
                self.spectrogram_log_scale,
                &mut self.heat_rows,
            );
            self.frequency_labels = [frequency_label(low), frequency_label(high)];
        }
        if let Some((_, _, low, high)) = self.spectrum_range() {
            self.spectrum_labels = [frequency_label(low), frequency_label(high)];
        }
    }

    fn report_error(&mut self, error: String) {
        if self.error.as_ref() != Some(&error) {
            eprintln!("rivu visualization: {error}");
            self.error = Some(error);
        }
    }

    fn release_retired(&mut self, window: &mut Window) {
        // Both visual canvases clean up, even when only the spectrum is shown.
        while let Some(image) = self.retired.pop() {
            if let Err(error) = window.drop_image(image) {
                self.report_error(format!("Cannot release spectrogram image: {error}"));
            }
        }
    }

    fn paint_spectrum(
        &mut self,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<(), String> {
        let left_margin = if self.spectrum_show_labels { 42.0 } else { 8.0 };
        let bottom_margin = if self.spectrum_show_labels { 22.0 } else { 8.0 };
        let plot = Bounds::new(
            point(bounds.left() + px(left_margin), bounds.top() + px(8.0)),
            size(
                (bounds.size.width - px(left_margin + 8.0)).max(px(1.0)),
                (bounds.size.height - px(bottom_margin + 8.0)).max(px(1.0)),
            ),
        );
        let Some((_, _, low, high)) = self.spectrum_range() else {
            return Ok(());
        };
        if plot.size.width <= px(2.0) || plot.size.height <= px(2.0) {
            return Ok(());
        }
        // Auto density follows pixel pitch, not the analysis band's count.
        // Empty display slots can interpolate between adjacent musical centers.
        let bars = spectrum_bar_count(
            plot.size.width / px(1.0),
            self.spectrum_bars,
            self.spectrum_bar_width,
            self.spectrum_gap,
        );
        if self.spectrum_bins.len() != bars {
            group_bands(
                &self.frequencies,
                low,
                high,
                bars,
                self.spectrum_log_scale,
                &mut self.spectrum_bins,
            );
        }
        for step in 0..self.db_labels.len() {
            let fraction = step as f32 / (self.db_labels.len() - 1) as f32;
            let y = plot.top() + plot.size.height * fraction;
            if self.spectrum_grid {
                window.paint_quad(fill(
                    Bounds::new(point(plot.left(), y), size(plot.size.width, px(1.0))),
                    rgb(0x14161b),
                ));
            }
            if self.spectrum_show_labels {
                paint_aligned_label(
                    self.db_labels[step].clone(),
                    point(plot.left() - px(5.0), y - px(5.0)),
                    1.0,
                    window,
                    cx,
                );
            }
        }

        window.with_content_mask(
            Some(gpui::ContentMask { bounds: plot }),
            |window| -> Result<(), String> {
                // Paint markers after the continuous fill so it cannot cover them.
                self.peak_markers.clear();
                self.spectrum_points.clear();
                let style = self.spectrum_style;
                let bottom = plot.bottom();
                let top = plot.top();
                let continuous = matches!(style, SpectrumStyle::Line | SpectrumStyle::Solid);
                let mut any_visible = false;
                for bar in 0..bars {
                    let left_t = bar as f32 / bars as f32;
                    let right_t = (bar + 1) as f32 / bars as f32;
                    let range = self.spectrum_bins[bar].clone();
                    let mut level = f32::NEG_INFINITY;
                    let mut peak = f32::NEG_INFINITY;
                    for index in range.clone() {
                        level = level.max(self.display_level(index));
                        peak = peak.max(finite_level(self.peaks[index]));
                    }
                    let center_hz = axis_frequency(
                        low,
                        high,
                        (left_t + right_t) * 0.5,
                        self.spectrum_log_scale,
                    );
                    if self.spectrum_interpolate {
                        // Interpolation fills undersampled slots, but never averages
                        // away a transient already max-pooled into this slot.
                        level = level.max(self.interpolate_level(center_hz, low, high, false));
                        peak = peak.max(self.interpolate_level(center_hz, low, high, true));
                    }
                    let slot_width = plot.size.width * (right_t - left_t);
                    let bar_height = (plot.size.height
                        * db_height(level, self.top_db, self.spectrum_db_range))
                    .clamp(px(0.0), plot.size.height);
                    any_visible |= bar_height > px(0.0);
                    let color = rgb(palette_color_with(
                        self.visual_palette,
                        (left_t + right_t) * 0.5,
                    ));
                    let (x, width) = match style {
                        SpectrumStyle::Line | SpectrumStyle::Solid => {
                            (plot.left() + plot.size.width * left_t, slot_width)
                        }
                        _ => {
                            let gap = px(self.spectrum_gap)
                                .max(px(0.0))
                                .min((slot_width - px(1.0)).max(px(0.0)));
                            (
                                plot.left() + plot.size.width * left_t + gap * 0.5,
                                (slot_width - gap).max(px(1.0)),
                            )
                        }
                    };
                    if continuous || bar_height > px(0.0) {
                        match style {
                            SpectrumStyle::Bars => {
                                let height = bar_height.max(px(1.0)).min(plot.size.height);
                                window.paint_quad(fill(
                                    Bounds::new(point(x, bottom - height), size(width, height)),
                                    color,
                                ));
                            }
                            SpectrumStyle::Outline => {
                                let height = bar_height.max(px(1.0)).min(plot.size.height);
                                let edge = px(1.0).min(width * 0.5).min(height * 0.5);
                                window.paint_quad(fill(
                                    Bounds::new(point(x, bottom - height), size(width, edge)),
                                    color,
                                ));
                                if height > edge {
                                    window.paint_quad(fill(
                                        Bounds::new(point(x, bottom - edge), size(width, edge)),
                                        color,
                                    ));
                                }
                                if height > edge * 2.0 {
                                    window.paint_quad(fill(
                                        Bounds::new(
                                            point(x, bottom - height + edge),
                                            size(edge, height - edge * 2.0),
                                        ),
                                        color,
                                    ));
                                    window.paint_quad(fill(
                                        Bounds::new(
                                            point(x + width - edge, bottom - height + edge),
                                            size(edge, height - edge * 2.0),
                                        ),
                                        color,
                                    ));
                                }
                            }
                            SpectrumStyle::Led => {
                                let mut segment_bottom = bottom;
                                let segment_height = px(3.0);
                                let segment_gap = px(2.0);
                                while segment_bottom > bottom - bar_height {
                                    let segment_top =
                                        (segment_bottom - segment_height).max(bottom - bar_height);
                                    window.paint_quad(fill(
                                        Bounds::new(
                                            point(x, segment_top),
                                            size(width, segment_bottom - segment_top),
                                        ),
                                        color,
                                    ));
                                    segment_bottom = segment_top - segment_gap;
                                }
                            }
                            SpectrumStyle::Line | SpectrumStyle::Solid => {
                                let y =
                                    (bottom - bar_height).clamp(top + px(1.0), bottom - px(1.0));
                                self.spectrum_points.push(point(x + width * 0.5, y));
                            }
                        }
                    }
                    if self.spectrum_peaks && peak > self.top_db - self.spectrum_db_range {
                        let peak_height = (plot.size.height
                            * db_height(peak, self.top_db, self.spectrum_db_range))
                        .clamp(px(0.0), plot.size.height);
                        let peak_y = (bottom - peak_height).clamp(top, bottom - px(1.0));
                        let peak_width =
                            if matches!(style, SpectrumStyle::Line | SpectrumStyle::Solid) {
                                slot_width.min(px(8.0)).max(px(1.0))
                            } else {
                                width
                            };
                        let peak_x = (x + (width - peak_width) * 0.5)
                            .clamp(plot.left(), (plot.right() - peak_width).max(plot.left()));
                        self.peak_markers.push(Bounds::new(
                            point(peak_x, peak_y),
                            size(peak_width, px(1.0)),
                        ));
                    }
                }
                if continuous && any_visible {
                    paint_spectrum_shape(
                        &self.spectrum_points,
                        plot,
                        style,
                        self.visual_palette,
                        self.spectrum_interpolate,
                        window,
                    )?;
                }
                for &bounds in &self.peak_markers {
                    window.paint_quad(fill(bounds, rgb(0xd7def0)));
                }
                Ok(())
            },
        )?;
        if self.spectrum_show_labels {
            let labels_y = plot.bottom() + px(3.0);
            paint_label(
                self.spectrum_labels[0].clone(),
                point(plot.left(), labels_y),
                0xb1bbc5,
                window,
                cx,
            );
            paint_aligned_label(
                self.spectrum_labels[1].clone(),
                point(plot.right(), labels_y),
                1.0,
                window,
                cx,
            );
        }
        Ok(())
    }

    fn paint_spectrogram(
        &mut self,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<(), String> {
        let left_margin = if self.spectrogram_show_labels {
            42.0
        } else {
            8.0
        };
        let plot = Bounds::new(
            point(bounds.left() + px(left_margin), bounds.top() + px(6.0)),
            size(
                bounds.size.width - px(left_margin + 8.0),
                bounds.size.height - px(12.0),
            ),
        );
        if self.heat_rows.is_empty() {
            return Ok(());
        }
        if plot.size.width <= px(2.0) || plot.size.height <= px(2.0) {
            return Ok(());
        }
        let Some(latest_time) = self.latest_sample_time else {
            return Ok(());
        };
        let history = Duration::from_secs(self.spectrogram_history_seconds as u64);
        let start_time = latest_time.saturating_sub(history);
        let height = u32::try_from(self.heat_rows.len())
            .map_err(|_| "Spectrogram image height exceeds GPU image dimensions".to_string())?;
        for index in 0..HISTORY_COLUMNS {
            let Some(sample_time) = self.columns[index].sample_time else {
                continue;
            };
            if sample_time < start_time || sample_time > latest_time {
                continue;
            }
            if self.columns[index].image.is_none() {
                let levels = &self.columns[index].levels;
                let mut pixels = image::RgbaImage::new(1, height);
                for row in 0..height {
                    let mut level = f32::NEG_INFINITY;
                    for source in self.heat_rows[height as usize - 1 - row as usize].clone() {
                        level = level.max(finite_level(levels[source]));
                    }
                    let intensity = ((level + self.spectrogram_db_range)
                        / self.spectrogram_db_range)
                        .clamp(0.0, 1.0);
                    let [red, green, blue] = if intensity > 0.0 {
                        gradient(intensity, palette_stops(self.visual_palette))
                    } else {
                        [
                            (self.visual_background >> 16) as u8,
                            (self.visual_background >> 8) as u8,
                            self.visual_background as u8,
                        ]
                    };
                    pixels.put_pixel(0, row, image::Rgba([blue, green, red, 255]));
                }
                self.columns[index].image =
                    Some(Arc::new(RenderImage::new([image::Frame::new(pixels)])));
            }
            let right = history_fraction(sample_time, latest_time, history);
            let left = history_fraction(self.columns[index].interval_start, latest_time, history);
            let width = (plot.size.width * (right - left)).max(px(1.0));
            let column_bounds = Bounds::new(
                point(
                    (plot.left() + plot.size.width * right - width).max(plot.left()),
                    plot.top(),
                ),
                size(width, plot.size.height),
            );
            if let Some(image) = &self.columns[index].image {
                window
                    .paint_image(
                        column_bounds,
                        column_bounds,
                        px(0.0).into(),
                        Arc::clone(image),
                        0,
                        false,
                    )
                    .map_err(|error| format!("Cannot upload/paint spectrogram image: {error}"))?;
            }
        }
        if self.spectrogram_show_labels {
            paint_aligned_label(
                self.frequency_labels[1].clone(),
                point(plot.left() - px(5.0), plot.top()),
                1.0,
                window,
                cx,
            );
            paint_aligned_label(
                self.frequency_labels[0].clone(),
                point(plot.left() - px(5.0), plot.bottom() - px(12.0)),
                1.0,
                window,
                cx,
            );
        }
        Ok(())
    }
}

/// Ballistic fall in dB/s², integrating only the portion beyond the hold.
/// Zero gravity means an immediate snap after hold, not a suspended peak.
fn advance_envelope(
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

fn smoothing_decay(elapsed: Duration, smoothing_ms: u32) -> f32 {
    if smoothing_ms == 0 {
        0.0
    } else {
        (-elapsed.as_secs_f32() / (smoothing_ms as f32 * 0.001)).exp()
    }
}

fn smooth_release(previous: f32, value: f32, decay: f32) -> f32 {
    if value >= previous || decay == 0.0 {
        return value;
    }
    if decay == 1.0 {
        return previous;
    }
    let floor = -240.0;
    value.max(floor) + (previous.max(floor) - value.max(floor)) * decay
}

fn spectrum_bar_count(width: f32, requested: u32, bar_width: f32, gap: f32) -> usize {
    let drawable = (width.floor() as usize).max(1);
    let count = if requested == 0 {
        (width / (bar_width + gap).max(1.0)).floor() as usize
    } else {
        requested as usize
    };
    count.clamp(1, MAX_SPECTRUM_BARS).min(drawable)
}

fn axis_frequency(low: f32, high: f32, fraction: f32, logarithmic: bool) -> f32 {
    if logarithmic {
        (low.ln() + (high.ln() - low.ln()) * fraction).exp()
    } else {
        low + (high - low) * fraction
    }
}

fn shape_y(points: &[Point<Pixels>], x: Pixels, interpolate: bool) -> Pixels {
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

/// GPUI supports two gradient stops per path. Build one bounded path per
/// palette interval, not one allocation per band or a copied full mesh.
fn paint_spectrum_shape(
    points: &[Point<Pixels>],
    plot: Bounds<Pixels>,
    style: SpectrumStyle,
    palette: VisualizationPalette,
    interpolate: bool,
    window: &mut Window,
) -> Result<(), String> {
    if points.is_empty() {
        return Ok(());
    }
    for stops in palette_stops(palette).windows(2) {
        let left = plot.left() + plot.size.width * stops[0].0;
        let right = plot.left() + plot.size.width * stops[1].0;
        let solid = style == SpectrumStyle::Solid;
        let mut builder = if solid {
            PathBuilder::fill()
        } else {
            PathBuilder::stroke(px(2.0))
        };
        if solid {
            builder.move_to(point(left, plot.bottom()));
            builder.line_to(point(left, shape_y(points, left, interpolate)));
        } else {
            builder.move_to(point(left, shape_y(points, left, interpolate)));
        }
        if interpolate {
            for &vertex in points {
                if vertex.x > left && vertex.x < right {
                    builder.line_to(vertex);
                }
            }
        } else {
            for pair in points.windows(2) {
                let x = (pair[0].x + pair[1].x) * 0.5;
                if x > left && x < right {
                    builder.line_to(point(x, pair[0].y));
                    builder.line_to(point(x, pair[1].y));
                }
            }
        }
        builder.line_to(point(right, shape_y(points, right, interpolate)));
        if solid {
            builder.line_to(point(right, plot.bottom()));
            builder.close();
        }
        let path = builder
            .build()
            .map_err(|error| format!("Cannot tessellate spectrum shape: {error}"))?;
        let width = path.bounds.size.width.max(px(1.0));
        let from = (left - path.bounds.left()) / width;
        let to = (right - path.bounds.left()) / width;
        let color = linear_gradient(
            90.0,
            linear_color_stop(rgb(palette_color_with(palette, stops[0].0)), from),
            linear_color_stop(rgb(palette_color_with(palette, stops[1].0)), to),
        );
        window.with_content_mask(
            Some(gpui::ContentMask {
                bounds: Bounds::new(
                    point(left, plot.top()),
                    size(right - left, plot.size.height),
                ),
            }),
            |window| window.paint_path(path, color),
        );
    }
    Ok(())
}
fn group_bands(
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

fn clipped_range(
    frequencies: &[f32],
    sample_rate: u32,
    min: f32,
    max: f32,
) -> Option<(usize, usize, f32, f32)> {
    let low = min.max(*frequencies.first()?);
    let nyquist = if sample_rate == 0 {
        *frequencies.last()?
    } else {
        sample_rate as f32 * 0.5
    };
    let high = max.min(nyquist).min(*frequencies.last()?);
    if low >= high {
        return None;
    }
    let first = frequencies.partition_point(|hz| *hz < low);
    let end = frequencies.partition_point(|hz| *hz <= high);
    (first < end).then_some((first, end.saturating_sub(1), low, high))
}

fn finite_level(value: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        f32::NEG_INFINITY
    }
}

fn db_height(value: f32, top: f32, span: f32) -> f32 {
    ((finite_level(value) - (top - span)) / span).clamp(0.0, 1.0)
}

fn db_label(value: f32) -> SharedString {
    if value.abs() < 0.5 {
        "0 dB".into()
    } else {
        format!("{value:+.0} dB").into()
    }
}

fn frequency_label(frequency: f32) -> SharedString {
    if frequency >= 1000.0 {
        format!("{:.1} kHz", frequency / 1000.0).into()
    } else {
        format!("{frequency:.0} Hz").into()
    }
}

fn gradient(value: f32, stops: &[(f32, [u8; 3])]) -> [u8; 3] {
    let value = value.clamp(0.0, 1.0);
    for pair in stops.windows(2) {
        if value <= pair[1].0 {
            let fraction = (value - pair[0].0) / (pair[1].0 - pair[0].0);
            return std::array::from_fn(|channel| {
                (pair[0].1[channel] as f32
                    + fraction * (pair[1].1[channel] as f32 - pair[0].1[channel] as f32))
                    as u8
            });
        }
    }
    stops[stops.len() - 1].1
}
#[cfg(test)]
pub(super) fn palette_color(fraction: f32) -> u32 {
    palette_color_with(VisualizationPalette::Deadbeef, fraction)
}

pub(super) fn palette_function(palette: VisualizationPalette) -> fn(f32) -> u32 {
    match palette {
        VisualizationPalette::TokyoNight => tokyonight_palette_color,
        VisualizationPalette::Deadbeef => deadbeef_palette_color,
        VisualizationPalette::Nord => nord_palette_color,
    }
}

fn palette_color_with(palette: VisualizationPalette, fraction: f32) -> u32 {
    let color = gradient(fraction, palette_stops(palette));
    (u32::from(color[0]) << 16) | (u32::from(color[1]) << 8) | u32::from(color[2])
}

fn tokyonight_palette_color(fraction: f32) -> u32 {
    palette_color_with(VisualizationPalette::TokyoNight, fraction)
}

fn deadbeef_palette_color(fraction: f32) -> u32 {
    palette_color_with(VisualizationPalette::Deadbeef, fraction)
}

fn nord_palette_color(fraction: f32) -> u32 {
    palette_color_with(VisualizationPalette::Nord, fraction)
}

fn paint_label(
    text: SharedString,
    origin: Point<Pixels>,
    color: u32,
    window: &mut Window,
    cx: &mut App,
) {
    let mut style = window.text_style();
    style.color = rgb(color).into();
    let run = style.to_run(text.len());
    let line = window
        .text_system()
        .shape_line(text, px(10.0), &[run], None);
    if let Err(error) = line.paint(origin, px(14.0), TextAlign::Left, None, window, cx) {
        eprintln!("rivu visualization label: {error}");
    }
}

fn paint_aligned_label(
    text: SharedString,
    anchor: Point<Pixels>,
    alignment: f32,
    window: &mut Window,
    cx: &mut App,
) {
    let mut style = window.text_style();
    style.color = rgb(0xb1bbc5).into();
    let run = style.to_run(text.len());
    let line = window
        .text_system()
        .shape_line(text, px(10.0), &[run], None);
    let origin = point(anchor.x - line.width() * alignment, anchor.y);
    if let Err(error) = line.paint(origin, px(14.0), TextAlign::Left, None, window, cx) {
        eprintln!("rivu visualization label: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn palettes_have_distinct_named_endpoints() {
        assert_eq!(
            palette_color_with(VisualizationPalette::Deadbeef, 0.0),
            0x203a8a
        );
        assert_eq!(
            palette_color_with(VisualizationPalette::TokyoNight, 0.0),
            0x41486e
        );
        assert_eq!(
            palette_color_with(VisualizationPalette::Nord, 0.0),
            0x2e3440
        );
        assert_eq!(
            palette_color_with(VisualizationPalette::TokyoNight, 1.0),
            0xf7768e
        );
    }
    fn frame(time: f64, level: f32) -> AnalysisFrame {
        AnalysisFrame {
            sample_rate: 48_000,
            sample_time: Duration::from_secs_f64(time),
            frequencies_hz: vec![20.0, 200.0, 2000.0, 20_000.0],
            spectrum_db: vec![level; 4],
            ..AnalysisFrame::default()
        }
    }

    #[test]
    fn history_retains_requested_audio_duration_at_different_rates() {
        for fps in [5, 60] {
            let mut visuals = Visuals::new();
            visuals.configure(&Config {
                spectrogram_history_seconds: 120,
                ..Config::default()
            });
            for step in 1..=240 * fps {
                visuals.update(&frame(step as f64 / fps as f64, -30.0));
            }
            let data = visuals.data.borrow();
            assert_eq!(data.len, HISTORY_COLUMNS);
            let oldest = data
                .columns
                .iter()
                .filter_map(|column| column.sample_time)
                .min()
                .unwrap();
            let newest = data.latest_sample_time.unwrap();
            assert!(newest - oldest >= Duration::from_secs(119));
            assert!(newest - oldest <= Duration::from_secs(120));
            assert_eq!(
                history_fraction(newest, newest, Duration::from_secs(120)),
                1.0
            );
        }
    }

    #[test]
    fn buckets_preserve_transients_without_contaminating_latest_spectrum() {
        let mut visuals = Visuals::new();
        visuals.configure(&Config {
            spectrogram_history_seconds: 120,
            spectrum_peaks: false,
            ..Config::default()
        });
        visuals.update(&frame(1.1, 12.0));
        assert_eq!(visuals.data.borrow().top_db, 12.0);
        visuals.update(&frame(1.2, -50.0));
        let data = visuals.data.borrow();
        assert_eq!(data.len, 1);
        assert_eq!(data.columns[data.latest.unwrap()].levels, vec![12.0; 4]);
        assert_eq!(data.latest_levels, vec![-50.0; 4]);
        assert_eq!(data.top_db, 0.0);
        assert!(
            (history_fraction(
                Duration::ZERO,
                Duration::from_secs(1),
                Duration::from_secs(120)
            ) - 119.0 / 120.0)
                .abs()
                < 0.00001
        );
    }

    #[test]
    fn history_does_not_fill_long_gaps_or_mix_sample_rates() {
        let mut visuals = Visuals::new();
        visuals.update(&frame(1.0, -20.0));
        visuals.update(&frame(6.0, -30.0));
        {
            let data = visuals.data.borrow();
            let last = &data.columns[data.latest.unwrap()];
            assert_eq!(last.interval_start, Duration::from_millis(5750));
        }
        let mut changed_rate = frame(6.5, -40.0);
        changed_rate.sample_rate = 44_100;
        visuals.update(&changed_rate);
        assert_eq!(visuals.data.borrow().len, 1);
    }

    #[test]
    fn paused_range_changes_keep_independent_scales_and_history() {
        let mut visuals = Visuals::new();
        let mut config = Config::default();
        visuals.update(&frame(1.0, -30.0));
        let image = Arc::new(RenderImage::new([image::Frame::new(
            image::RgbaImage::new(1, 1),
        )]));
        visuals.data.borrow_mut().columns[0].image = Some(Arc::clone(&image));
        config.spectrum_min_hz = 200.0;
        config.spectrum_db_range = 40.0;
        visuals.configure(&config);
        {
            let data = visuals.data.borrow();
            assert_eq!(data.visible_range().unwrap().2, 20.0);
            assert_eq!(data.spectrum_range().unwrap().2, 200.0);
            assert_eq!(data.spectrogram_db_range, 70.0);
            assert!(Arc::ptr_eq(data.columns[0].image.as_ref().unwrap(), &image));
        }
        config.spectrogram_db_range = 100.0;
        visuals.configure(&config);
        let data = visuals.data.borrow();
        assert!(data.columns[0].image.is_none());
        assert_eq!(data.columns[0].levels, vec![-30.0; 4]);
        assert_eq!(data.len, 1);
    }

    #[test]
    fn selected_range_respects_nyquist_and_empty_intersections() {
        let frequencies = [20.0, 100.0, 1000.0, 8000.0, 24_000.0];
        assert_eq!(
            clipped_range(&frequencies, 48_000, 100.0, 50_000.0),
            Some((1, 4, 100.0, 24_000.0))
        );
        assert_eq!(
            clipped_range(&frequencies, 16_000, 20.0, 20_000.0),
            Some((0, 3, 20.0, 8000.0))
        );
        assert_eq!(clipped_range(&frequencies, 48_000, 9000.0, 12_000.0), None);
        assert_eq!(
            clipped_range(&frequencies, 48_000, 30_000.0, 40_000.0),
            None
        );
    }

    #[test]
    fn log_groups_include_each_selected_band_once_and_exclude_outside_peaks() {
        let frequencies = [50.0, 100.0, 200.0, 400.0, 800.0, 1600.0];
        let mut groups = Vec::new();
        for width in [1, 3, 8] {
            group_bands(&frequencies, 100.0, 800.0, width, true, &mut groups);
            assert_eq!(
                groups.iter().flat_map(Clone::clone).collect::<Vec<_>>(),
                [1, 2, 3, 4]
            );
        }
    }

    #[test]
    fn gravity_integrates_across_hold_at_different_time_steps() {
        for fps in [5, 10, 60, 120] {
            let mut peak = -10.0;
            let mut deadline = Duration::from_millis(400);
            let mut last = Duration::ZERO;
            for step in 1..=fps {
                let now = Duration::from_secs_f64(step as f64 / fps as f64);
                (peak, deadline) = advance_envelope(peak, deadline, -100.0, last, now, 400, 50.0);
                last = now;
            }
            assert!(
                (peak - (-10.0 - 0.5 * 50.0 * 0.6 * 0.6)).abs() < 0.001,
                "{fps} fps: {peak}"
            );
        }
    }

    #[test]
    fn zero_gravity_snaps_at_hold_boundary_and_paused_clock_stays_still() {
        let deadline = Duration::from_millis(400);
        assert_eq!(
            advance_envelope(
                -10.0,
                deadline,
                -80.0,
                Duration::ZERO,
                Duration::from_millis(399),
                400,
                0.0
            )
            .0,
            -10.0
        );
        assert_eq!(
            advance_envelope(-10.0, deadline, -80.0, Duration::ZERO, deadline, 400, 0.0).0,
            -80.0
        );
        let now = Duration::from_secs(2);
        for gravity in [0.0, 50.0, 500.0] {
            assert_eq!(
                advance_envelope(-10.0, deadline, -80.0, now, now, 400, gravity).0,
                -10.0
            );
        }
    }

    #[test]
    fn smoothing_is_time_based_and_preserves_attacks() {
        for fps in [5, 60, 120] {
            let mut level = -10.0;
            let step = Duration::from_secs_f64(1.0 / fps as f64);
            for _ in 0..fps {
                level = smooth_release(level, -80.0, smoothing_decay(step, 1000));
            }
            assert!((level - (-80.0 + 70.0 * (-1.0f32).exp())).abs() < 0.001);
            assert_eq!(smooth_release(level, 6.0, smoothing_decay(step, 1000)), 6.0);
            assert_eq!(
                smooth_release(level, -80.0, smoothing_decay(Duration::ZERO, 1000)),
                level
            );
        }
        assert_eq!(
            smooth_release(-10.0, -80.0, smoothing_decay(Duration::from_millis(1), 0)),
            -80.0
        );
    }

    #[test]
    fn dense_counts_are_bounded_by_pixels_not_old_marker_capacity() {
        assert_eq!(spectrum_bar_count(1200.0, 0, 3.0, 1.0), 300);
        assert_eq!(spectrum_bar_count(4096.0, 0, 3.0, 1.0), 512);
        assert_eq!(spectrum_bar_count(1200.0, 512, 20.0, 8.0), 512);
        assert_eq!(spectrum_bar_count(7.0, 512, 3.0, 1.0), 7);
        assert_eq!(spectrum_bar_count(0.0, 0, 3.0, 1.0), 1);
    }

    #[test]
    fn interpolation_fills_gaps_without_reducing_source_transients() {
        let mut visuals = Visuals::new();
        let mut transient = frame(1.0, -60.0);
        transient.spectrum_db[1] = 0.0;
        visuals.update(&transient);
        let data = visuals.data.borrow();
        let middle = data.interpolate_level((200.0f32 * 2000.0).sqrt(), 20.0, 20_000.0, false);
        assert!((middle + 30.0).abs() < 0.001);
        assert_eq!(data.display_level(1).max(middle), 0.0);
        let points = [point(px(0.0), px(40.0)), point(px(10.0), px(0.0))];
        assert_eq!(shape_y(&points, px(2.5), true), px(30.0));
        assert_eq!(shape_y(&points, px(2.5), false), px(40.0));
        assert_eq!(shape_y(&points, px(10.0), true), px(0.0));
    }

    #[test]
    fn analysis_resolution_changes_reset_motion_and_history_safely() {
        let mut visuals = Visuals::new();
        visuals.update(&frame(1.0, -10.0));
        let mut changed = frame(2.0, -50.0);
        changed.frequencies_hz = vec![20.0, 100.0, 300.0, 1000.0, 5000.0, 20_000.0];
        changed.spectrum_db = vec![-50.0; 6];
        visuals.update(&changed);
        let data = visuals.data.borrow();
        assert_eq!(data.len, 1);
        assert_eq!(data.peaks, changed.spectrum_db);
        assert_eq!(data.bar_levels.len(), 6);
        assert_eq!(data.bar_deadlines.len(), 6);
        assert_eq!(data.spectrum_labels.len(), 2);
        assert!((axis_frequency(20.0, 20_000.0, 0.5, false) - 10_010.0).abs() < 0.001);
    }
}
