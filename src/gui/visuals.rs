//! GPU-backed analysis views. The audio worker owns the configurable FFT;
//! these views retain every musical band supplied by `AnalysisFrame`.
//!
//! Spectrogram history follows the active plot width at one time column per
//! pixel, bounded by the configured duration and analysis rate. Dense incoming
//! frames are max-pooled so narrow transients survive; Spectrum separately
//! retains the latest frame. The available history expands until that dynamic
//! limit; unavailable history is left blank, including long gaps in publication.
//! Replaced images are explicitly removed from the window's atlas. Hidden
//! spectrograms retain reusable CPU buckets but create no new GPU images.
//! Range and axis changes remap cached levels even while paused; style changes
//! redraw Spectrum in place. Unchanged buckets reuse their GPU image.
//! Spectrogram image rows use half the active panel height, never fewer than
//! the visible source frequency points, and remain capped by configuration.

use super::{ERROR, ERROR_BG};
use gpui::{
    AnyElement, App, Bounds, Path, PathBuilder, Pixels, Point, RenderImage, SharedString,
    TextAlign, Window, canvas, div, fill, linear_color_stop, linear_gradient, point, prelude::*,
    px, rgb, size,
};
use std::{cell::RefCell, collections::HashMap, ops::Range, rc::Rc, sync::Arc, time::Duration};

use crate::{
    analysis::AnalysisFrame,
    config::{Config, SpectrumStyle, SpectrumWindow, VisualizationPalette},
};

const HISTORY_GAP_RESET: Duration = Duration::from_secs(1);
const MAX_SPECTRUM_BARS: usize = 2000;
const TOKYO_NIGHT_STOPS: &[(f32, [u8; 3])] = &[
    (0.0, [65, 72, 110]),
    (0.2, [187, 154, 247]),
    (0.42, [125, 207, 255]),
    (0.62, [158, 206, 106]),
    (0.82, [224, 175, 104]),
    (1.0, [247, 118, 142]),
];
// Preserve DeaDBeeF's six color positions after a small black low-end floor.
const DEADBEEF_STOPS: &[(f32, [u8; 3])] = &[
    (0.0, [0, 0, 0]),
    (0.06, [0, 32, 100]),
    (0.06 + 0.94 * 0.2, [0, 148, 160]),
    (0.06 + 0.94 * 0.4, [128, 255, 120]),
    (0.06 + 0.94 * 0.6, [255, 255, 0]),
    (0.06 + 0.94 * 0.8, [255, 128, 0]),
    (1.0, [255, 0, 0]),
];

fn palette_stops(palette: VisualizationPalette) -> &'static [(f32, [u8; 3])] {
    match palette {
        VisualizationPalette::TokyoNight => TOKYO_NIGHT_STOPS,
        VisualizationPalette::Deadbeef => DEADBEEF_STOPS,
    }
}

const PALETTE_LUT_SIZE: usize = 1024;

pub(super) struct PaletteLut {
    palette: VisualizationPalette,
    colors: [[u8; 3]; PALETTE_LUT_SIZE],
}

impl PaletteLut {
    fn new(palette: VisualizationPalette) -> Self {
        Self {
            palette,
            colors: std::array::from_fn(|index| {
                let fraction = index as f32 / (PALETTE_LUT_SIZE - 1) as f32;
                gradient(fraction, palette_stops(palette))
            }),
        }
    }

    pub(super) fn lookup_rgb(&self, fraction: f32) -> [u8; 3] {
        let index = (fraction.clamp(0.0, 1.0) * (PALETTE_LUT_SIZE - 1) as f32).round() as usize;
        self.colors[index]
    }

    pub(super) fn lookup_raw(&self, fraction: f32) -> u32 {
        stop_color(self.lookup_rgb(fraction))
    }

    pub(super) fn lookup_mapped(&self, fraction: f32) -> u32 {
        let fraction = fraction.clamp(0.0, 1.0);
        let mapped = match self.palette {
            VisualizationPalette::Deadbeef => 0.06 + fraction * 0.94,
            VisualizationPalette::TokyoNight => fraction,
        };
        self.lookup_raw(mapped)
    }
    pub(super) fn visible_stops(&self) -> &'static [(f32, [u8; 3])] {
        match self.palette {
            VisualizationPalette::Deadbeef => &DEADBEEF_STOPS[1..],
            VisualizationPalette::TokyoNight => TOKYO_NIGHT_STOPS,
        }
    }

    pub(super) fn stop_position(&self, raw: f32) -> f32 {
        match self.palette {
            VisualizationPalette::Deadbeef => ((raw - 0.06) / 0.94).clamp(0.0, 1.0),
            VisualizationPalette::TokyoNight => raw,
        }
    }
}

thread_local! {
    static ACTIVE_PALETTE_LUT: RefCell<Option<Rc<PaletteLut>>> = const { RefCell::new(None) };
}

pub(super) fn active_palette_lut(palette: VisualizationPalette) -> Rc<PaletteLut> {
    ACTIVE_PALETTE_LUT.with_borrow_mut(|active| {
        if active.as_ref().is_none_or(|lut| lut.palette != palette) {
            *active = Some(Rc::new(PaletteLut::new(palette)));
        }
        Rc::clone(active.as_ref().expect("active palette LUT is initialized"))
    })
}

#[derive(Default)]
struct Column {
    levels: Vec<f32>,
    sample_time: Option<Duration>,
    bucket_start: Option<Duration>,
    interval_start: Duration,
    image: Option<Arc<RenderImage>>,
    generation: u64,
}

#[derive(Clone, Copy)]
struct HeatSample {
    left: usize,
    right: usize,
    fraction: f32,
}

fn history_fraction(time: Duration, latest: Duration, history: Duration) -> f32 {
    if history.is_zero() {
        return 1.0;
    }
    (1.0 - latest.saturating_sub(time).as_secs_f32() / history.as_secs_f32()).clamp(0.0, 1.0)
}

fn history_column_count(history_seconds: u32, analysis_fps: u32) -> usize {
    usize::try_from(history_seconds)
        .unwrap_or(usize::MAX)
        .saturating_mul(analysis_fps as usize)
        .max(2)
}
fn history_columns_for_width(history_seconds: u32, analysis_fps: u32, width: usize) -> usize {
    history_column_count(history_seconds, analysis_fps).min(width.max(2))
}
fn spectrogram_source_height(
    panel_height: usize,
    source_rows: usize,
    max_rows: u32,
    scale: f32,
) -> usize {
    let pixel_rows = ((panel_height as f32) * scale).ceil() as usize;
    pixel_rows
        .max(source_rows.max(1))
        .min(usize::try_from(max_rows.max(1)).unwrap_or(usize::MAX))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VisualMode {
    Spectrum,
    Spectrogram,
}

struct VisualData {
    spectrogram_panel_heights: HashMap<u64, usize>,
    frequencies: Vec<f32>,
    sample_rate: u32,
    frequency_labels: [SharedString; 2],
    spectrum_labels: [SharedString; 2],
    db_labels: [SharedString; 4],
    spectrum_bins: Vec<Range<usize>>,
    heat_samples: Vec<HeatSample>,
    heat_sample_generation: u64,
    spectrogram_image_height: usize,
    columns: Vec<Column>,
    latest_levels: Vec<f32>,
    peaks: Vec<f32>,
    peak_deadlines: Vec<Duration>,
    bar_levels: Vec<f32>,
    bar_deadlines: Vec<Duration>,
    smoothed_levels: Vec<f32>,
    spectrum_points: Vec<Point<Pixels>>,
    last_update: Option<Duration>,
    top_db: f32,
    visual_background: u32,
    palette_lut: Rc<PaletteLut>,
    spectrum_db_range: f32,
    spectrum_bars: u32,
    spectrum_bar_width: f32,
    spectrum_interpolate: bool,
    spectrum_show_labels: bool,
    spectrum_fft_size: u32,
    spectrum_window: SpectrumWindow,
    spectrum_style: SpectrumStyle,
    spectrum_gap: f32,
    spectrum_peaks: bool,
    spectrum_peak_hold_ms: u32,
    spectrum_peak_gravity: f32,
    spectrum_bar_hold_ms: u32,
    spectrum_bar_gravity: f32,
    spectrum_smoothing_ms: u32,
    spectrum_grid: bool,
    spectrogram_db_range: f32,
    spectrogram_show_labels: bool,
    spectrogram_interpolate: bool,
    spectrogram_interpolation_points: u32,
    spectrogram_sampling_points_scale: f32,
    spectrogram_history_seconds: u32,
    history_limit_seconds: f64,
    analysis_fps: u32,
    configured_history_columns: usize,
    history_columns: usize,
    stream_generation: u64,
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
        // History is initialized by configure when an analysis panel becomes visible.
        Self {
            data: Rc::new(RefCell::new(VisualData {
                frequencies: Vec::new(),
                sample_rate: 0,
                frequency_labels: ["8 Hz".into(), "11 kHz".into()],
                spectrum_labels: ["20 Hz".into(), "20 kHz".into()],
                db_labels: std::array::from_fn(|step| db_label(-70.0 * step as f32 / 3.0)),
                spectrum_bins: Vec::new(),
                heat_samples: Vec::new(),
                spectrogram_panel_heights: HashMap::new(),
                spectrogram_image_height: 0,
                columns: Vec::new(),
                latest_levels: Vec::new(),
                peaks: Vec::new(),
                peak_deadlines: Vec::new(),
                bar_levels: Vec::new(),
                bar_deadlines: Vec::new(),
                smoothed_levels: Vec::new(),
                spectrum_points: Vec::new(),
                last_update: None,
                top_db: 0.0,
                palette_lut: active_palette_lut(VisualizationPalette::Deadbeef),
                visual_background: 0x08090c,
                spectrum_db_range: 70.0,
                spectrum_bars: 0,
                spectrum_bar_width: 3.0,
                spectrum_interpolate: true,
                spectrum_show_labels: true,
                spectrum_fft_size: 8192,
                spectrum_window: SpectrumWindow::BlackmanHarris,
                spectrum_style: SpectrumStyle::Bars,
                spectrum_gap: 1.0,
                spectrum_peaks: true,
                spectrum_peak_hold_ms: 400,
                spectrum_peak_gravity: 50.0,
                spectrum_bar_hold_ms: 0,
                spectrum_bar_gravity: 50.0,
                spectrum_smoothing_ms: 80,
                spectrum_grid: true,
                spectrogram_db_range: 70.0,
                spectrogram_show_labels: true,
                spectrogram_interpolate: true,
                spectrogram_interpolation_points: 1024,
                spectrogram_sampling_points_scale: 0.5,
                spectrogram_history_seconds: 20,
                history_limit_seconds: 20.0,
                analysis_fps: 20,
                configured_history_columns: 0,
                history_columns: 0,
                heat_sample_generation: 0,
                stream_generation: 0,
                head: 0,
                len: 0,
                latest: None,
                latest_sample_time: None,
                retired: Vec::new(),
                error: None,
            })),
        }
    }
    pub(super) fn configure(&mut self, config: &Config) {
        let mut data = self.data.borrow_mut();
        let configured_history_columns =
            history_column_count(config.spectrogram_history_seconds, config.analysis_fps);
        let palette_changed = data.palette_lut.palette != config.visual_palette;
        let changed = palette_changed
            || data.visual_background != config.visual_background.rgb()
            || data.spectrum_db_range != config.spectrum_db_range
            || data.spectrum_grid != config.spectrum_grid
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
            || data.spectrum_show_labels != config.spectrum_labels
            || data.spectrum_fft_size != config.spectrum_fft_size
            || data.spectrum_window != config.spectrum_window
            || data.spectrogram_interpolate != config.spectrogram_interpolate
            || data.spectrogram_interpolation_points != config.spectrogram_interpolation_points
            || data.spectrogram_sampling_points_scale != config.spectrogram_sampling_points_scale
            || data.spectrogram_history_seconds != config.spectrogram_history_seconds
            || data.analysis_fps != config.analysis_fps
            || data.configured_history_columns != configured_history_columns;
        if !changed {
            return;
        }
        let history_changed = data.spectrogram_history_seconds
            != config.spectrogram_history_seconds
            || data.analysis_fps != config.analysis_fps
            || data.configured_history_columns != configured_history_columns;
        let analysis_changed = data.spectrum_fft_size != config.spectrum_fft_size
            || data.spectrum_window != config.spectrum_window;
        let heat_changed = palette_changed
            || data.visual_background != config.visual_background.rgb()
            || data.spectrogram_db_range != config.spectrogram_db_range
            || data.spectrogram_interpolate != config.spectrogram_interpolate
            || data.spectrogram_interpolation_points != config.spectrogram_interpolation_points
            || data.spectrogram_sampling_points_scale != config.spectrogram_sampling_points_scale;
        data.palette_lut = if palette_changed {
            active_palette_lut(config.visual_palette)
        } else {
            Rc::clone(&data.palette_lut)
        };
        data.visual_background = config.visual_background.rgb();
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
        data.spectrum_show_labels = config.spectrum_labels;
        data.spectrogram_show_labels = config.spectrogram_labels;
        data.spectrogram_interpolate = config.spectrogram_interpolate;
        data.spectrogram_interpolation_points = config.spectrogram_interpolation_points;
        data.spectrogram_sampling_points_scale = config.spectrogram_sampling_points_scale;
        data.spectrum_fft_size = config.spectrum_fft_size;
        data.spectrum_window = config.spectrum_window;
        data.spectrum_grid = config.spectrum_grid;
        data.spectrogram_db_range = config.spectrogram_db_range;
        data.spectrogram_history_seconds = config.spectrogram_history_seconds;
        data.history_limit_seconds = config.spectrogram_history_seconds as f64;
        data.analysis_fps = config.analysis_fps;
        data.configured_history_columns = configured_history_columns;
        if history_changed {
            data.resize_history(configured_history_columns);
        }
        if analysis_changed {
            data.clear_history();
            data.latest_levels.clear();
            data.last_update = None;
        } else if heat_changed {
            data.invalidate_images();
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
            data.stream_generation = data.stream_generation.wrapping_add(1);
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
    pub(super) fn retain_spectrogram_panels(&mut self, panel_ids: &[u64]) {
        self.data
            .borrow_mut()
            .spectrogram_panel_heights
            .retain(|panel_id, _| panel_ids.contains(panel_id));
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
                    VisualMode::Spectrogram if data.visible_range().is_none() => {
                        Some("No spectrogram data in 16 Hz–Nyquist".into())
                    }
                    _ => None,
                }
            }
        };
        let showing_status = message.is_some();
        let prepaint_data = Rc::clone(&data);
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
                    move |bounds, _, _| {
                        if mode == VisualMode::Spectrogram {
                            let height = ((bounds.size.height - px(12.0)).max(px(1.0)) / px(1.0))
                                .ceil() as usize;
                            prepaint_data
                                .borrow_mut()
                                .spectrogram_panel_heights
                                .insert(panel_id, height.max(1));
                        }
                    },
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
        let coordinate = |value: f32| value.ln();
        let fraction = ((coordinate(hz) - coordinate(self.frequencies[left]))
            / (coordinate(self.frequencies[right]) - coordinate(self.frequencies[left])))
        .clamp(0.0, 1.0);
        let floor = self.top_db - self.spectrum_db_range;
        a.max(floor) + (b.max(floor) - a.max(floor)) * fraction
    }

    fn push_history(&mut self, time: Duration, levels: &[f32]) {
        if self.history_columns < 2 {
            return;
        }
        let discontinuity = self
            .latest
            .is_some_and(|index| self.columns[index].generation != self.stream_generation)
            || self
                .latest_sample_time
                .is_some_and(|previous| time.saturating_sub(previous) > HISTORY_GAP_RESET);
        let width =
            Duration::from_secs_f64(self.history_limit_seconds / (self.history_columns - 1) as f64);
        let bucket =
            Duration::from_nanos(((time.as_nanos() / width.as_nanos()) * width.as_nanos()) as u64);
        let merge = self.latest.filter(|&index| {
            self.columns[index].generation == self.stream_generation
                && self.columns[index].bucket_start == Some(bucket)
        });
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
            self.columns[index].interval_start = if discontinuity {
                time
            } else {
                self.latest_sample_time
                    .map(|previous| previous.max(time.saturating_sub(Duration::from_millis(250))))
                    .unwrap_or_else(|| time.saturating_sub(Duration::from_millis(250)))
            };
            self.head = (index + 1) % self.history_columns;
            self.len = (self.len + 1).min(self.history_columns);
        }
        self.columns[index].generation = self.stream_generation;
        self.columns[index].sample_time = Some(time);
        if changed && let Some(image) = self.columns[index].image.take() {
            self.retired.push(image);
        }
        self.latest = Some(index);
    }
    fn resize_history(&mut self, requested_columns: usize) {
        let new_columns = requested_columns.max(2);
        if new_columns == self.history_columns {
            return;
        }
        let mut old_columns = std::mem::take(&mut self.columns);
        let old_capacity = old_columns.len();
        let old_len = self.len.min(old_capacity);
        let old_head = self.head.min(old_capacity);
        let keep = old_len.min(new_columns);
        let oldest = if old_capacity == 0 {
            0
        } else {
            (old_head + old_capacity - old_len) % old_capacity
        };
        let skip = old_len - keep;
        let mut columns = (0..new_columns)
            .map(|_| Column::default())
            .collect::<Vec<_>>();
        for offset in 0..keep {
            let index = (oldest + skip + offset) % old_capacity;
            columns[offset] = std::mem::take(&mut old_columns[index]);
        }
        for mut column in old_columns {
            if let Some(image) = column.image.take() {
                self.retired.push(image);
            }
        }
        self.columns = columns;
        self.history_columns = new_columns;
        self.len = keep;
        self.head = keep % new_columns;
        self.latest = keep.checked_sub(1);
        if keep == 0 {
            self.latest_sample_time = None;
        }
    }
    fn invalidate_images(&mut self) {
        for column in &mut self.columns {
            if let Some(image) = column.image.take() {
                self.retired.push(image);
            }
        }
    }

    fn invalidate_current_images(&mut self) {
        for column in &mut self.columns {
            if column.generation == self.stream_generation
                && let Some(image) = column.image.take()
            {
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
        self.spectrogram_image_height = 0;
        self.heat_samples.clear();
        self.heat_sample_generation = 0;
    }

    fn history_window(&self, latest: Duration) -> Duration {
        let limit = Duration::from_secs_f64(self.history_limit_seconds);
        let available = self
            .columns
            .iter()
            .filter_map(|column| column.sample_time)
            .min()
            .map(|oldest| latest.saturating_sub(oldest));
        available
            .filter(|duration| !duration.is_zero())
            .unwrap_or(limit)
            .min(limit)
    }

    fn visible_range(&self) -> Option<(usize, usize, f32, f32)> {
        clipped_range(
            &self.frequencies,
            self.sample_rate,
            16.0,
            self.sample_rate as f32 * 0.5,
        )
    }

    fn spectrum_range(&self) -> Option<(usize, usize, f32, f32)> {
        clipped_range(
            &self.frequencies,
            self.sample_rate,
            16.0,
            self.sample_rate as f32 * 0.5,
        )
    }

    fn refresh_db_labels(&mut self) {
        self.db_labels = std::array::from_fn(|step| {
            db_label(self.top_db - self.spectrum_db_range * step as f32 / 3.0)
        });
    }

    fn refresh_frequency_labels(&mut self) {
        self.spectrum_bins.clear();
        if let Some((_, _, low, high)) = self.visible_range() {
            self.frequency_labels = [frequency_label(low), frequency_label(high)];
        }
        if let Some((_, _, low, high)) = self.spectrum_range() {
            self.spectrum_labels = [note_label(low), note_label(high)];
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
        let left_margin = axis_label_margin(self.spectrum_show_labels, &self.spectrum_labels);
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
            self.spectrum_style == SpectrumStyle::Solid,
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
                true,
                &mut self.spectrum_bins,
            );
        }
        let mut grid_path = PathBuilder::stroke(px(1.0));
        let mut has_grid = false;
        for step in 0..self.db_labels.len() {
            let fraction = step as f32 / (self.db_labels.len() - 1) as f32;
            let y = plot.top() + plot.size.height * fraction;
            if self.spectrum_grid {
                grid_path.move_to(point(plot.left(), y));
                grid_path.line_to(point(plot.right(), y));
                has_grid = true;
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
        if has_grid {
            let path = grid_path
                .build()
                .map_err(|error| format!("Cannot tessellate spectrum grid: {error}"))?;
            window.paint_path(path, rgb(0x14161b));
        }

        window.with_content_mask(
            Some(gpui::ContentMask { bounds: plot }),
            |window| -> Result<(), String> {
                self.spectrum_points.clear();
                let style = self.spectrum_style;
                let bottom = plot.bottom();
                let top = plot.top();
                let continuous = matches!(style, SpectrumStyle::Line | SpectrumStyle::Solid);
                if continuous {
                    // Reserve once before the point loop; clear retains capacity for later frames.
                    self.spectrum_points.reserve(bars);
                }
                // Fill contours share clockwise winding. NonZero keeps overlapping
                // body regions filled instead of cutting even-odd holes.
                let mut shape_path = if style == SpectrumStyle::Line {
                    PathBuilder::stroke(px(2.0))
                } else {
                    PathBuilder::fill().with_style(gpui::PathStyle::Fill(
                        gpui::FillOptions::default().with_fill_rule(gpui::FillRule::NonZero),
                    ))
                };
                let mut peak_path = PathBuilder::fill();
                let mut any_visible = false;
                let mut any_peak = false;
                for bar in 0..bars {
                    let left_t = bar as f32 / bars as f32;
                    let right_t = (bar + 1) as f32 / bars as f32;
                    let range = self.spectrum_bins[bar].clone();
                    let mut level = f32::NEG_INFINITY;
                    let mut peak = f32::NEG_INFINITY;
                    for index in range.clone() {
                        level = level.max(self.display_level(index));
                        if self.spectrum_peaks {
                            peak = peak.max(finite_level(self.peaks[index]));
                        }
                    }
                    let center_hz = axis_frequency(low, high, (left_t + right_t) * 0.5, true);
                    if self.spectrum_interpolate {
                        // Interpolation fills undersampled slots, but never averages
                        // away a transient already max-pooled into this slot.
                        level = level.max(self.interpolate_level(center_hz, low, high, false));
                        if self.spectrum_peaks {
                            peak = peak.max(self.interpolate_level(center_hz, low, high, true));
                        }
                    }
                    let slot_width = plot.size.width * (right_t - left_t);
                    let slot_gap = px(self.spectrum_gap)
                        .max(px(0.0))
                        .min((slot_width - px(1.0)).max(px(0.0)));
                    let level_fraction = db_height(level, self.top_db, self.spectrum_db_range);
                    let bar_height =
                        (plot.size.height * level_fraction).clamp(px(0.0), plot.size.height);
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
                            SpectrumStyle::Bars => {
                                let height = bar_height.max(px(1.0)).min(plot.size.height);
                                append_rect(
                                    &mut shape_path,
                                    Bounds::new(point(x, bottom - height), size(width, height)),
                                );
                            }
                            SpectrumStyle::Outline => {
                                let height = bar_height.max(px(1.0)).min(plot.size.height);
                                let edge = px(1.0).min(width * 0.5).min(height * 0.5);
                                append_rect(
                                    &mut shape_path,
                                    Bounds::new(point(x, bottom - height), size(width, edge)),
                                );
                                if height > edge {
                                    append_rect(
                                        &mut shape_path,
                                        Bounds::new(point(x, bottom - edge), size(width, edge)),
                                    );
                                }
                                if height > edge * 2.0 {
                                    append_rect(
                                        &mut shape_path,
                                        Bounds::new(
                                            point(x, bottom - height + edge),
                                            size(edge, height - edge * 2.0),
                                        ),
                                    );
                                    append_rect(
                                        &mut shape_path,
                                        Bounds::new(
                                            point(x + width - edge, bottom - height + edge),
                                            size(edge, height - edge * 2.0),
                                        ),
                                    );
                                }
                            }
                            SpectrumStyle::Led => {
                                let mut segment_bottom = bottom;
                                let segment_height = px(3.0);
                                let segment_gap = px(2.0);
                                while segment_bottom > bottom - bar_height {
                                    let segment_top =
                                        (segment_bottom - segment_height).max(bottom - bar_height);
                                    append_rect(
                                        &mut shape_path,
                                        Bounds::new(
                                            point(x, segment_top),
                                            size(width, segment_bottom - segment_top),
                                        ),
                                    );
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
                    if self.spectrum_peaks {
                        let peak_fraction = db_height(peak, self.top_db, self.spectrum_db_range);
                        let peak_visible = peak_fraction > 0.0;
                        let peak_height = plot.size.height * peak_fraction;
                        if peak_visible {
                            let peak_y = (bottom - peak_height).clamp(top, bottom - px(1.0));
                            let peak_width = if style == SpectrumStyle::Solid {
                                px(1.0)
                            } else if style == SpectrumStyle::Line {
                                (slot_width - slot_gap).max(px(1.0))
                            } else {
                                width
                            };
                            let peak_x = (x + (width - peak_width) * 0.5)
                                .clamp(plot.left(), (plot.right() - peak_width).max(plot.left()));
                            append_rect(
                                &mut peak_path,
                                Bounds::new(point(peak_x, peak_y), size(peak_width, px(1.0))),
                            );
                        }
                        any_peak |= peak_visible;
                    }
                }
                if continuous && any_visible {
                    append_spectrum_shape(
                        &mut shape_path,
                        &self.spectrum_points,
                        plot,
                        style == SpectrumStyle::Solid,
                        style == SpectrumStyle::Line || self.spectrum_interpolate,
                    );
                }
                if any_visible {
                    let path = shape_path
                        .build()
                        .map_err(|error| format!("Cannot tessellate spectrum: {error}"))?;
                    paint_spectrum_path(path, plot, window, self.palette_lut.as_ref());
                }
                if any_peak {
                    let path = peak_path
                        .build()
                        .map_err(|error| format!("Cannot tessellate spectrum peaks: {error}"))?;
                    paint_spectrum_path(path, plot, window, self.palette_lut.as_ref());
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
        let left_margin = axis_label_margin(self.spectrogram_show_labels, &self.frequency_labels);
        let plot = Bounds::new(
            point(bounds.left() + px(left_margin), bounds.top() + px(6.0)),
            size(
                bounds.size.width - px(left_margin + 8.0),
                bounds.size.height - px(12.0),
            ),
        );
        if plot.size.width <= px(2.0) || plot.size.height <= px(2.0) {
            return Ok(());
        }
        let Some(latest_time) = self.latest_sample_time else {
            return Ok(());
        };
        let Some((first, last, low, high)) = self.visible_range() else {
            return Ok(());
        };
        let plot_width = (plot.size.width / px(1.0)).ceil().max(2.0) as usize;
        let history_columns = history_columns_for_width(
            self.spectrogram_history_seconds,
            self.analysis_fps,
            plot_width,
        );
        self.resize_history(history_columns);
        self.history_limit_seconds = (self.history_columns as f64
            / self.analysis_fps.max(1) as f64)
            .min(self.spectrogram_history_seconds as f64);
        let history = self.history_window(latest_time);
        let start_time = latest_time.saturating_sub(history);
        let fallback_height = (plot.size.height / px(1.0)).ceil() as usize;
        let max_panel_height = self
            .spectrogram_panel_heights
            .values()
            .copied()
            .max()
            .unwrap_or(fallback_height);
        let source_rows = last.saturating_sub(first) + 1;
        let height = spectrogram_source_height(
            max_panel_height,
            source_rows,
            self.spectrogram_interpolation_points,
            self.spectrogram_sampling_points_scale,
        );
        let height_u32 = u32::try_from(height)
            .map_err(|_| "Spectrogram image height exceeds GPU image dimensions".to_string())?;
        if self.spectrogram_image_height != height
            || self.heat_sample_generation != self.stream_generation
        {
            self.invalidate_current_images();
            self.spectrogram_image_height = height;
            self.heat_samples = build_heat_samples(&self.frequencies, low, high, height);
            self.heat_sample_generation = self.stream_generation;
        }
        for index in 0..self.history_columns {
            let Some(sample_time) = self.columns[index].sample_time else {
                continue;
            };
            if sample_time < start_time || sample_time > latest_time {
                continue;
            }
            if self.columns[index].generation != self.stream_generation
                && self.columns[index].image.is_none()
            {
                continue;
            }
            if self.columns[index].image.is_none() {
                let levels = &self.columns[index].levels;
                let mut pixels = image::RgbaImage::new(1, height_u32);
                for (row, sample) in self.heat_samples.iter().enumerate() {
                    let level = sample_heat_level_at(levels, *sample, self.spectrogram_interpolate);
                    let intensity = ((level + self.spectrogram_db_range)
                        / self.spectrogram_db_range)
                        .clamp(0.0, 1.0);
                    let [red, green, blue] = if intensity > 0.0 {
                        self.palette_lut.lookup_rgb(intensity)
                    } else {
                        [0, 0, 0]
                    };
                    pixels.put_pixel(0, row as u32, image::Rgba([blue, green, red, 255]));
                }
                self.columns[index].image =
                    Some(Arc::new(RenderImage::new([image::Frame::new(pixels)])));
            }
            let right = history_fraction(sample_time, latest_time, history);
            let left = history_fraction(self.columns[index].interval_start, latest_time, history);
            let width = (plot.size.width * (right - left)).max(px(1.0));
            let column_bounds = Bounds::new(
                point(plot.left() + plot.size.width * left, plot.top()),
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

fn spectrum_bar_count(width: f32, solid: bool, requested: u32, bar_width: f32, gap: f32) -> usize {
    let drawable = (width.floor() as usize).max(1);
    let count = if solid {
        drawable
    } else if requested == 0 {
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

fn axis_label_margin(show_labels: bool, labels: &[SharedString]) -> f32 {
    if !show_labels {
        return 8.0;
    }
    let max_chars = labels.iter().map(|label| label.len()).max().unwrap_or(0) as f32;
    (max_chars * 7.0 + 8.0).max(32.0)
}

/// Append the continuous body outline without connecting separate Peak markers.
fn append_spectrum_shape(
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

fn append_rect(path: &mut PathBuilder, bounds: Bounds<Pixels>) {
    path.move_to(bounds.origin);
    path.line_to(point(bounds.right(), bounds.top()));
    path.line_to(point(bounds.right(), bounds.bottom()));
    path.line_to(point(bounds.left(), bounds.bottom()));
    path.close();
}

fn paint_spectrum_path(
    mut path: Path<Pixels>,
    plot: Bounds<Pixels>,
    window: &mut Window,
    palette: &PaletteLut,
) {
    path.bounds = plot;
    let stops = palette.visible_stops();
    let mut visible = Some(path);
    for (index, pair) in stops.windows(2).enumerate() {
        let start = palette.stop_position(pair[0].0);
        let end = palette.stop_position(pair[1].0);
        let mask = Bounds::new(
            point(plot.left(), plot.bottom() - plot.size.height * end),
            size(plot.size.width, plot.size.height * (end - start)),
        );
        let background = linear_gradient(
            0.0,
            linear_color_stop(rgb(stop_color(pair[0].1)), 0.0),
            linear_color_stop(rgb(stop_color(pair[1].1)), 1.0),
        );
        let mut segment_path = if index + 2 == stops.len() {
            visible
                .take()
                .expect("last spectrum gradient owns the path")
        } else {
            visible
                .as_ref()
                .expect("spectrum gradient path is retained")
                .clone()
        };
        segment_path.bounds = mask;
        window.with_content_mask(Some(gpui::ContentMask { bounds: mask }), |window| {
            window.paint_path(segment_path, background);
        });
    }
}

fn stop_color([red, green, blue]: [u8; 3]) -> u32 {
    (u32::from(red) << 16) | (u32::from(green) << 8) | u32::from(blue)
}
fn build_heat_samples(frequencies: &[f32], low: f32, high: f32, height: usize) -> Vec<HeatSample> {
    (0..height)
        .filter_map(|row| {
            let fraction = if height == 1 {
                0.0
            } else {
                row as f32 / (height - 1) as f32
            };
            let hz = (high.ln() + (low.ln() - high.ln()) * fraction).exp();
            heat_sample(frequencies, hz, low, high)
        })
        .collect()
}

fn heat_sample(frequencies: &[f32], hz: f32, low: f32, high: f32) -> Option<HeatSample> {
    let first = frequencies.partition_point(|value| *value < low);
    let end = frequencies.partition_point(|value| *value <= high);
    if first >= end {
        return None;
    }
    let right = frequencies
        .partition_point(|value| *value < hz)
        .clamp(first, end - 1);
    let left = right.saturating_sub(1).max(first);
    let fraction = if left == right {
        0.0
    } else {
        ((hz.ln() - frequencies[left].ln()) / (frequencies[right].ln() - frequencies[left].ln()))
            .clamp(0.0, 1.0)
    };
    Some(HeatSample {
        left,
        right,
        fraction,
    })
}

#[cfg(test)]
fn sample_heat_level(
    frequencies: &[f32],
    levels: &[f32],
    hz: f32,
    low: f32,
    high: f32,
    interpolate: bool,
) -> f32 {
    let Some(sample) = heat_sample(frequencies, hz, low, high) else {
        return f32::NEG_INFINITY;
    };
    sample_heat_level_at(levels, sample, interpolate)
}

fn sample_heat_level_at(levels: &[f32], sample: HeatSample, interpolate: bool) -> f32 {
    if levels.len() <= sample.right {
        return f32::NEG_INFINITY;
    }
    let left_level = finite_level(levels[sample.left]);
    let right_level = finite_level(levels[sample.right]);
    if sample.left == sample.right || !interpolate {
        if !interpolate && sample.left != sample.right {
            return if sample.fraction <= 0.5 {
                left_level
            } else {
                right_level
            };
        }
        return right_level;
    }
    if !left_level.is_finite() {
        return right_level;
    }
    if !right_level.is_finite() {
        return left_level;
    }
    left_level + (right_level - left_level) * sample.fraction
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
        let khz = frequency / 1000.0;
        if (khz - khz.round()).abs() < 0.05 {
            format!("{khz:.0} kHz").into()
        } else {
            format!("{khz:.1} kHz").into()
        }
    } else {
        format!("{frequency:.0} Hz").into()
    }
}

/// Cubic Hermite interpolation preserves the palette position and tangent
/// continuously inside each authored color-stop interval. The renderers use
/// these same intervals to assemble the full gradient with masked spans.
fn gradient(value: f32, stops: &[(f32, [u8; 3])]) -> [u8; 3] {
    debug_assert!(stops.len() >= 2);
    let value = value.clamp(0.0, 1.0);
    let index = stops
        .windows(2)
        .position(|pair| value <= pair[1].0)
        .unwrap_or(stops.len() - 2);
    let (x0, c0) = stops[index];
    let (x1, c1) = stops[index + 1];
    let span = (x1 - x0).max(f32::EPSILON);
    let t = ((value - x0) / span).clamp(0.0, 1.0);
    let slope = |point: usize, channel: usize| {
        if point == 0 {
            (stops[1].1[channel] as f32 - stops[0].1[channel] as f32)
                / (stops[1].0 - stops[0].0).max(f32::EPSILON)
        } else if point + 1 == stops.len() {
            let left = point - 1;
            (stops[point].1[channel] as f32 - stops[left].1[channel] as f32)
                / (stops[point].0 - stops[left].0).max(f32::EPSILON)
        } else {
            let left = point - 1;
            let right = point + 1;
            (stops[right].1[channel] as f32 - stops[left].1[channel] as f32)
                / (stops[right].0 - stops[left].0).max(f32::EPSILON)
        }
    };
    let t2 = t * t;
    let t3 = t2 * t;
    let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
    let h10 = t3 - 2.0 * t2 + t;
    let h01 = -2.0 * t3 + 3.0 * t2;
    let h11 = t3 - t2;
    std::array::from_fn(|channel| {
        let value = h00 * c0[channel] as f32
            + h10 * span * slope(index, channel)
            + h01 * c1[channel] as f32
            + h11 * span * slope(index + 1, channel);
        value.round().clamp(0.0, 255.0) as u8
    })
}
fn note_label(frequency: f32) -> SharedString {
    const NAMES: [&str; 12] = [
        "C", "C♯", "D", "D♯", "E", "F", "F♯", "G", "G♯", "A", "A♯", "B",
    ];
    let midi = (12.0 * (frequency / 440.0).log2() + 69.0).round() as i32;
    let octave = midi.div_euclid(12) - 1;
    format!("{}{octave}", NAMES[midi.rem_euclid(12) as usize]).into()
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
    fn deadbeef_palettes_have_expected_endpoints() {
        assert_eq!(
            active_palette_lut(VisualizationPalette::Deadbeef).lookup_raw(0.0),
            0x000000
        );
        assert_eq!(
            active_palette_lut(VisualizationPalette::Deadbeef).lookup_raw(1.0),
            0xff0000
        );
        assert_eq!(gradient(0.0, DEADBEEF_STOPS), [0, 0, 0]);
        assert_eq!(gradient(1.0, DEADBEEF_STOPS), [255, 0, 0]);
    }
    #[test]
    fn visual_palette_lookup_applies_deadbeef_low_end_mapping() {
        let lut = active_palette_lut(VisualizationPalette::Deadbeef);
        assert_eq!(lut.lookup_mapped(0.0), lut.lookup_raw(0.06));
        assert_eq!(lut.lookup_mapped(1.0), lut.lookup_raw(1.0));
        let tokyo = active_palette_lut(VisualizationPalette::TokyoNight);
        assert_eq!(tokyo.lookup_mapped(0.0), tokyo.lookup_raw(0.0));
    }

    #[test]
    fn palette_stop_positions_fill_the_visible_height() {
        let deadbeef = active_palette_lut(VisualizationPalette::Deadbeef);
        let deadbeef_stops = deadbeef.visible_stops();
        assert_eq!(deadbeef.stop_position(deadbeef_stops[0].0), 0.0);
        assert_eq!(
            deadbeef.stop_position(deadbeef_stops.last().unwrap().0),
            1.0
        );

        let tokyo = active_palette_lut(VisualizationPalette::TokyoNight);
        let tokyo_stops = tokyo.visible_stops();
        assert_eq!(tokyo.stop_position(tokyo_stops[0].0), 0.0);
        assert_eq!(tokyo.stop_position(tokyo_stops.last().unwrap().0), 1.0);
    }

    #[test]
    fn smooth_gradient_preserves_palette_stops() {
        for stops in [TOKYO_NIGHT_STOPS, DEADBEEF_STOPS] {
            for &(position, color) in stops {
                assert_eq!(gradient(position, stops), color);
            }
        }
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
                analysis_fps: fps,
                spectrogram_history_seconds: 120,
                ..Config::default()
            });
            for step in 1..=240 * fps {
                visuals.update(&frame(step as f64 / fps as f64, -30.0));
            }
            let data = visuals.data.borrow();
            assert_eq!(data.len, history_column_count(120, fps));
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

    fn configured_visuals() -> Visuals {
        let mut visuals = Visuals::new();
        visuals.configure(&Config::default());
        visuals
    }

    #[test]
    fn history_window_grows_with_continuous_audio_until_limit() {
        let mut visuals = configured_visuals();
        visuals.update(&frame(1.0, -30.0));
        visuals.update(&frame(1.5, -30.0));
        assert_eq!(
            visuals
                .data
                .borrow()
                .history_window(Duration::from_secs_f64(1.5)),
            Duration::from_millis(500)
        );
        for step in 4..=62 {
            visuals.update(&frame(step as f64 * 0.5, -30.0));
        }
        assert_eq!(
            visuals
                .data
                .borrow()
                .history_window(Duration::from_secs(31)),
            Duration::from_secs(20)
        );
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
        assert_eq!(data.len, 2);
        assert!(
            data.columns
                .iter()
                .any(|column| column.levels == vec![12.0; 4])
        );
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
    fn history_preserves_gaps_and_stream_changes() {
        let mut visuals = configured_visuals();
        visuals.update(&frame(1.0, -20.0));
        visuals.update(&frame(6.0, -30.0));
        {
            let data = visuals.data.borrow();
            assert_eq!(data.len, 2);
            let last = &data.columns[data.latest.unwrap()];
            assert_eq!(last.interval_start, Duration::from_secs(6));
            assert!(
                data.columns
                    .iter()
                    .any(|column| column.sample_time == Some(Duration::from_secs(1)))
            );
        }
        let mut changed_rate = frame(6.5, -40.0);
        changed_rate.sample_rate = 44_100;
        visuals.update(&changed_rate);
        assert_eq!(visuals.data.borrow().len, 3);
    }

    #[test]
    fn range_changes_keep_shared_frequency_axis_and_history() {
        let mut visuals = configured_visuals();
        let mut config = Config::default();
        visuals.update(&frame(1.0, -30.0));
        let image = Arc::new(RenderImage::new([image::Frame::new(
            image::RgbaImage::new(1, 1),
        )]));
        visuals.data.borrow_mut().columns[0].image = Some(Arc::clone(&image));
        config.spectrum_db_range = 40.0;
        visuals.configure(&config);
        {
            let data = visuals.data.borrow();
            assert_eq!(data.visible_range().unwrap().2, 20.0);
            assert_eq!(data.spectrum_range().unwrap().2, 20.0);
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
    fn spectrum_range_and_grid_update_without_other_changes() {
        let mut visuals = Visuals::new();
        let initial = Config::default();
        visuals.configure(&initial);

        let mut range = initial.clone();
        range.spectrum_db_range = 40.0;
        visuals.configure(&range);
        {
            let data = visuals.data.borrow();
            assert_eq!(data.spectrum_db_range, 40.0);
            assert!(data.spectrum_grid);
        }

        let mut grid = range;
        grid.spectrum_grid = false;
        visuals.configure(&grid);
        let data = visuals.data.borrow();
        assert!(!data.spectrum_grid);
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
    fn spectrogram_source_uses_configured_scale_and_source_floor() {
        assert_eq!(spectrogram_source_height(320, 140, 1024, 0.5), 160);
        assert_eq!(spectrogram_source_height(700, 140, 1024, 0.5), 350);
        assert_eq!(spectrogram_source_height(1400, 140, 1024, 0.5), 700);
        assert_eq!(spectrogram_source_height(1400, 140, 512, 0.5), 512);
        assert_eq!(spectrogram_source_height(80, 140, 1024, 0.5), 140);
        assert_eq!(spectrogram_source_height(320, 140, 1024, 1.4), 448);
    }

    #[test]
    fn history_columns_follow_width_and_rate() {
        assert_eq!(history_columns_for_width(30, 20, 400), 400);
        assert_eq!(history_columns_for_width(30, 20, 800), 600);
        assert_eq!(history_columns_for_width(5, 20, 800), 100);
    }

    #[test]
    fn shrinking_history_keeps_the_newest_columns() {
        let mut visuals = configured_visuals();
        for time in 1..=5 {
            visuals.update(&frame(time as f64, -30.0));
        }
        let mut data = visuals.data.borrow_mut();
        data.resize_history(3);
        assert_eq!(data.len, 3);
        assert_eq!(
            data.columns
                .iter()
                .filter_map(|column| column.sample_time)
                .collect::<Vec<_>>(),
            [
                Duration::from_secs(3),
                Duration::from_secs(4),
                Duration::from_secs(5),
            ]
        );
    }

    #[test]
    fn spectrogram_interpolation_is_linear_in_log_frequency_and_db() {
        let frequencies = [100.0, 200.0, 400.0];
        let levels = [-40.0, -20.0, 0.0];
        let midpoint = (100.0_f32 * 200.0).sqrt();
        let level = sample_heat_level(&frequencies, &levels, midpoint, 100.0, 400.0, true);
        assert!((level - (-30.0)).abs() < 0.0001);
        assert_eq!(
            sample_heat_level(&frequencies, &levels, 120.0, 100.0, 400.0, false),
            -40.0
        );
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
        assert_eq!(spectrum_bar_count(1200.0, false, 0, 3.0, 1.0), 300);
        assert_eq!(spectrum_bar_count(4096.0, false, 0, 3.0, 1.0), 1024);
        assert_eq!(spectrum_bar_count(1200.0, false, 512, 20.0, 8.0), 512);
        assert_eq!(spectrum_bar_count(7.0, false, 512, 3.0, 1.0), 7);
        assert_eq!(spectrum_bar_count(0.0, false, 0, 3.0, 1.0), 1);
        assert_eq!(spectrum_bar_count(1200.0, true, 1, 3.0, 1.0), 1200);
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
}
