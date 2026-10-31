//! Bounded spectrogram history, sampling budgets, tiled publications, and plotting.
use super::sampling::{
    FrequencyRow, compact_frequency_rows, frequency_level, frequency_rows, logarithmic_axis,
    mapped_frequency_level,
};
use super::{FrequencyState, axis_label_margin, finite_level, plot_bounds};
use crate::{VisualParameters, labels::paint_aligned_label, palette::PALETTE_LUT_SIZE};
use gpui::{
    App, Bounds, GpuFieldParams, GpuFrame, GpuLimits, GpuSource, GpuValueMapping, Pixels,
    SharedString, Window, point, px, size,
};
use std::{collections::HashMap, sync::Arc, time::Duration};

pub(super) struct Spectrogram {
    pub(super) visible: bool,
    pub(super) panel_sizes: HashMap<u64, (usize, usize)>,
    pub(super) panel_height: usize,
    pub(super) frequency_labels: [SharedString; 2],
    pub(super) heat_samples: Vec<FrequencyRow>,
    fft_frequencies: Vec<f32>,
    source_axis: (u32, u32),
    frequencies: Vec<f32>,
    fft_rows: Vec<FrequencyRow>,
    pub(super) history_indices: Vec<u16>,
    row_values: Vec<f32>,
    pub(super) scale_factor: f32,
    pub(super) heat_sample_generation: u64,
    pub(super) sample_height: usize,
    pub(super) tiles: Vec<Option<SpectrogramTile>>,
    pub(super) tile_values: Vec<f32>,
    pub(super) columns: Vec<Column>,
    pub(super) presentations: HashMap<u64, SpectrogramPresentation>,
    pub(super) draws: Vec<(Bounds<f32>, Arc<GpuFrame>)>,
    pub(super) db_range: f32,
    pub(super) interpolate: bool,
    pub(super) history_seconds: u32,
    pub(super) history_limit_seconds: f64,
    pub(super) history_columns: usize,
    pub(super) head: usize,
    pub(super) len: usize,
    pub(super) latest: Option<usize>,
}

impl Spectrogram {
    pub(super) fn new(config: &VisualParameters) -> Self {
        Self {
            visible: false,
            panel_sizes: HashMap::new(),
            panel_height: 0,
            frequency_labels: ["8 Hz".into(), "11 kHz".into()],
            heat_samples: Vec::new(),
            fft_frequencies: Vec::new(),
            source_axis: (0, 0),
            frequencies: Vec::new(),
            fft_rows: Vec::new(),
            history_indices: Vec::new(),
            row_values: Vec::new(),
            scale_factor: 1.0,
            heat_sample_generation: 0,
            sample_height: 0,
            tiles: Vec::new(),
            tile_values: Vec::new(),
            columns: Vec::new(),
            presentations: HashMap::new(),
            draws: Vec::new(),
            db_range: config.spectrogram_db_range(),
            interpolate: config.spectrogram_interpolate,
            history_seconds: config.spectrogram_history_seconds,
            history_limit_seconds: config.spectrogram_history_seconds as f64,
            history_columns: 0,
            head: 0,
            len: 0,
            latest: None,
        }
    }
}

pub(super) const HISTORY_GAP_RESET: Duration = Duration::from_secs(1);

pub(super) const SPECTROGRAM_TILE_COLUMNS: usize = 32;
pub(super) type SpectrogramTileKey = [Option<(u64, Duration, Duration)>; SPECTROGRAM_TILE_COLUMNS];
pub(super) struct SpectrogramTile {
    pub(super) key: SpectrogramTileKey,
    pub(super) start: Duration,
    pub(super) end: Duration,
    pub(super) publication: Arc<GpuFrame>,
}

#[derive(Default)]
pub(super) struct Column {
    pub(super) levels: Vec<f32>,
    pub(super) sample_time: Option<Duration>,
    pub(super) bucket_start: Option<Duration>,
    pub(super) interval_start: Duration,
    pub(super) revision: u64,
    pub(super) generation: u64,
    pub(super) source_generation: Option<u64>,
}

pub(super) struct SpectrogramPresentation {
    pub(super) layers: Vec<(Bounds<f32>, Arc<GpuFrame>)>,
    pub(super) labels: [SharedString; 2],
}

pub(super) fn history_column_count(history_seconds: u32, analysis_rate_hz: u32) -> usize {
    usize::try_from(history_seconds)
        .unwrap_or(usize::MAX)
        .saturating_mul(analysis_rate_hz as usize)
        .max(2)
}
pub(super) fn history_columns_for_width(
    history_seconds: u32,
    analysis_rate_hz: u32,
    width: usize,
) -> usize {
    history_column_count(history_seconds, analysis_rate_hz).min(width.max(2))
}
const SPECTROGRAM_MAX_FREQUENCY_ROWS: usize = 1024;

pub(super) fn spectrogram_source_height(panel_height: usize) -> usize {
    panel_height.clamp(1, SPECTROGRAM_MAX_FREQUENCY_ROWS)
}

pub(super) fn time_position(time: Duration, latest: Duration, history_seconds: f64) -> f32 {
    (1.0 + (time.as_secs_f64() - latest.as_secs_f64()) / history_seconds) as f32
}

// Half the shared budget remains available for other content and placements.
// The other half accommodates two complete history generations, each charged
// for both its immutable buffer and initialization copy. This is a sampling
// target, not a reservation or a guarantee for arbitrary pending updates.
pub(super) fn spectrogram_budget_rows(columns: usize, budget_bytes: u64) -> usize {
    let per_column = budget_bytes / 8 / columns.max(1) as u64;
    usize::try_from(per_column.saturating_sub(8) / 4)
        .unwrap_or(usize::MAX)
        .max(1)
}

pub(super) fn sample_spectrogram_column(
    levels: &[f32],
    indices: &[u16],
    samples: &[FrequencyRow],
    interpolate: bool,
    values: &mut [f32],
    stride: usize,
    column: usize,
) {
    for (row, sample) in samples.iter().enumerate() {
        values[row * stride + column] =
            mapped_frequency_level(levels, sample, indices, interpolate);
    }
}

impl FrequencyState {
    pub(super) fn finish_prepaint(&mut self) -> bool {
        if !self.enabled || !self.spectrogram.visible {
            return false;
        }
        // No drawable panels still retain the minimum history while their
        // active panels await more room. Do not reuse a retired measurement or
        // repeatedly clear/reallocate history on unchanged small frames.
        let width = self
            .spectrogram
            .panel_sizes
            .values()
            .map(|size| size.0)
            .max()
            .unwrap_or(2);
        let previous = (
            self.spectrogram.history_columns,
            self.spectrogram.panel_height,
        );
        self.spectrogram.panel_height = self
            .spectrogram
            .panel_sizes
            .values()
            .map(|size| size.1)
            .max()
            .unwrap_or(0);
        let columns = history_columns_for_width(
            self.spectrogram.history_seconds,
            self.analysis_rate_hz,
            width,
        );
        self.resize_history(columns);
        previous
            != (
                self.spectrogram.history_columns,
                self.spectrogram.panel_height,
            )
    }

    fn prepare_history_sampling(&mut self) {
        let rows = SPECTROGRAM_MAX_FREQUENCY_ROWS.min(spectrogram_budget_rows(
            self.spectrogram.history_columns,
            self.material_budget_bytes
                .expect("configured visualization budget"),
        ));
        if self.sample_rate == 0 || self.analysis_fft_size == 0 {
            return;
        }
        let source_axis = (self.sample_rate, self.analysis_fft_size);
        if self.spectrogram.frequencies.len() == rows && self.spectrogram.source_axis == source_axis
        {
            return;
        }
        let step = self.sample_rate as f32 / self.analysis_fft_size as f32;
        let low = 16.0_f32.max(step);
        let high = self.sample_rate as f32 * 0.5;
        if low >= high {
            return;
        }
        if self.spectrogram.source_axis != source_axis {
            self.spectrogram.fft_frequencies.clear();
            self.spectrogram.source_axis = source_axis;
            self.spectrogram
                .fft_frequencies
                .extend((1..=self.analysis_fft_size / 2).map(|bin| bin as f32 * step));
        }
        let new_axis = logarithmic_axis(low, high, rows);
        let previous = (!self.spectrogram.frequencies.is_empty())
            .then(|| frequency_rows(&self.spectrogram.frequencies, low, high, rows, false));
        let (fft_rows, indices) = compact_frequency_rows(
            frequency_rows(&self.spectrogram.fft_frequencies, low, high, rows, false),
            previous.as_deref(),
        );
        if let Some(sampling) = previous {
            for column in &mut self.spectrogram.columns {
                if column.levels.is_empty() {
                    continue;
                }
                self.spectrogram.row_values.clear();
                for (row, &index) in sampling.iter().zip(&indices) {
                    if index as usize == self.spectrogram.row_values.len() {
                        self.spectrogram.row_values.push(mapped_frequency_level(
                            &column.levels,
                            row,
                            &self.spectrogram.history_indices,
                            false,
                        ));
                    }
                }
                column.levels.clear();
                column
                    .levels
                    .extend_from_slice(&self.spectrogram.row_values);
                column.revision = column.revision.wrapping_add(1);
                column.generation = self.stream_generation;
            }
        }
        self.spectrogram.frequencies = new_axis;
        self.spectrogram.fft_rows = fft_rows;
        self.spectrogram.history_indices = indices;
        self.spectrogram.sample_height = 0;
        self.spectrogram.heat_samples.clear();
        self.invalidate_spectrogram();
        self.refresh_frequency_labels();
    }

    pub(super) fn push_fft_history(&mut self, time: Duration, levels: &[f32]) {
        if levels.len() != self.analysis_fft_size as usize / 2 {
            return;
        }
        self.prepare_history_sampling();
        if self.spectrogram.fft_rows.is_empty() || self.spectrogram.history_columns < 2 {
            return;
        }
        let discontinuity = self.spectrogram.latest.is_some_and(|index| {
            self.spectrogram.columns[index].generation != self.stream_generation
                || self.spectrogram.columns[index].source_generation != self.source_generation
                || self.spectrogram.columns[index].sample_time != self.latest_sample_time
        }) || self
            .latest_sample_time
            .is_some_and(|previous| time.saturating_sub(previous) > HISTORY_GAP_RESET);
        let width = Duration::from_secs_f64(
            self.spectrogram.history_limit_seconds / (self.spectrogram.history_columns - 1) as f64,
        );
        let bucket =
            Duration::from_nanos(((time.as_nanos() / width.as_nanos()) * width.as_nanos()) as u64);
        let merge = self.spectrogram.latest.filter(|&index| {
            !discontinuity
                && self.spectrogram.columns[index].generation == self.stream_generation
                && self.spectrogram.columns[index].source_generation == self.source_generation
                && self.spectrogram.columns[index].bucket_start == Some(bucket)
        });
        let index = merge.unwrap_or(self.spectrogram.head);
        let mut changed = merge.is_none();
        if merge.is_some() {
            for (old, row) in self.spectrogram.columns[index]
                .levels
                .iter_mut()
                .zip(&self.spectrogram.fft_rows)
            {
                let pooled = old.max(frequency_level(levels, row, false));
                changed |= pooled != *old;
                *old = pooled;
            }
        } else {
            self.spectrogram.columns[index].levels.clear();
            self.spectrogram.columns[index].levels.extend(
                self.spectrogram
                    .fft_rows
                    .iter()
                    .map(|row| frequency_level(levels, row, false)),
            );
            self.spectrogram.columns[index].bucket_start = Some(bucket);
            self.spectrogram.columns[index].interval_start = if discontinuity {
                time
            } else {
                self.latest_sample_time
                    .map(|previous| previous.max(time.saturating_sub(Duration::from_millis(250))))
                    .unwrap_or_else(|| time.saturating_sub(Duration::from_millis(250)))
            };
            self.spectrogram.head = (index + 1) % self.spectrogram.history_columns;
            self.spectrogram.len = (self.spectrogram.len + 1).min(self.spectrogram.history_columns);
        }
        self.spectrogram.columns[index].generation = self.stream_generation;
        self.spectrogram.columns[index].source_generation = self.source_generation;
        self.spectrogram.columns[index].sample_time = Some(time);
        if changed {
            self.spectrogram.columns[index].revision =
                self.spectrogram.columns[index].revision.wrapping_add(1);
        }
        self.spectrogram.latest = Some(index);
    }
    pub(super) fn resize_history(&mut self, requested_columns: usize) {
        let new_columns = requested_columns.max(2);
        let limit_changed =
            self.spectrogram.history_limit_seconds != self.spectrogram.history_seconds as f64;
        self.spectrogram.history_limit_seconds = self.spectrogram.history_seconds as f64;
        if new_columns == self.spectrogram.history_columns && !limit_changed {
            return;
        }
        let mut old_columns = std::mem::take(&mut self.spectrogram.columns);
        let old_capacity = old_columns.len();
        let old_len = self.spectrogram.len.min(old_capacity);
        let old_head = self.spectrogram.head.min(old_capacity);
        let oldest = if old_capacity == 0 {
            0
        } else {
            (old_head + old_capacity - old_len) % old_capacity
        };
        let mut columns = (0..new_columns)
            .map(|_| Column::default())
            .collect::<Vec<_>>();
        let width = Duration::from_secs_f64(
            self.spectrogram.history_limit_seconds / (new_columns - 1) as f64,
        );
        let mut head = 0;
        let mut len = 0;
        let mut latest: Option<usize> = None;
        for offset in 0..old_len {
            let mut column = std::mem::take(&mut old_columns[(oldest + offset) % old_capacity]);
            let Some(time) = column.sample_time else {
                continue;
            };
            let bucket = Duration::from_nanos(
                ((time.as_nanos() / width.as_nanos()) * width.as_nanos()) as u64,
            );
            let merge = latest.filter(|&index| {
                let previous = &columns[index];
                previous.bucket_start == Some(bucket)
                    && previous.generation == column.generation
                    && previous.source_generation == column.source_generation
                    && previous
                        .sample_time
                        .is_some_and(|end| column.interval_start <= end)
            });
            let index = merge.unwrap_or(head);
            if merge.is_some() {
                let previous = &mut columns[index];
                for (old, new) in previous.levels.iter_mut().zip(&column.levels) {
                    *old = old.max(finite_level(*new));
                }
                previous.sample_time = Some(time);
                previous.revision = previous.revision.wrapping_add(1);
            } else {
                column.bucket_start = Some(bucket);
                columns[index] = column;
                head = (head + 1) % new_columns;
                len = (len + 1).min(new_columns);
            }
            latest = Some(index);
        }
        self.spectrogram.columns = columns;
        self.spectrogram.tiles.clear();
        self.spectrogram.history_columns = new_columns;
        self.spectrogram.len = len;
        self.spectrogram.head = head;
        self.spectrogram.latest = latest;
        if len == 0 {
            self.latest_sample_time = None;
        }
        if !self.spectrogram.fft_frequencies.is_empty() {
            self.prepare_history_sampling();
        }
    }
    pub(super) fn invalidate_spectrogram(&mut self) {
        self.spectrogram.tiles.clear();
    }

    pub(super) fn clear_history(&mut self) {
        self.invalidate_spectrogram();
        for column in &mut self.spectrogram.columns {
            column.levels.clear();
            column.sample_time = None;
            column.bucket_start = None;
        }
        self.spectrogram.head = 0;
        self.spectrogram.len = 0;
        self.spectrogram.latest = None;
        self.latest_sample_time = None;
        self.display_time = None;
        self.spectrogram.fft_frequencies.clear();
        self.spectrogram.source_axis = (0, 0);
        self.spectrogram.frequencies.clear();
        self.spectrogram.fft_rows.clear();
        self.spectrogram.history_indices.clear();
        self.spectrogram.row_values.clear();
        self.spectrogram.sample_height = 0;
        self.spectrogram.heat_samples.clear();
        self.spectrogram.heat_sample_generation = 0;
    }

    pub(super) fn release_history(&mut self) {
        self.clear_history();
        self.spectrogram.columns = Vec::new();
        self.spectrogram.history_columns = 0;
        self.spectrogram.panel_height = 0;
        self.spectrogram.heat_samples = Vec::new();
        self.spectrogram.fft_frequencies = Vec::new();
        self.spectrogram.frequencies = Vec::new();
        self.spectrogram.fft_rows = Vec::new();
        self.spectrogram.history_indices = Vec::new();
        self.spectrogram.row_values = Vec::new();
        self.spectrogram.tiles = Vec::new();
        self.spectrogram.tile_values = Vec::new();
        self.spectrogram.draws = Vec::new();
    }

    pub(super) fn visible_range(&self) -> Option<(usize, usize, f32, f32)> {
        let rows = self.spectrogram.frequencies.len();
        (rows > 0).then(|| {
            (
                0,
                rows - 1,
                16.0_f32.max(self.sample_rate as f32 / self.analysis_fft_size as f32),
                self.sample_rate as f32 * 0.5,
            )
        })
    }

    pub(super) fn prepare_spectrogram_presentation(&mut self, panel_id: u64) -> Result<(), String> {
        if !self.enabled || !self.spectrogram.visible || self.spectrogram.len == 0 {
            return Ok(());
        }
        let Some(latest_time) = self.display_time else {
            return Ok(());
        };
        let Some((_, _, low, high)) = self.visible_range() else {
            return Ok(());
        };
        let start_time = latest_time
            .saturating_sub(Duration::from_secs(self.spectrogram.history_seconds as u64));
        let height = spectrogram_source_height(self.spectrogram.panel_height)
            .min(self.spectrogram.frequencies.len());
        if self.spectrogram.sample_height != height
            || self.spectrogram.heat_sample_generation != self.stream_generation
        {
            self.invalidate_spectrogram();
            self.spectrogram.sample_height = height;
            self.spectrogram.heat_samples =
                frequency_rows(&self.spectrogram.frequencies, low, high, height, true);
            self.spectrogram.heat_sample_generation = self.stream_generation;
        }
        let tiles = self
            .spectrogram
            .history_columns
            .div_ceil(SPECTROGRAM_TILE_COLUMNS);
        self.spectrogram.tiles.resize_with(tiles, || None);
        self.spectrogram.draws.clear();
        for tile_index in 0..tiles {
            let first = tile_index * SPECTROGRAM_TILE_COLUMNS;
            let last = (first + SPECTROGRAM_TILE_COLUMNS).min(self.spectrogram.history_columns);
            let mut key: SpectrogramTileKey = [None; SPECTROGRAM_TILE_COLUMNS];
            let mut start = Duration::MAX;
            let mut end = Duration::ZERO;
            for index in first..last {
                let Some(time) = self.spectrogram.columns[index].sample_time else {
                    continue;
                };
                let column = &self.spectrogram.columns[index];
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
            if self.spectrogram.tiles[tile_index]
                .as_ref()
                .is_none_or(|tile| tile.key != key)
            {
                let count = key.iter().flatten().count();
                let span = end.saturating_sub(start).as_secs_f64().max(1e-9);
                let mut intervals = [[0.0; 2]; SPECTROGRAM_TILE_COLUMNS];
                self.spectrogram.tile_values.clear();
                self.spectrogram.tile_values.resize(count * height, 0.0);
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
                        &self.spectrogram.columns[first + local].levels,
                        &self.spectrogram.history_indices,
                        &self.spectrogram.heat_samples,
                        self.spectrogram.interpolate,
                        &mut self.spectrogram.tile_values,
                        count,
                        offset,
                    );
                    offset += 1;
                }
                let publication = GpuFrame::field_columns(
                    &intervals[..count],
                    height as u32,
                    &self.spectrogram.tile_values,
                    self.palette_lut.spectrogram_gpu_palette()?,
                    GpuFieldParams {
                        mapping: GpuValueMapping::Linear {
                            min: -self.spectrogram.db_range,
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
                self.spectrogram.tiles[tile_index] = Some(SpectrogramTile {
                    key,
                    start,
                    end,
                    publication,
                });
            }
            let tile = self.spectrogram.tiles[tile_index].as_ref().unwrap();
            if tile.end < start_time || tile.start > latest_time {
                continue;
            }
            let left = time_position(
                tile.start,
                latest_time,
                self.spectrogram.history_limit_seconds,
            );
            let width = (tile.end.saturating_sub(tile.start).as_secs_f64()
                / self.spectrogram.history_limit_seconds) as f32;
            if width <= 0.0 || left >= 1.0 || left + width <= 0.0 {
                continue;
            }
            self.spectrogram.draws.push((
                Bounds::new(point(left, 0.0), size(width, 1.0)),
                tile.publication.clone(),
            ));
        }
        // Do not retire a restored publication for a frame with no drawable span.
        if !self.spectrogram.draws.is_empty() {
            let presentation = self
                .spectrogram
                .presentations
                .entry(panel_id)
                .or_insert_with(|| SpectrogramPresentation {
                    layers: Vec::new(),
                    labels: self.spectrogram.frequency_labels.clone(),
                });
            std::mem::swap(&mut presentation.layers, &mut self.spectrogram.draws);
            presentation.labels = self.spectrogram.frequency_labels.clone();
            self.spectrogram.draws.clear();
        }
        Ok(())
    }

    pub(super) fn paint_spectrogram(
        &mut self,
        panel_id: u64,
        bounds: Bounds<Pixels>,
        options: rivu_format::config::AudioVisualizerOptions,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<(), String> {
        let labels = self
            .spectrogram
            .presentations
            .get(&panel_id)
            .map(|presentation| &presentation.labels)
            .unwrap_or(&self.spectrogram.frequency_labels);
        let left_margin = axis_label_margin(options.labels, labels);
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
        let prepared = self.prepare_spectrogram_presentation(panel_id);
        let Some(presentation) = self.spectrogram.presentations.get(&panel_id) else {
            return prepared;
        };
        window.with_content_mask(Some(gpui::ContentMask { bounds: plot }), |window| {
            for (bounds, publication) in &presentation.layers {
                window
                    .paint_gpu(
                        plot_bounds(plot, *bounds),
                        GpuSource::Fixed(publication.clone()),
                    )
                    .map_err(|error| format!("Cannot paint spectrogram tile: {error}"))?;
            }
            Ok::<(), String>(())
        })?;
        if options.labels {
            paint_aligned_label(
                presentation.labels[1].clone(),
                point(plot.left() - px(5.0), plot.top()),
                1.0,
                window,
                cx,
            );
            paint_aligned_label(
                presentation.labels[0].clone(),
                point(plot.left() - px(5.0), plot.bottom() - px(12.0)),
                1.0,
                window,
                cx,
            );
        }
        prepared
    }
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
