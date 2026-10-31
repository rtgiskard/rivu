//! Peak-preserving frequency resampling for bounded spectrogram history.
use super::{HeatSample, finite_level, heat_sample, sample_heat_level_at};
use std::{collections::HashMap, ops::Range};

pub(super) struct FrequencyRow {
    bins: Range<usize>,
    center: Option<HeatSample>,
}

impl FrequencyRow {
    // History uses nearest-neighbor fallback, not interpolation. An empty
    // interval taking bin N is equivalent to a singleton interval taking N.
    fn history_key(&self) -> (usize, usize, Option<usize>) {
        let nearest = self.center.map(|sample| {
            if sample.fraction <= 0.5 {
                sample.left
            } else {
                sample.right
            }
        });
        let bins = if self.bins.is_empty() {
            nearest.map_or(self.bins.clone(), |index| index..index + 1)
        } else {
            self.bins.clone()
        };
        (bins.start, bins.end, nearest)
    }
}

pub(super) fn compact_frequency_rows(
    rows: Vec<FrequencyRow>,
    previous: Option<&[FrequencyRow]>,
) -> (Vec<FrequencyRow>, Vec<u16>) {
    debug_assert!(rows.len() <= u16::MAX as usize);
    debug_assert!(previous.is_none_or(|previous| previous.len() == rows.len()));
    let mut keys = HashMap::with_capacity(rows.len());
    let mut unique = Vec::new();
    let mut indices = Vec::with_capacity(rows.len());
    for (index, row) in rows.into_iter().enumerate() {
        // A new FFT can alias rows whose retained historical values differ.
        // Require equivalent reprojection too before sharing their storage.
        let key = (
            row.history_key(),
            previous.map(|rows| rows[index].history_key()),
        );
        let next = unique.len() as u16;
        let stored = *keys.entry(key).or_insert(next);
        if stored == next {
            unique.push(row);
        }
        indices.push(stored);
    }
    (unique, indices)
}

pub(super) fn logarithmic_axis(low: f32, high: f32, rows: usize) -> Vec<f32> {
    let span = high.ln() - low.ln();
    (0..rows)
        .map(|row| (low.ln() + span * (row as f32 + 0.5) / rows as f32).exp())
        .collect()
}

pub(super) fn frequency_rows(
    frequencies: &[f32],
    low: f32,
    high: f32,
    rows: usize,
    descending: bool,
) -> Vec<FrequencyRow> {
    let span = high.ln() - low.ln();
    (0..rows)
        .map(|index| {
            let row = if descending { rows - 1 - index } else { index };
            let left = (low.ln() + span * row as f32 / rows as f32).exp();
            let right = (low.ln() + span * (row + 1) as f32 / rows as f32).exp();
            let first = if row == 0 {
                frequencies.partition_point(|hz| *hz < low)
            } else {
                frequencies.partition_point(|hz| *hz < left)
            };
            let end = if row + 1 == rows {
                frequencies.partition_point(|hz| *hz <= high)
            } else {
                frequencies.partition_point(|hz| *hz < right)
            };
            let center = (left * right).sqrt();
            let center = frequencies
                .first()
                .zip(frequencies.last())
                .and_then(|(first, last)| {
                    (*first <= center && center <= *last)
                        .then(|| heat_sample(frequencies, center, low, high))
                        .flatten()
                });
            FrequencyRow {
                bins: first..end,
                center,
            }
        })
        .collect()
}

pub(super) fn frequency_level(levels: &[f32], row: &FrequencyRow, interpolate: bool) -> f32 {
    let peak = levels
        .get(row.bins.clone())
        .unwrap_or(&[])
        .iter()
        .copied()
        .map(finite_level)
        .fold(f32::NEG_INFINITY, f32::max);
    // Interpolation may smooth between known samples, never erase a narrow peak.
    if interpolate || !peak.is_finite() {
        peak.max(row.center.map_or(f32::NEG_INFINITY, |center| {
            sample_heat_level_at(levels, center, interpolate)
        }))
    } else {
        peak
    }
}

pub(super) fn mapped_frequency_level(
    levels: &[f32],
    row: &FrequencyRow,
    indices: &[u16],
    interpolate: bool,
) -> f32 {
    let peak = indices
        .get(row.bins.clone())
        .unwrap_or(&[])
        .iter()
        .filter_map(|&index| levels.get(index as usize))
        .copied()
        .map(finite_level)
        .fold(f32::NEG_INFINITY, f32::max);
    if interpolate || !peak.is_finite() {
        let center = row.center.and_then(|sample| {
            let left = *indices.get(sample.left)? as usize;
            let right = *indices.get(sample.right)? as usize;
            (left.max(right) < levels.len()).then_some(HeatSample {
                left,
                right,
                fraction: sample.fraction,
            })
        });
        peak.max(center.map_or(f32::NEG_INFINITY, |sample| {
            sample_heat_level_at(levels, sample, interpolate)
        }))
    } else {
        peak
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_rows_preserve_frequency_peaks_and_interpolated_output() {
        for fft in [512, 8192, 32768] {
            let step = 48000.0 / fft as f32;
            let low = 16.0_f32.max(step);
            let axis: Vec<_> = (1..=fft / 2).map(|i| i as f32 * step).collect();
            let logical_axis = logarithmic_axis(low, 24000.0, 1024);
            let reference = frequency_rows(&axis, low, 24000.0, 1024, false);
            let (compact, indices) =
                compact_frequency_rows(frequency_rows(&axis, low, 24000.0, 1024, false), None);
            for invalid in [false, true] {
                let mut levels: Vec<_> = (0..axis.len())
                    .map(|i| -140.0 + ((i * 7919) % 14000) as f32 / 100.0)
                    .collect();
                levels[0] = -1.0;
                *levels.last_mut().unwrap() = -2.0;
                if invalid {
                    for (i, value) in levels.iter_mut().enumerate() {
                        if i % 7 == 0 {
                            *value = f32::NAN;
                        } else if i % 11 == 0 {
                            *value = f32::INFINITY;
                        }
                    }
                }
                let expanded: Vec<_> = reference
                    .iter()
                    .map(|row| frequency_level(&levels, row, false))
                    .collect();
                let stored: Vec<_> = compact
                    .iter()
                    .map(|row| frequency_level(&levels, row, false))
                    .collect();
                for (expected, &index) in expanded.iter().zip(&indices) {
                    assert_eq!(expected.to_bits(), stored[index as usize].to_bits());
                }
                for height in [7, 640, 1024] {
                    for row in frequency_rows(&logical_axis, low, 24000.0, height, true) {
                        for interpolate in [false, true] {
                            assert_eq!(
                                frequency_level(&expanded, &row, interpolate).to_bits(),
                                mapped_frequency_level(&stored, &row, &indices, interpolate)
                                    .to_bits(),
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn new_fft_aliases_do_not_collapse_distinct_historical_values() {
        let old_axis = logarithmic_axis(100.0, 400.0, 8);
        let reprojection = frequency_rows(&old_axis, 100.0, 400.0, 8, false);
        let (_, indices) = compact_frequency_rows(
            frequency_rows(&[100.0, 400.0], 100.0, 400.0, 8, false),
            Some(&reprojection),
        );
        let old_levels = [-80.0, -70.0, -60.0, -50.0, -40.0, -30.0, -20.0, -10.0];
        let mut stored = Vec::new();
        for (row, &index) in reprojection.iter().zip(&indices) {
            if index as usize == stored.len() {
                stored.push(frequency_level(&old_levels, row, false));
            }
        }
        for (row, &index) in reprojection.iter().zip(&indices) {
            assert_eq!(
                stored[index as usize],
                frequency_level(&old_levels, row, false)
            );
        }
    }

    #[test]
    fn downsampling_preserves_every_bin_including_narrow_and_endpoint_peaks() {
        let axis: Vec<_> = (1..=4096).map(|bin| bin as f32 * 6.0).collect();
        for height in [1, 7, 320, 1024] {
            let rows = frequency_rows(&axis, 16.0, 24_576.0, height, true);
            let mut bins = rows
                .iter()
                .flat_map(|row| row.bins.clone())
                .collect::<Vec<_>>();
            bins.sort_unstable();
            assert_eq!(bins, (2..4096).collect::<Vec<_>>());
            for peak_bin in [2, 166, 169, 4095] {
                let mut levels = vec![-140.0; axis.len()];
                levels[peak_bin] = -6.0;
                for interpolate in [false, true] {
                    assert_eq!(
                        rows.iter()
                            .map(|row| frequency_level(&levels, row, interpolate))
                            .fold(f32::NEG_INFINITY, f32::max),
                        -6.0,
                    );
                }
            }
        }
    }

    #[test]
    fn interpolation_smooths_known_values_without_replacing_missing_data_with_zero() {
        let rows = frequency_rows(&[100.0, 400.0], 100.0, 400.0, 8, false);
        let levels = [-80.0, -20.0];
        let smooth = frequency_level(&levels, &rows[3], true);
        assert!(smooth > -80.0 && smooth < -20.0);
        assert_eq!(frequency_level(&levels, &rows[3], false), -80.0);
        assert_eq!(frequency_level(&[], &rows[3], true), f32::NEG_INFINITY);
        assert_eq!(
            frequency_level(&[f32::NAN, f32::INFINITY], &rows[3], true),
            f32::NEG_INFINITY
        );
    }
}
