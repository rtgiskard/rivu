//! GPU-backed analysis views consuming borrowed analysis frames.
//! Spectrum retains musical display bands; Spectrogram peak-pools complete FFT
//! bins into bounded logarithmic history rows. Time buckets follow physical plot
//! width without changing the configured audio-time span. Missing history stays
//! blank. Only accepted audio changes history; display ticks move retained tiles.
//! Hidden panels release working data and keep only bounded final publications.

mod sampling;
mod spectrogram;
mod spectrum;

use crate::labels::paint_label;
#[cfg(test)]
use crate::palette::{DEADBEEF_STOPS, TOKYO_NIGHT_STOPS, gradient};
use crate::palette::{PaletteLut, active_palette_lut};
#[cfg(test)]
use spectrogram::{
    SPECTROGRAM_TILE_COLUMNS, SpectrogramTile, history_columns_for_width,
    sample_spectrogram_column, spectrogram_source_height,
};
use spectrogram::{Spectrogram, history_column_count};
#[cfg(test)]
use spectrum::{
    MAX_SPECTRUM_BARS, group_bands, shape_y, spectrum_bar_count, spectrum_labels_visible,
};
use spectrum::{Spectrum, advance_envelope, note_label, smooth_release, smoothing_decay};

use crate::VisualLabels;
use crate::VisualParameters;

#[cfg(test)]
use gpui::GpuFrame;
use gpui::{
    AnyElement, Bounds, Pixels, SharedString, canvas, div, fill, point, prelude::*, px, rgb, size,
};
use std::{cell::RefCell, collections::HashMap, rc::Rc, sync::Arc, time::Duration};

use rivu_audio::analysis::AnalysisFrame;
use rivu_format::config::{AnalysisWindow, VisualizationSignal};
#[cfg(test)]
use rivu_format::config::{SpectrumBarLayout, SpectrumStyle, VisualizationPalette};

#[derive(Clone, Copy)]
struct HeatSample {
    left: usize,
    right: usize,
    fraction: f32,
}

fn visual_canvas_has_room(size: gpui::Size<Pixels>) -> bool {
    size.width > px(64.0) && size.height > px(40.0)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FrequencyKind {
    Spectrum,
    Spectrogram,
}

struct FrequencyState {
    enabled: bool,
    last_analysis_sequence: Option<u64>,
    frequencies: Vec<f32>,
    sample_rate: u32,
    material_budget_bytes: Option<u64>,
    latest_levels: Vec<f32>,
    display_time: Option<Duration>,
    visualization_background: u32,
    palette_lut: Rc<PaletteLut>,
    analysis_fft_size: u32,
    analysis_window: AnalysisWindow,
    visualization_signal: VisualizationSignal,
    analysis_rate_hz: u32,
    stream_generation: u64,
    source_generation: Option<u64>,
    latest_sample_time: Option<Duration>,
    error: Option<String>,
    last_paint_error: Option<String>,
    spectrum: Spectrum,
    spectrogram: Spectrogram,
}

#[derive(Clone)]
pub struct FrequencyViews {
    // GPUI prepaint/paint closures are 'static. Share the UI-thread state rather
    // than copying the full history into each panel's render closure.
    data: Rc<RefCell<FrequencyState>>,
}

impl Default for FrequencyViews {
    fn default() -> Self {
        Self::new()
    }
}

impl FrequencyViews {
    pub fn new() -> Self {
        let config = VisualParameters::default();
        Self {
            data: Rc::new(RefCell::new(FrequencyState {
                enabled: true,
                last_analysis_sequence: None,
                frequencies: Vec::new(),
                sample_rate: 0,
                material_budget_bytes: None,
                latest_levels: Vec::new(),
                display_time: None,
                palette_lut: active_palette_lut(config.visualization_palette),
                visualization_background: config.visualization_background.rgb(),
                analysis_fft_size: config.analysis_fft_size,
                analysis_window: config.analysis_window,
                visualization_signal: config.visualization_signal,
                analysis_rate_hz: config.analysis_rate_hz,
                stream_generation: 0,
                source_generation: None,
                latest_sample_time: None,
                error: None,
                last_paint_error: None,
                spectrum: Spectrum::new(&config),
                spectrogram: Spectrogram::new(&config),
            })),
        }
    }

    /// Suspend analysis and release working data, retaining the last drawable
    /// publications until fresh output is ready or explicitly invalidated.
    pub fn set_enabled(&mut self, enabled: bool) {
        let mut data = self.data.borrow_mut();
        if data.enabled == enabled {
            return;
        }
        data.enabled = enabled;
        if enabled {
            return;
        }
        data.release_history();
        data.spectrogram.panel_sizes = HashMap::new();
        data.frequencies = Vec::new();
        data.sample_rate = 0;
        data.latest_levels = Vec::new();
        data.spectrum.peaks = Vec::new();
        data.spectrum.peak_deadlines = Vec::new();
        data.spectrum.bar_levels = Vec::new();
        data.spectrum.bar_deadlines = Vec::new();
        data.spectrum.smoothed_levels = Vec::new();
        data.spectrum.points = Vec::new();
        data.spectrum.rectangles = Vec::new();
        data.spectrum.mappings = HashMap::new();
        data.last_analysis_sequence = None;
        data.error = None;
        data.last_paint_error = None;
        data.spectrum.envelope_initialized = false;
        data.spectrum.last_update = None;
        data.source_generation = None;
        data.stream_generation = data.stream_generation.wrapping_add(1);
    }

    /// Clear both retained output and analysis state at a semantic boundary.
    pub fn clear_presentation(&mut self) {
        let enabled = self.data.borrow().enabled;
        self.set_enabled(false);
        let mut data = self.data.borrow_mut();
        data.spectrum.presentations = HashMap::new();
        data.spectrogram.presentations = HashMap::new();
        data.enabled = enabled;
        data.spectrum.top_db = 0.0;
        data.refresh_db_labels();
    }
    pub fn configure(&mut self, config: &VisualParameters) -> bool {
        let mut data = self.data.borrow_mut();
        data.material_budget_bytes
            .get_or_insert(config.gpu_material_budget_bytes());
        let spectrum_db_range = config.spectrum_db_range();
        let spectrogram_db_range = config.spectrogram_db_range();
        let palette_changed = data.palette_lut.palette != config.visualization_palette;
        let history_missing =
            data.enabled && data.spectrogram.visible && data.spectrogram.history_columns == 0;
        let settings_changed = palette_changed
            || data.visualization_background != config.visualization_background.rgb()
            || data.spectrum.db_range != spectrum_db_range
            || data.spectrogram.db_range != spectrogram_db_range
            || data.spectrum.bar_layout != config.spectrum_bar_layout
            || data.spectrum.style != config.spectrum_style
            || data.spectrum.gap != config.spectrum_gap
            || data.spectrum.show_peaks != config.spectrum_peaks
            || data.spectrum.peak_hold_ms != config.spectrum_peak_hold_ms
            || data.spectrum.peak_gravity != config.spectrum_peak_gravity
            || data.spectrum.bar_hold_ms != config.spectrum_bar_hold_ms
            || data.spectrum.bar_gravity != config.spectrum_bar_gravity
            || data.spectrum.smoothing_ms != config.spectrum_smoothing_ms
            || data.spectrum.interpolate != config.spectrum_interpolate
            || data.analysis_fft_size != config.analysis_fft_size
            || data.analysis_window != config.analysis_window
            || data.visualization_signal != config.visualization_signal
            || data.spectrogram.interpolate != config.spectrogram_interpolate
            || data.spectrogram.history_seconds != config.spectrogram_history_seconds
            || data.analysis_rate_hz != config.analysis_rate_hz;
        let changed = history_missing || settings_changed;
        if !changed {
            return false;
        }
        let history_changed = history_missing
            || data.spectrogram.history_seconds != config.spectrogram_history_seconds
            || data.analysis_rate_hz != config.analysis_rate_hz;
        let analysis_changed = data.analysis_fft_size != config.analysis_fft_size
            || data.analysis_window != config.analysis_window
            || data.visualization_signal != config.visualization_signal;
        let heat_changed = palette_changed
            || data.spectrogram.db_range != spectrogram_db_range
            || data.spectrogram.interpolate != config.spectrogram_interpolate;
        // Allocation of missing visible history is not a settings change.
        if settings_changed {
            data.spectrum.presentations.clear();
            data.spectrogram.presentations.clear();
        }
        if palette_changed {
            data.palette_lut = active_palette_lut(config.visualization_palette);
        }
        data.visualization_background = config.visualization_background.rgb();
        data.spectrum.db_range = spectrum_db_range;
        data.spectrum.bar_layout = config.spectrum_bar_layout;
        data.spectrum.style = config.spectrum_style;
        data.spectrum.gap = config.spectrum_gap;
        data.spectrum.show_peaks = config.spectrum_peaks;
        data.spectrum.peak_hold_ms = config.spectrum_peak_hold_ms;
        data.spectrum.peak_gravity = config.spectrum_peak_gravity;
        data.spectrum.bar_hold_ms = config.spectrum_bar_hold_ms;
        data.spectrum.bar_gravity = config.spectrum_bar_gravity;
        data.spectrum.smoothing_ms = config.spectrum_smoothing_ms;
        data.spectrum.interpolate = config.spectrum_interpolate;
        data.spectrogram.interpolate = config.spectrogram_interpolate;
        data.analysis_fft_size = config.analysis_fft_size;
        data.analysis_window = config.analysis_window;
        data.visualization_signal = config.visualization_signal;
        data.spectrogram.db_range = spectrogram_db_range;
        data.spectrogram.history_seconds = config.spectrogram_history_seconds;
        data.analysis_rate_hz = config.analysis_rate_hz;
        if data.enabled && data.spectrogram.visible && history_changed {
            data.resize_history(history_column_count(
                config.spectrogram_history_seconds,
                config.analysis_rate_hz,
            ));
        }
        if analysis_changed {
            data.clear_history();
            data.latest_levels.clear();
            data.spectrum.peaks.clear();
            data.spectrum.peak_deadlines.clear();
            data.spectrum.bar_levels.clear();
            data.spectrum.bar_deadlines.clear();
            data.spectrum.smoothed_levels.clear();
            data.spectrum.mappings.clear();
            data.last_analysis_sequence = None;
            data.spectrum.last_update = None;
            data.spectrum.envelope_initialized = false;
        } else if heat_changed {
            data.invalidate_spectrogram();
        }
        let levels = if data.spectrum.show_peaks {
            &data.spectrum.peaks
        } else {
            &data.latest_levels
        };
        data.spectrum.top_db = (levels.iter().copied().fold(0.0, f32::max) / 6.0).ceil() * 6.0;
        data.refresh_db_labels();
        true
    }

    pub fn update(&mut self, frame: &AnalysisFrame, presentation_time: Duration) {
        let mut data = self.data.borrow_mut();
        if !data.enabled {
            return;
        }
        if !frame.settings.is_some_and(|settings| {
            settings.fft_size == data.analysis_fft_size
                && settings.window == data.analysis_window
                && settings.signal == data.visualization_signal
        }) {
            data.latest_levels.clear();
            data.spectrum.peaks.clear();
            data.spectrum.peak_deadlines.clear();
            data.spectrum.bar_levels.clear();
            data.spectrum.bar_deadlines.clear();
            data.spectrum.smoothed_levels.clear();
            data.spectrum.mappings.clear();
            data.spectrum.presentations.clear();
            // Continuity loss revokes live Spectrum output, not compatible
            // historical Spectrogram buckets or their last publication.
            data.display_time = None;
            data.spectrum.envelope_initialized = false;
            data.last_analysis_sequence = None;
            data.spectrum.last_update = None;
            return;
        }
        if data.source_generation != Some(frame.generation) {
            data.source_generation = Some(frame.generation);
            data.spectrum.envelope_initialized = false;
            data.spectrum.last_update = None;
        }
        if frame.spectrum_db.len() != frame.frequencies_hz.len() {
            data.report_error("Analysis frequency and level counts differ".into());
            return;
        }
        data.error = None;
        let sample_rate_changed = data.sample_rate != frame.sample_rate;
        data.sample_rate = frame.sample_rate;
        if sample_rate_changed || data.frequencies != frame.frequencies_hz {
            data.invalidate_spectrogram();
            data.stream_generation = data.stream_generation.wrapping_add(1);
            data.latest_levels.clear();
            data.spectrum.envelope_initialized = false;
            data.frequencies.clone_from(&frame.frequencies_hz);
            data.spectrum.top_db = 0.0;
            let generation = data.stream_generation;
            for column in &mut data.spectrogram.columns {
                column.generation = generation;
            }
            data.spectrum.last_update = None;
            data.refresh_db_labels();
            data.refresh_frequency_labels();
        }
        if frame.spectrum_db.is_empty() {
            return;
        }
        let now = presentation_time;
        if data
            .latest_sample_time
            .is_some_and(|last| frame.sample_time < last)
        {
            data.clear_history();
            data.spectrum.last_update = None;
            data.spectrum.envelope_initialized = false;
        }
        let new_input = data.last_analysis_sequence != Some(frame.sequence);
        data.latest_levels.clone_from(&frame.spectrum_db);
        if data.spectrogram.visible
            && new_input
            && frame.visualizer.demand.spectrogram
            && !frame.visualizer.spectrogram.is_empty()
        {
            data.push_fft_history(frame.sample_time, &frame.visualizer.spectrogram);
        }
        data.last_analysis_sequence = Some(frame.sequence);
        data.latest_sample_time = Some(frame.sample_time);
        data.display_time = Some(presentation_time);
        if !data.spectrum.visible {
            return;
        }
        if !new_input && data.spectrum.envelope_initialized {
            return;
        }
        data.spectrum.last_update = Some(now);
        if !data.spectrum.envelope_initialized
            || data.spectrum.peaks.len() != frame.spectrum_db.len()
        {
            data.spectrum.peaks.clone_from(&frame.spectrum_db);
            data.spectrum.bar_levels.clone_from(&frame.spectrum_db);
            data.spectrum.smoothed_levels.clone_from(&frame.spectrum_db);
            let peak_deadline = now + Duration::from_millis(data.spectrum.peak_hold_ms as u64);
            let bar_deadline = now + Duration::from_millis(data.spectrum.bar_hold_ms as u64);
            data.spectrum.peak_deadlines.clear();
            data.spectrum
                .peak_deadlines
                .resize(frame.spectrum_db.len(), peak_deadline);
            data.spectrum.bar_deadlines.clear();
            data.spectrum
                .bar_deadlines
                .resize(frame.spectrum_db.len(), bar_deadline);
            data.spectrum.envelope_initialized = true;
        }
        // Only accepted input may raise levels or renew hold deadlines.
        for index in 0..data.spectrum.peaks.len() {
            let value = finite_level(frame.spectrum_db[index]);
            if value >= data.spectrum.peaks[index] {
                data.spectrum.peaks[index] = value;
                data.spectrum.peak_deadlines[index] =
                    now + Duration::from_millis(data.spectrum.peak_hold_ms as u64);
            }
            if value >= data.spectrum.bar_levels[index] {
                data.spectrum.bar_levels[index] = value;
                data.spectrum.bar_deadlines[index] =
                    now + Duration::from_millis(data.spectrum.bar_hold_ms as u64);
            }
            data.spectrum.smoothed_levels[index] = data.spectrum.smoothed_levels[index].max(value);
        }
        let top_levels = if data.spectrum.show_peaks {
            &data.spectrum.peaks
        } else {
            &data.latest_levels
        };
        let top_db = (top_levels.iter().copied().fold(0.0, f32::max) / 6.0).ceil() * 6.0;
        if top_db != data.spectrum.top_db {
            data.spectrum.top_db = top_db;
            data.refresh_db_labels();
        }
    }

    /// Evolve retained display state without accepting or pooling analysis again.
    pub fn animate(&mut self, presentation_time: Duration) -> bool {
        let mut data = self.data.borrow_mut();
        if !data.enabled || data.error.is_some() {
            return false;
        }
        let Some(latest) = data.latest_sample_time else {
            return false;
        };
        let now = presentation_time.min(latest);
        let Some(previous) = data.display_time else {
            return false;
        };
        if now <= previous {
            return false;
        }
        data.display_time = Some(now);
        let mut changed = data.spectrogram.visible && data.spectrogram.len > 0;
        if !data.spectrum.visible || !data.spectrum.envelope_initialized {
            return changed;
        }
        let last = data.spectrum.last_update.replace(now).unwrap_or(now);
        let decay = smoothing_decay(now.saturating_sub(last), data.spectrum.smoothing_ms);
        let mut body_changed = false;
        for index in 0..data.spectrum.peaks.len() {
            let before = data.display_level(index);
            let peak_before = data.spectrum.peaks[index];
            let value = finite_level(data.latest_levels[index]);
            // A display tick never renews an accepted input's hold deadline.
            data.spectrum.peaks[index] = advance_envelope(
                data.spectrum.peaks[index],
                data.spectrum.peak_deadlines[index],
                value,
                last,
                now,
                data.spectrum.peak_hold_ms,
                data.spectrum.peak_gravity,
            )
            .0;
            data.spectrum.bar_levels[index] = advance_envelope(
                data.spectrum.bar_levels[index],
                data.spectrum.bar_deadlines[index],
                value,
                last,
                now,
                data.spectrum.bar_hold_ms,
                data.spectrum.bar_gravity,
            )
            .0;
            data.spectrum.smoothed_levels[index] =
                smooth_release(data.spectrum.smoothed_levels[index], value, decay);
            body_changed |= before != data.display_level(index);
            changed |= peak_before != data.spectrum.peaks[index];
        }
        let top_levels = if data.spectrum.show_peaks {
            &data.spectrum.peaks
        } else {
            &data.latest_levels
        };
        let top_db = (top_levels.iter().copied().fold(0.0, f32::max) / 6.0).ceil() * 6.0;
        if top_db != data.spectrum.top_db {
            data.spectrum.top_db = top_db;
            data.refresh_db_labels();
            changed = true;
        }
        changed || body_changed
    }

    pub fn spectrum(
        &self,
        theme: &'static rivu_ui::theme::Palette,
        i18n: &Arc<VisualLabels>,
        panel_id: u64,
        options: rivu_format::config::AudioVisualizerOptions,
    ) -> AnyElement {
        self.view(theme, i18n, panel_id, FrequencyKind::Spectrum, options)
    }

    pub fn spectrogram(
        &self,
        theme: &'static rivu_ui::theme::Palette,
        i18n: &Arc<VisualLabels>,
        panel_id: u64,
        options: rivu_format::config::AudioVisualizerOptions,
    ) -> AnyElement {
        self.view(theme, i18n, panel_id, FrequencyKind::Spectrogram, options)
    }
    pub fn retain_spectrogram_panels(&mut self, panel_ids: &[u64]) {
        let mut data = self.data.borrow_mut();
        // Cached panel views may reuse prepaint. Keep their last measurement
        // until the panel is hidden/removed or prepaints with changed bounds.
        data.spectrogram
            .panel_sizes
            .retain(|panel_id, _| panel_ids.contains(panel_id));
        data.spectrogram.visible = !panel_ids.is_empty();
        if panel_ids.is_empty() {
            data.release_history();
        }
    }

    pub fn set_spectrum_visible(&self, visible: bool) -> bool {
        let mut data = self.data.borrow_mut();
        let restored = visible && !data.spectrum.visible;
        if data.spectrum.visible != visible {
            data.spectrum.visible = visible;
            data.spectrum.envelope_initialized = false;
            data.spectrum.last_update = None;
            if !visible {
                data.release_spectrum();
            }
        }
        restored
    }

    pub fn retain_spectrum_panels(&mut self, panel_ids: &[u64]) {
        if panel_ids.is_empty() {
            self.data.borrow_mut().spectrum.mappings = HashMap::new();
            return;
        }
        self.data
            .borrow_mut()
            .spectrum
            .mappings
            .retain(|panel_id, _| panel_ids.contains(panel_id));
    }

    /// Keep publications for every layout panel, including inactive tabs.
    pub fn retain_presentation_panels(&mut self, spectrum_ids: &[u64], spectrogram_ids: &[u64]) {
        let mut data = self.data.borrow_mut();
        data.spectrum
            .presentations
            .retain(|panel_id, _| spectrum_ids.contains(panel_id));
        data.spectrogram
            .presentations
            .retain(|panel_id, _| spectrogram_ids.contains(panel_id));
    }

    // The root's first paint runs after every panel's prepaint. Size shared
    // history once here, never while individual panels are painting it.
    pub fn finish_prepaint(&self) -> bool {
        self.data.borrow_mut().finish_prepaint()
    }

    fn view(
        &self,
        theme: &'static rivu_ui::theme::Palette,
        i18n: &Arc<VisualLabels>,
        panel_id: u64,
        mode: FrequencyKind,
        options: rivu_format::config::AudioVisualizerOptions,
    ) -> AnyElement {
        let data = Rc::clone(&self.data);
        let message: Option<SharedString> = {
            let data = self.data.borrow();
            if data.error.is_some() || data.latest_levels.is_empty() {
                None
            } else {
                match mode {
                    FrequencyKind::Spectrum if data.spectrum_range().is_none() => {
                        Some(i18n.no_spectrum.clone())
                    }
                    FrequencyKind::Spectrogram if data.visible_range().is_none() => {
                        Some(i18n.no_spectrogram.clone())
                    }
                    _ => None,
                }
            }
        };
        let i18n = Arc::clone(i18n);
        let showing_status = message.is_some();
        let prepaint_data = Rc::clone(&data);
        let kind = match mode {
            FrequencyKind::Spectrum => "spectrum",
            FrequencyKind::Spectrogram => "spectrogram",
        };
        let visualization_background = self.data.borrow().visualization_background;
        gpui::container_query(move |available, window, _| {
            prepaint_data.borrow_mut().spectrogram.scale_factor = window.scale_factor();
            if !prepaint_data.borrow_mut().prepaint_panel_with_labels(
                panel_id,
                mode,
                available,
                options.labels,
            ) {
                return div()
                    .id((kind, panel_id))
                    .size_full()
                    .min_w_0()
                    .min_h_0()
                    .overflow_hidden()
                    .bg(rgb(visualization_background))
                    .into_any_element();
            }
            div()
                .id((kind, panel_id))
                .relative()
                .size_full()
                .min_w_0()
                .min_h_0()
                .overflow_hidden()
                .bg(rgb(visualization_background))
                .child(
                    canvas(
                        |_, _, _| (),
                        move |bounds, _, window, cx| {
                            let mut data = data.borrow_mut();
                            window.paint_quad(fill(bounds, rgb(data.visualization_background)));
                            if !visual_canvas_has_room(bounds.size) {
                                return;
                            }
                            let result = if showing_status {
                                Ok(())
                            } else {
                                match mode {
                                    FrequencyKind::Spectrum => {
                                        data.paint_spectrum(panel_id, bounds, options, window, cx)
                                    }
                                    FrequencyKind::Spectrogram => data
                                        .paint_spectrogram(panel_id, bounds, options, window, cx),
                                }
                            };
                            let paint_error = result.err();
                            if data.last_paint_error != paint_error {
                                if let Some(error) = &paint_error {
                                    eprintln!("rivu visualization: {error}");
                                }
                                data.last_paint_error.clone_from(&paint_error);
                            }
                            if let Some(error) = paint_error.as_ref().or(data.error.as_ref()) {
                                window.paint_quad(fill(
                                    Bounds::new(bounds.origin, size(bounds.size.width, px(22.0))),
                                    rgb(theme.error_bg),
                                ));
                                paint_label(
                                    i18n.visual_error(error.as_str()).into(),
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

impl FrequencyState {
    fn prepaint_panel_with_labels(
        &mut self,
        panel_id: u64,
        mode: FrequencyKind,
        available: gpui::Size<Pixels>,
        show_labels: bool,
    ) -> bool {
        let has_room = visual_canvas_has_room(available);
        if mode == FrequencyKind::Spectrogram {
            if has_room {
                let margin = axis_label_margin(show_labels, &self.spectrogram.frequency_labels);
                let scale = self.spectrogram.scale_factor;
                let width = (((available.width - px(margin + 8.0)) / px(1.0)) * scale)
                    .ceil()
                    .max(2.0) as usize;
                let height = (((available.height - px(12.0)) / px(1.0)) * scale)
                    .ceil()
                    .max(1.0) as usize;
                self.spectrogram
                    .panel_sizes
                    .insert(panel_id, (width, height));
            } else {
                self.spectrogram.panel_sizes.remove(&panel_id);
            }
        }
        has_room
    }

    fn refresh_frequency_labels(&mut self) {
        if let Some((_, _, low, high)) = self.visible_range() {
            self.spectrogram.frequency_labels = [frequency_label(low), frequency_label(high)];
        }
        if let Some((_, _, low, high)) = self.spectrum_range() {
            self.spectrum.labels = [note_label(low), note_label(high)];
        }
    }

    fn report_error(&mut self, error: String) {
        if self.error.as_ref() != Some(&error) {
            eprintln!("rivu visualization: {error}");
            self.error = Some(error);
        }
    }
}

fn plot_bounds(plot: Bounds<Pixels>, normalized: Bounds<f32>) -> Bounds<Pixels> {
    Bounds::new(
        point(
            plot.left() + plot.size.width * normalized.origin.x,
            plot.top() + plot.size.height * normalized.origin.y,
        ),
        size(
            plot.size.width * normalized.size.width,
            plot.size.height * normalized.size.height,
        ),
    )
}

fn axis_frequency(low: f32, high: f32, fraction: f32, logarithmic: bool) -> f32 {
    if logarithmic {
        (low.ln() + (high.ln() - low.ln()) * fraction).exp()
    } else {
        low + (high - low) * fraction
    }
}

fn axis_label_margin(show_labels: bool, labels: &[SharedString]) -> f32 {
    if !show_labels {
        return 8.0;
    }
    let max_chars = labels.iter().map(|label| label.len()).max().unwrap_or(0) as f32;
    (max_chars * 7.0 + 8.0).max(32.0)
}

fn stop_color([red, green, blue]: [u8; 3]) -> u32 {
    (u32::from(red) << 16) | (u32::from(green) << 8) | u32::from(blue)
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

#[cfg(test)]
impl FrequencyViews {
    fn configure_config(&mut self, config: &rivu_format::config::Config) -> bool {
        self.configure(&crate::VisualParameters::from(config))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use rivu_format::config::Config;

    #[test]
    fn native_ticks_release_and_scroll_without_reaccepting_audio() {
        let mut visuals = configured_visuals();
        visuals.configure_config(&Config {
            analysis_rate_hz: 5,
            spectrum_peak_hold_ms: 0,
            spectrum_bar_hold_ms: 0,
            ..Config::default()
        });
        let start = Duration::from_secs(1);
        visuals.update(&frame(1.0, -10.0), start);
        visuals.update(&frame(1.2, -80.0), start);
        let publication = prepared_body(&visuals, 1, 100.0, 100.0).unwrap();
        let (history, sample_time, deadlines) = {
            let mut data = visuals.data.borrow_mut();
            data.spectrogram.tiles.push(Some(SpectrogramTile {
                key: [None; SPECTROGRAM_TILE_COLUMNS],
                start,
                end: Duration::from_millis(1200),
                publication: publication.clone(),
            }));
            (
                data.spectrogram
                    .columns
                    .iter()
                    .map(|column| {
                        (
                            column.revision,
                            column.sample_time,
                            column.interval_start,
                            column.levels.clone(),
                        )
                    })
                    .collect::<Vec<_>>(),
                data.latest_sample_time,
                data.spectrum.peak_deadlines.clone(),
            )
        };
        for tick in 1..=11 {
            let now = start + Duration::from_secs_f64(tick as f64 / 60.0);
            assert!(visuals.animate(now));
            assert!(!visuals.animate(now));
        }
        {
            let data = visuals.data.borrow();
            assert!(data.spectrum.peaks[0] < -10.0);
            assert!(data.spectrum.bar_levels[0] < -10.0);
            assert!(data.spectrum.smoothed_levels[0] < -10.0);
            assert_eq!(data.latest_sample_time, sample_time);
            assert_eq!(data.spectrum.peak_deadlines, deadlines);
            assert_eq!(
                data.spectrogram
                    .columns
                    .iter()
                    .map(|column| (
                        column.revision,
                        column.sample_time,
                        column.interval_start,
                        column.levels.clone()
                    ))
                    .collect::<Vec<_>>(),
                history
            );
            assert!(Arc::ptr_eq(
                &data
                    .spectrogram
                    .tiles
                    .last()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .publication,
                &publication
            ));
            assert!(
                spectrogram::time_position(
                    start,
                    data.display_time.unwrap(),
                    data.spectrogram.history_limit_seconds
                ) < 1.0
            );
        }
        visuals.set_spectrum_visible(false);
        visuals.animate(Duration::from_millis(1190));
        visuals.set_spectrum_visible(true);
        visuals.update(&frame(1.2, -80.0), Duration::from_millis(1190));
        assert_eq!(visuals.data.borrow().spectrum.peaks, vec![-80.0; 4]);
        visuals.update(&frame(1.4, -5.0), Duration::from_millis(1190));
        assert_eq!(visuals.data.borrow().spectrum.peaks, vec![-5.0; 4]);
    }

    #[test]
    fn native_zero_gravity_snaps_once_at_hold_without_renewal() {
        let mut visuals = configured_visuals();
        visuals.configure_config(&Config {
            spectrum_peak_hold_ms: 100,
            spectrum_peak_gravity: 0.0,
            spectrum_bar_hold_ms: 100,
            spectrum_bar_gravity: 0.0,
            ..Config::default()
        });
        let start = Duration::from_secs(1);
        visuals.update(&frame(1.0, -10.0), start);
        visuals.update(&frame(1.2, -80.0), start);
        visuals.animate(Duration::from_millis(1099));
        assert_eq!(visuals.data.borrow().spectrum.peaks, vec![-10.0; 4]);
        assert!(visuals.animate(Duration::from_millis(1100)));
        assert_eq!(visuals.data.borrow().spectrum.peaks, vec![-80.0; 4]);
        assert_eq!(visuals.data.borrow().spectrum.bar_levels, vec![-80.0; 4]);
        assert!(!visuals.animate(Duration::from_millis(1100)));
        assert_eq!(
            visuals.data.borrow().spectrum.peak_deadlines,
            vec![Duration::from_millis(1100); 4]
        );
    }

    #[test]
    fn appearance_changes_preserve_history_bucketing() {
        let mut visuals = configured_visuals();
        let mut config = Config::default();
        visuals.data.borrow_mut().resize_history(100);
        visuals.update(&frame(10.010, -10.0), Duration::from_secs_f64(10.010));
        config.visualization_background = "#202020".parse().unwrap();
        config.spectrogram_db_range = Some(100.0);
        visuals.configure_config(&config);
        visuals.update(&frame(10.020, -60.0), Duration::from_secs_f64(10.020));
        let data = visuals.data.borrow();
        assert_eq!(data.spectrogram.len, 1);
        assert_eq!(
            level_range(&data.spectrogram.columns[data.spectrogram.latest.unwrap()].levels),
            Some((-10.0, -10.0))
        );
    }

    #[test]
    fn valid_analysis_replaces_previous_input_error() {
        let mut visuals = configured_visuals();
        let mut invalid = frame(1.0, -20.0);
        invalid.spectrum_db.pop();
        visuals.update(&invalid, invalid.sample_time);
        assert!(visuals.data.borrow().error.is_some());
        visuals.update(&frame(1.1, -30.0), Duration::from_secs_f64(1.1));
        let data = visuals.data.borrow();
        assert!(data.error.is_none());
        assert_eq!(data.latest_levels, vec![-30.0; 4]);
        assert!(data.spectrum_render_range().is_some());
    }

    fn prepared_body(
        visuals: &FrequencyViews,
        panel: u64,
        width: f32,
        height: f32,
    ) -> Option<Arc<GpuFrame>> {
        let mut data = visuals.data.borrow_mut();
        let (_, _, low, high) = data.spectrum_render_range().unwrap();
        let bars = spectrum_bar_count(
            width,
            data.spectrum.style == SpectrumStyle::Solid,
            data.spectrum.bar_layout,
            data.spectrum.gap,
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
    fn gpu_spectrum_continuous_signal_preserves_amplitude_and_plot_domain() {
        for style in [SpectrumStyle::Line, SpectrumStyle::Solid] {
            for interpolate in [false, true] {
                for level in [-99.9, -75.0, 0.0] {
                    let mut visuals = configured_visuals();
                    visuals.configure_config(&Config {
                        spectrum_style: style,
                        spectrum_interpolate: interpolate,
                        spectrum_db_range: Some(100.0),
                        spectrum_bar_layout: SpectrumBarLayout::Fixed { count: 4 },
                        ..Config::default()
                    });
                    visuals.update(&frame(1.0, level), Duration::from_secs_f64(1.0));
                    let body = prepared_body(&visuals, 1, 100.0, 100.0).unwrap();
                    let gpui::GpuContent::Path { path, params } = body.content() else {
                        panic!("expected continuous spectrum path")
                    };
                    let plot = Bounds::new(point(px(13.25), px(21.5)), size(px(100.0), px(100.0)));
                    assert_eq!(path.bounds, plot);
                    assert_eq!(params.palette_range, [0.0, 1.0]);
                    let minimum_y = path
                        .vertices
                        .iter()
                        .map(|vertex| vertex.xy_position.y)
                        .fold(plot.bottom(), |a, b| a.min(b));
                    if style == SpectrumStyle::Solid {
                        let expected = plot.bottom() - plot.size.height * ((level + 100.0) / 100.0);
                        assert!((minimum_y - expected).abs() < px(0.001));
                    } else {
                        assert!(minimum_y >= plot.top() - px(0.001));
                        assert!(minimum_y < plot.bottom());
                    }
                    assert!(
                        path.vertices
                            .iter()
                            .all(|vertex| vertex.xy_position.y <= plot.bottom() + px(1.0))
                    );
                }
            }
        }
    }

    #[test]
    fn solid_spectrum_preserves_subpixel_and_full_height_signal() {
        for level in [-99.9, -75.0, 0.0] {
            let mut visuals = configured_visuals();
            visuals.configure_config(&Config {
                spectrum_style: SpectrumStyle::Solid,
                spectrum_interpolate: false,
                spectrum_db_range: Some(100.0),
                ..Config::default()
            });
            visuals.update(&frame(1.0, level), Duration::from_secs_f64(1.0));
            let body = prepared_body(&visuals, 1, 100.0, 100.0).unwrap();
            let gpui::GpuContent::Path { path, .. } = body.content() else {
                panic!("expected filled spectrum path")
            };
            let minimum_y = path
                .vertices
                .iter()
                .map(|vertex| vertex.xy_position.y)
                .fold(px(f32::INFINITY), |a, b| a.min(b));
            let expected_y = px(121.5 - (level + 100.0));
            assert!(
                (minimum_y - expected_y).abs() < px(0.001),
                "{level} dB must retain its measured height, not a stroke inset"
            );
        }
    }

    #[test]
    fn gpu_spectrum_outline_preserves_nonoverlapping_edges_and_palette_domain() {
        for height in [0.125, 1.0, 2.0, 25.0, 100.0] {
            let mut visuals = configured_visuals();
            visuals.configure_config(&Config {
                spectrum_style: SpectrumStyle::Outline,
                spectrum_db_range: Some(100.0),
                spectrum_bar_layout: SpectrumBarLayout::Fixed { count: 4 },
                spectrum_gap: 2.0,
                ..Config::default()
            });
            visuals.update(&frame(1.0, height - 100.0), Duration::from_secs_f64(1.0));
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
    fn gpu_spectrum_layout_changes_publish_matching_bar_geometry() {
        let mut visuals = configured_visuals();
        for (layout, expected_count) in [
            (SpectrumBarLayout::Automatic { width: 3.0 }, 20),
            (SpectrumBarLayout::Fixed { count: 4 }, 4),
            (SpectrumBarLayout::Automatic { width: 8.0 }, 10),
            (SpectrumBarLayout::Fixed { count: 1 }, 1),
        ] {
            visuals.configure_config(&Config {
                spectrum_style: SpectrumStyle::Bars,
                spectrum_bar_layout: layout,
                spectrum_gap: 2.0,
                spectrum_interpolate: true,
                spectrum_db_range: Some(100.0),
                ..Config::default()
            });
            visuals.update(&frame(1.0, 0.0), Duration::from_secs(1));
            let body = rectangle_frame(prepared_body(&visuals, 1, 100.0, 100.0));
            let gpui::GpuContent::Rectangles { rectangles, .. } = body.content() else {
                unreachable!()
            };
            assert_eq!(rectangles.len(), expected_count);
            let stride = 100.0 / expected_count as f32;
            for (bar, rectangle) in rectangles.iter().enumerate() {
                let [x, _, width, _] = rectangle.bounds;
                assert!((x * 100.0 - (bar as f32 * stride + 1.0)).abs() < 1e-5);
                assert!((width * 100.0 - (stride - 2.0)).abs() < 1e-5);
            }
        }
    }

    #[test]
    fn gpu_spectrum_preserves_nonzero_heights_and_led_partial_cells() {
        for style in [SpectrumStyle::Bars, SpectrumStyle::Led] {
            for height in [0.0, 0.125, 1.0, 2.75, 3.0, 4.5, 5.0, 5.25, 8.0, 100.0] {
                let mut visuals = configured_visuals();
                visuals.configure_config(&Config {
                    spectrum_style: style,
                    spectrum_db_range: Some(100.0),
                    spectrum_bar_layout: SpectrumBarLayout::Fixed { count: 4 },
                    spectrum_gap: 2.0,
                    ..Config::default()
                });
                visuals.update(&frame(1.0, height - 100.0), Duration::from_secs_f64(1.0));
                let body = prepared_body(&visuals, 1, 100.0, 100.0);
                if height == 0.0 {
                    assert!(body.is_none());
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
                assert!(visuals.data.borrow().spectrum.rectangles.is_empty());
            }
        }
    }

    #[test]
    fn gpu_spectrum_outline_caps_dense_fractional_instances() {
        for width in [3.125, 7.25, 1200.375, 4096.125] {
            let mut visuals = configured_visuals();
            visuals.configure_config(&Config {
                spectrum_style: SpectrumStyle::Outline,
                spectrum_bar_layout: SpectrumBarLayout::Fixed { count: u32::MAX },
                spectrum_gap: 10.0,
                spectrum_interpolate: true,
                ..Config::default()
            });
            visuals.update(&frame(1.0, 0.0), Duration::from_secs_f64(1.0));
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
            visuals.configure_config(&Config {
                spectrum_style: SpectrumStyle::Bars,
                spectrum_bar_layout: SpectrumBarLayout::Fixed { count: u32::MAX },
                spectrum_gap: 10.0,
                spectrum_interpolate: true,
                ..Config::default()
            });
            visuals.update(&frame(1.0, 0.0), Duration::from_secs_f64(1.0));
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
    fn spectrum_suspension_releases_working_data_but_retains_final_output() {
        let mut visuals = configured_visuals();
        visuals.configure_config(&Config {
            spectrum_style: SpectrumStyle::Bars,
            ..Config::default()
        });
        visuals.update(&frame(1.0, -30.0), Duration::from_secs_f64(1.0));
        visuals.retain_spectrum_panels(&[]);
        visuals.set_spectrum_visible(false);
        visuals.update(&frame(1.1, -60.0), Duration::from_secs_f64(1.1));
        assert!(visuals.data.borrow().spectrum_render_range().is_none());
        visuals.set_spectrum_visible(true);
        assert!(visuals.data.borrow().spectrum_render_range().is_none());
        visuals.update(&frame(1.2, -60.0), Duration::from_secs_f64(1.2));
        let restored = rectangle_frame(prepared_body(&visuals, 1, 100.0, 100.0));
        let gpui::GpuContent::Rectangles { rectangles, .. } = restored.content() else {
            unreachable!()
        };
        assert!((rectangles[0].bounds[3] - 1.0 / 7.0).abs() < 1e-6);
        let plot = Bounds::new(point(px(0.0), px(0.0)), size(px(100.0), px(100.0)));
        visuals
            .data
            .borrow_mut()
            .prepare_spectrum_presentation(1, plot)
            .unwrap();
        let cached = Arc::downgrade(
            visuals.data.borrow().spectrum.presentations[&1]
                .body
                .as_ref()
                .unwrap(),
        );
        visuals.set_enabled(false);
        assert!(cached.upgrade().is_some());
        assert!(visuals.data.borrow().spectrum.mappings.is_empty());
        assert_eq!(visuals.data.borrow().spectrum.rectangles.capacity(), 0);
        visuals.clear_presentation();
        assert!(cached.upgrade().is_none());
    }

    #[test]
    fn gpu_spectrum_palette_failure_propagates_without_path_fallback() {
        let mut visuals = configured_visuals();
        visuals.configure_config(&Config {
            spectrum_style: SpectrumStyle::Bars,
            ..Config::default()
        });
        visuals.update(&frame(1.0, -30.0), Duration::from_secs_f64(1.0));
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
    }

    #[test]
    fn small_canvases_skip_plots_and_cramped_spectra_omit_labels() {
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

    fn level_range(levels: &[f32]) -> Option<(f32, f32)> {
        let (&first, rest) = levels.split_first()?;
        Some(rest.iter().fold((first, first), |(low, high), value| {
            (low.min(*value), high.max(*value))
        }))
    }

    fn frame(time: f64, level: f32) -> AnalysisFrame {
        AnalysisFrame {
            sequence: Duration::from_secs_f64(time).as_nanos() as u64,
            sample_rate: 48_000,
            sample_time: Duration::from_secs_f64(time),
            frequencies_hz: vec![20.0, 200.0, 2000.0, 20_000.0],
            spectrum_db: vec![level; 4],
            settings: Some(rivu_audio::analysis::AnalysisSettings::from(
                &Config::default(),
            )),
            visualizer: rivu_audio::analysis::VisualizerFrame {
                demand: rivu_audio::analysis::VisualizerDemand {
                    spectrogram: true,
                    ..Default::default()
                },
                spectrogram: vec![level; Config::default().analysis_fft_size as usize / 2].into(),
                ..Default::default()
            },
            generation: 1,
            ..AnalysisFrame::default()
        }
    }

    #[test]
    fn invalidated_source_hides_spectrum_and_restarts_envelopes_on_fresh_audio() {
        let mut visuals = configured_visuals();
        let mut audio = frame(1.0, -10.0);
        visuals.update(&audio, audio.sample_time);
        assert!(visuals.data.borrow().spectrum_render_range().is_some());

        audio.settings = None;
        audio.generation += 1;
        visuals.update(&audio, audio.sample_time);
        assert!(visuals.data.borrow().spectrum_render_range().is_none());

        let mut fresh = frame(1.1, -60.0);
        fresh.generation = audio.generation;
        visuals.update(&fresh, fresh.sample_time);
        let data = visuals.data.borrow();
        assert!(data.spectrum_render_range().is_some());
        assert_eq!(data.spectrum.peaks, vec![-60.0; 4]);
        assert_eq!(data.spectrum.bar_levels, vec![-60.0; 4]);
        assert_eq!(data.spectrum.smoothed_levels, vec![-60.0; 4]);
    }

    #[test]
    fn playing_signal_invalidation_releases_final_bodies_and_peaks_without_a_source_change() {
        let mut visuals = configured_visuals();
        let mut audio = frame(1.0, -10.0);
        visuals.update(&audio, audio.sample_time);
        visuals.update(&frame(1.1, -20.0), Duration::from_millis(1100));
        publish_analysis_views(&visuals, 1);
        let (body, tile) = {
            let data = visuals.data.borrow();
            assert!(!data.spectrum.presentations[&1].peaks.is_empty());
            (
                Arc::downgrade(data.spectrum.presentations[&1].body.as_ref().unwrap()),
                Arc::downgrade(&data.spectrogram.presentations[&1].layers[0].1),
            )
        };
        // Worker continuity loss can invalidate settings while the GUI's
        // playback/source/seek/config identities remain unchanged.
        audio.settings = None;
        visuals.update(&audio, audio.sample_time);
        publish_analysis_views(&visuals, 1);
        assert!(body.upgrade().is_none());
        assert!(tile.upgrade().is_some());
        {
            let data = visuals.data.borrow();
            assert!(data.spectrum.presentations.is_empty());
            assert!(!data.spectrogram.presentations.is_empty());
        }
        let fresh = frame(1.2, -60.0);
        visuals.update(&fresh, fresh.sample_time);
        publish_analysis_views(&visuals, 1);
        let data = visuals.data.borrow();
        assert!(data.spectrum.presentations[&1].body.is_some());
        assert_eq!(data.spectrum.peaks, vec![-60.0; 4]);
        assert_eq!(data.spectrogram.len, 3);
    }

    #[test]
    fn same_rate_seek_starts_new_bucket_and_resets_hold_and_smoothing() {
        let mut visuals = configured_visuals();
        visuals.update(&frame(1.02, -10.0), Duration::from_secs_f64(1.02));
        let mut next = frame(1.03, -60.0);
        next.generation = 2;
        visuals.update(&next, next.sample_time);
        let data = visuals.data.borrow();
        assert_eq!(data.spectrogram.len, 2);
        assert_eq!(
            level_range(&data.spectrogram.columns[0].levels),
            Some((-10.0, -10.0))
        );
        assert_eq!(
            level_range(&data.spectrogram.columns[1].levels),
            Some((-60.0, -60.0))
        );
        assert_eq!(data.spectrogram.columns[1].interval_start, next.sample_time);
        assert_eq!(data.spectrum.peaks, vec![-60.0; 4]);
        assert_eq!(data.spectrum.bar_levels, vec![-60.0; 4]);
        assert_eq!(data.spectrum.smoothed_levels, vec![-60.0; 4]);
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
            visuals.update(&frame(1.0, -10.0), Duration::from_secs_f64(1.0));
            visuals.configure_config(&config);
            visuals.update(&frame(2.0, -20.0), Duration::from_secs_f64(2.0));
            assert!(visuals.data.borrow().latest_levels.is_empty());
            assert_eq!(visuals.data.borrow().spectrogram.len, 0);
            let mut fresh = frame(2.1, -60.0);
            fresh.settings = Some(rivu_audio::analysis::AnalysisSettings::from(&config));
            fresh.generation = 2;
            visuals.update(&fresh, fresh.sample_time);
            assert_eq!(visuals.data.borrow().spectrum.peaks, vec![-60.0; 4]);
            let before = visuals.data.borrow().latest_sample_time;
            visuals.update(&frame(2.2, -5.0), Duration::from_secs_f64(2.2));
            assert_eq!(visuals.data.borrow().latest_sample_time, before);
        }
    }

    #[test]
    fn signal_switch_clears_history_and_rejects_stale_frames_but_eq_preserves_history() {
        let mut visuals = configured_visuals();
        visuals.update(&frame(1.0, -10.0), Duration::from_secs_f64(1.0));
        visuals.update(&frame(1.1, -20.0), Duration::from_secs_f64(1.1));
        assert_eq!(visuals.data.borrow().spectrogram.len, 2);

        let mut config = Config {
            visualization_signal: VisualizationSignal::Before,
            ..Config::default()
        };
        assert!(visuals.configure_config(&config));
        {
            let data = visuals.data.borrow();
            assert_eq!(data.spectrogram.len, 0);
            assert!(data.latest_sample_time.is_none());
            assert!(data.latest_levels.is_empty());
            assert!(data.spectrum.peaks.is_empty());
            assert!(data.spectrum.bar_levels.is_empty());
            assert!(data.spectrum.smoothed_levels.is_empty());
            assert!(data.spectrum.mappings.is_empty());
        }
        visuals.update(&frame(1.2, -5.0), Duration::from_secs_f64(1.2));
        assert_eq!(visuals.data.borrow().spectrogram.len, 0);
        assert!(visuals.data.borrow().latest_levels.is_empty());

        // Reusing a sequence is valid after switching the selected signal.
        let mut before = frame(1.1, -40.0);
        before.settings = Some(rivu_audio::analysis::AnalysisSettings::from(&config));
        visuals.update(&before, before.sample_time);
        assert_eq!(visuals.data.borrow().spectrogram.len, 1);
        assert_eq!(visuals.data.borrow().latest_levels, vec![-40.0; 4]);

        config.equalizer.enabled = true;
        config.equalizer.preamp_db = -6.0;
        visuals.configure_config(&config);
        {
            let data = visuals.data.borrow();
            assert_eq!(data.spectrogram.len, 1);
            assert_eq!(data.latest_levels, vec![-40.0; 4]);
            assert_eq!(data.latest_sample_time, Some(before.sample_time));
        }
        before.sequence += 1;
        before.sample_time = Duration::from_secs_f64(1.2);
        before.spectrum_db.fill(-50.0);
        before.visualizer.spectrogram =
            vec![-50.0; Config::default().analysis_fft_size as usize / 2].into();
        visuals.update(&before, before.sample_time);
        let data = visuals.data.borrow();
        assert_eq!(data.spectrogram.len, 2);
        assert_eq!(
            level_range(&data.spectrogram.columns[0].levels),
            Some((-40.0, -40.0))
        );
        assert_eq!(
            level_range(&data.spectrogram.columns[1].levels),
            Some((-50.0, -50.0))
        );
    }

    #[test]
    fn history_retains_requested_audio_duration_at_different_rates() {
        for fps in [5, 60] {
            let mut visuals = FrequencyViews::new();
            visuals.retain_spectrogram_panels(&[1]);
            visuals.configure_config(&Config {
                analysis_rate_hz: fps,
                spectrogram_history_seconds: 60,
                ..Config::default()
            });
            for step in 1..=240 * fps {
                visuals.update(
                    &frame(step as f64 / fps as f64, -30.0),
                    Duration::from_secs_f64(step as f64 / fps as f64),
                );
            }
            let data = visuals.data.borrow();
            assert_eq!(data.spectrogram.len, history_column_count(60, fps));
            let oldest = data
                .spectrogram
                .columns
                .iter()
                .filter_map(|column| column.sample_time)
                .min()
                .unwrap();
            let newest = data.latest_sample_time.unwrap();
            assert!(newest - oldest >= Duration::from_secs(59));
            assert!(newest - oldest <= Duration::from_secs(60));
        }
    }

    fn configured_visuals() -> FrequencyViews {
        let mut visuals = FrequencyViews::new();
        visuals.retain_spectrogram_panels(&[1]);
        visuals.configure_config(&Config::default());
        visuals
    }

    #[test]
    fn spectrogram_history_requires_visible_demand_and_restarts_without_hidden_frames() {
        let mut visuals = FrequencyViews::new();
        let mut config = Config::default();
        visuals.configure_config(&config);
        visuals.update(&frame(1.0, -10.0), Duration::from_secs_f64(1.0));
        assert_eq!(visuals.data.borrow().spectrogram.columns.capacity(), 0);
        assert_eq!(visuals.data.borrow().spectrum.peaks, vec![-10.0; 4]);

        visuals.retain_spectrogram_panels(&[1, 2]);
        visuals.configure_config(&config);
        visuals.update(&frame(1.1, -20.0), Duration::from_secs_f64(1.1));
        visuals.retain_spectrogram_panels(&[2]);
        assert_eq!(visuals.data.borrow().spectrogram.len, 1);
        visuals.update(&frame(1.2, -30.0), Duration::from_secs_f64(1.2));
        assert_eq!(visuals.data.borrow().spectrogram.len, 2);
        {
            let mut data = visuals.data.borrow_mut();
            data.spectrogram.tiles.resize_with(3, || None);
            data.spectrogram.tile_values.resize(1024, 1.0);
        }

        visuals.retain_spectrogram_panels(&[]);
        {
            config.analysis_rate_hz += 1;
            visuals.configure_config(&config);
            visuals.update(&frame(2.0, -40.0), Duration::from_secs_f64(2.0));
            let data = visuals.data.borrow();
            assert_eq!(data.spectrogram.columns.capacity(), 0);
            assert_eq!(data.spectrogram.heat_samples.capacity(), 0);
            assert_eq!(data.spectrogram.tiles.capacity(), 0);
            assert_eq!(data.spectrogram.tile_values.capacity(), 0);
            assert_eq!(data.spectrogram.history_columns, 0);
            assert_eq!(data.spectrogram.len, 0);
            assert_eq!(data.latest_levels, vec![-40.0; 4]);
        }

        visuals.retain_spectrogram_panels(&[3]);
        visuals.configure_config(&config);
        // A settings refresh can replay the current analysis publication.
        // It must not resurrect the last hidden frame as new history.
        visuals.update(&frame(2.0, -40.0), Duration::from_secs_f64(2.0));
        assert_eq!(visuals.data.borrow().spectrogram.len, 0);
        visuals.update(&frame(3.0, -50.0), Duration::from_secs_f64(3.0));
        let data = visuals.data.borrow();
        assert_eq!(data.spectrogram.len, 1);
        let newest = &data.spectrogram.columns[data.spectrogram.latest.unwrap()];
        assert_eq!(newest.sample_time, Some(Duration::from_secs(3)));
        assert_eq!(level_range(&newest.levels), Some((-50.0, -50.0)));
    }

    #[test]
    fn hidden_spectrum_preserves_history_and_resumes_with_latest_levels() {
        let mut visuals = configured_visuals();
        visuals.update(&frame(1.0, -10.0), Duration::from_secs_f64(1.0));
        visuals.set_spectrum_visible(false);
        visuals.update(&frame(1.1, -60.0), Duration::from_secs_f64(1.1));
        {
            let data = visuals.data.borrow();
            assert_eq!(data.spectrum.peaks.capacity(), 0);
            assert_eq!(data.spectrum.bar_levels.capacity(), 0);
            assert_eq!(data.spectrum.smoothed_levels.capacity(), 0);
            assert_eq!(data.latest_levels, vec![-60.0; 4]);
            assert_eq!(data.spectrogram.len, 2);
        }
        visuals.set_spectrum_visible(true);
        visuals.update(&frame(1.1, -60.0), Duration::from_secs_f64(1.1));
        let data = visuals.data.borrow();
        assert_eq!(data.spectrum.peaks, vec![-60.0; 4]);
        assert_eq!(data.spectrum.bar_levels, vec![-60.0; 4]);
        assert_eq!(data.spectrum.smoothed_levels, vec![-60.0; 4]);
        assert_eq!(data.spectrogram.len, 2);
    }

    #[test]
    fn hidden_frequency_shape_change_withholds_rendering_until_real_envelopes_return() {
        for count in [2, 8] {
            let mut visuals = configured_visuals();
            visuals.update(&frame(1.0, -10.0), Duration::from_secs_f64(1.0));
            visuals.set_spectrum_visible(false);
            let mut changed = frame(1.1, -60.0);
            changed.frequencies_hz = (0..count)
                .map(|index| 20.0 * 1000.0_f32.powf(index as f32 / (count - 1) as f32))
                .collect();
            changed.spectrum_db = vec![-60.0; count];
            visuals.update(&changed, changed.sample_time);
            assert!(visuals.set_spectrum_visible(true));
            {
                let data = visuals.data.borrow();
                assert_eq!(data.latest_levels, changed.spectrum_db);
                assert_eq!(data.spectrogram.len, 2);
                assert_eq!(
                    level_range(&data.spectrogram.columns[0].levels),
                    Some((-10.0, -10.0))
                );
                assert_eq!(
                    level_range(&data.spectrogram.columns[1].levels),
                    Some((-60.0, -60.0))
                );
                // This is the paint consumer's boundary, before pooled levels,
                // interpolated levels, or peaks may index an envelope.
                assert!(data.spectrum_render_range().is_none());
            }
            changed.sample_time = Duration::from_secs_f64(1.2);
            visuals.update(&changed, changed.sample_time);
            let data = visuals.data.borrow();
            let (first, end, low, high) = data.spectrum_render_range().unwrap();
            for index in first..end {
                assert_eq!(data.display_level(index), -60.0);
                assert_eq!(data.spectrum.peaks[index], -60.0);
            }
            let sample = heat_sample(&data.frequencies, high, low, high);
            assert_eq!(data.interpolate_level(sample, false), -60.0);
            assert_eq!(data.interpolate_level(sample, true), -60.0);
            assert_eq!(data.spectrum.bar_levels.len(), count);
            assert_eq!(data.spectrum.smoothed_levels.len(), count);
        }
    }

    #[test]
    fn differently_sized_spectrum_panels_keep_independent_bands_and_release_them() {
        let mut visuals = configured_visuals();
        visuals.update(&frame(1.0, -30.0), Duration::from_secs_f64(1.0));
        {
            let mut data = visuals.data.borrow_mut();
            let (_, _, low, high) = data.spectrum_render_range().unwrap();
            let narrow = spectrum_bar_count(
                120.0,
                false,
                SpectrumBarLayout::Automatic { width: 8.0 },
                2.0,
            );
            let wide = spectrum_bar_count(
                800.0,
                false,
                SpectrumBarLayout::Automatic { width: 8.0 },
                2.0,
            );
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
        assert_eq!(visuals.data.borrow().spectrum.mappings.len(), 1);
    }

    #[test]
    fn disabled_views_drop_history_and_rebuild_only_after_enabling() {
        for config in [
            Config::default(),
            Config {
                analysis_rate_hz: 5,
                spectrogram_history_seconds: 60,
                ..Config::default()
            },
        ] {
            let mut visuals = configured_visuals();
            visuals.update(&frame(1.0, -10.0), Duration::from_secs_f64(1.0));
            visuals.set_enabled(false);
            visuals.configure_config(&config);
            visuals.update(&frame(2.0, -20.0), Duration::from_secs_f64(2.0));
            visuals.finish_prepaint();
            {
                let data = visuals.data.borrow();
                assert_eq!(data.spectrogram.columns.capacity(), 0);
                assert_eq!(data.spectrogram.history_columns, 0);
                assert_eq!(data.spectrogram.len, 0);
                assert!(data.latest_sample_time.is_none());
                assert!(!data.spectrum.envelope_initialized);
                assert_eq!(data.latest_levels.capacity(), 0);
                assert_eq!(data.spectrum.peaks.capacity(), 0);
            }
            visuals.set_enabled(true);
            assert!(visuals.configure_config(&config));
            let mut fresh = frame(3.0, -60.0);
            fresh.generation = 2;
            visuals.update(&fresh, fresh.sample_time);
            let data = visuals.data.borrow();
            assert_eq!(data.spectrogram.len, 1);
            assert_eq!(data.latest_sample_time, Some(fresh.sample_time));
            assert_eq!(data.spectrum.peaks, vec![-60.0; 4]);
            assert_eq!(data.spectrum.bar_levels, vec![-60.0; 4]);
            assert_eq!(data.spectrum.smoothed_levels, vec![-60.0; 4]);
        }
    }

    #[test]
    fn spectrogram_publication_sampling_respects_panel_history_and_budget() {
        for (height, columns, budget, expected) in [
            (320, 400, 64 * 1024 * 1024, 320),
            (700, 400, 64 * 1024 * 1024, 700),
            (3000, 400, 64 * 1024 * 1024, 1024),
            (3000, 3600, 64 * 1024 * 1024, 580),
            (3000, 3600, 16 * 1024 * 1024, 143),
            (0, 400, 64 * 1024 * 1024, 1),
        ] {
            let mut visuals = configured_visuals();
            {
                let mut data = visuals.data.borrow_mut();
                data.material_budget_bytes = Some(budget);
                data.resize_history(columns);
                data.spectrogram.panel_height = height;
            }
            visuals.update(&frame(1.0, -20.0), Duration::from_secs(1));
            visuals.update(&frame(1.1, -20.0), Duration::from_millis(1100));
            let mut data = visuals.data.borrow_mut();
            data.prepare_spectrogram_presentation(1).unwrap();
            assert_eq!(data.spectrogram.sample_height, expected);
            assert_eq!(data.spectrogram.heat_samples.len(), expected);
        }
    }

    fn retention_plot() -> Bounds<Pixels> {
        Bounds::new(point(px(0.0), px(0.0)), size(px(200.0), px(100.0)))
    }

    fn publish_analysis_views(visuals: &FrequencyViews, panel_id: u64) {
        let mut data = visuals.data.borrow_mut();
        data.prepare_spectrum_presentation(panel_id, retention_plot())
            .unwrap();
        data.prepare_spectrogram_presentation(panel_id).unwrap();
    }

    #[test]
    fn hidden_views_keep_only_final_output_until_fresh_drawable_publication() {
        let mut visuals = configured_visuals();
        let config = Config::default();
        visuals.configure_config(&config);
        visuals.update(&frame(1.0, -10.0), Duration::from_secs_f64(1.0));
        visuals.update(&frame(1.1, -20.0), Duration::from_secs_f64(1.1));
        publish_analysis_views(&visuals, 1);
        let (body, tile, peaks, placement) = {
            let data = visuals.data.borrow();
            let spectrum = &data.spectrum.presentations[&1];
            let spectrogram = &data.spectrogram.presentations[&1];
            assert!(!spectrum.peaks.is_empty());
            assert!(spectrum.peaks.len() <= MAX_SPECTRUM_BARS);
            assert!(
                spectrogram.layers.len()
                    <= data
                        .spectrogram
                        .history_columns
                        .div_ceil(SPECTROGRAM_TILE_COLUMNS)
            );
            (
                Arc::downgrade(spectrum.body.as_ref().unwrap()),
                Arc::downgrade(&spectrogram.layers[0].1),
                spectrum.peaks.clone(),
                spectrogram.layers[0].0,
            )
        };
        visuals.retain_spectrum_panels(&[]);
        visuals.set_spectrum_visible(false);
        visuals.retain_spectrogram_panels(&[]);
        visuals.set_enabled(false);
        visuals.set_enabled(false);
        visuals.update(&frame(2.0, -60.0), Duration::from_secs(2));
        assert!(!visuals.animate(Duration::from_secs(2)));
        publish_analysis_views(&visuals, 1);
        {
            let data = visuals.data.borrow();
            assert_eq!(data.spectrogram.columns.capacity(), 0);
            assert_eq!(data.spectrogram.heat_samples.capacity(), 0);
            assert_eq!(data.spectrogram.tiles.capacity(), 0);
            assert_eq!(data.spectrogram.tile_values.capacity(), 0);
            assert_eq!(data.spectrogram.draws.capacity(), 0);
            assert_eq!(data.latest_levels.capacity(), 0);
            assert_eq!(data.frequencies.capacity(), 0);
            assert_eq!(data.spectrum.peak_deadlines.capacity(), 0);
            assert_eq!(data.spectrum.bar_deadlines.capacity(), 0);
            assert_eq!(data.spectrum.points.capacity(), 0);
            assert!(data.spectrum.mappings.is_empty());
            assert_eq!(data.spectrum.presentations[&1].peaks, peaks);
            assert_eq!(data.spectrogram.presentations[&1].layers[0].0, placement);
        }
        visuals.set_enabled(true);
        visuals.set_spectrum_visible(true);
        visuals.retain_spectrogram_panels(&[1]);
        assert!(visuals.configure_config(&config));
        publish_analysis_views(&visuals, 1);
        assert!(body.upgrade().is_some());
        assert!(tile.upgrade().is_some());
        visuals.update(&frame(3.0, -60.0), Duration::from_secs(3));
        publish_analysis_views(&visuals, 1);
        assert!(body.upgrade().is_none());
        visuals.update(&frame(3.1, -50.0), Duration::from_secs_f64(3.1));
        publish_analysis_views(&visuals, 1);
        assert!(tile.upgrade().is_none());
        let data = visuals.data.borrow();
        assert_eq!(data.spectrogram.len, 2);
        assert_eq!(
            data.spectrogram.columns[data.spectrogram.latest.unwrap()].sample_time,
            Some(Duration::from_secs_f64(3.1))
        );
    }

    #[test]
    fn inactive_tab_publications_survive_other_tabs_and_removed_panels_are_released() {
        let mut visuals = configured_visuals();
        visuals.update(&frame(1.0, -10.0), Duration::from_secs(1));
        visuals.update(&frame(1.1, -20.0), Duration::from_secs_f64(1.1));
        publish_analysis_views(&visuals, 1);
        publish_analysis_views(&visuals, 2);
        let (body, tile) = {
            let data = visuals.data.borrow();
            (
                Arc::downgrade(data.spectrum.presentations[&1].body.as_ref().unwrap()),
                Arc::downgrade(&data.spectrogram.presentations[&1].layers[0].1),
            )
        };
        visuals.retain_spectrum_panels(&[2]);
        visuals.retain_spectrogram_panels(&[2]);
        visuals.retain_presentation_panels(&[1, 2], &[1, 2]);
        assert_eq!(visuals.data.borrow().spectrum.presentations.len(), 2);
        assert_eq!(visuals.data.borrow().spectrogram.presentations.len(), 2);
        visuals.set_enabled(false);
        visuals.retain_presentation_panels(&[2], &[2]);
        assert!(body.upgrade().is_none());
        // Shared immutable Spectrogram tiles remain referenced by panel 2.
        assert!(tile.upgrade().is_some());
        assert!(
            !visuals
                .data
                .borrow()
                .spectrogram
                .presentations
                .contains_key(&1)
        );
        visuals.retain_presentation_panels(&[], &[]);
        assert!(tile.upgrade().is_none());
    }

    #[test]
    fn hidden_semantic_invalidation_clears_both_publications_without_restarting_work() {
        for explicit in [false, true] {
            let mut visuals = configured_visuals();
            visuals.update(&frame(1.0, -10.0), Duration::from_secs(1));
            visuals.update(&frame(1.1, -20.0), Duration::from_secs_f64(1.1));
            publish_analysis_views(&visuals, 1);
            visuals.set_enabled(false);
            if explicit {
                visuals.clear_presentation();
                visuals.clear_presentation();
            } else {
                visuals.configure_config(&Config {
                    analysis_fft_size: 4096,
                    ..Config::default()
                });
            }
            let data = visuals.data.borrow();
            assert!(!data.enabled);
            assert!(data.spectrum.presentations.is_empty());
            assert!(data.spectrogram.presentations.is_empty());
            assert_eq!(data.spectrogram.columns.capacity(), 0);
            assert_eq!(data.latest_levels.capacity(), 0);
        }
    }

    #[test]
    fn buckets_preserve_transients_without_contaminating_latest_spectrum() {
        let mut visuals = FrequencyViews::new();
        visuals.retain_spectrogram_panels(&[1]);
        visuals.configure_config(&Config {
            spectrogram_history_seconds: 60,
            spectrum_peaks: false,
            ..Config::default()
        });
        visuals.update(&frame(1.1, 12.0), Duration::from_secs_f64(1.1));
        assert_eq!(visuals.data.borrow().spectrum.top_db, 12.0);
        visuals.update(&frame(1.2, -50.0), Duration::from_secs_f64(1.2));
        let data = visuals.data.borrow();
        assert_eq!(data.spectrogram.len, 2);
        assert!(
            data.spectrogram
                .columns
                .iter()
                .any(|column| level_range(&column.levels) == Some((12.0, 12.0)))
        );
        assert_eq!(data.latest_levels, vec![-50.0; 4]);
        assert_eq!(data.spectrum.top_db, 0.0);
    }

    #[test]
    fn history_preserves_gaps_and_stream_changes() {
        let mut visuals = configured_visuals();
        visuals.update(&frame(1.0, -20.0), Duration::from_secs_f64(1.0));
        visuals.update(&frame(6.0, -30.0), Duration::from_secs_f64(6.0));
        {
            let data = visuals.data.borrow();
            assert_eq!(data.spectrogram.len, 2);
            let last = &data.spectrogram.columns[data.spectrogram.latest.unwrap()];
            assert_eq!(last.interval_start, Duration::from_secs(6));
            assert!(
                data.spectrogram
                    .columns
                    .iter()
                    .any(|column| column.sample_time == Some(Duration::from_secs(1)))
            );
        }
        let mut changed_rate = frame(6.5, -40.0);
        changed_rate.sample_rate = 44_100;
        visuals.update(&changed_rate, changed_rate.sample_time);
        assert_eq!(visuals.data.borrow().spectrogram.len, 3);
    }

    #[test]
    fn range_changes_keep_shared_frequency_axis_and_history() {
        let mut visuals = configured_visuals();
        let mut config = Config::default();
        visuals.update(&frame(1.0, -30.0), Duration::from_secs_f64(1.0));
        config.spectrum_db_range = Some(40.0);
        visuals.configure_config(&config);
        {
            let data = visuals.data.borrow();
            assert_eq!(data.spectrum_range().unwrap().2, 20.0);
            assert_eq!(data.spectrogram.db_range, 70.0);
        }
        config.spectrogram_db_range = Some(100.0);
        visuals.configure_config(&config);
        let data = visuals.data.borrow();
        assert_eq!(
            level_range(&data.spectrogram.columns[0].levels),
            Some((-30.0, -30.0))
        );
        assert_eq!(data.spectrogram.len, 1);
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
                let mut visuals = FrequencyViews::new();
                visuals.retain_spectrogram_panels(&[1]);
                visuals.configure_config(&config);
                visuals.update(&frame(1.0, -30.0), Duration::from_secs_f64(1.0));

                config.visualization_db_range = 100.0;
                visuals.configure_config(&config);
                let data = visuals.data.borrow();
                assert_eq!(data.spectrum.db_range, spectrum_override.unwrap_or(100.0));
                assert_eq!(
                    data.spectrogram.db_range,
                    spectrogram_override.unwrap_or(100.0)
                );
                assert_eq!(data.spectrogram.len, 1);
                assert_eq!(
                    level_range(&data.spectrogram.columns[0].levels),
                    Some((-30.0, -30.0))
                );
                assert_eq!(data.latest_levels, vec![-30.0; 4]);
                assert_eq!(data.latest_sample_time, Some(Duration::from_secs(1)));
            }
        }
    }

    #[test]
    fn radial_range_override_does_not_invalidate_spectrum_or_spectrogram() {
        let mut visuals = configured_visuals();
        visuals.update(&frame(1.0, -30.0), Duration::from_secs_f64(1.0));
        visuals.configure_config(&Config {
            reactive_db_range: Some(40.0),
            ..Config::default()
        });
        let data = visuals.data.borrow();
        assert_eq!(data.latest_levels, vec![-30.0; 4]);
        assert_eq!(
            level_range(&data.spectrogram.columns[0].levels),
            Some((-30.0, -30.0))
        );
    }

    #[test]
    fn per_panel_spectrogram_labels_change_geometry_without_resetting_history() {
        let mut visuals = configured_visuals();
        visuals.update(&frame(1.0, -30.0), Duration::from_secs_f64(1.0));
        let mut data = visuals.data.borrow_mut();
        let available = size(px(416.0), px(112.0));
        assert!(data.prepaint_panel_with_labels(1, FrequencyKind::Spectrogram, available, true));
        let labelled_width = data.spectrogram.panel_sizes[&1].0;
        assert!(data.prepaint_panel_with_labels(2, FrequencyKind::Spectrogram, available, false));
        assert!(data.spectrogram.panel_sizes[&2].0 > labelled_width);
        assert_eq!(data.spectrogram.panel_sizes[&1].0, labelled_width);
        assert_eq!(data.spectrogram.len, 1);
        assert_eq!(
            level_range(&data.spectrogram.columns[0].levels),
            Some((-30.0, -30.0))
        );
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
    fn spectrogram_display_rows_follow_physical_height_and_hard_cap() {
        assert_eq!(spectrogram_source_height(320), 320);
        assert_eq!(spectrogram_source_height(700), 700);
        assert_eq!(spectrogram_source_height(1400), 1024);
        assert_eq!(spectrogram_source_height(0), 1);
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
            visuals.update(
                &frame(time as f64, -30.0),
                Duration::from_secs_f64(time as f64),
            );
        }
        let history_times = || {
            visuals
                .data
                .borrow()
                .spectrogram
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
                .spectrogram
                .panel_sizes
                .extend(sizes);
            visuals.finish_prepaint();
            assert_eq!(visuals.data.borrow().spectrogram.history_columns, 400);
            assert_eq!(visuals.data.borrow().spectrogram.panel_height, 700);
            assert_eq!(history_times(), original);
        }
        // Reusing cached prepaint leaves both panel measurements authoritative.
        visuals.finish_prepaint();
        assert_eq!(visuals.data.borrow().spectrogram.history_columns, 400);
        assert_eq!(history_times(), original);
    }

    #[test]
    fn cached_spectrogram_sizes_change_only_on_measurement_or_panel_retirement() {
        let mut visuals = configured_visuals();
        visuals.retain_spectrogram_panels(&[1, 2]);
        visuals
            .data
            .borrow_mut()
            .spectrogram
            .panel_sizes
            .extend([(1, (400, 100)), (2, (3, 700))]);
        assert!(visuals.finish_prepaint());
        assert!(
            !visuals.finish_prepaint(),
            "cached prepaint must not invalidate siblings"
        );
        visuals
            .data
            .borrow_mut()
            .spectrogram
            .panel_sizes
            .insert(1, (200, 100));
        assert!(
            visuals.finish_prepaint(),
            "shared width changes invalidate sibling scenes"
        );
        assert_eq!(visuals.data.borrow().spectrogram.history_columns, 200);
        visuals.retain_spectrogram_panels(&[1]);
        assert!(visuals.finish_prepaint());
        assert_eq!(visuals.data.borrow().spectrogram.panel_height, 100);
        assert!(!visuals.finish_prepaint());
        visuals.retain_spectrogram_panels(&[]);
        assert!(visuals.data.borrow().spectrogram.panel_sizes.is_empty());
        assert_eq!(visuals.data.borrow().spectrogram.history_columns, 0);
    }

    #[test]
    fn small_spectrogram_prepaint_preserves_cached_sibling_demand() {
        for (width, height) in [(64.0, 712.0), (416.0, 40.0)] {
            for shrinking_panel in [1, 2] {
                let mut visuals = configured_visuals();
                visuals.retain_spectrogram_panels(&[1, 2]);
                {
                    let mut data = visuals.data.borrow_mut();
                    assert!(data.prepaint_panel_with_labels(
                        1,
                        FrequencyKind::Spectrogram,
                        size(px(416.0), px(112.0)),
                        false,
                    ));
                    assert!(data.prepaint_panel_with_labels(
                        2,
                        FrequencyKind::Spectrogram,
                        size(px(216.0), px(712.0)),
                        false,
                    ));
                }
                assert!(visuals.finish_prepaint());
                for time in 1..=5 {
                    visuals.update(
                        &frame(time as f64, -30.0),
                        Duration::from_secs_f64(time as f64),
                    );
                }
                assert!(!visuals.data.borrow_mut().prepaint_panel_with_labels(
                    shrinking_panel,
                    FrequencyKind::Spectrogram,
                    size(px(width), px(height)),
                    false,
                ));
                assert!(visuals.finish_prepaint());
                let expected = if shrinking_panel == 1 {
                    (2, (200, 700))
                } else {
                    (1, (400, 100))
                };
                {
                    let data = visuals.data.borrow();
                    assert_eq!(data.spectrogram.panel_sizes.len(), 1);
                    assert_eq!(data.spectrogram.panel_sizes[&expected.0], expected.1);
                    assert_eq!(data.spectrogram.history_columns, expected.1.0);
                    assert_eq!(data.spectrogram.panel_height, expected.1.1);
                    assert_eq!(data.spectrogram.len, 5);
                    assert_eq!(
                        data.spectrogram.columns[0].sample_time,
                        Some(Duration::from_secs(1))
                    );
                    assert_eq!(
                        data.spectrogram.columns[4].sample_time,
                        Some(Duration::from_secs(5))
                    );
                }
                // An active small panel and a sibling reusing cached prepaint
                // must not resurrect the retired size or erase the sibling.
                visuals.retain_spectrogram_panels(&[1, 2]);
                assert!(!visuals.finish_prepaint());
                assert_eq!(
                    visuals.data.borrow().spectrogram.history_columns,
                    expected.1.0
                );
                assert_eq!(visuals.data.borrow().spectrogram.panel_height, expected.1.1);
                let restored = if shrinking_panel == 1 {
                    size(px(416.0), px(112.0))
                } else {
                    size(px(216.0), px(712.0))
                };
                assert!(visuals.data.borrow_mut().prepaint_panel_with_labels(
                    shrinking_panel,
                    FrequencyKind::Spectrogram,
                    restored,
                    false,
                ));
                assert!(visuals.finish_prepaint());
                let data = visuals.data.borrow();
                assert_eq!(data.spectrogram.history_columns, 400);
                assert_eq!(data.spectrogram.panel_height, 700);
                assert_eq!(data.spectrogram.len, 5);
            }
        }
    }

    #[test]
    fn all_small_spectrogram_prepaint_keeps_minimum_history_and_restores_demand() {
        let mut visuals = FrequencyViews::new();
        let config = Config::default();
        visuals.retain_spectrogram_panels(&[1, 2]);
        visuals.configure_config(&config);
        for panel_id in [1, 2] {
            assert!(visuals.data.borrow_mut().prepaint_panel_with_labels(
                panel_id,
                FrequencyKind::Spectrogram,
                size(px(416.0), px(712.0)),
                false,
            ));
        }
        assert!(visuals.finish_prepaint());
        for time in 1..=5 {
            visuals.update(
                &frame(time as f64, -30.0),
                Duration::from_secs_f64(time as f64),
            );
        }
        for (panel_id, available) in [
            (1, size(px(64.0), px(712.0))),
            (2, size(px(416.0), px(40.0))),
        ] {
            assert!(!visuals.data.borrow_mut().prepaint_panel_with_labels(
                panel_id,
                FrequencyKind::Spectrogram,
                available,
                false,
            ));
        }
        assert!(visuals.finish_prepaint());
        let columns = {
            let data = visuals.data.borrow();
            assert!(data.spectrogram.panel_sizes.is_empty());
            assert_eq!(data.spectrogram.history_columns, 2);
            assert_eq!(data.spectrogram.panel_height, 0);
            assert_eq!(data.spectrogram.len, 2);
            assert_eq!(
                data.spectrogram
                    .columns
                    .iter()
                    .filter_map(|column| column.sample_time)
                    .min(),
                Some(Duration::from_secs(4))
            );
            assert_eq!(
                data.spectrogram
                    .columns
                    .iter()
                    .filter_map(|column| column.sample_time)
                    .max(),
                Some(Duration::from_secs(5))
            );
            data.spectrogram.columns.as_ptr()
        };
        for _ in 0..3 {
            visuals.retain_spectrogram_panels(&[1, 2]);
            assert!(!visuals.configure_config(&config));
            assert!(!visuals.data.borrow_mut().prepaint_panel_with_labels(
                1,
                FrequencyKind::Spectrogram,
                size(px(64.0), px(712.0)),
                false,
            ));
            assert!(!visuals.finish_prepaint());
            let data = visuals.data.borrow();
            assert_eq!(data.spectrogram.columns.as_ptr(), columns);
            assert_eq!(data.spectrogram.history_columns, 2);
            assert_eq!(data.spectrogram.panel_height, 0);
            assert_eq!(data.spectrogram.len, 2);
        }
        assert!(visuals.data.borrow_mut().prepaint_panel_with_labels(
            1,
            FrequencyKind::Spectrogram,
            size(px(416.0), px(112.0)),
            false,
        ));
        assert!(visuals.finish_prepaint());
        assert!(!visuals.finish_prepaint());
        visuals.update(&frame(6.0, -20.0), Duration::from_secs_f64(6.0));
        let data = visuals.data.borrow();
        assert_eq!(data.spectrogram.panel_sizes.len(), 1);
        assert_eq!(data.spectrogram.panel_sizes[&1], (400, 100));
        assert_eq!(data.spectrogram.history_columns, 400);
        assert_eq!(data.spectrogram.panel_height, 100);
        assert_eq!(data.spectrogram.len, 3);
        assert_eq!(
            data.spectrogram
                .columns
                .iter()
                .filter_map(|column| column.sample_time)
                .min(),
            Some(Duration::from_secs(4))
        );
        assert_eq!(
            data.spectrogram
                .columns
                .iter()
                .filter_map(|column| column.sample_time)
                .max(),
            Some(Duration::from_secs(6))
        );
    }

    #[test]
    fn paused_history_window_changes_reproject_without_advancing_audio() {
        for change in ["config", "resize", "retire"] {
            let mut visuals = configured_visuals();
            visuals.retain_spectrogram_panels(&[1, 2]);
            visuals
                .data
                .borrow_mut()
                .spectrogram
                .panel_sizes
                .extend([(1, (400, 100)), (2, (200, 100))]);
            visuals.finish_prepaint();
            for tick in 0..=800 {
                let time = tick as f64 / 20.0;
                visuals.update(&frame(time, -30.0), Duration::from_secs_f64(time));
            }
            let now = Duration::from_secs(40);
            let retained_start = Duration::from_secs(30);
            assert_eq!(spectrogram::time_position(retained_start, now, 20.0), 0.5);
            match change {
                "config" => {
                    visuals.configure_config(&Config {
                        spectrogram_history_seconds: 10,
                        ..Config::default()
                    });
                }
                "resize" => {
                    visuals
                        .data
                        .borrow_mut()
                        .spectrogram
                        .panel_sizes
                        .insert(1, (200, 100));
                    visuals.finish_prepaint();
                }
                "retire" => {
                    visuals.retain_spectrogram_panels(&[2]);
                    visuals.finish_prepaint();
                }
                _ => unreachable!(),
            }
            assert!(!visuals.animate(now));
            let data = visuals.data.borrow();
            assert_eq!(data.display_time, Some(now));
            assert_eq!(data.latest_sample_time, Some(now));
            let expected = if change == "config" { 0.0 } else { 0.5 };
            assert_eq!(
                spectrogram::time_position(
                    retained_start,
                    now,
                    data.spectrogram.history_limit_seconds
                ),
                expected,
                "{change}"
            );
        }
    }

    #[test]
    fn shrinking_history_keeps_the_newest_columns() {
        let mut visuals = configured_visuals();
        for time in 1..=5 {
            visuals.update(
                &frame(time as f64, -30.0),
                Duration::from_secs_f64(time as f64),
            );
        }
        let mut data = visuals.data.borrow_mut();
        data.resize_history(3);
        assert_eq!(data.spectrogram.len, 3);
        assert_eq!(
            data.spectrogram
                .columns
                .iter()
                .filter_map(|column| column.sample_time)
                .min(),
            Some(Duration::from_secs(3))
        );
        assert_eq!(
            data.spectrogram
                .columns
                .iter()
                .filter_map(|column| column.sample_time)
                .max(),
            Some(Duration::from_secs(5))
        );
    }

    #[test]
    fn spectrogram_tile_sampling_preserves_rows_columns_and_missing_values() {
        let samples = sampling::frequency_rows(&[100.0, 400.0], 100.0, 400.0, 3, false);
        let mut values = [0.0; 6];
        sample_spectrogram_column(&[-80.0, -20.0], &[0, 1], &samples, false, &mut values, 2, 0);
        sample_spectrogram_column(&[-10.0, 0.0], &[0, 1], &samples, false, &mut values, 2, 1);
        assert_eq!(values, [-80.0, -10.0, -80.0, -10.0, -20.0, 0.0]);
        sample_spectrogram_column(&[], &[0, 1], &samples, false, &mut values, 2, 0);
        assert_eq!(
            values,
            [
                f32::NEG_INFINITY,
                -10.0,
                f32::NEG_INFINITY,
                -10.0,
                f32::NEG_INFINITY,
                0.0
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
        let automatic = SpectrumBarLayout::Automatic { width: 3.0 };
        let fixed = SpectrumBarLayout::Fixed { count: 512 };
        assert_eq!(spectrum_bar_count(1200.0, false, automatic, 1.0), 300);
        assert_eq!(spectrum_bar_count(4096.0, false, automatic, 1.0), 1024);
        assert_eq!(spectrum_bar_count(1200.0, false, fixed, 8.0), 512);
        assert_eq!(spectrum_bar_count(7.0, false, fixed, 1.0), 7);
        assert_eq!(spectrum_bar_count(0.0, false, automatic, 1.0), 1);
        assert_eq!(spectrum_bar_count(1200.0, true, fixed, 1.0), 1200);
        assert_eq!(spectrum_bar_count(1200.0, true, automatic, 1.0), 1200);
    }

    #[test]
    fn interpolation_fills_gaps_without_reducing_source_transients() {
        let mut visuals = FrequencyViews::new();
        let mut transient = frame(1.0, -60.0);
        transient.spectrum_db[1] = 0.0;
        visuals.update(&transient, transient.sample_time);
        let data = visuals.data.borrow();
        let sample = heat_sample(
            &data.frequencies,
            (200.0f32 * 2000.0).sqrt(),
            20.0,
            20_000.0,
        );
        let middle = data.interpolate_level(sample, false);
        assert!((middle + 30.0).abs() < 0.001);
        assert_eq!(data.display_level(1).max(middle), 0.0);
        let points = [point(px(0.0), px(40.0)), point(px(10.0), px(0.0))];
        assert_eq!(shape_y(&points, px(2.5), true), px(30.0));
        assert_eq!(shape_y(&points, px(2.5), false), px(40.0));
        assert_eq!(shape_y(&points, px(10.0), true), px(0.0));
    }

    #[test]
    fn spectrogram_keeps_two_fft_peaks_inside_one_musical_band() {
        let config = Config {
            analysis_fft_size: 32768,
            spectrogram_interpolate: false,
            ..Config::default()
        };
        let mut visuals = configured_visuals();
        visuals.configure_config(&config);
        visuals.data.borrow_mut().spectrogram.panel_height = 1024;
        let mut input = frame(1.0, -80.0);
        input.settings = Some(rivu_audio::analysis::AnalysisSettings::from(&config));
        let mut bins = vec![-140.0; 16384];
        bins[679] = -6.0;
        bins[693] = -12.0;
        input.visualizer.spectrogram = bins.into();
        visuals.update(&input, input.sample_time);
        let mut data = visuals.data.borrow_mut();
        data.prepare_spectrogram_presentation(1).unwrap();
        let values = &data.spectrogram.tile_values;
        assert_eq!(values.iter().filter(|level| **level == -6.0).count(), 1);
        assert_eq!(values.iter().filter(|level| **level == -12.0).count(), 1);
        let first = values.iter().position(|level| *level == -12.0).unwrap();
        let second = values.iter().position(|level| *level == -6.0).unwrap();
        assert!(first + 1 < second);
        assert!(
            values[first + 1..second]
                .iter()
                .all(|level| *level == -140.0)
        );
        assert_eq!(data.latest_levels, vec![-80.0; 4]);
    }

    #[test]
    fn compact_history_reprojects_old_peaks_and_accepts_new_audio_after_row_budget_changes() {
        let config = Config {
            analysis_fft_size: 8192,
            spectrogram_interpolate: false,
            ..Config::default()
        };
        let mut visuals = configured_visuals();
        visuals.configure_config(&config);
        let mut input = frame(1.0, -80.0);
        input.settings = Some(rivu_audio::analysis::AnalysisSettings::from(&config));
        let bins: Vec<_> = (0..4096)
            .map(|i| -140.0 + ((i * 7919) % 14000) as f32 / 100.0)
            .collect();
        input.visualizer.spectrogram = bins.clone().into();
        visuals.update(&input, input.sample_time);
        let axis: Vec<_> = (1..=4096).map(|i| i as f32 * 48000.0 / 8192.0).collect();
        let initial_rows = sampling::frequency_rows(&axis, 16.0, 24000.0, 1024, false);
        let mut logical: Vec<_> = initial_rows
            .iter()
            .map(|r| sampling::frequency_level(&bins, r, false))
            .collect();
        let mut old_axis = sampling::logarithmic_axis(16.0, 24000.0, 1024);
        let mut data = visuals.data.borrow_mut();
        for (columns, rows) in [(3600, 580), (300, 1024)] {
            let remap = sampling::frequency_rows(&old_axis, 16.0, 24000.0, rows, false);
            logical = remap
                .iter()
                .map(|r| sampling::frequency_level(&logical, r, false))
                .collect();
            data.resize_history(columns);
            let retained = &data.spectrogram.columns[0].levels;
            let expanded: Vec<_> = data
                .spectrogram
                .history_indices
                .iter()
                .map(|&i| retained[i as usize])
                .collect();
            assert_eq!(expanded, logical);
            old_axis = sampling::logarithmic_axis(16.0, 24000.0, rows);
        }
        // A later frame must use the rebuilt compact FFT plan, not its old
        // storage layout or the historical reprojection values.
        data.push_fft_history(Duration::from_millis(1100), &bins);
        let latest = &data.spectrogram.columns[data.spectrogram.latest.unwrap()].levels;
        let expected: Vec<_> = initial_rows
            .iter()
            .map(|r| sampling::frequency_level(&bins, r, false))
            .collect();
        let expanded: Vec<_> = data
            .spectrogram
            .history_indices
            .iter()
            .map(|&i| latest[i as usize])
            .collect();
        assert_eq!(expanded, expected);
        drop(data);

        let remap = sampling::frequency_rows(&old_axis, 16.0, 22050.0, 1024, false);
        let expected: Vec<_> = remap
            .iter()
            .map(|row| sampling::frequency_level(&logical, row, false))
            .collect();
        input.sequence += 1;
        input.sample_time = Duration::from_millis(1200);
        input.sample_rate = 44100;
        visuals.update(&input, input.sample_time);
        let data = visuals.data.borrow();
        let retained = &data.spectrogram.columns[0].levels;
        let expanded: Vec<_> = data
            .spectrogram
            .history_indices
            .iter()
            .map(|&i| retained[i as usize])
            .collect();
        assert_eq!(expanded, expected);
    }

    #[test]
    fn startup_and_physical_resize_keep_the_same_audio_time_scale() {
        let mut visuals = configured_visuals();
        visuals.update(&frame(0.1, -30.0), Duration::from_millis(100));
        visuals.update(&frame(0.2, -30.0), Duration::from_millis(200));
        let mut data = visuals.data.borrow_mut();
        data.prepare_spectrogram_presentation(1).unwrap();
        let before = data.spectrogram.presentations[&1].layers[0].0;
        assert!((before.left() - 0.99).abs() < 1e-6);
        assert!((before.size.width - 0.01).abs() < 1e-6);
        data.spectrogram.scale_factor = 2.0;
        assert!(data.prepaint_panel_with_labels(
            1,
            FrequencyKind::Spectrogram,
            size(px(416.0), px(112.0)),
            false
        ));
        assert_eq!(data.spectrogram.panel_sizes[&1], (800, 200));
        data.finish_prepaint();
        data.prepare_spectrogram_presentation(1).unwrap();
        let after = data.spectrogram.presentations[&1].layers[0].0;
        assert!((after.left() - before.left()).abs() < 1e-6);
        assert!((after.size.width - before.size.width).abs() < 1e-6);
        assert_eq!(data.latest_sample_time, Some(Duration::from_millis(200)));
    }

    #[test]
    fn missing_fft_snapshot_never_falls_back_to_musical_bands_or_bridges_a_gap() {
        let mut visuals = configured_visuals();
        visuals.data.borrow_mut().resize_history(2);
        visuals.update(&frame(1.0, -10.0), Duration::from_secs(1));
        let mut missing = frame(1.1, 0.0);
        missing.visualizer.spectrogram = Default::default();
        visuals.update(&missing, missing.sample_time);
        assert_eq!(visuals.data.borrow().spectrogram.len, 1);
        visuals.update(&frame(1.2, -50.0), Duration::from_millis(1200));
        let data = visuals.data.borrow();
        assert_eq!(data.spectrogram.len, 2);
        let latest = &data.spectrogram.columns[data.spectrogram.latest.unwrap()];
        assert_eq!(latest.interval_start, Duration::from_millis(1200));
        assert_eq!(level_range(&latest.levels), Some((-50.0, -50.0)));
    }

    #[test]
    fn narrowing_history_coalesces_dense_buckets_without_losing_early_transients() {
        let mut visuals = configured_visuals();
        for tick in 0..=400 {
            let time = tick as f64 / 20.0;
            let level = if tick == 20 { -3.0 } else { -80.0 };
            visuals.update(&frame(time, level), Duration::from_secs_f64(time));
        }
        let mut data = visuals.data.borrow_mut();
        data.resize_history(3);
        assert_eq!(data.spectrogram.len, 3);
        assert_eq!(
            data.spectrogram
                .columns
                .iter()
                .map(|column| column.interval_start)
                .min(),
            Some(Duration::ZERO)
        );
        assert_eq!(
            data.spectrogram
                .columns
                .iter()
                .filter_map(|column| column.sample_time)
                .max(),
            Some(Duration::from_secs(20))
        );
        assert!(
            data.spectrogram
                .columns
                .iter()
                .any(|column| level_range(&column.levels) == Some((-3.0, -3.0)))
        );
        assert_eq!(data.spectrogram.history_limit_seconds, 20.0);
    }
}
