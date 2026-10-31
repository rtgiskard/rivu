//! Bounded spectrogram history, sampling budgets, tiled publications, and plotting.
use super::{
    FrequencyState, HeatSample, axis_label_margin, build_heat_samples, clipped_range, finite_level,
    plot_bounds, sample_heat_level_at,
};
use crate::{
    VisualParameters, labels::paint_aligned_label, palette::PALETTE_LUT_SIZE,
    spectrogram_flow::SpectrogramFlow,
};
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
    pub(super) heat_samples: Vec<HeatSample>,
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
    pub(super) startup_expansion_speed: f32,
    pub(super) outflow_threshold_percent: u32,
    pub(super) flow: SpectrogramFlow,
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
            startup_expansion_speed: config.spectrogram_startup_expansion_speed,
            outflow_threshold_percent: config.spectrogram_outflow_threshold_percent,
            flow: SpectrogramFlow::default(),
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
const SPECTROGRAM_FREQUENCY_ROW_SCALE: f32 = 0.5;
const SPECTROGRAM_MAX_FREQUENCY_ROWS: usize = 1024;

pub(super) fn spectrogram_source_height(panel_height: usize, source_rows: usize) -> usize {
    let pixel_rows = ((panel_height as f32) * SPECTROGRAM_FREQUENCY_ROW_SCALE).ceil() as usize;
    pixel_rows
        .max(source_rows.max(1))
        .min(SPECTROGRAM_MAX_FREQUENCY_ROWS)
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

impl FrequencyState {
    pub(super) fn advance_spectrogram(&mut self, now: Duration) {
        if !self.spectrogram.visible {
            return;
        }
        let Some(latest) = self.spectrogram.latest else {
            return;
        };
        self.spectrogram.flow.advance(
            now,
            self.spectrogram.columns[latest].interval_start,
            self.spectrogram.history_limit_seconds,
            self.spectrogram.startup_expansion_speed,
            self.spectrogram.outflow_threshold_percent,
        );
    }

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

    pub(super) fn push_history(&mut self, time: Duration, levels: &[f32]) {
        if self.spectrogram.history_columns < 2 {
            return;
        }
        let discontinuity = self.spectrogram.latest.is_some_and(|index| {
            self.spectrogram.columns[index].generation != self.stream_generation
                || self.spectrogram.columns[index].source_generation != self.source_generation
        }) || self
            .latest_sample_time
            .is_some_and(|previous| time.saturating_sub(previous) > HISTORY_GAP_RESET);
        let width = Duration::from_secs_f64(
            self.spectrogram.history_limit_seconds / (self.spectrogram.history_columns - 1) as f64,
        );
        let bucket =
            Duration::from_nanos(((time.as_nanos() / width.as_nanos()) * width.as_nanos()) as u64);
        let merge = self.spectrogram.latest.filter(|&index| {
            self.spectrogram.columns[index].generation == self.stream_generation
                && self.spectrogram.columns[index].source_generation == self.source_generation
                && self.spectrogram.columns[index].bucket_start == Some(bucket)
        });
        let index = merge.unwrap_or(self.spectrogram.head);
        let mut changed = merge.is_none();
        if merge.is_some() {
            for (old, new) in self.spectrogram.columns[index]
                .levels
                .iter_mut()
                .zip(levels)
            {
                let pooled = old.max(finite_level(*new));
                changed |= pooled != *old;
                *old = pooled;
            }
        } else {
            self.spectrogram.columns[index].levels.clear();
            self.spectrogram.columns[index]
                .levels
                .extend_from_slice(levels);
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
        let limit = (new_columns as f64 / self.analysis_rate_hz.max(1) as f64)
            .min(self.spectrogram.history_seconds as f64);
        if self.spectrogram.history_limit_seconds != limit {
            self.spectrogram.history_limit_seconds = limit;
            self.spectrogram.flow.remap(limit);
        }
        if new_columns == self.spectrogram.history_columns {
            return;
        }
        let mut old_columns = std::mem::take(&mut self.spectrogram.columns);
        let old_capacity = old_columns.len();
        let old_len = self.spectrogram.len.min(old_capacity);
        let old_head = self.spectrogram.head.min(old_capacity);
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
        self.spectrogram.columns = columns;
        self.spectrogram.tiles.clear();
        self.spectrogram.history_columns = new_columns;
        self.spectrogram.len = keep;
        self.spectrogram.head = keep % new_columns;
        self.spectrogram.latest = keep.checked_sub(1);
        if keep == 0 {
            self.latest_sample_time = None;
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
        self.spectrogram.flow = SpectrogramFlow::default();
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
        self.spectrogram.tiles = Vec::new();
        self.spectrogram.tile_values = Vec::new();
        self.spectrogram.draws = Vec::new();
    }

    pub(super) fn visible_range(&self) -> Option<(usize, usize, f32, f32)> {
        clipped_range(
            &self.frequencies,
            self.sample_rate,
            16.0,
            self.sample_rate as f32 * 0.5,
        )
    }

    pub(super) fn prepare_spectrogram_presentation(&mut self, panel_id: u64) -> Result<(), String> {
        if !self.enabled || !self.spectrogram.visible || self.spectrogram.len == 0 {
            return Ok(());
        }
        let Some(latest_time) = self.display_time else {
            return Ok(());
        };
        let Some((first, last, low, high)) = self.visible_range() else {
            return Ok(());
        };
        let mapping = self.spectrogram.flow.mapping();
        if mapping.scale() <= 0.0 {
            return Ok(());
        }
        let start_time = mapping.visible_start(latest_time);
        let source_rows = last.saturating_sub(first) + 1;
        let height = spectrogram_source_height(self.spectrogram.panel_height, source_rows).min(
            spectrogram_budget_rows(
                self.spectrogram.history_columns,
                self.material_budget_bytes
                    .expect("configured visualization budget"),
            ),
        );
        if self.spectrogram.sample_height != height
            || self.spectrogram.heat_sample_generation != self.stream_generation
        {
            self.invalidate_spectrogram();
            self.spectrogram.sample_height = height;
            self.spectrogram.heat_samples =
                build_heat_samples(&self.frequencies, low, high, height);
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
            let left = mapping.position(tile.start, latest_time);
            let width =
                (tile.end.saturating_sub(tile.start).as_secs_f64() * mapping.scale()) as f32;
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
