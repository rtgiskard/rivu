//! GPU-backed analysis views. The audio worker owns the configurable FFT;
//! these views retain every musical band supplied by `AnalysisFrame`.
//!
//! Spectrogram history follows the active plot width at one time column per
//! pixel, bounded by the configured duration and analysis rate. Dense incoming
//! frames are max-pooled so narrow transients survive; Spectrum separately
//! retains the latest frame. The available history expands until that dynamic
//! limit; unavailable history is left blank, including long gaps in publication.
//! History and derived numerical publications are released when no Spectrogram
//! panel is visible. Range and axis changes remap cached levels while paused;
//! unchanged buckets reuse immutable GPU fields. Sampling rows follow panel height
//! and visible frequency points, capped by configuration and the startup GPU budget.
use crate::i18n::Localizer;

use gpui::{
    AnyElement, App, Bounds, GpuFieldParams, GpuFrame, GpuLimits, GpuPalette, GpuPathParams,
    GpuRectangle, GpuRectangleParams, GpuSource, GpuValueMapping, PathBuilder, Pixels, Point,
    SharedString, TextAlign, Window, canvas, div, fill, point, prelude::*, px, rgb, size,
};
use std::{
    cell::{OnceCell, RefCell},
    collections::HashMap,
    ops::Range,
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use crate::{
    analysis::AnalysisFrame,
    config::{AnalysisWindow, Config, SpectrumStyle, VisualizationPalette},
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

fn visible_palette_fraction(palette: VisualizationPalette, fraction: f32) -> f32 {
    let fraction = fraction.clamp(0.0, 1.0);
    match palette {
        VisualizationPalette::Deadbeef => 0.06 + fraction * 0.94,
        VisualizationPalette::TokyoNight => fraction,
    }
}

const PALETTE_LUT_SIZE: usize = 1024;
const SPECTRUM_GPU_PALETTE_SIZE: usize = PALETTE_LUT_SIZE;

pub(super) struct PaletteLut {
    palette: VisualizationPalette,
    colors: [[u8; 3]; PALETTE_LUT_SIZE],
    spectrum_gpu_palette: OnceCell<Result<Arc<GpuPalette>, String>>,
    spectrogram_gpu_palette: OnceCell<Result<Arc<GpuPalette>, String>>,
    waveform_gpu_palette: OnceCell<Result<Arc<GpuPalette>, String>>,
}

impl PaletteLut {
    fn new(palette: VisualizationPalette) -> Self {
        Self {
            palette,
            colors: std::array::from_fn(|index| {
                let fraction = index as f32 / (PALETTE_LUT_SIZE - 1) as f32;
                gradient(fraction, palette_stops(palette))
            }),
            spectrum_gpu_palette: OnceCell::new(),
            spectrogram_gpu_palette: OnceCell::new(),
            waveform_gpu_palette: OnceCell::new(),
        }
    }

    fn spectrum_gpu_palette(&self) -> Result<Arc<GpuPalette>, String> {
        self.spectrum_gpu_palette
            .get_or_init(|| {
                let colors: [[f32; 4]; SPECTRUM_GPU_PALETTE_SIZE] = std::array::from_fn(|index| {
                    let fraction = index as f32 / (SPECTRUM_GPU_PALETTE_SIZE - 1) as f32;
                    let [red, green, blue] = gradient(
                        visible_palette_fraction(self.palette, fraction),
                        palette_stops(self.palette),
                    );
                    [red, green, blue, 255].map(|channel| channel as f32 / 255.0)
                });
                GpuPalette::new(&colors, SPECTRUM_GPU_PALETTE_SIZE)
                    .map_err(|error| format!("Cannot create spectrum GPU palette: {error}"))
            })
            .clone()
    }

    fn spectrogram_gpu_palette(&self) -> Result<Arc<GpuPalette>, String> {
        self.spectrogram_gpu_palette
            .get_or_init(|| {
                let colors: [[f32; 4]; PALETTE_LUT_SIZE] = std::array::from_fn(|index| {
                    let [r, g, b] = self.colors[index];
                    [r, g, b, 255].map(|v| v as f32 / 255.0)
                });
                GpuPalette::new(&colors, PALETTE_LUT_SIZE).map_err(|error| error.to_string())
            })
            .clone()
    }

    // Adjacent pairs keep each RMS ramp a two-color RGB mix, not a palette sweep.
    pub(super) fn waveform_gpu_palette(&self) -> Result<Arc<GpuPalette>, String> {
        self.waveform_gpu_palette
            .get_or_init(|| {
                let floor = self.lookup_mapped_rgb(0.0);
                let neutral = floor.iter().map(|&v| u32::from(v)).sum::<u32>() / 3;
                let low = floor.map(|v| ((u32::from(v) + 3 * neutral) / 6) as u8);
                let colors: Vec<_> = self
                    .colors
                    .iter()
                    .flat_map(|&high| [low, high])
                    .map(|[r, g, b]| [r, g, b, 255].map(|v| f32::from(v) / 255.0))
                    .collect();
                GpuPalette::new(&colors, PALETTE_LUT_SIZE * 2).map_err(|error| error.to_string())
            })
            .clone()
    }

    pub(super) fn waveform_gpu_range(&self, fraction: f32) -> [f32; 2] {
        let mapped = visible_palette_fraction(self.palette, fraction);
        let index = (mapped.clamp(0.0, 1.0) * (PALETTE_LUT_SIZE - 1) as f32).round() as usize;
        let denominator = (PALETTE_LUT_SIZE * 2 - 1) as f32;
        [
            (index * 2) as f32 / denominator,
            (index * 2 + 1) as f32 / denominator,
        ]
    }

    pub(super) fn lookup_rgb(&self, fraction: f32) -> [u8; 3] {
        let index = (fraction.clamp(0.0, 1.0) * (PALETTE_LUT_SIZE - 1) as f32).round() as usize;
        self.colors[index]
    }

    // Waveform and Spectrum exclude DeaDBeeF's black floor; Spectrogram does not.
    pub(super) fn lookup_mapped_rgb(&self, fraction: f32) -> [u8; 3] {
        self.lookup_rgb(visible_palette_fraction(self.palette, fraction))
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

const SPECTROGRAM_TILE_COLUMNS: usize = 32;
type SpectrogramTileKey = [Option<(u64, Duration, Duration)>; SPECTROGRAM_TILE_COLUMNS];
struct SpectrogramTile {
    key: SpectrogramTileKey,
    start: Duration,
    end: Duration,
    publication: Arc<GpuFrame>,
}

#[derive(Default)]
struct Column {
    levels: Vec<f32>,
    sample_time: Option<Duration>,
    bucket_start: Option<Duration>,
    interval_start: Duration,
    revision: u64,
    generation: u64,
    source_generation: Option<u64>,
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
    // Tiles can straddle the visible window; clipping must not stretch their gaps.
    (1.0 + (time.as_secs_f64() - latest.as_secs_f64()) / history.as_secs_f64()) as f32
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

#[cfg(test)]
mod sampling_budget_tests {
    use super::*;
    #[test]
    fn maximum_history_reduces_rows_without_changing_time_resolution() {
        assert_eq!(history_column_count(60, 60), 3600);
        assert_eq!(spectrogram_budget_rows(3600, 64 * 1024 * 1024), 580);
        assert_eq!(spectrogram_budget_rows(3600, 16 * 1024 * 1024), 143);
        assert!(spectrogram_budget_rows(400, 64 * 1024 * 1024) >= 4096);
    }
}

// Half the shared budget remains available for other content and placements.
// The other half accommodates two complete history generations, each charged
// for both its immutable buffer and initialization copy. This is a sampling
// target, not a reservation or a guarantee for arbitrary pending updates.
fn spectrogram_budget_rows(columns: usize, budget_bytes: u64) -> usize {
    let per_column = budget_bytes / 8 / columns.max(1) as u64;
    usize::try_from(per_column.saturating_sub(8) / 4)
        .unwrap_or(usize::MAX)
        .max(1)
}

fn visual_canvas_has_room(size: gpui::Size<Pixels>) -> bool {
    size.width > px(64.0) && size.height > px(40.0)
}

fn spectrum_labels_visible(configured: bool, height: Pixels) -> bool {
    // Four 10px dB labels need separation as well as the bottom note labels.
    configured && height >= px(78.0)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VisualMode {
    Spectrum,
    Spectrogram,
}

#[derive(PartialEq)]
struct SpectrumGeometryKey {
    origin: Point<Pixels>,
    width: Pixels,
    height: Pixels,
    low: f32,
    high: f32,
    style: SpectrumStyle,
    palette: VisualizationPalette,
    bars: usize,
    bar_width: f32,
    bar_gravity: f32,
    interpolate: bool,
    peaks: bool,
    top_db: f32,
    db_range: f32,
    gap: f32,
    revision: u64,
}

#[derive(PartialEq)]
struct SpectrumGroupingKey {
    bars: usize,
    generation: u64,
    low: f32,
    high: f32,
}

struct SpectrumGeometry {
    key: Option<SpectrumGeometryKey>,
    grouping_key: SpectrumGroupingKey,
    bins: Vec<Range<usize>>,
    body: Option<Arc<GpuFrame>>,
}

struct VisualData {
    enabled: bool,
    spectrogram_visible: bool,
    last_analysis_sequence: Option<u64>,
    spectrogram_panel_sizes: HashMap<u64, (usize, usize)>,
    spectrogram_panel_height: usize,
    frequencies: Vec<f32>,
    sample_rate: u32,
    frequency_labels: [SharedString; 2],
    spectrum_labels: [SharedString; 2],
    db_labels: [SharedString; 4],
    spectrum_bins: Vec<Range<usize>>,
    heat_samples: Vec<HeatSample>,
    heat_sample_generation: u64,
    spectrogram_sample_height: usize,
    material_budget_bytes: Option<u64>,
    spectrogram_tiles: Vec<Option<SpectrogramTile>>,
    tile_values: Vec<f32>,
    columns: Vec<Column>,
    latest_levels: Vec<f32>,
    peaks: Vec<f32>,
    peak_deadlines: Vec<Duration>,
    bar_levels: Vec<f32>,
    bar_deadlines: Vec<Duration>,
    smoothed_levels: Vec<f32>,
    spectrum_visible: bool,
    envelope_initialized: bool,
    spectrum_points: Vec<Point<Pixels>>,
    spectrum_rectangles: Vec<GpuRectangle>,
    spectrum_geometry: HashMap<u64, SpectrumGeometry>,
    spectrum_geometry_revision: u64,
    visualization_cache: bool,
    last_update: Option<Duration>,
    top_db: f32,
    visual_background: u32,
    palette_lut: Rc<PaletteLut>,
    spectrum_db_range: f32,
    spectrum_bars: u32,
    spectrum_bar_width: f32,
    spectrum_interpolate: bool,
    spectrum_show_labels: bool,
    analysis_fft_size: u32,
    analysis_window: AnalysisWindow,
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
    source_generation: Option<u64>,
    head: usize,
    len: usize,
    latest: Option<usize>,
    latest_sample_time: Option<Duration>,
    error: Option<String>,
}

#[derive(Clone)]
pub(in crate::gui) struct Visuals {
    // GPUI prepaint/paint closures are 'static. Share the UI-thread state rather
    // than copying the full history into each panel's render closure.
    data: Rc<RefCell<VisualData>>,
}

impl Visuals {
    pub(in crate::gui) fn new() -> Self {
        // History is initialized only after visible Spectrogram demand.
        Self {
            data: Rc::new(RefCell::new(VisualData {
                enabled: true,
                spectrogram_visible: false,
                last_analysis_sequence: None,
                frequencies: Vec::new(),
                sample_rate: 0,
                frequency_labels: ["8 Hz".into(), "11 kHz".into()],
                spectrum_labels: ["20 Hz".into(), "20 kHz".into()],
                db_labels: std::array::from_fn(|step| db_label(-70.0 * step as f32 / 3.0)),
                spectrum_bins: Vec::new(),
                heat_samples: Vec::new(),
                spectrogram_panel_sizes: HashMap::new(),
                spectrogram_panel_height: 0,
                spectrogram_sample_height: 0,
                material_budget_bytes: None,
                spectrogram_tiles: Vec::new(),
                tile_values: Vec::new(),
                columns: Vec::new(),
                latest_levels: Vec::new(),
                peaks: Vec::new(),
                peak_deadlines: Vec::new(),
                bar_levels: Vec::new(),
                bar_deadlines: Vec::new(),
                smoothed_levels: Vec::new(),
                spectrum_visible: true,
                envelope_initialized: false,
                spectrum_points: Vec::new(),
                spectrum_rectangles: Vec::new(),
                spectrum_geometry: HashMap::new(),
                spectrum_geometry_revision: 0,
                visualization_cache: true,
                last_update: None,
                top_db: 0.0,
                palette_lut: active_palette_lut(VisualizationPalette::Deadbeef),
                visual_background: 0x08090c,
                spectrum_db_range: 70.0,
                spectrum_bars: 0,
                spectrum_bar_width: 3.0,
                spectrum_interpolate: true,
                spectrum_show_labels: true,
                analysis_fft_size: 8192,
                analysis_window: AnalysisWindow::BlackmanHarris,
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
                source_generation: None,
                head: 0,
                len: 0,
                latest: None,
                latest_sample_time: None,
                error: None,
            })),
        }
    }

    /// Suspend analysis views and release display publications without changing
    /// their settings. Framework scenes detach their owners when hidden.
    pub(in crate::gui) fn set_enabled(&mut self, enabled: bool) {
        let mut data = self.data.borrow_mut();
        if data.enabled == enabled {
            return;
        }
        data.enabled = enabled;
        if enabled {
            return;
        }
        data.release_history();
        data.spectrogram_panel_sizes = HashMap::new();
        data.frequencies = Vec::new();
        data.sample_rate = 0;
        data.latest_levels = Vec::new();
        data.peaks = Vec::new();
        data.peak_deadlines = Vec::new();
        data.bar_levels = Vec::new();
        data.bar_deadlines = Vec::new();
        data.smoothed_levels = Vec::new();
        data.spectrum_bins = Vec::new();
        data.spectrum_points = Vec::new();
        data.spectrum_rectangles = Vec::new();
        data.spectrum_geometry = HashMap::new();
        data.last_analysis_sequence = None;
        data.envelope_initialized = false;
        data.last_update = None;
        data.source_generation = None;
        data.stream_generation = data.stream_generation.wrapping_add(1);
        data.spectrum_geometry_revision = data.spectrum_geometry_revision.wrapping_add(1);
        data.top_db = 0.0;
        data.refresh_db_labels();
    }

    pub(in crate::gui) fn configure(&mut self, config: &Config) -> bool {
        let mut data = self.data.borrow_mut();
        data.material_budget_bytes
            .get_or_insert(config.gpu_material_budget_bytes());
        let spectrum_db_range = config.spectrum_db_range();
        let spectrogram_db_range = config.spectrogram_db_range();
        let configured_history_columns =
            history_column_count(config.spectrogram_history_seconds, config.analysis_fps);
        let palette_changed = data.palette_lut.palette != config.visual_palette;
        let cache_changed = data.visualization_cache != config.visualization_cache;
        let history_missing = data.enabled && data.spectrogram_visible && data.history_columns == 0;
        let changed = history_missing
            || palette_changed
            || data.visual_background != config.visual_background.rgb()
            || data.spectrum_db_range != spectrum_db_range
            || data.spectrogram_db_range != spectrogram_db_range
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
            || data.spectrogram_show_labels != config.spectrogram_labels
            || data.analysis_fft_size != config.analysis_fft_size
            || data.analysis_window != config.analysis_window
            || data.spectrogram_interpolate != config.spectrogram_interpolate
            || data.spectrogram_interpolation_points != config.spectrogram_interpolation_points
            || data.spectrogram_sampling_points_scale != config.spectrogram_sampling_points_scale
            || data.spectrogram_history_seconds != config.spectrogram_history_seconds
            || data.analysis_fps != config.analysis_fps
            || data.configured_history_columns != configured_history_columns
            || cache_changed;
        if !changed {
            return false;
        }
        let history_changed = history_missing
            || data.spectrogram_history_seconds != config.spectrogram_history_seconds
            || data.analysis_fps != config.analysis_fps
            || data.configured_history_columns != configured_history_columns;
        let analysis_changed = data.analysis_fft_size != config.analysis_fft_size
            || data.analysis_window != config.analysis_window;
        let heat_changed = palette_changed
            || data.visual_background != config.visual_background.rgb()
            || data.spectrogram_db_range != spectrogram_db_range
            || data.spectrogram_interpolate != config.spectrogram_interpolate
            || data.spectrogram_interpolation_points != config.spectrogram_interpolation_points
            || data.spectrogram_sampling_points_scale != config.spectrogram_sampling_points_scale;
        if palette_changed {
            data.palette_lut = active_palette_lut(config.visual_palette);
        }
        data.visual_background = config.visual_background.rgb();
        data.spectrum_db_range = spectrum_db_range;
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
        data.analysis_fft_size = config.analysis_fft_size;
        data.analysis_window = config.analysis_window;
        data.spectrum_grid = config.spectrum_grid;
        data.spectrogram_db_range = spectrogram_db_range;
        data.visualization_cache = config.visualization_cache;
        if cache_changed {
            data.clear_derived_caches();
        }
        data.spectrogram_history_seconds = config.spectrogram_history_seconds;
        data.history_limit_seconds = config.spectrogram_history_seconds as f64;
        data.analysis_fps = config.analysis_fps;
        data.configured_history_columns = configured_history_columns;
        if data.enabled && data.spectrogram_visible && history_changed {
            data.resize_history(configured_history_columns);
        }
        if analysis_changed {
            data.clear_history();
            data.latest_levels.clear();
            data.last_update = None;
            data.envelope_initialized = false;
        } else if heat_changed {
            data.invalidate_spectrogram();
        }
        let levels = if data.spectrum_peaks {
            &data.peaks
        } else {
            &data.latest_levels
        };
        data.top_db = (levels.iter().copied().fold(0.0, f32::max) / 6.0).ceil() * 6.0;
        data.refresh_frequency_labels();
        data.refresh_db_labels();
        true
    }

    pub(in crate::gui) fn update(&mut self, frame: &AnalysisFrame) {
        let mut data = self.data.borrow_mut();
        if !data.enabled {
            return;
        }
        if !frame.settings.is_some_and(|settings| {
            settings.fft_size == data.analysis_fft_size && settings.window == data.analysis_window
        }) {
            return;
        }
        if data.source_generation != Some(frame.generation) {
            data.source_generation = Some(frame.generation);
            data.envelope_initialized = false;
            data.last_update = None;
        }
        if frame.spectrum_db.len() != frame.frequencies_hz.len() {
            data.report_error("Analysis frequency and level counts differ".into());
            return;
        }
        let sample_rate_changed = data.sample_rate != frame.sample_rate;
        data.sample_rate = frame.sample_rate;
        if sample_rate_changed || data.frequencies != frame.frequencies_hz {
            data.invalidate_spectrogram();
            data.stream_generation = data.stream_generation.wrapping_add(1);
            data.latest_levels.clear();
            data.envelope_initialized = false;
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
        if data.latest_sample_time.is_some_and(|last| now < last) {
            data.clear_history();
            data.last_update = None;
            data.envelope_initialized = false;
        }
        data.latest_levels.clone_from(&frame.spectrum_db);
        if data.spectrogram_visible && data.last_analysis_sequence != Some(frame.sequence) {
            data.push_history(frame.sample_time, &frame.spectrum_db);
        }
        data.last_analysis_sequence = Some(frame.sequence);
        data.latest_sample_time = Some(frame.sample_time);
        if !data.spectrum_visible {
            return;
        }
        let last = data.last_update.replace(now).unwrap_or(now);
        let mut body_changed =
            !data.envelope_initialized || data.peaks.len() != frame.spectrum_db.len();
        if body_changed {
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
            data.envelope_initialized = true;
        }
        let decay = smoothing_decay(now.saturating_sub(last), data.spectrum_smoothing_ms);
        for index in 0..data.peaks.len() {
            // Deadlines and peak markers advance independently of the cached body.
            let before = data.display_level(index);
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
            body_changed |= before != data.display_level(index);
        }
        if body_changed {
            data.spectrum_geometry_revision = data.spectrum_geometry_revision.wrapping_add(1);
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
    }

    pub(in crate::gui) fn spectrum(
        &self,
        theme: &'static rivu_ui::theme::Palette,
        i18n: &Arc<Localizer>,
        panel_id: u64,
    ) -> AnyElement {
        self.view(theme, i18n, panel_id, VisualMode::Spectrum)
    }

    pub(in crate::gui) fn spectrogram(
        &self,
        theme: &'static rivu_ui::theme::Palette,
        i18n: &Arc<Localizer>,
        panel_id: u64,
    ) -> AnyElement {
        self.view(theme, i18n, panel_id, VisualMode::Spectrogram)
    }
    pub(in crate::gui) fn retain_spectrogram_panels(&mut self, panel_ids: &[u64]) {
        let mut data = self.data.borrow_mut();
        // Cached panel views may reuse prepaint. Keep their last measurement
        // until the panel is hidden/removed or prepaints with changed bounds.
        data.spectrogram_panel_sizes
            .retain(|panel_id, _| panel_ids.contains(panel_id));
        data.spectrogram_visible = !panel_ids.is_empty();
        if panel_ids.is_empty() {
            data.release_history();
        }
    }

    pub(in crate::gui) fn set_spectrum_visible(&self, visible: bool) -> bool {
        let mut data = self.data.borrow_mut();
        let restored = visible && !data.spectrum_visible;
        if data.spectrum_visible != visible {
            data.spectrum_visible = visible;
            data.envelope_initialized = false;
            data.last_update = None;
        }
        restored
    }

    pub(in crate::gui) fn retain_spectrum_panels(&mut self, panel_ids: &[u64]) {
        self.data
            .borrow_mut()
            .spectrum_geometry
            .retain(|panel_id, _| panel_ids.contains(panel_id));
    }

    // The root's first paint runs after every panel's prepaint. Size shared
    // history once here, never while individual panels are painting it.
    pub(in crate::gui) fn finish_prepaint(&self) -> bool {
        self.data.borrow_mut().finish_prepaint()
    }

    fn view(
        &self,
        theme: &'static rivu_ui::theme::Palette,
        i18n: &Arc<Localizer>,
        panel_id: u64,
        mode: VisualMode,
    ) -> AnyElement {
        let data = Rc::clone(&self.data);
        let message: Option<SharedString> = {
            let data = self.data.borrow();
            if data.error.is_some() || data.latest_levels.is_empty() {
                None
            } else {
                match mode {
                    VisualMode::Spectrum if data.spectrum_range().is_none() => {
                        Some(i18n.text("visual-no-spectrum").to_owned().into())
                    }
                    VisualMode::Spectrogram if data.visible_range().is_none() => {
                        Some(i18n.text("visual-no-spectrogram").to_owned().into())
                    }
                    _ => None,
                }
            }
        };
        let i18n = Arc::clone(i18n);
        let showing_status = message.is_some();
        let prepaint_data = Rc::clone(&data);
        let kind = match mode {
            VisualMode::Spectrum => "spectrum",
            VisualMode::Spectrogram => "spectrogram",
        };
        let visual_background = self.data.borrow().visual_background;
        gpui::container_query(move |available, _, _| {
            if !visual_canvas_has_room(available) {
                let message: SharedString = i18n.text("visual-too-small").to_owned().into();
                let tooltip = message.clone();
                return div()
                    .id((kind, panel_id))
                    .size_full()
                    .min_w_0()
                    .min_h_0()
                    .overflow_y_scroll()
                    .bg(rgb(theme.panel))
                    .text_color(rgb(theme.secondary))
                    .text_size(px(10.0))
                    .line_height(px(12.0))
                    .tooltip(rivu_ui::text_tooltip(tooltip))
                    .child(message)
                    .into_any_element();
            }
            div()
                .id((kind, panel_id))
                .relative()
                .size_full()
                .min_w_0()
                .min_h_0()
                .overflow_hidden()
                .bg(rgb(visual_background))
                .child(
                    canvas(
                        move |bounds, _, _| {
                            if mode == VisualMode::Spectrogram {
                                let mut data = prepaint_data.borrow_mut();
                                let margin = axis_label_margin(
                                    data.spectrogram_show_labels,
                                    &data.frequency_labels,
                                );
                                let width = ((bounds.size.width - px(margin + 8.0)) / px(1.0))
                                    .ceil()
                                    .max(2.0) as usize;
                                let height = ((bounds.size.height - px(12.0)) / px(1.0))
                                    .ceil()
                                    .max(1.0) as usize;
                                data.spectrogram_panel_sizes
                                    .insert(panel_id, (width, height));
                            }
                        },
                        move |bounds, _, window, cx| {
                            let mut data = data.borrow_mut();
                            window.paint_quad(fill(bounds, rgb(data.visual_background)));
                            if !visual_canvas_has_room(bounds.size) {
                                return;
                            }
                            if !showing_status {
                                let result = match mode {
                                    VisualMode::Spectrum => {
                                        data.paint_spectrum(panel_id, bounds, window, cx)
                                    }
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
                                    rgb(theme.error_bg),
                                ));
                                paint_label(
                                    i18n.format(
                                        "visual-error",
                                        &[("error", error.as_str().into())],
                                    )
                                    .into(),
                                    point(bounds.left() + px(6.0), bounds.top() + px(3.0)),
                                    theme.error,
                                    window,
                                    cx,
                                );
                            }
                        },
                    )
                    .size_full(),
                )
                .children(message.map(|message| {
                    super::visualization_status(message)
                        .absolute()
                        .top_0()
                        .left_0()
                }))
                .into_any_element()
        })
        .into_any_element()
    }
}

impl VisualData {
    fn finish_prepaint(&mut self) -> bool {
        if !self.enabled || !self.spectrogram_visible {
            return false;
        }
        let Some(width) = self
            .spectrogram_panel_sizes
            .values()
            .map(|size| size.0)
            .max()
        else {
            // No measurement yet; keep sizing out of individual panel paints.
            return false;
        };
        let previous = (self.history_columns, self.spectrogram_panel_height);
        self.spectrogram_panel_height = self
            .spectrogram_panel_sizes
            .values()
            .map(|size| size.1)
            .max()
            .unwrap_or(1);
        let columns =
            history_columns_for_width(self.spectrogram_history_seconds, self.analysis_fps, width);
        self.resize_history(columns);
        self.history_limit_seconds = (columns as f64 / self.analysis_fps.max(1) as f64)
            .min(self.spectrogram_history_seconds as f64);
        previous != (self.history_columns, self.spectrogram_panel_height)
    }

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
        let discontinuity = self.latest.is_some_and(|index| {
            self.columns[index].generation != self.stream_generation
                || self.columns[index].source_generation != self.source_generation
        }) || self
            .latest_sample_time
            .is_some_and(|previous| time.saturating_sub(previous) > HISTORY_GAP_RESET);
        let width =
            Duration::from_secs_f64(self.history_limit_seconds / (self.history_columns - 1) as f64);
        let bucket =
            Duration::from_nanos(((time.as_nanos() / width.as_nanos()) * width.as_nanos()) as u64);
        let merge = self.latest.filter(|&index| {
            self.columns[index].generation == self.stream_generation
                && self.columns[index].source_generation == self.source_generation
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
        self.columns[index].source_generation = self.source_generation;
        self.columns[index].sample_time = Some(time);
        if changed {
            self.columns[index].revision = self.columns[index].revision.wrapping_add(1);
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
        for (offset, column) in columns.iter_mut().take(keep).enumerate() {
            let index = (oldest + skip + offset) % old_capacity;
            *column = std::mem::take(&mut old_columns[index]);
        }
        self.columns = columns;
        self.spectrogram_tiles.clear();
        self.history_columns = new_columns;
        self.len = keep;
        self.head = keep % new_columns;
        self.latest = keep.checked_sub(1);
        if keep == 0 {
            self.latest_sample_time = None;
        }
    }
    fn clear_derived_caches(&mut self) {
        self.spectrum_bins.clear();
        self.spectrum_points.clear();
        self.spectrum_rectangles.clear();
        self.spectrum_geometry.clear();
        self.heat_samples.clear();
        self.heat_sample_generation = 0;
        self.spectrogram_sample_height = 0;
        self.invalidate_spectrogram();
    }
    fn invalidate_spectrogram(&mut self) {
        self.spectrogram_tiles.clear();
    }

    fn clear_history(&mut self) {
        self.invalidate_spectrogram();
        for column in &mut self.columns {
            column.levels.clear();
            column.sample_time = None;
            column.bucket_start = None;
        }
        self.head = 0;
        self.len = 0;
        self.latest = None;
        self.latest_sample_time = None;
        self.spectrogram_sample_height = 0;
        self.heat_samples.clear();
        self.heat_sample_generation = 0;
    }

    fn release_history(&mut self) {
        self.clear_history();
        self.columns = Vec::new();
        self.history_columns = 0;
        self.spectrogram_panel_height = 0;
        self.heat_samples = Vec::new();
        self.spectrogram_tiles = Vec::new();
        self.tile_values = Vec::new();
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
        self.spectrum_geometry.clear();
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

    fn spectrum_render_range(&self) -> Option<(usize, usize, f32, f32)> {
        // Hidden views keep history current but do not advance envelopes. A
        // restored view must wait for a real frame before indexing those arrays.
        if !self.spectrum_visible
            || !self.envelope_initialized
            || self.bar_levels.len() != self.frequencies.len()
            || self.smoothed_levels.len() != self.frequencies.len()
            || self.peaks.len() != self.frequencies.len()
        {
            return None;
        }
        self.spectrum_range()
    }

    fn prepare_spectrum_bins(&mut self, panel_id: u64, bars: usize, low: f32, high: f32) {
        if !self.visualization_cache {
            group_bands(
                &self.frequencies,
                low,
                high,
                bars,
                true,
                &mut self.spectrum_bins,
            );
            return;
        }
        let key = SpectrumGroupingKey {
            bars,
            generation: self.stream_generation,
            low,
            high,
        };
        let geometry = self
            .spectrum_geometry
            .entry(panel_id)
            .or_insert_with(|| SpectrumGeometry {
                key: None,
                grouping_key: SpectrumGroupingKey {
                    bars: 0,
                    generation: 0,
                    low: 0.0,
                    high: 0.0,
                },
                bins: Vec::new(),
                body: None,
            });
        if geometry.grouping_key != key {
            group_bands(&self.frequencies, low, high, bars, true, &mut geometry.bins);
            geometry.grouping_key = key;
            geometry.key = None;
            geometry.body = None;
        }
    }

    fn spectrum_bin(&self, panel_id: u64, bar: usize) -> Range<usize> {
        if self.visualization_cache {
            self.spectrum_geometry[&panel_id].bins[bar].clone()
        } else {
            self.spectrum_bins[bar].clone()
        }
    }

    fn prepare_spectrum_body(
        &mut self,
        panel_id: u64,
        plot: Bounds<Pixels>,
        bars: usize,
        low: f32,
        high: f32,
    ) -> Result<Option<Arc<GpuFrame>>, String> {
        self.prepare_spectrum_bins(panel_id, bars, low, high);
        let geometry_key = SpectrumGeometryKey {
            origin: plot.origin,
            width: plot.size.width,
            height: plot.size.height,
            low,
            high,
            style: self.spectrum_style,
            palette: self.palette_lut.palette,
            bars,
            bar_width: self.spectrum_bar_width,
            bar_gravity: self.spectrum_bar_gravity,
            interpolate: self.spectrum_interpolate,
            peaks: self.spectrum_peaks,
            top_db: self.top_db,
            db_range: self.spectrum_db_range,
            gap: self.spectrum_gap,
            revision: self.spectrum_geometry_revision,
        };
        if self.visualization_cache
            && let Some(geometry) = self.spectrum_geometry.get(&panel_id)
            && geometry.key.as_ref() == Some(&geometry_key)
        {
            return Ok(geometry.body.clone());
        }
        self.spectrum_points.clear();
        self.spectrum_rectangles.clear();
        let style = self.spectrum_style;
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
            self.spectrum_points.reserve(bars);
        }
        if rectangles {
            self.spectrum_rectangles.reserve(rectangle_capacity);
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
            let center_hz = axis_frequency(low, high, (left_t + right_t) * 0.5, true);
            if self.spectrum_interpolate {
                // Interpolation fills undersampled slots, but never averages
                // away a transient already max-pooled into this slot.
                level = level.max(self.interpolate_level(center_hz, low, high, false));
            }
            let slot_width = plot.size.width * (right_t - left_t);
            let slot_gap = px(self.spectrum_gap)
                .max(px(0.0))
                .min((slot_width - px(1.0)).max(px(0.0)));
            let level_fraction = db_height(level, self.top_db, self.spectrum_db_range);
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
                        self.spectrum_rectangles.push(GpuRectangle {
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
                            &mut self.spectrum_rectangles,
                            plot,
                            Bounds::new(point(x, bottom - height), size(width, edge)),
                        );
                        if height > edge {
                            append_spectrum_rectangle(
                                &mut self.spectrum_rectangles,
                                plot,
                                Bounds::new(point(x, bottom - edge), size(width, edge)),
                            );
                        }
                        if height > edge * 2.0 {
                            append_spectrum_rectangle(
                                &mut self.spectrum_rectangles,
                                plot,
                                Bounds::new(
                                    point(x, bottom - height + edge),
                                    size(edge, height - edge * 2.0),
                                ),
                            );
                            append_spectrum_rectangle(
                                &mut self.spectrum_rectangles,
                                plot,
                                Bounds::new(
                                    point(x + width - edge, bottom - height + edge),
                                    size(edge, height - edge * 2.0),
                                ),
                            );
                        }
                    }
                    SpectrumStyle::Line | SpectrumStyle::Solid => {
                        let y = (bottom - bar_height).clamp(top + px(1.0), bottom - px(1.0));
                        self.spectrum_points.push(point(x + width * 0.5, y));
                    }
                }
            }
        }
        if continuous && any_visible {
            append_spectrum_shape(
                shape_path.as_mut().unwrap(),
                &self.spectrum_points,
                plot,
                style == SpectrumStyle::Solid,
                style == SpectrumStyle::Line || self.spectrum_interpolate,
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
                &self.spectrum_rectangles,
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
        // Only immutable publications, never mutable scratch, enter panel caches.
        self.spectrum_rectangles.clear();
        if self.visualization_cache {
            let geometry = self.spectrum_geometry.get_mut(&panel_id).unwrap();
            geometry.key = Some(geometry_key);
            geometry.body = body.clone();
        } else {
            self.spectrum_geometry.remove(&panel_id);
        }
        Ok(body)
    }

    fn paint_spectrum(
        &mut self,
        panel_id: u64,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<(), String> {
        let show_labels = spectrum_labels_visible(self.spectrum_show_labels, bounds.size.height);
        let left_margin = axis_label_margin(show_labels, &self.spectrum_labels);
        let bottom_margin = if show_labels { 22.0 } else { 8.0 };
        let plot = Bounds::new(
            point(bounds.left() + px(left_margin), bounds.top() + px(8.0)),
            size(
                (bounds.size.width - px(left_margin + 8.0)).max(px(1.0)),
                (bounds.size.height - px(bottom_margin + 8.0)).max(px(1.0)),
            ),
        );
        let Some((_, _, low, high)) = self.spectrum_render_range() else {
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
            if show_labels {
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
                if let Some(body) = self.prepare_spectrum_body(panel_id, plot, bars, low, high)? {
                    window
                        .paint_gpu(plot, GpuSource::Fixed(body))
                        .map_err(|error| format!("Cannot paint spectrum GPU body: {error}"))?;
                }
                Ok(())
            },
        )?;
        if self.spectrum_peaks {
            window.with_content_mask(Some(gpui::ContentMask { bounds: plot }), |window| {
                for bar in 0..bars {
                    let left_t = bar as f32 / bars as f32;
                    let right_t = (bar + 1) as f32 / bars as f32;
                    let mut peak = f32::NEG_INFINITY;
                    for index in self.spectrum_bin(panel_id, bar) {
                        peak = peak.max(finite_level(self.peaks[index]));
                    }
                    if self.spectrum_interpolate {
                        let center_hz = axis_frequency(low, high, (left_t + right_t) * 0.5, true);
                        peak = peak.max(self.interpolate_level(center_hz, low, high, true));
                    }
                    let peak_fraction = db_height(peak, self.top_db, self.spectrum_db_range);
                    if peak_fraction <= 0.0 {
                        continue;
                    }
                    let slot_width = plot.size.width * (right_t - left_t);
                    let slot_gap = px(self.spectrum_gap)
                        .max(px(0.0))
                        .min((slot_width - px(1.0)).max(px(0.0)));
                    let (x, width) = match self.spectrum_style {
                        SpectrumStyle::Line | SpectrumStyle::Solid => {
                            (plot.left() + plot.size.width * left_t, slot_width)
                        }
                        _ => (
                            plot.left() + plot.size.width * left_t + slot_gap * 0.5,
                            (slot_width - slot_gap).max(px(1.0)),
                        ),
                    };
                    let peak_width = match self.spectrum_style {
                        SpectrumStyle::Solid => px(1.0),
                        SpectrumStyle::Line => (slot_width - slot_gap).max(px(1.0)),
                        _ => width,
                    };
                    let peak_x = (x + (width - peak_width) * 0.5)
                        .clamp(plot.left(), (plot.right() - peak_width).max(plot.left()));
                    let peak_y = (plot.bottom() - plot.size.height * peak_fraction)
                        .clamp(plot.top(), plot.bottom() - px(1.0));
                    window.paint_quad(fill(
                        Bounds::new(point(peak_x, peak_y), size(peak_width, px(1.0))),
                        rgb(stop_color(
                            self.palette_lut.lookup_mapped_rgb(peak_fraction),
                        )),
                    ));
                }
            });
        }
        if !self.visualization_cache {
            self.spectrum_bins.clear();
        }
        if show_labels {
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
        let history = self.history_window(latest_time);
        let start_time = latest_time.saturating_sub(history);
        let source_rows = last.saturating_sub(first) + 1;
        let height = spectrogram_source_height(
            self.spectrogram_panel_height,
            source_rows,
            self.spectrogram_interpolation_points,
            self.spectrogram_sampling_points_scale,
        )
        .min(spectrogram_budget_rows(
            self.history_columns,
            self.material_budget_bytes
                .expect("configured visualization budget"),
        ));
        if !self.visualization_cache
            || self.spectrogram_sample_height != height
            || self.heat_sample_generation != self.stream_generation
        {
            self.invalidate_spectrogram();
            self.spectrogram_sample_height = height;
            self.heat_samples = build_heat_samples(&self.frequencies, low, high, height);
            self.heat_sample_generation = self.stream_generation;
        }
        let tiles = self.history_columns.div_ceil(SPECTROGRAM_TILE_COLUMNS);
        self.spectrogram_tiles.resize_with(tiles, || None);
        for tile_index in 0..tiles {
            let first = tile_index * SPECTROGRAM_TILE_COLUMNS;
            let last = (first + SPECTROGRAM_TILE_COLUMNS).min(self.history_columns);
            let mut key: SpectrogramTileKey = [None; SPECTROGRAM_TILE_COLUMNS];
            let mut start = Duration::MAX;
            let mut end = Duration::ZERO;
            for index in first..last {
                let Some(time) = self.columns[index].sample_time else {
                    continue;
                };
                let column = &self.columns[index];
                if column.generation != self.stream_generation {
                    continue;
                }
                key[index - first] = Some((column.revision, column.interval_start, time));
                start = start.min(column.interval_start);
                end = end.max(time);
            }
            if start == Duration::MAX {
                continue;
            }
            if self.spectrogram_tiles[tile_index]
                .as_ref()
                .is_none_or(|tile| tile.key != key)
            {
                let count = key.iter().flatten().count();
                let span = end.saturating_sub(start).as_secs_f64().max(1e-9);
                let mut intervals = [[0.0; 2]; SPECTROGRAM_TILE_COLUMNS];
                self.tile_values.clear();
                self.tile_values.resize(count * height, 0.0);
                let mut offset = 0;
                for (local, entry) in key.iter().enumerate() {
                    let Some((_, left, right)) = entry else {
                        continue;
                    };
                    intervals[offset] = [
                        left.saturating_sub(start).as_secs_f64(),
                        right.saturating_sub(start).as_secs_f64(),
                    ]
                    .map(|value| (value / span) as f32);
                    sample_spectrogram_column(
                        &self.columns[first + local].levels,
                        &self.heat_samples,
                        self.spectrogram_interpolate,
                        &mut self.tile_values,
                        count,
                        offset,
                    );
                    offset += 1;
                }
                let publication = GpuFrame::field_columns(
                    &intervals[..count],
                    height as u32,
                    &self.tile_values,
                    self.palette_lut.spectrogram_gpu_palette()?,
                    GpuFieldParams {
                        mapping: GpuValueMapping::Linear {
                            min: -self.spectrogram_db_range,
                            max: 0.0,
                        },
                    },
                    GpuLimits {
                        max_field_width: SPECTROGRAM_TILE_COLUMNS as u32,
                        max_field_height: height as u32,
                        max_field_cells: SPECTROGRAM_TILE_COLUMNS * height,
                        max_palette_colors: PALETTE_LUT_SIZE,
                        ..Default::default()
                    },
                )
                .map_err(|error| error.to_string())?;
                self.spectrogram_tiles[tile_index] = Some(SpectrogramTile {
                    key,
                    start,
                    end,
                    publication,
                });
            }
            let tile = self.spectrogram_tiles[tile_index].as_ref().unwrap();
            if tile.end < start_time || tile.start > latest_time {
                continue;
            }
            let span = history.as_secs_f64();
            let left = history_fraction(tile.start, latest_time, history);
            let width = (tile.end.saturating_sub(tile.start).as_secs_f64() / span) as f32;
            let bounds = Bounds::new(
                point(plot.left() + plot.size.width * left, plot.top()),
                size((plot.size.width * width).max(px(1.0)), plot.size.height),
            );
            window
                .with_content_mask(Some(gpui::ContentMask { bounds: plot }), |window| {
                    window.paint_gpu(bounds, GpuSource::Fixed(tile.publication.clone()))
                })
                .map_err(|error| format!("Cannot paint spectrogram tile: {error}"))?;
        }
        if !self.visualization_cache {
            self.heat_samples.clear();
            self.heat_sample_generation = 0;
            self.spectrogram_sample_height = 0;
            self.spectrogram_tiles.clear();
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

fn append_spectrum_rectangle(
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

fn sample_spectrogram_column(
    levels: &[f32],
    samples: &[HeatSample],
    interpolate: bool,
    values: &mut [f32],
    stride: usize,
    column: usize,
) {
    for (row, sample) in samples.iter().enumerate() {
        values[row * stride + column] = sample_heat_level_at(levels, *sample, interpolate);
    }
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
/// continuously inside each authored color-stop interval. The lookup table and
/// Spectrum's shared GPU palette samples this same authored mapping.
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

pub(super) fn paint_label(
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

    fn prepared_body(
        visuals: &Visuals,
        panel: u64,
        width: f32,
        height: f32,
    ) -> Option<Arc<GpuFrame>> {
        let mut data = visuals.data.borrow_mut();
        let (_, _, low, high) = data.spectrum_render_range().unwrap();
        let bars = spectrum_bar_count(
            width,
            data.spectrum_style == SpectrumStyle::Solid,
            data.spectrum_bars,
            data.spectrum_bar_width,
            data.spectrum_gap,
        );
        data.prepare_spectrum_body(
            panel,
            Bounds::new(point(px(13.25), px(21.5)), size(px(width), px(height))),
            bars,
            low,
            high,
        )
        .unwrap()
    }

    fn rectangle_frame(body: Option<Arc<GpuFrame>>) -> Arc<GpuFrame> {
        let frame = body.expect("expected spectrum body");
        assert!(matches!(
            frame.content(),
            gpui::GpuContent::Rectangles { .. }
        ));
        frame
    }

    #[test]
    fn gpu_spectrum_continuous_partial_height_preserves_geometry_and_plot_domain() {
        for style in [SpectrumStyle::Line, SpectrumStyle::Solid] {
            for interpolate in [false, true] {
                let mut visuals = configured_visuals();
                visuals.configure(&Config {
                    spectrum_style: style,
                    spectrum_interpolate: interpolate,
                    spectrum_db_range: Some(100.0),
                    spectrum_bars: 4,
                    ..Config::default()
                });
                visuals.update(&frame(1.0, -75.0));
                let body = prepared_body(&visuals, 1, 100.0, 100.0).unwrap();
                let gpui::GpuContent::Path { path, params } = body.content() else {
                    panic!("expected path")
                };
                let plot = Bounds::new(point(px(13.25), px(21.5)), size(px(100.0), px(100.0)));
                assert_eq!(path.bounds, plot);
                assert_eq!(params.palette_range, [0.0, 1.0]);
                assert!(!path.vertices.is_empty());
                let minimum_y = path
                    .vertices
                    .iter()
                    .map(|vertex| vertex.xy_position.y)
                    .fold(plot.bottom(), |a, b| a.min(b));
                // A quarter-height signal stays in the lower plot region; it must
                // not be stretched to the ceiling by a geometry-local domain.
                assert!(minimum_y > plot.top() + plot.size.height * 0.5);
                assert!(minimum_y < plot.bottom() - plot.size.height * 0.2);
                assert!(
                    path.vertices
                        .iter()
                        .all(|vertex| vertex.xy_position.y <= plot.bottom() + px(1.0))
                );
            }
        }
    }

    #[test]
    fn gpu_spectrum_outline_preserves_nonoverlapping_edges_and_palette_domain() {
        for height in [0.125, 1.0, 2.0, 25.0, 100.0] {
            let mut visuals = configured_visuals();
            visuals.configure(&Config {
                spectrum_style: SpectrumStyle::Outline,
                spectrum_db_range: Some(100.0),
                spectrum_bars: 4,
                spectrum_gap: 2.0,
                ..Config::default()
            });
            visuals.update(&frame(1.0, height - 100.0));
            let body = rectangle_frame(prepared_body(&visuals, 1, 100.0, 100.0));
            let gpui::GpuContent::Rectangles { rectangles, params } = body.content() else {
                unreachable!()
            };
            assert_eq!(params.cell_height, 0.0);
            let height = height.max(1.0);
            let edge = 1.0_f32.min(height * 0.5);
            let count = if height > 2.0 * edge { 4 } else { 2 };
            assert_eq!(rectangles.len(), 4 * count);
            for (bar, edges) in rectangles.chunks(count).enumerate() {
                let x = (bar as f32 * 25.0 + 1.0) / 100.0;
                let mut expected = vec![
                    [x, 1.0 - height / 100.0, 0.23, edge / 100.0],
                    [x, 1.0 - edge / 100.0, 0.23, edge / 100.0],
                ];
                if count == 4 {
                    expected.push([
                        x,
                        1.0 - (height - edge) / 100.0,
                        edge / 100.0,
                        (height - 2.0 * edge) / 100.0,
                    ]);
                    expected.push([
                        x + (23.0 - edge) / 100.0,
                        1.0 - (height - edge) / 100.0,
                        edge / 100.0,
                        (height - 2.0 * edge) / 100.0,
                    ]);
                }
                for (edge, expected) in edges.iter().zip(expected) {
                    for (actual, expected) in edge.bounds.iter().zip(expected) {
                        assert!((actual - expected).abs() < 1e-6);
                    }
                    assert!(
                        (edge.palette_range[0] - (1.0 - edge.bounds[1] - edge.bounds[3])).abs()
                            < 1e-6
                    );
                    assert!((edge.palette_range[1] - (1.0 - edge.bounds[1])).abs() < 1e-6);
                }
            }
        }
    }

    #[test]
    fn gpu_spectrum_preserves_nonzero_heights_and_led_partial_cells() {
        for style in [SpectrumStyle::Bars, SpectrumStyle::Led] {
            for height in [0.0, 0.125, 1.0, 2.75, 3.0, 4.5, 5.0, 5.25, 8.0, 100.0] {
                let mut visuals = configured_visuals();
                visuals.configure(&Config {
                    spectrum_style: style,
                    spectrum_db_range: Some(100.0),
                    spectrum_bars: 4,
                    spectrum_gap: 2.0,
                    ..Config::default()
                });
                visuals.update(&frame(1.0, height - 100.0));
                let body = prepared_body(&visuals, 1, 100.0, 100.0);
                if height == 0.0 {
                    assert!(body.is_none());
                    assert!(visuals.data.borrow().spectrum_geometry[&1].body.is_none());
                    continue;
                }
                let frame = rectangle_frame(body);
                let gpui::GpuContent::Rectangles { rectangles, params } = frame.content() else {
                    unreachable!()
                };
                assert_eq!(
                    rectangles.len(),
                    4,
                    "one instance per bar, not per LED cell"
                );
                assert_eq!(
                    params.cell_height,
                    if style == SpectrumStyle::Led {
                        3.0
                    } else {
                        0.0
                    }
                );
                assert_eq!(
                    params.cell_gap,
                    if style == SpectrumStyle::Led {
                        2.0
                    } else {
                        0.0
                    }
                );
                let expected_height = if style == SpectrumStyle::Bars {
                    height.max(1.0)
                } else {
                    height
                };
                for (bar, rectangle) in rectangles.iter().enumerate() {
                    let [x, y, width, actual_height] = rectangle.bounds;
                    assert!((x * 100.0 - (bar as f32 * 25.0 + 1.0)).abs() < 1e-5);
                    assert!((width * 100.0 - 23.0).abs() < 1e-5);
                    assert!((actual_height * 100.0 - expected_height).abs() < 1e-5);
                    assert!((y + actual_height - 1.0).abs() < 1e-6);
                    assert_eq!(rectangle.palette_range, [0.0, actual_height]);
                }
                assert!(visuals.data.borrow().spectrum_rectangles.is_empty());
            }
        }
    }

    #[test]
    fn gpu_spectrum_outline_caps_dense_fractional_instances() {
        for width in [3.125, 7.25, 1200.375, 4096.125] {
            let mut visuals = configured_visuals();
            visuals.configure(&Config {
                spectrum_style: SpectrumStyle::Outline,
                spectrum_bars: u32::MAX,
                spectrum_gap: 10.0,
                spectrum_interpolate: true,
                ..Config::default()
            });
            visuals.update(&frame(1.0, 0.0));
            let frame = rectangle_frame(prepared_body(&visuals, 1, width, 77.125));
            let gpui::GpuContent::Rectangles { rectangles, .. } = frame.content() else {
                unreachable!()
            };
            assert_eq!(
                rectangles.len(),
                4 * (width.floor() as usize).min(MAX_SPECTRUM_BARS)
            );
            for rectangle in rectangles {
                let [x, y, w, h] = rectangle.bounds;
                assert!(x >= 0.0 && y >= 0.0 && w > 0.0 && h > 0.0);
                assert!(x + w <= 1.0 && y + h <= 1.0);
            }
        }
    }

    #[test]
    fn gpu_spectrum_dense_fractional_bounds_stay_inside_plot() {
        for width in [3.125, 7.25, 1200.375, 4096.125] {
            let mut visuals = configured_visuals();
            visuals.configure(&Config {
                spectrum_style: SpectrumStyle::Bars,
                spectrum_bars: u32::MAX,
                spectrum_gap: 10.0,
                spectrum_interpolate: true,
                ..Config::default()
            });
            visuals.update(&frame(1.0, 0.0));
            let frame = rectangle_frame(prepared_body(&visuals, 1, width, 77.125));
            let gpui::GpuContent::Rectangles { rectangles, .. } = frame.content() else {
                unreachable!()
            };
            assert_eq!(
                rectangles.len(),
                (width.floor() as usize).min(MAX_SPECTRUM_BARS)
            );
            for rectangle in rectangles {
                let [x, y, w, h] = rectangle.bounds;
                assert!(x >= 0.0 && w > 0.0 && x + w <= 1.0);
                assert_eq!([y, h], [0.0, 1.0]);
            }
        }
    }

    #[test]
    fn gpu_spectrum_cache_tracks_revision_size_style_palette_and_range() {
        let mut visuals = configured_visuals();
        let mut config = Config {
            spectrum_style: SpectrumStyle::Bars,
            spectrum_bars: 4,
            ..Config::default()
        };
        visuals.configure(&config);
        visuals.update(&frame(1.0, -30.0));
        let first = rectangle_frame(prepared_body(&visuals, 1, 100.0, 100.0));
        visuals.update(&frame(1.0, -30.0));
        assert!(Arc::ptr_eq(
            &first,
            &rectangle_frame(prepared_body(&visuals, 1, 100.0, 100.0))
        ));
        let other = rectangle_frame(prepared_body(&visuals, 2, 101.0, 100.0));
        assert!(Arc::ptr_eq(first.palette(), other.palette()));
        visuals.update(&frame(1.1, -10.0));
        let revised = rectangle_frame(prepared_body(&visuals, 1, 100.0, 100.0));
        assert_ne!(first.id(), revised.id());
        let resized = rectangle_frame(prepared_body(&visuals, 1, 100.0, 101.0));
        assert_ne!(revised.id(), resized.id());
        config.spectrum_style = SpectrumStyle::Led;
        visuals.configure(&config);
        let led = rectangle_frame(prepared_body(&visuals, 1, 100.0, 101.0));
        assert_ne!(resized.id(), led.id());
        assert!(Arc::ptr_eq(first.palette(), led.palette()));
        config.spectrum_db_range = Some(40.0);
        visuals.configure(&config);
        let ranged = rectangle_frame(prepared_body(&visuals, 1, 100.0, 101.0));
        assert_ne!(led.id(), ranged.id());
        let first_height = |frame: &GpuFrame| match frame.content() {
            gpui::GpuContent::Rectangles { rectangles, .. } => rectangles[0].bounds[3],
            _ => unreachable!(),
        };
        assert!((first_height(&ranged) - 0.75).abs() < 1e-6);
        assert!(first_height(&ranged) < first_height(&led));
        let (levels, held, smoothed, peaks, revision) = {
            let data = visuals.data.borrow();
            (
                data.latest_levels.clone(),
                data.bar_levels.clone(),
                data.smoothed_levels.clone(),
                data.peaks.clone(),
                data.spectrum_geometry_revision,
            )
        };
        config.visual_palette = match config.visual_palette {
            VisualizationPalette::Deadbeef => VisualizationPalette::TokyoNight,
            VisualizationPalette::TokyoNight => VisualizationPalette::Deadbeef,
        };
        visuals.configure(&config);
        let recolored = rectangle_frame(prepared_body(&visuals, 1, 100.0, 101.0));
        assert_ne!(ranged.id(), recolored.id());
        assert_eq!(first_height(&ranged), first_height(&recolored));
        {
            let data = visuals.data.borrow();
            assert_eq!(data.latest_levels, levels);
            assert_eq!(data.bar_levels, held);
            assert_eq!(data.smoothed_levels, smoothed);
            assert_eq!(data.peaks, peaks);
            assert_eq!(data.spectrum_geometry_revision, revision);
        }
        assert!(!Arc::ptr_eq(first.palette(), recolored.palette()));
        for style in [
            SpectrumStyle::Outline,
            SpectrumStyle::Line,
            SpectrumStyle::Solid,
        ] {
            config.spectrum_style = style;
            visuals.configure(&config);
            let body = prepared_body(&visuals, 1, 100.0, 101.0).unwrap();
            assert_eq!(
                matches!(body.content(), gpui::GpuContent::Rectangles { .. }),
                style == SpectrumStyle::Outline
            );
            assert!(Arc::ptr_eq(
                &body,
                &prepared_body(&visuals, 1, 100.0, 101.0).unwrap()
            ));
        }
        config.spectrum_style = SpectrumStyle::Bars;
        visuals.configure(&config);
        assert_ne!(
            recolored.id(),
            rectangle_frame(prepared_body(&visuals, 1, 100.0, 101.0)).id()
        );
    }

    #[test]
    fn gpu_spectrum_cache_off_pruning_and_disable_release_publications() {
        let mut visuals = configured_visuals();
        visuals.configure(&Config {
            spectrum_style: SpectrumStyle::Bars,
            ..Config::default()
        });
        visuals.update(&frame(1.0, -30.0));
        let cached = Arc::downgrade(&rectangle_frame(prepared_body(&visuals, 1, 100.0, 100.0)));
        visuals.retain_spectrum_panels(&[]);
        visuals.set_spectrum_visible(false);
        visuals.update(&frame(1.1, -60.0));
        assert!(visuals.data.borrow().spectrum_render_range().is_none());
        visuals.set_spectrum_visible(true);
        assert!(visuals.data.borrow().spectrum_render_range().is_none());
        visuals.update(&frame(1.2, -60.0));
        let restored = rectangle_frame(prepared_body(&visuals, 1, 100.0, 100.0));
        let gpui::GpuContent::Rectangles { rectangles, .. } = restored.content() else {
            unreachable!()
        };
        assert!((rectangles[0].bounds[3] - 1.0 / 7.0).abs() < 1e-6);
        assert!(cached.upgrade().is_none());
        visuals.configure(&Config {
            spectrum_style: SpectrumStyle::Bars,
            visualization_cache: false,
            ..Config::default()
        });
        let first = rectangle_frame(prepared_body(&visuals, 1, 100.0, 100.0));
        let second = rectangle_frame(prepared_body(&visuals, 1, 100.0, 100.0));
        assert_ne!(first.id(), second.id());
        assert!(Arc::ptr_eq(first.palette(), second.palette()));
        assert!(visuals.data.borrow().spectrum_geometry.is_empty());
        let uncached = Arc::downgrade(&second);
        drop(second);
        assert!(uncached.upgrade().is_none());
        visuals.configure(&Config {
            spectrum_style: SpectrumStyle::Bars,
            ..Config::default()
        });
        let cached = Arc::downgrade(&rectangle_frame(prepared_body(&visuals, 1, 100.0, 100.0)));
        visuals.set_enabled(false);
        assert!(cached.upgrade().is_none());
        assert!(visuals.data.borrow().spectrum_geometry.is_empty());
        assert_eq!(visuals.data.borrow().spectrum_rectangles.capacity(), 0);
    }

    #[test]
    fn gpu_spectrum_palette_failure_propagates_without_path_fallback() {
        let mut visuals = configured_visuals();
        visuals.configure(&Config {
            spectrum_style: SpectrumStyle::Bars,
            ..Config::default()
        });
        visuals.update(&frame(1.0, -30.0));
        let lut = PaletteLut::new(VisualizationPalette::Deadbeef);
        lut.spectrum_gpu_palette
            .set(Err("palette allocation failed".into()))
            .unwrap();
        let mut data = visuals.data.borrow_mut();
        data.palette_lut = Rc::new(lut);
        let (_, _, low, high) = data.spectrum_render_range().unwrap();
        let result = data.prepare_spectrum_body(
            1,
            Bounds::new(point(px(0.0), px(0.0)), size(px(100.0), px(100.0))),
            4,
            low,
            high,
        );
        assert!(result.is_err());
        assert!(data.spectrum_geometry[&1].key.is_none());
        assert!(data.spectrum_geometry[&1].body.is_none());
    }

    #[test]
    fn small_canvases_explain_missing_space_instead_of_painting_unreadable_plots() {
        for (width, height) in [(64.0, 100.0), (100.0, 40.0), (300.0, 25.0)] {
            assert!(!visual_canvas_has_room(size(px(width), px(height))));
        }
        assert!(visual_canvas_has_room(size(px(65.0), px(41.0))));
        assert!(!spectrum_labels_visible(true, px(41.0)));
        assert!(!spectrum_labels_visible(true, px(77.0)));
        assert!(spectrum_labels_visible(true, px(78.0)));
        assert!(!spectrum_labels_visible(false, px(100.0)));
    }

    #[test]
    fn smooth_gradient_preserves_palette_stops() {
        for stops in [TOKYO_NIGHT_STOPS, DEADBEEF_STOPS] {
            for &(position, color) in stops {
                assert_eq!(gradient(position, stops), color);
            }
        }
        // The interior retains the authored cubic tangent, not a linear stop blend.
        assert_eq!(gradient(0.1, TOKYO_NIGHT_STOPS), [138, 115, 187]);
    }

    #[test]
    fn spectrum_palette_retains_visible_floor_and_high_energy_colors() {
        for palette in [
            VisualizationPalette::Deadbeef,
            VisualizationPalette::TokyoNight,
        ] {
            let lut = PaletteLut::new(palette);
            let gpu = lut.spectrum_gpu_palette().unwrap();
            let colors = gpu.colors();
            assert!(colors.iter().all(|color| color[3] == 1.0));
            assert!(colors[0][2] > colors[0][0]);
            assert!(colors[colors.len() - 1][0] > colors[0][0]);
            assert!(colors[0][..3].iter().any(|channel| *channel > 0.0));
        }
    }

    fn frame(time: f64, level: f32) -> AnalysisFrame {
        AnalysisFrame {
            sequence: Duration::from_secs_f64(time).as_nanos() as u64,
            sample_rate: 48_000,
            sample_time: Duration::from_secs_f64(time),
            frequencies_hz: vec![20.0, 200.0, 2000.0, 20_000.0],
            spectrum_db: vec![level; 4],
            settings: Some(crate::analysis::AnalysisSettings::from(&Config::default())),
            generation: 1,
            ..AnalysisFrame::default()
        }
    }

    #[test]
    fn same_rate_seek_starts_new_bucket_and_resets_hold_and_smoothing() {
        let mut visuals = configured_visuals();
        visuals.update(&frame(1.02, -10.0));
        let mut next = frame(1.03, -60.0);
        next.generation = 2;
        visuals.update(&next);
        let data = visuals.data.borrow();
        assert_eq!(data.len, 2);
        assert_eq!(data.columns[0].levels, vec![-10.0; 4]);
        assert_eq!(data.columns[1].levels, vec![-60.0; 4]);
        assert_eq!(data.columns[1].interval_start, next.sample_time);
        assert_eq!(data.peaks, vec![-60.0; 4]);
        assert_eq!(data.bar_levels, vec![-60.0; 4]);
        assert_eq!(data.smoothed_levels, vec![-60.0; 4]);
    }

    #[test]
    fn reconfigured_analysis_rejects_old_fft_and_window_frames() {
        for config in [
            Config {
                analysis_fft_size: 16384,
                ..Config::default()
            },
            Config {
                analysis_window: AnalysisWindow::Hann,
                ..Config::default()
            },
        ] {
            let mut visuals = configured_visuals();
            visuals.update(&frame(1.0, -10.0));
            visuals.configure(&config);
            visuals.update(&frame(2.0, -20.0));
            assert!(visuals.data.borrow().latest_levels.is_empty());
            assert_eq!(visuals.data.borrow().len, 0);
            let mut fresh = frame(2.1, -60.0);
            fresh.settings = Some(crate::analysis::AnalysisSettings::from(&config));
            fresh.generation = 2;
            visuals.update(&fresh);
            assert_eq!(visuals.data.borrow().peaks, vec![-60.0; 4]);
            let before = visuals.data.borrow().latest_sample_time;
            visuals.update(&frame(2.2, -5.0));
            assert_eq!(visuals.data.borrow().latest_sample_time, before);
        }
    }

    #[test]
    fn history_retains_requested_audio_duration_at_different_rates() {
        for fps in [5, 60] {
            let mut visuals = Visuals::new();
            visuals.retain_spectrogram_panels(&[1]);
            visuals.configure(&Config {
                analysis_fps: fps,
                spectrogram_history_seconds: 60,
                ..Config::default()
            });
            for step in 1..=240 * fps {
                visuals.update(&frame(step as f64 / fps as f64, -30.0));
            }
            let data = visuals.data.borrow();
            assert_eq!(data.len, history_column_count(60, fps));
            let oldest = data
                .columns
                .iter()
                .filter_map(|column| column.sample_time)
                .min()
                .unwrap();
            let newest = data.latest_sample_time.unwrap();
            assert!(newest - oldest >= Duration::from_secs(59));
            assert!(newest - oldest <= Duration::from_secs(60));
            assert_eq!(
                history_fraction(newest, newest, Duration::from_secs(60)),
                1.0
            );
        }
    }

    fn configured_visuals() -> Visuals {
        let mut visuals = Visuals::new();
        visuals.retain_spectrogram_panels(&[1]);
        visuals.configure(&Config::default());
        visuals
    }

    #[test]
    fn spectrogram_history_requires_visible_demand_and_restarts_without_hidden_frames() {
        let mut visuals = Visuals::new();
        let mut config = Config::default();
        visuals.configure(&config);
        visuals.update(&frame(1.0, -10.0));
        assert_eq!(visuals.data.borrow().columns.capacity(), 0);
        assert_eq!(visuals.data.borrow().peaks, vec![-10.0; 4]);

        visuals.retain_spectrogram_panels(&[1, 2]);
        visuals.configure(&config);
        visuals.update(&frame(1.1, -20.0));
        visuals.retain_spectrogram_panels(&[2]);
        assert_eq!(visuals.data.borrow().len, 1);
        visuals.update(&frame(1.2, -30.0));
        assert_eq!(visuals.data.borrow().len, 2);
        {
            let mut data = visuals.data.borrow_mut();
            data.spectrogram_tiles.resize_with(3, || None);
            data.tile_values.resize(1024, 1.0);
        }

        visuals.retain_spectrogram_panels(&[]);
        for cache in [false, true] {
            config.visualization_cache = cache;
            config.analysis_fps += 1;
            visuals.configure(&config);
            visuals.update(&frame(2.0, -40.0));
            let data = visuals.data.borrow();
            assert_eq!(data.columns.capacity(), 0);
            assert_eq!(data.heat_samples.capacity(), 0);
            assert_eq!(data.spectrogram_tiles.capacity(), 0);
            assert_eq!(data.tile_values.capacity(), 0);
            assert_eq!(data.history_columns, 0);
            assert_eq!(data.len, 0);
            assert_eq!(data.latest_levels, vec![-40.0; 4]);
        }

        visuals.retain_spectrogram_panels(&[3]);
        visuals.configure(&config);
        // A settings refresh can replay the current analysis publication.
        // It must not resurrect the last hidden frame as new history.
        visuals.update(&frame(2.0, -40.0));
        assert_eq!(visuals.data.borrow().len, 0);
        visuals.update(&frame(3.0, -50.0));
        let data = visuals.data.borrow();
        assert_eq!(data.len, 1);
        let newest = &data.columns[data.latest.unwrap()];
        assert_eq!(newest.sample_time, Some(Duration::from_secs(3)));
        assert_eq!(newest.levels, vec![-50.0; 4]);
    }

    #[test]
    fn hidden_spectrum_preserves_history_and_resumes_with_latest_levels() {
        let mut visuals = configured_visuals();
        visuals.update(&frame(1.0, -10.0));
        visuals.set_spectrum_visible(false);
        visuals.update(&frame(1.1, -60.0));
        {
            let data = visuals.data.borrow();
            assert_eq!(data.peaks, vec![-10.0; 4]);
            assert_eq!(data.latest_levels, vec![-60.0; 4]);
            assert_eq!(data.len, 2);
        }
        visuals.set_spectrum_visible(true);
        visuals.update(&frame(1.1, -60.0));
        let data = visuals.data.borrow();
        assert_eq!(data.peaks, vec![-60.0; 4]);
        assert_eq!(data.bar_levels, vec![-60.0; 4]);
        assert_eq!(data.smoothed_levels, vec![-60.0; 4]);
        assert_eq!(data.len, 2);
    }

    #[test]
    fn hidden_frequency_shape_change_withholds_rendering_until_real_envelopes_return() {
        for count in [2, 8] {
            let mut visuals = configured_visuals();
            visuals.update(&frame(1.0, -10.0));
            visuals.set_spectrum_visible(false);
            let mut changed = frame(1.1, -60.0);
            changed.frequencies_hz = (0..count)
                .map(|index| 20.0 * 1000.0_f32.powf(index as f32 / (count - 1) as f32))
                .collect();
            changed.spectrum_db = vec![-60.0; count];
            visuals.update(&changed);
            assert!(visuals.set_spectrum_visible(true));
            {
                let data = visuals.data.borrow();
                assert_eq!(data.latest_levels, changed.spectrum_db);
                assert_eq!(data.len, 2);
                assert_eq!(data.columns[0].levels, vec![-10.0; 4]);
                assert_eq!(data.columns[1].levels, changed.spectrum_db);
                // This is the paint consumer's boundary, before pooled levels,
                // interpolated levels, or peaks may index an envelope.
                assert!(data.spectrum_render_range().is_none());
            }
            changed.sample_time = Duration::from_secs_f64(1.2);
            visuals.update(&changed);
            let data = visuals.data.borrow();
            let (first, end, low, high) = data.spectrum_render_range().unwrap();
            for index in first..end {
                assert_eq!(data.display_level(index), -60.0);
                assert_eq!(data.peaks[index], -60.0);
            }
            assert_eq!(data.interpolate_level(high, low, high, false), -60.0);
            assert_eq!(data.interpolate_level(high, low, high, true), -60.0);
            assert_eq!(data.bar_levels.len(), count);
            assert_eq!(data.smoothed_levels.len(), count);
        }
    }

    #[test]
    fn differently_sized_spectrum_panels_keep_independent_bands_and_release_them() {
        let mut visuals = configured_visuals();
        visuals.update(&frame(1.0, -30.0));
        {
            let mut data = visuals.data.borrow_mut();
            let (_, _, low, high) = data.spectrum_render_range().unwrap();
            let narrow = spectrum_bar_count(120.0, false, 0, 8.0, 2.0);
            let wide = spectrum_bar_count(800.0, false, 0, 8.0, 2.0);
            assert_ne!(narrow, wide);
            data.prepare_spectrum_bins(1, narrow, low, high);
            let narrow_bins: Vec<_> = (0..narrow).map(|bar| data.spectrum_bin(1, bar)).collect();
            data.prepare_spectrum_bins(2, wide, low, high);
            let wide_bins: Vec<_> = (0..wide).map(|bar| data.spectrum_bin(2, bar)).collect();
            for (bins, bars) in [(&narrow_bins, narrow), (&wide_bins, wide)] {
                for (bar, range) in bins.iter().enumerate() {
                    let left = axis_frequency(low, high, bar as f32 / bars as f32, true);
                    let right = axis_frequency(low, high, (bar + 1) as f32 / bars as f32, true);
                    for index in range.clone() {
                        assert!(data.frequencies[index] >= left * (1.0 - 1e-6));
                        assert!(data.frequencies[index] <= right * (1.0 + 1e-6));
                    }
                }
                let included: Vec<_> = bins.iter().flat_map(|range| range.clone()).collect();
                let expected: Vec<_> = data
                    .frequencies
                    .iter()
                    .enumerate()
                    .filter_map(|(index, hz)| (*hz >= low && *hz <= high).then_some(index))
                    .collect();
                assert_eq!(included, expected);
            }
            data.prepare_spectrum_bins(1, narrow, low, high);
            assert_eq!(
                (0..narrow)
                    .map(|bar| data.spectrum_bin(1, bar))
                    .collect::<Vec<_>>(),
                narrow_bins
            );
            assert_eq!(
                (0..wide)
                    .map(|bar| data.spectrum_bin(2, bar))
                    .collect::<Vec<_>>(),
                wide_bins
            );
        }
        visuals.retain_spectrum_panels(&[2]);
        assert_eq!(visuals.data.borrow().spectrum_geometry.len(), 1);
        visuals.configure(&Config {
            visualization_cache: false,
            ..Config::default()
        });
        {
            let mut data = visuals.data.borrow_mut();
            assert!(data.spectrum_geometry.is_empty());
            let (_, _, low, high) = data.spectrum_range().unwrap();
            data.prepare_spectrum_bins(3, 12, low, high);
            assert!(data.spectrum_geometry.is_empty());
            assert!((0..12).any(|bar| !data.spectrum_bin(3, bar).is_empty()));
        }
    }

    #[test]
    fn disabled_views_drop_history_and_rebuild_only_after_enabling() {
        for config in [
            Config::default(),
            Config {
                analysis_fps: 5,
                spectrogram_history_seconds: 60,
                ..Config::default()
            },
        ] {
            let mut visuals = configured_visuals();
            visuals.update(&frame(1.0, -10.0));
            visuals.set_enabled(false);
            visuals.configure(&config);
            visuals.update(&frame(2.0, -20.0));
            visuals.finish_prepaint();
            {
                let data = visuals.data.borrow();
                assert_eq!(data.columns.capacity(), 0);
                assert_eq!(data.history_columns, 0);
                assert_eq!(data.len, 0);
                assert!(data.latest_sample_time.is_none());
                assert!(!data.envelope_initialized);
                assert_eq!(data.latest_levels.capacity(), 0);
                assert_eq!(data.peaks.capacity(), 0);
            }
            visuals.set_enabled(true);
            assert!(visuals.configure(&config));
            let mut fresh = frame(3.0, -60.0);
            fresh.generation = 2;
            visuals.update(&fresh);
            let data = visuals.data.borrow();
            assert_eq!(data.len, 1);
            assert_eq!(data.latest_sample_time, Some(fresh.sample_time));
            assert_eq!(data.peaks, vec![-60.0; 4]);
            assert_eq!(data.bar_levels, vec![-60.0; 4]);
            assert_eq!(data.smoothed_levels, vec![-60.0; 4]);
        }
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
        visuals.retain_spectrogram_panels(&[1]);
        visuals.configure(&Config {
            spectrogram_history_seconds: 60,
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
                Duration::from_secs(60)
            ) - 59.0 / 60.0)
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
        config.spectrum_db_range = Some(40.0);
        visuals.configure(&config);
        {
            let data = visuals.data.borrow();
            assert_eq!(data.visible_range().unwrap().2, 20.0);
            assert_eq!(data.spectrum_range().unwrap().2, 20.0);
            assert_eq!(data.spectrogram_db_range, 70.0);
        }
        config.spectrogram_db_range = Some(100.0);
        visuals.configure(&config);
        let data = visuals.data.borrow();
        assert_eq!(data.columns[0].levels, vec![-30.0; 4]);
        assert_eq!(data.len, 1);
    }

    #[test]
    fn common_range_changes_only_inherited_views_without_clearing_history() {
        for spectrum_override in [None, Some(70.0)] {
            for spectrogram_override in [None, Some(70.0)] {
                let mut config = Config {
                    spectrum_db_range: spectrum_override,
                    spectrogram_db_range: spectrogram_override,
                    ..Config::default()
                };
                let mut visuals = Visuals::new();
                visuals.retain_spectrogram_panels(&[1]);
                visuals.configure(&config);
                visuals.update(&frame(1.0, -30.0));
                let revision = visuals.data.borrow().spectrum_geometry_revision;

                config.visualization_db_range = 100.0;
                visuals.configure(&config);
                let data = visuals.data.borrow();
                assert_eq!(data.spectrum_db_range, spectrum_override.unwrap_or(100.0));
                assert_eq!(
                    data.spectrogram_db_range,
                    spectrogram_override.unwrap_or(100.0)
                );
                assert_eq!(data.len, 1);
                assert_eq!(data.columns[0].levels, vec![-30.0; 4]);
                assert_eq!(data.latest_levels, vec![-30.0; 4]);
                assert_eq!(data.spectrum_geometry_revision, revision);
                assert_eq!(data.latest_sample_time, Some(Duration::from_secs(1)));
            }
        }
    }

    #[test]
    fn radial_range_override_does_not_invalidate_spectrum_or_spectrogram() {
        let mut visuals = configured_visuals();
        visuals.update(&frame(1.0, -30.0));
        let revision = visuals.data.borrow().spectrum_geometry_revision;
        visuals.configure(&Config {
            audio_visualizer_db_range: Some(40.0),
            ..Config::default()
        });
        let data = visuals.data.borrow();
        assert_eq!(data.spectrum_geometry_revision, revision);
        assert_eq!(data.latest_levels, vec![-30.0; 4]);
        assert_eq!(data.columns[0].levels, vec![-30.0; 4]);
    }

    #[test]
    fn spectrum_range_and_grid_update_without_other_changes() {
        let mut visuals = Visuals::new();
        let initial = Config::default();
        visuals.configure(&initial);

        let mut range = initial.clone();
        range.spectrum_db_range = Some(40.0);
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
    fn spectrogram_labels_toggle_without_resetting_history() {
        let mut visuals = configured_visuals();
        let mut config = Config {
            spectrum_labels: true,
            ..Config::default()
        };
        visuals.configure(&config);
        visuals.update(&frame(1.0, -30.0));
        for visible in [false, true] {
            config.spectrogram_labels = visible;
            visuals.configure(&config);
            let data = visuals.data.borrow();
            assert_eq!(data.spectrogram_show_labels, visible);
            assert!(data.spectrum_show_labels);
            assert_eq!(data.len, 1);
            assert_eq!(data.columns[0].levels, vec![-30.0; 4]);
        }
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
    fn multiple_panel_sizes_preserve_wide_history_across_frames() {
        let mut visuals = configured_visuals();
        for time in 1..=10 {
            visuals.update(&frame(time as f64, -30.0));
        }
        let history_times = || {
            visuals
                .data
                .borrow()
                .columns
                .iter()
                .filter_map(|column| column.sample_time)
                .collect::<Vec<_>>()
        };
        let original = history_times();
        for sizes in [
            [(1, (400, 100)), (2, (3, 700))],
            [(2, (3, 700)), (1, (400, 100))],
        ] {
            visuals
                .data
                .borrow_mut()
                .spectrogram_panel_sizes
                .extend(sizes);
            visuals.finish_prepaint();
            assert_eq!(visuals.data.borrow().history_columns, 400);
            assert_eq!(visuals.data.borrow().spectrogram_panel_height, 700);
            assert_eq!(history_times(), original);
        }
        // Reusing cached prepaint leaves both panel measurements authoritative.
        visuals.finish_prepaint();
        assert_eq!(visuals.data.borrow().history_columns, 400);
        assert_eq!(history_times(), original);
    }

    #[test]
    fn cached_spectrogram_sizes_change_only_on_measurement_or_panel_retirement() {
        let mut visuals = configured_visuals();
        visuals.retain_spectrogram_panels(&[1, 2]);
        visuals
            .data
            .borrow_mut()
            .spectrogram_panel_sizes
            .extend([(1, (400, 100)), (2, (3, 700))]);
        assert!(visuals.finish_prepaint());
        assert!(
            !visuals.finish_prepaint(),
            "cached prepaint must not invalidate siblings"
        );
        visuals
            .data
            .borrow_mut()
            .spectrogram_panel_sizes
            .insert(1, (200, 100));
        assert!(
            visuals.finish_prepaint(),
            "shared width changes invalidate sibling scenes"
        );
        assert_eq!(visuals.data.borrow().history_columns, 200);
        visuals.retain_spectrogram_panels(&[1]);
        assert!(visuals.finish_prepaint());
        assert_eq!(visuals.data.borrow().spectrogram_panel_height, 100);
        assert!(!visuals.finish_prepaint());
        visuals.retain_spectrogram_panels(&[]);
        assert!(visuals.data.borrow().spectrogram_panel_sizes.is_empty());
        assert_eq!(visuals.data.borrow().history_columns, 0);
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
    fn spectrogram_tile_sampling_preserves_rows_columns_and_missing_values() {
        let samples = [
            HeatSample {
                left: 0,
                right: 0,
                fraction: 0.0,
            },
            HeatSample {
                left: 0,
                right: 1,
                fraction: 0.25,
            },
            HeatSample {
                left: 1,
                right: 1,
                fraction: 0.0,
            },
        ];
        for (interpolate, expected) in [
            (true, [-80.0, -10.0, -65.0, -7.5, -20.0, 0.0]),
            (false, [-80.0, -10.0, -80.0, -10.0, -20.0, 0.0]),
        ] {
            let mut values = [0.0; 6];
            sample_spectrogram_column(&[-80.0, -20.0], &samples, interpolate, &mut values, 2, 0);
            sample_spectrogram_column(&[-10.0, 0.0], &samples, interpolate, &mut values, 2, 1);
            assert_eq!(values, expected);
            sample_spectrogram_column(&[], &samples, interpolate, &mut values, 2, 0);
            assert_eq!(
                values,
                [
                    f32::NEG_INFINITY,
                    -10.0,
                    f32::NEG_INFINITY,
                    expected[3],
                    f32::NEG_INFINITY,
                    0.0
                ]
            );
        }
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
