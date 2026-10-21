//! Progressive whole-track amplitude previews from the playback decoder.
//!
//! Explicit scans provide a baseline for one rolling display. New playback PCM
//! replaces visited intervals; untouched bins retain their previous statistics.
//! Source/hint changes and Still Mode retire results; signal and EQ edits do not.
//! Each panel publishes one immutable numerical frame; placement-only changes
//! reuse it. RMS pooling weights decoded sample counts, preserving missing bins.
//! Rational sample geometry excludes merely adjacent source bins from pooling.
//!
//! Pointer captures require a live interactive panel, not just cached peaks.
//! Losing that surface cancels its generation and the matching window seek;
//! ordinary MouseUp keeps the queued final Seek valid until consumption.
#[cfg(test)]
use crate::config::ConfigPersistence;
use crate::i18n::Localizer;

use std::{
    cell::RefCell,
    collections::HashMap,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use gpui::{
    AnyElement, Bounds, Context, EventEmitter, GpuFrame, GpuLimits, GpuSource, GpuWaveformColumn,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, SharedString, Window,
    canvas, div, fill, point, prelude::*, px, rgb, size,
};
use parking_lot::RwLock;

use crate::{
    audio::{PlaybackRange, WaveformFrame},
    config::Config,
    model::{DecoderHint, Track},
};

#[derive(Clone, PartialEq)]
struct Source {
    path: PathBuf,
    range: Option<PlaybackRange>,
    fingerprint: Option<String>,
    decoder_hint: DecoderHint,
}

impl Source {
    #[cfg(test)]
    fn from_frame(frame: &WaveformFrame) -> Option<Self> {
        frame.path.as_ref().map(|path| Self {
            path: path.clone(),
            range: frame.range,
            fingerprint: None,
            decoder_hint: frame.decoder_hint,
        })
    }

    fn matches(&self, frame: &WaveformFrame) -> bool {
        frame.matches(&self.path, self.range, self.decoder_hint)
    }

    fn same_target(&self, other: &Self) -> bool {
        self.path == other.path
            && self.range == other.range
            && self.fingerprint == other.fingerprint
    }

    fn matches_target(&self, frame: &WaveformFrame) -> bool {
        frame.path.as_ref() == Some(&self.path) && frame.range == self.range
    }
}

struct ManualRequest {
    source: Rc<Source>,
    suspension_generation: u64,
    cancelled: Arc<AtomicBool>,
    progressive_revision: u64,
    identity: Option<crate::audio::WaveformIdentity>,
}

#[derive(Clone)]
struct FullPreview {
    source: Rc<Source>,
    suspension_generation: u64,
    frame: WaveformFrame,
    progressive_revision: u64,
    baseline_revision: u64,
    identity: Option<crate::audio::WaveformIdentity>,
}

impl FullPreview {
    fn matches_progressive(&self, progressive: &WaveformFrame) -> bool {
        !self.source.matches_target(progressive)
            || (self
                .identity
                .is_none_or(|identity| progressive.identity == Some(identity))
                && scan_matches_progressive(&self.frame, progressive))
    }
}

fn scan_matches_progressive(scan: &WaveformFrame, progressive: &WaveformFrame) -> bool {
    match (scan.scan_source_identity, progressive.identity) {
        (Some(scanned), Some(crate::audio::WaveformIdentity::Progressive { content, .. })) => {
            scanned == content
        }
        _ => true,
    }
}

type SnapshotReply =
    crossbeam_channel::Receiver<Result<Option<crate::audio::WaveformScanSnapshot>, String>>;

struct PreparedScan {
    generation: u64,
    snapshot: Option<anyhow::Result<SnapshotReply>>,
    fallback: crate::audio::WaveformScanSnapshot,
    buffer_mb: u32,
    ffmpeg_enabled: bool,
}

#[derive(Clone, Copy)]
struct WaveformBinGeometry {
    numerator: u128,
    denominator: u128,
    sample_rate: u32,
}

fn waveform_gcd(mut left: u128, mut right: u128) -> u128 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}

impl WaveformBinGeometry {
    fn from_frame(frame: &WaveformFrame) -> Option<Self> {
        let sample_rate = match frame.identity {
            Some(crate::audio::WaveformIdentity::Progressive { sample_rate, .. })
            | Some(crate::audio::WaveformIdentity::FullScan { sample_rate, .. }) => sample_rate,
            None => 0,
        };
        if frame.bin_frames == 0 || frame.bin_count == 0 || sample_rate == 0 {
            return None;
        }
        let denominator = u128::from(frame.bin_count) * u128::from(sample_rate);
        let divisor = waveform_gcd(frame.bin_frames, denominator);
        Some(Self {
            numerator: frame.bin_frames / divisor,
            denominator: denominator / divisor,
            sample_rate,
        })
    }

    /// Express both widths in common integer ticks, cancelling before products.
    /// EOF truncation changes array lengths, not these rational bin widths.
    fn relative_widths(self, source: Self) -> (u128, u128) {
        let numerator_divisor = waveform_gcd(self.numerator, source.numerator);
        let denominator_divisor = waveform_gcd(self.denominator, source.denominator);
        (
            (self.numerator / numerator_divisor) * (source.denominator / denominator_divisor),
            (source.numerator / numerator_divisor) * (self.denominator / denominator_divisor),
        )
    }
}

/// Overlay new playback intervals onto the bounded full-scan geometry. Unknown
/// duration coarsening grows the display only when PCM reaches beyond its end.
fn merge_progressive(
    display: &mut WaveformFrame,
    progressive: &WaveformFrame,
    after: u64,
    baseline: u64,
) {
    if progressive.peaks.is_empty() {
        return;
    }
    let Some(progressive_geometry) = WaveformBinGeometry::from_frame(progressive) else {
        return;
    };
    let display_geometry = if display.peaks.is_empty() {
        display.identity = progressive.identity;
        progressive_geometry
    } else if let Some(geometry) = WaveformBinGeometry::from_frame(display) {
        geometry
    } else {
        return;
    };
    let (width, source_width) = display_geometry.relative_widths(progressive_geometry);
    let exceeds_display = progressive
        .rms_spans
        .iter()
        .flatten()
        .map(|span| span.end)
        .max()
        .map_or_else(
            || {
                progressive
                    .peaks
                    .iter()
                    .rposition(Option::is_some)
                    .is_some_and(|bin| {
                        (bin as u128 + 1) * source_width > display.peaks.len() as u128 * width
                    })
            },
            |end| {
                u128::from(end) * display_geometry.denominator
                    > display_geometry.numerator
                        * display.peaks.len() as u128
                        * u128::from(progressive_geometry.sample_rate)
            },
        );
    let geometry_changed = display.peaks.is_empty() || exceeds_display;
    if geometry_changed {
        let bins = progressive.peaks.len();
        let (width, source_width) = progressive_geometry.relative_widths(display_geometry);
        for (peaks, rms, counts) in [
            (
                &mut display.peaks,
                &mut display.rms,
                &mut display.rms_counts,
            ),
            (
                &mut display.left_peaks,
                &mut display.left_rms,
                &mut display.left_rms_counts,
            ),
            (
                &mut display.right_peaks,
                &mut display.right_rms,
                &mut display.right_rms_counts,
            ),
        ] {
            // Only geometry growth needs replacement storage; never clone the
            // frame's unrelated history, identities or channel metadata.
            let old_peaks = std::mem::take(peaks);
            let old_rms = std::mem::take(rms);
            let old_counts = std::mem::take(counts);
            peaks.reserve(bins);
            rms.reserve(bins);
            counts.reserve(bins);
            for bin in 0..bins {
                let start = bin as u128 * width;
                let end = (bin as u128 + 1) * width;
                let first = (start / source_width).min(old_peaks.len() as u128) as usize;
                let last = end.div_ceil(source_width).min(old_peaks.len() as u128) as usize;
                let mut peak: Option<f32> = None;
                let mut energy = 0.0;
                let mut count = 0.0;
                for (source, value) in old_peaks.iter().copied().enumerate().take(last).skip(first)
                {
                    if let Some(value) = value {
                        peak = Some(peak.unwrap_or(0.0).max(value));
                    }
                    let overlap = end.min((source as u128 + 1) * source_width)
                        - start.max(source as u128 * source_width);
                    let weight = old_counts.get(source).copied().unwrap_or(0) as f64
                        * (overlap as f64 / source_width as f64);
                    if let Some(value) = old_rms.get(source).copied().flatten() {
                        energy += f64::from(value).powi(2) * weight;
                        count += weight;
                    }
                }
                peaks.push(peak);
                rms.push((count > 0.0).then(|| (energy / count).sqrt() as f32));
                counts.push(if count > 0.0 {
                    (count.round() as u64).max(1)
                } else {
                    0
                });
            }
        }
        display.span_seconds = progressive.span_seconds;
        // Keep the display's source identity, expressing the new width in its
        // sample-rate domain even if a scan used another decoder's sample rate.
        let divisor = waveform_gcd(
            u128::from(display_geometry.sample_rate),
            progressive_geometry.denominator,
        );
        display.bin_frames =
            progressive_geometry.numerator * (u128::from(display_geometry.sample_rate) / divisor);
        display.bin_count = (progressive_geometry.denominator / divisor) as u64;
    }
    let mut changed = geometry_changed;
    let (width, source_width) = if geometry_changed {
        (1, 1)
    } else {
        (width, source_width)
    };
    let identical_geometry = width == source_width;
    for bin in 0..display.peaks.len() {
        let start = bin as u128 * width;
        let end = (bin as u128 + 1) * width;
        let (first, last) = if identical_geometry {
            let first = bin.min(progressive.peaks.len());
            (first, (bin + 1).min(progressive.peaks.len()))
        } else {
            (
                (start / source_width).min(progressive.peaks.len() as u128) as usize,
                end.div_ceil(source_width)
                    .min(progressive.peaks.len() as u128) as usize,
            )
        };
        if !(first..last).any(|source| {
            progressive
                .bin_revisions
                .get(source)
                .is_some_and(|revision| *revision > after)
        }) {
            continue;
        }
        let epoch = if identical_geometry {
            0
        } else {
            (first..last)
                .filter(|source| {
                    progressive
                        .bin_revisions
                        .get(*source)
                        .is_some_and(|revision| *revision > baseline)
                })
                .map(|source| progressive.bin_epochs.get(source).copied().unwrap_or(0))
                .max()
                .unwrap_or(0)
        };
        for (peaks, rms, counts, source_peaks, source_rms, source_counts) in [
            (
                &mut display.peaks,
                &mut display.rms,
                &mut display.rms_counts,
                &progressive.peaks,
                &progressive.rms,
                &progressive.rms_counts,
            ),
            (
                &mut display.left_peaks,
                &mut display.left_rms,
                &mut display.left_rms_counts,
                &progressive.left_peaks,
                &progressive.left_rms,
                &progressive.left_rms_counts,
            ),
            (
                &mut display.right_peaks,
                &mut display.right_rms,
                &mut display.right_rms_counts,
                &progressive.right_peaks,
                &progressive.right_rms,
                &progressive.right_rms_counts,
            ),
        ] {
            if identical_geometry {
                let included = progressive.bin_revisions[bin] > baseline;
                peaks[bin] = source_peaks
                    .get(bin)
                    .copied()
                    .flatten()
                    .filter(|_| included);
                let value = source_rms.get(bin).copied().flatten().filter(|_| included);
                let count = if value.is_some() {
                    source_counts.get(bin).copied().unwrap_or(0)
                } else {
                    0
                };
                rms[bin] = value.filter(|_| count > 0);
                counts[bin] = count;
                continue;
            }
            let mut peak: Option<f32> = None;
            let mut energy = 0.0;
            let mut count = 0.0;
            for source in first..last {
                if progressive
                    .bin_revisions
                    .get(source)
                    .is_none_or(|revision| *revision <= baseline)
                    || progressive.bin_epochs.get(source).copied().unwrap_or(0) != epoch
                {
                    continue;
                }
                if let Some(value) = source_peaks.get(source).copied().flatten() {
                    peak = Some(peak.unwrap_or(0.0).max(value));
                }
                let overlap = end.min((source as u128 + 1) * source_width)
                    - start.max(source as u128 * source_width);
                let weight = source_counts.get(source).copied().unwrap_or(0) as f64
                    * (overlap as f64 / source_width as f64);
                if let Some(value) = source_rms.get(source).copied().flatten() {
                    energy += f64::from(value).powi(2) * weight;
                    count += weight;
                }
            }
            peaks[bin] = peak;
            rms[bin] = (count > 0.0).then(|| (energy / count).sqrt() as f32);
            counts[bin] = if count > 0.0 {
                (count.round() as u64).max(1)
            } else {
                0
            };
        }
        changed = true;
    }
    if changed {
        display.duration = display.duration.or(progressive.duration);
        display.max_peak = display
            .peaks
            .iter()
            .chain(&display.left_peaks)
            .chain(&display.right_peaks)
            .flatten()
            .copied()
            .fold(1.0, f32::max);
        display.revision = display.revision.wrapping_add(1);
    }
}

fn track_range(track: &Track) -> Option<PlaybackRange> {
    track.cue.as_ref().map(|cue| PlaybackRange {
        start_seconds: cue.start_seconds(),
        end_seconds: cue.end_seconds(),
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct WaveformColumn {
    left_rms: Option<f32>,
    left_peak: Option<f32>,
    right_rms: Option<f32>,
    right_peak: Option<f32>,
}

#[derive(Clone, Copy, PartialEq)]
struct WaveformMapping {
    rms_gain: f32,
    peak_gain: f32,
    peak_gamma: f32,
}

struct Columns {
    revision: u64,
    mapping: Option<WaveformMapping>,
    values: Vec<WaveformColumn>,
    has_known_component: bool,
    publication: Option<(crate::config::VisualizationPalette, Arc<GpuFrame>)>,
}

#[derive(Default)]
struct WaveformPlot {
    columns: HashMap<u64, Columns>,
}

fn waveform_level(value: f32, gain: f32, gamma: f32) -> f32 {
    (value * gain).clamp(0.0, 1.0).powf(gamma)
}

#[cfg(test)]
fn waveform_height(level: f32, half_height: Pixels) -> Pixels {
    (half_height * level).max(px(1.0))
}

fn pooled_side(
    peaks: &[Option<f32>],
    rms: &[Option<f32>],
    counts: &[u64],
    first: usize,
    end: usize,
    scale: f32,
    mapping: WaveformMapping,
) -> (Option<f32>, Option<f32>) {
    let peak = peaks[first..end]
        .iter()
        .flatten()
        .copied()
        .reduce(f32::max)
        .map(|value| waveform_level(value * scale, mapping.peak_gain, mapping.peak_gamma));
    let mut energy = 0.0_f64;
    let mut known = 0.0_f64;
    for (value, &count) in rms[first..end].iter().zip(&counts[first..end]) {
        if let Some(value) = value {
            let value = f64::from(*value);
            energy += value * value * count as f64;
            known += count as f64;
        }
    }
    let rms = (known != 0.0).then(|| {
        waveform_level(
            (energy / known).sqrt() as f32 * scale,
            mapping.rms_gain,
            0.86,
        )
    });
    (rms, peak)
}

impl WaveformPlot {
    fn retain_panels(&mut self, mut keep: impl FnMut(u64) -> bool) {
        self.columns.retain(|id, _| keep(*id));
    }

    fn clear_cache(&mut self) {
        for columns in self.columns.values_mut() {
            columns.values.clear();
            columns.has_known_component = false;
            columns.mapping = None;
            columns.publication = None;
        }
    }

    fn publication(
        &mut self,
        panel_id: u64,
        palette_id: crate::config::VisualizationPalette,
        cache_enabled: bool,
        palette: &super::visuals::PaletteLut,
    ) -> Result<Option<Arc<GpuFrame>>, String> {
        let columns = self.columns.get_mut(&panel_id).unwrap();
        if !columns.has_known_component {
            return Ok(None);
        }
        if cache_enabled
            && let Some((retained_palette, frame)) = &columns.publication
            && *retained_palette == palette_id
        {
            return Ok(Some(Arc::clone(frame)));
        }
        let values: Vec<_> = columns
            .values
            .iter()
            .map(|column| GpuWaveformColumn {
                levels: [
                    column.left_rms,
                    column.left_peak,
                    column.right_rms,
                    column.right_peak,
                ]
                .map(|value| value.unwrap_or(f32::NAN)),
            })
            .collect();
        let gpu_palette = palette.waveform_gpu_palette()?;
        let max_palette_colors = gpu_palette.colors().len();
        let max_waveform_columns = values.len();
        let frame = GpuFrame::waveform(
            values,
            gpu_palette,
            palette.waveform_gpu_params(),
            GpuLimits {
                max_waveform_columns,
                max_palette_colors,
                ..Default::default()
            },
        )
        .map_err(|error| error.to_string())?;
        columns.publication = cache_enabled.then(|| (palette_id, Arc::clone(&frame)));
        Ok(Some(frame))
    }
    #[cfg(test)]
    fn columns(&mut self, panel_id: u64, width: usize, frame: &WaveformFrame) -> &[WaveformColumn] {
        self.columns_with_cache(
            panel_id,
            width,
            frame,
            true,
            WaveformMapping {
                rms_gain: 1.0,
                peak_gain: 1.0,
                peak_gamma: 1.0,
            },
        )
    }

    fn columns_with_cache(
        &mut self,
        panel_id: u64,
        width: usize,
        frame: &WaveformFrame,
        cache_enabled: bool,
        mapping: WaveformMapping,
    ) -> &[WaveformColumn] {
        let columns = self.columns.entry(panel_id).or_insert_with(|| Columns {
            revision: frame.revision,
            mapping: None,
            values: Vec::new(),
            has_known_component: false,
            publication: None,
        });
        let left_peaks = if frame.left_peaks.is_empty() {
            &frame.peaks
        } else {
            &frame.left_peaks
        };
        let (left_rms, left_counts) = if frame.left_rms.is_empty() {
            (&frame.rms, &frame.rms_counts)
        } else {
            (&frame.left_rms, &frame.left_rms_counts)
        };
        let right_peaks = if frame.right_peaks.is_empty() {
            &frame.peaks
        } else {
            &frame.right_peaks
        };
        let (right_rms, right_counts) = if frame.right_rms.is_empty() {
            (&frame.rms, &frame.rms_counts)
        } else {
            (&frame.right_rms, &frame.right_rms_counts)
        };
        let count = width.max(1).min(left_peaks.len());
        if !cache_enabled
            || columns.revision != frame.revision
            || columns.mapping != Some(mapping)
            || columns.values.len() != count
        {
            let mut changed = columns.values.len() != count;
            let scale = frame.max_peak.max(1.0).recip();
            let mut has_known_component = false;
            for column in 0..count {
                let first = column * left_peaks.len() / count;
                let end = (column + 1) * left_peaks.len() / count;
                let (left_rms, left_peak) = pooled_side(
                    left_peaks,
                    left_rms,
                    left_counts,
                    first,
                    end,
                    scale,
                    mapping,
                );
                let (right_rms, right_peak) = pooled_side(
                    right_peaks,
                    right_rms,
                    right_counts,
                    first,
                    end,
                    scale,
                    mapping,
                );
                let value = WaveformColumn {
                    left_rms,
                    left_peak,
                    right_rms,
                    right_peak,
                };
                has_known_component |= left_rms.is_some()
                    || left_peak.is_some()
                    || right_rms.is_some()
                    || right_peak.is_some();
                if let Some(previous) = columns.values.get_mut(column) {
                    changed |= *previous != value;
                    *previous = value;
                } else {
                    columns.values.push(value);
                }
            }
            columns.values.truncate(count);
            columns.has_known_component = has_known_component;
            if changed || !cache_enabled {
                columns.publication = None;
            }
        }
        columns.revision = frame.revision;
        columns.mapping = Some(mapping);
        &columns.values
    }
}

fn smooth_ping_pong(position: f64, period: f64) -> f64 {
    let phase = position.rem_euclid(period) / period;
    let ramp = if phase <= 0.5 {
        phase * 2.0
    } else {
        (1.0 - phase) * 2.0
    };
    let eased = ramp * ramp * (3.0 - 2.0 * ramp);
    eased * 2.0 - 1.0
}

/// A slow, periodic spline-like motion. Both ends have zero velocity, so the
/// playhead changes direction rhythmically instead of vibrating at high speed.
fn cursor_wobble(position: f64) -> f32 {
    if !position.is_finite() {
        return 0.0;
    }
    (0.68 * smooth_ping_pong(position, 5.0) + 0.32 * smooth_ping_pong(position + 1.4, 8.0)) as f32
}

fn preview_available(source: &Source, frame: &WaveformFrame) -> bool {
    source.matches_target(frame) && frame.peaks.iter().any(Option::is_some)
}

fn timeline(frame: &WaveformFrame, duration: Option<f64>) -> Option<f64> {
    if frame.span_seconds.is_finite() && frame.span_seconds > 0.0 {
        Some(frame.span_seconds)
    } else {
        frame.duration.or(duration)
    }
}

fn progress(position: f64, duration: Option<f64>) -> Option<f32> {
    duration
        .filter(|value| value.is_finite() && *value > 0.0 && position.is_finite())
        .map(|duration| (position / duration).clamp(0.0, 1.0) as f32)
}
fn seek_seconds(position: Pixels, left: Pixels, width: Pixels, duration: f64) -> f64 {
    let fraction = ((position - left) / width.max(px(1.0))).clamp(0.0, 1.0);
    f64::from(fraction) * duration
}

#[derive(Default)]
struct WaveformInteraction {
    panels: HashMap<u64, (Bounds<Pixels>, Option<f64>)>,
    active_panel: Option<u64>,
    // Pointer interpretation is fixed at press; playback can change mid-drag.
    seek_duration: Option<f64>,
}

impl WaveformInteraction {
    fn seek_seconds(&self, position: Point<Pixels>, panel_id: Option<u64>) -> Option<f64> {
        let panel_id = panel_id.or(self.active_panel)?;
        let (bounds, _) = self.panels.get(&panel_id)?;
        Some(seek_seconds(
            position.x,
            bounds.left(),
            bounds.size.width,
            self.seek_duration?,
        ))
    }
}

pub(in crate::gui) enum WaveformEvent {
    Preview { generation: u64, seconds: f64 },
    Seek { generation: u64, seconds: f64 },
    Cancelled { generation: u64 },
}
impl EventEmitter<WaveformEvent> for Waveform {}

pub(in crate::gui) struct Waveform {
    enabled: bool,
    shared: Arc<RwLock<WaveformFrame>>,
    source: Option<Rc<Source>>,
    revision: Option<u64>,
    plot: Rc<RefCell<WaveformPlot>>,
    generation: u64,
    manual: Option<ManualRequest>,
    prepared_scan: Option<PreparedScan>,
    full: Option<Rc<FullPreview>>,
    manual_error: Option<SharedString>,
    interaction: Rc<RefCell<WaveformInteraction>>,
    dragging: bool,
    seek_generation: u64,
    seek_enabled: bool,
}

impl Waveform {
    pub(in crate::gui) fn new(shared: Arc<RwLock<WaveformFrame>>) -> Self {
        Self {
            enabled: true,
            shared,
            source: None,
            revision: None,
            plot: Rc::new(RefCell::new(WaveformPlot::default())),
            generation: 0,
            manual: None,
            prepared_scan: None,
            full: None,
            manual_error: None,
            interaction: Rc::new(RefCell::new(WaveformInteraction::default())),
            dragging: false,
            seek_generation: 0,
            seek_enabled: true,
        }
    }

    /// Suspend display work and cancel explicit scans. The audio worker discards
    /// shared results; resuming ordinary sync never starts a decoder scan.
    pub(in crate::gui) fn set_enabled(&mut self, enabled: bool) {
        if self.enabled == enabled {
            return;
        }
        self.enabled = enabled;
        if enabled {
            return;
        }
        self.cancel_full();
        self.source = None;
        self.revision = None;
        let mut plot = self.plot.borrow_mut();
        plot.retain_panels(|_| false);
        plot.columns = HashMap::new();
        drop(plot);
        *self.interaction.borrow_mut() = WaveformInteraction::default();
        self.cancel_seek();
    }

    fn release_seek_capture(&mut self) {
        self.dragging = false;
        let mut interaction = self.interaction.borrow_mut();
        interaction.active_panel = None;
        interaction.seek_duration = None;
    }

    pub(in crate::gui) fn cancel_seek(&mut self) {
        self.seek_generation = self.seek_generation.wrapping_add(1);
        self.release_seek_capture();
    }

    pub(in crate::gui) fn accepts_seek_event(&self, generation: u64) -> bool {
        self.seek_generation == generation
    }

    /// Called after the window's release dispatch. A normally released capture
    /// must keep its queued Seek valid; only an unhandled release is cancelled.
    pub(in crate::gui) fn cancel_unreleased_seek(&mut self) -> Option<u64> {
        if !self.dragging {
            return None;
        }
        let generation = self.seek_generation;
        self.cancel_seek();
        Some(generation)
    }

    fn cancel_unavailable_capture(&mut self) -> Option<u64> {
        let unavailable = {
            let interaction = self.interaction.borrow();
            interaction
                .active_panel
                .is_some_and(|id| !interaction.panels.contains_key(&id))
        };
        unavailable.then(|| self.cancel_unreleased_seek()).flatten()
    }

    fn remove_interaction_panel(&mut self, panel_id: u64) -> Option<u64> {
        self.interaction.borrow_mut().panels.remove(&panel_id);
        self.cancel_unavailable_capture()
    }

    fn retain_interaction_panels(&mut self, mut keep: impl FnMut(u64) -> bool) -> Option<u64> {
        self.interaction
            .borrow_mut()
            .panels
            .retain(|id, _| keep(*id));
        self.cancel_unavailable_capture()
    }

    pub(in crate::gui) fn set_seek_enabled(&mut self, enabled: bool) {
        self.seek_enabled = enabled;
        if !enabled {
            self.cancel_seek();
        }
    }

    pub(in crate::gui) fn retain_panels(
        &mut self,
        panels: &[&super::super::layout::Panel],
        cx: &mut Context<Self>,
    ) {
        let keep = |id| {
            panels
                .iter()
                .any(|panel| panel.id == id && panel.kind == "waveform")
        };
        self.plot.borrow_mut().retain_panels(keep);
        if let Some(generation) = self.retain_interaction_panels(keep) {
            cx.emit(WaveformEvent::Cancelled { generation });
        }
    }
    /// The caller chooses the playing track, or the selected track before play.
    /// Content fingerprints invalidate previews; metadata and visibility do not.
    pub(in crate::gui) fn sync(&mut self, track: Option<&Track>, cx: &mut Context<Self>) {
        if self.sync_state(track) {
            cx.notify();
        }
    }

    fn sync_state(&mut self, track: Option<&Track>) -> bool {
        if !self.enabled {
            return false;
        }
        let source_changed = self.sync_source(track);
        let (revision, suspension_generation, provenance_changed) = {
            let shared = self.shared.read();
            let changed = self
                .full
                .as_ref()
                .is_some_and(|full| !full.matches_progressive(&shared))
                || self.manual.as_ref().is_some_and(|request| {
                    request.source.matches_target(&shared)
                        && request
                            .identity
                            .is_some_and(|identity| shared.identity != Some(identity))
                });
            (shared.revision, shared.suspension_generation(), changed)
        };
        let suspended = self
            .manual
            .as_ref()
            .is_some_and(|request| request.suspension_generation != suspension_generation)
            || self
                .full
                .as_ref()
                .is_some_and(|full| full.suspension_generation != suspension_generation);
        if suspended || provenance_changed {
            self.cancel_full();
        }
        if let Some(full) = self.full.as_mut() {
            let shared = self.shared.read();
            if shared.revision != full.progressive_revision
                && full.source.matches_target(&shared)
                && full.matches_progressive(&shared)
            {
                let full = Rc::make_mut(full);
                merge_progressive(
                    &mut full.frame,
                    &shared,
                    full.progressive_revision,
                    full.baseline_revision,
                );
                full.progressive_revision = shared.revision;
            }
        }
        let changed =
            source_changed || suspended || provenance_changed || self.revision != Some(revision);
        self.revision = Some(revision);
        changed
    }

    fn sync_source(&mut self, track: Option<&Track>) -> bool {
        let source_changed = match (&self.source, track) {
            (Some(source), Some(track)) => {
                source.path != track.path
                    || source.range != track_range(track)
                    || source.fingerprint != track.fingerprint
            }
            (None, None) => false,
            _ => true,
        };
        if source_changed {
            self.cancel_full();
            self.plot.borrow_mut().clear_cache();
            self.source = track.map(|track| {
                Rc::new(Source {
                    path: track.path.clone(),
                    range: track_range(track),
                    fingerprint: track.fingerprint.clone(),
                    decoder_hint: track.decoder_hint,
                })
            });
        }
        source_changed
    }

    /// A committed hint edit invalidates only this target's explicit preview.
    /// The playback frame and its decoder identity remain untouched.
    pub(in crate::gui) fn saved_hint_changed(&mut self, track: &Track, cx: &mut Context<Self>) {
        if self.invalidate_saved_hint(track) {
            cx.notify();
        }
    }

    fn invalidate_saved_hint(&mut self, track: &Track) -> bool {
        let matches =
            |source: &Source| source.path == track.path && source.range == track_range(track);
        if self
            .manual
            .as_ref()
            .is_some_and(|request| matches(&request.source))
            || self.full.as_ref().is_some_and(|full| matches(&full.source))
        {
            self.cancel_full();
            return true;
        }
        false
    }

    /// Capture the click before querying the saved hint; keep the old display.
    pub(in crate::gui) fn prepare_full(
        &mut self,
        track: &Track,
        config: &Config,
        snapshot: Option<anyhow::Result<SnapshotReply>>,
        cx: &mut Context<Self>,
    ) {
        self.sync(Some(track), cx);
        let (generation, _) = self.start_full(track.decoder_hint);
        self.prepared_scan = Some(PreparedScan {
            generation,
            snapshot,
            fallback: crate::audio::WaveformScanSnapshot::original(
                track.path.clone(),
                track_range(track),
                track.decoder_hint,
                config.visualization_signal,
                Arc::new(config.equalizer.clone()),
            ),
            buffer_mb: config.media_read_buffer_mb,
            ffmpeg_enabled: config.ffmpeg_enabled,
        });
        cx.notify();
    }

    pub(in crate::gui) fn fail_full_query(&mut self, error: String, cx: &mut Context<Self>) {
        if let Some(request) = self.manual.take() {
            request.cancelled.store(true, Ordering::Relaxed);
        }
        self.prepared_scan = None;
        self.manual_error = Some(error.into());
        cx.notify();
    }

    /// Explicit context-menu action; ordinary sync never opens a decoder.
    pub(in crate::gui) fn load_full(&mut self, track: &Track, cx: &mut Context<Self>) {
        let Some(mut setup) = self.prepared_scan.take() else {
            return;
        };
        let Some(request) = self.manual.as_mut() else {
            return;
        };
        if !self.enabled || setup.generation != self.generation {
            return;
        }
        Rc::make_mut(&mut request.source).decoder_hint = track.decoder_hint;
        setup.fallback.decoder_hint = track.decoder_hint;
        let display_source = request.source.as_ref().clone();
        let cancelled = Arc::clone(&request.cancelled);
        let generation = setup.generation;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let mut snapshot = match setup.snapshot {
                        Some(reply) => reply?
                            .recv()
                            .map_err(|error| anyhow::anyhow!(error))?
                            .map_err(|error| anyhow::anyhow!(error))?
                            .unwrap_or(setup.fallback),
                        None => setup.fallback,
                    };
                    // Saved original-file hints must not be applied to an audition cache.
                    if snapshot.path == display_source.path
                        && snapshot.range == display_source.range
                    {
                        snapshot.decoder_hint = display_source.decoder_hint;
                    }
                    let mut frame = crate::audio::waveform(
                        &snapshot,
                        setup.buffer_mb,
                        setup.ffmpeg_enabled,
                        &cancelled,
                    )?;
                    // Scan validates the actual opened source. Display metadata and
                    // seek interaction intentionally retain the original timeline.
                    frame.path = Some(display_source.path.clone());
                    frame.range = display_source.range;
                    frame.decoder_hint = display_source.decoder_hint;
                    Ok(frame)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.finish_full(generation, result) {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn start_full(&mut self, decoder_hint: DecoderHint) -> (u64, Arc<AtomicBool>) {
        self.cancel_pending();
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut shared = self.shared.write();
        shared.cancel_scan_on_suspend(&cancelled);
        self.manual = Some(ManualRequest {
            source: Rc::new(Source {
                decoder_hint,
                ..self
                    .source
                    .as_deref()
                    .expect("full preview source is present")
                    .clone()
            }),
            suspension_generation: shared.suspension_generation(),
            cancelled: Arc::clone(&cancelled),
            progressive_revision: shared.revision,
            identity: self
                .source
                .as_ref()
                .filter(|source| source.matches_target(&shared))
                .and(shared.identity),
        });
        self.manual_error = None;
        (self.generation, cancelled)
    }

    fn cancel_full(&mut self) {
        self.cancel_pending();
        if self.full.take().is_some() {
            self.plot.borrow_mut().clear_cache();
        }
        self.manual_error = None;
    }

    fn cancel_pending(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.prepared_scan = None;
        if let Some(request) = self.manual.take() {
            request.cancelled.store(true, Ordering::Relaxed);
        }
        self.manual_error = None;
    }

    fn finish_full(&mut self, generation: u64, result: anyhow::Result<WaveformFrame>) -> bool {
        let Some(request) = self.manual.as_ref() else {
            return false;
        };
        if generation != self.generation {
            return false;
        }
        let mut completed = None;
        let result = (|| {
            let shared = self.shared.read();
            // Suspension invalidates late data and cancellation errors, even
            // when a quick on/off was coalesced by GUI refresh.
            if request.cancelled.load(Ordering::Relaxed)
                || request.suspension_generation != shared.suspension_generation()
            {
                return Ok(());
            }
            let mut result = result?;
            if !self
                .source
                .as_ref()
                .is_some_and(|source| source.same_target(&request.source))
                || !request.source.matches(&result)
                || (request.source.matches_target(&shared)
                    && request
                        .identity
                        .is_some_and(|identity| shared.identity != Some(identity)))
                || (request.source.matches_target(&shared)
                    && !scan_matches_progressive(&result, &shared))
            {
                anyhow::bail!("Full waveform source changed before completion");
            }
            result.complete = true;
            if request.source.matches_target(&shared) {
                merge_progressive(
                    &mut result,
                    &shared,
                    request.progressive_revision,
                    request.progressive_revision,
                );
            }
            completed = Some(Rc::new(FullPreview {
                source: Rc::clone(&request.source),
                suspension_generation: request.suspension_generation,
                frame: result,
                progressive_revision: shared.revision,
                baseline_revision: request.progressive_revision,
                identity: request.identity,
            }));
            Ok(())
        })();
        if let Some(completed) = completed {
            self.plot.borrow_mut().clear_cache();
            self.full = Some(completed);
        }
        self.manual = None;
        self.manual_error = result.err().map(|error| format!("{error:#}").into());
        true
    }

    fn full_preview(&self, progressive: &WaveformFrame) -> Option<&Rc<FullPreview>> {
        self.full.as_ref().filter(|full| {
            full.suspension_generation == progressive.suspension_generation()
                && full.matches_progressive(progressive)
                && self
                    .source
                    .as_ref()
                    .is_some_and(|source| source.same_target(&full.source))
        })
    }

    fn display_frame<'a>(&'a self, progressive: &'a WaveformFrame) -> &'a WaveformFrame {
        self.full_preview(progressive)
            .map_or(progressive, |full| &full.frame)
    }

    fn manual_message(&self, i18n: &Localizer) -> Option<SharedString> {
        if self.manual.is_some() {
            Some(i18n.text("waveform-loading").to_owned().into())
        } else {
            self.manual_error.as_ref().map(|error| {
                i18n.format("waveform-error", &[("error", error.as_ref().into())])
                    .into()
            })
        }
    }

    fn seek_from_position(
        &mut self,
        position: Point<Pixels>,
        panel_id: Option<u64>,
        preview: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.seek_enabled {
            return;
        }
        let Some(seconds) = self.interaction.borrow().seek_seconds(position, panel_id) else {
            return;
        };
        cx.emit(if preview {
            WaveformEvent::Preview {
                generation: self.seek_generation,
                seconds,
            }
        } else {
            WaveformEvent::Seek {
                generation: self.seek_generation,
                seconds,
            }
        });
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.seek_enabled {
            return;
        }
        let capture =
            self.interaction
                .borrow()
                .panels
                .iter()
                .find_map(|(id, (bounds, duration))| {
                    bounds.contains(&event.position).then_some((*id, *duration))
                });
        let Some((panel_id, Some(duration))) = capture else {
            return;
        };
        self.seek_generation = self.seek_generation.wrapping_add(1);
        self.dragging = true;
        {
            let mut interaction = self.interaction.borrow_mut();
            interaction.active_panel = Some(panel_id);
            interaction.seek_duration = Some(duration);
        }
        self.seek_from_position(event.position, Some(panel_id), true, cx);
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.dragging {
            self.seek_from_position(event.position, None, true, cx);
        }
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.dragging {
            self.seek_from_position(event.position, None, false, cx);
        }
        // Keep the just-emitted Seek valid until its subscriber consumes it.
        self.release_seek_capture();
    }

    /// Position and duration are relative to the selected track/CUE segment.
    /// Pass position zero for a selection preview rather than another track's
    /// playback position. The caller observes this entity for data changes.
    pub(in crate::gui) fn view(
        &mut self,
        i18n: &Arc<Localizer>,
        panel_id: u64,
        position: f64,
        duration: Option<f64>,
        config: &Config,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.set_seek_enabled(config.waveform_seek_enabled);
        let panel = div()
            .id(("waveform", panel_id))
            .relative()
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .bg(rgb(config.visual_background.rgb()));
        let Some(source) = self.source.as_deref() else {
            if let Some(generation) = self.remove_interaction_panel(panel_id) {
                cx.emit(WaveformEvent::Cancelled { generation });
            }
            return panel.into_any_element();
        };
        let status = self.manual_message(i18n);
        let (available, label_duration, full) = {
            let shared = self.shared.read();
            let frame = self.display_frame(&shared);
            let full = self.full_preview(&shared).map(Rc::clone);
            (
                preview_available(source, frame),
                timeline(frame, duration),
                full,
            )
        };
        if !available {
            if let Some(generation) = self.remove_interaction_panel(panel_id) {
                cx.emit(WaveformEvent::Cancelled { generation });
            }
            return panel
                .children(status.map(super::visualization_status))
                .into_any_element();
        }
        let source = full.as_ref().map_or_else(
            || Rc::clone(self.source.as_ref().expect("preview source is present")),
            |full| Rc::clone(&full.source),
        );
        let shared = Arc::clone(&self.shared);
        let waveform = Rc::clone(&self.plot);
        let palette = super::visuals::active_palette_lut(config.visual_palette);
        let config_palette = config.visual_palette;
        let error_i18n = Arc::clone(i18n);
        let error_theme = crate::theme::palette(config.ui_theme);
        let water = rgb(config.waveform_cursor_color.rgb());
        let background = rgb(config.visual_background.rgb());
        let glow = config.waveform_glow;
        let labels = config.waveform_labels;
        let rms_gain = config.waveform_rms_gain;
        let peak_gain = config.waveform_peak_gain;
        let peak_gamma = config.waveform_peak_gamma;
        let cache_enabled = config.visualization_cache;
        let interaction = Rc::clone(&self.interaction);
        if label_duration.is_some() {
            interaction
                .borrow_mut()
                .panels
                .insert(panel_id, (Bounds::default(), label_duration));
        } else if let Some(generation) = self.remove_interaction_panel(panel_id) {
            cx.emit(WaveformEvent::Cancelled { generation });
        }
        let prepaint_interaction = Rc::clone(&interaction);
        panel
            .child(
                div()
                    .size_full()
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
                    .on_mouse_move(cx.listener(Self::mouse_move))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::mouse_up))
                    .child(
                        canvas(
                            move |bounds, _, _| {
                                let plot = (bounds.size.width > px(16.0)
                                    && bounds.size.height > px(if labels { 32.0 } else { 16.0 }))
                                .then(|| {
                                    Bounds::new(
                                        bounds.origin + point(px(8.0), px(8.0)),
                                        size(
                                            bounds.size.width - px(16.0),
                                            bounds.size.height
                                                - px(if labels { 32.0 } else { 16.0 }),
                                        ),
                                    )
                                });
                                if let Some(entry) =
                                    prepaint_interaction.borrow_mut().panels.get_mut(&panel_id)
                                {
                                    entry.0 = plot.unwrap_or_default();
                                }
                            },
                            move |bounds, _, window, cx| {
                                window.paint_quad(fill(bounds, background));
                                if bounds.size.width <= px(16.0)
                                    || bounds.size.height <= px(if labels { 32.0 } else { 16.0 })
                                {
                                    return;
                                }
                                let plot = Bounds::new(
                                    bounds.origin + point(px(8.0), px(8.0)),
                                    size(
                                        bounds.size.width - px(16.0),
                                        bounds.size.height - px(if labels { 32.0 } else { 16.0 }),
                                    ),
                                );
                                let mut waveform = waveform.borrow_mut();
                                let (_revision, progress) = {
                                    let shared_frame = shared.read();
                                    let frame = full
                                        .as_ref()
                                        .filter(|full| {
                                            full.suspension_generation
                                                == shared_frame.suspension_generation()
                                        })
                                        .map_or(&*shared_frame, |full| &full.frame);
                                    // The shared generation also guards a full preview captured
                                    // before a Still Mode transition between layout and paint.
                                    if !source.matches_target(frame) {
                                        return;
                                    }
                                    waveform.columns_with_cache(
                                        panel_id,
                                        (plot.size.width / px(1.0)) as usize,
                                        frame,
                                        cache_enabled,
                                        WaveformMapping {
                                            rms_gain,
                                            peak_gain,
                                            peak_gamma,
                                        },
                                    );
                                    (
                                        frame.revision,
                                        progress(position, timeline(frame, duration)),
                                    )
                                };
                                let width_px = (plot.size.width / px(1.0)).ceil().max(1.0);
                                let result = (|| {
                                    if let Some(frame) = waveform.publication(
                                        panel_id,
                                        config_palette,
                                        cache_enabled,
                                        &palette,
                                    )? {
                                        window
                                            .paint_gpu(
                                                Bounds::new(
                                                    plot.origin,
                                                    size(px(width_px), plot.size.height),
                                                ),
                                                GpuSource::Fixed(frame),
                                            )
                                            .map_err(|error| error.to_string())?;
                                    }
                                    Ok::<(), String>(())
                                })();
                                if let Err(error) = result {
                                    window.paint_quad(fill(
                                        Bounds::new(plot.origin, size(plot.size.width, px(22.0))),
                                        rgb(error_theme.error_bg),
                                    ));
                                    super::visuals::paint_label(
                                        error_i18n
                                            .format(
                                                "visual-error",
                                                &[("error", error.as_str().into())],
                                            )
                                            .into(),
                                        plot.origin + point(px(6.0), px(3.0)),
                                        error_theme.error,
                                        window,
                                        cx,
                                    );
                                    return;
                                }
                                if let Some(fraction) = progress {
                                    // Slow periodic easing gives the droplet a deliberate rhythm;
                                    // it never snaps at a cycle boundary.
                                    let jitter = cursor_wobble(position) * 0.8;
                                    let cursor_center = plot.center().y + px(jitter);
                                    let scale = window.scale_factor();
                                    let line_width = px((1.5 * scale).round().max(1.0) / scale);
                                    let line_left = px((f32::from(
                                        plot.left() + plot.size.width * fraction - line_width * 0.5,
                                    ) * scale)
                                        .round()
                                        / scale);
                                    let x = line_left + line_width * 0.5;
                                    let cursor_top = (cursor_center - px(22.0)).max(plot.top());
                                    let cursor_bottom =
                                        (cursor_center + px(22.0)).min(plot.bottom());
                                    if glow > 0.0 {
                                        window.paint_quad(fill(
                                            Bounds::new(
                                                point(x - px(2.0), cursor_top),
                                                size(px(4.0), cursor_bottom - cursor_top),
                                            ),
                                            water.alpha(0.06 * glow),
                                        ));
                                    }
                                    // Keep the short line close to the droplet, without
                                    // cutting through its body. Small panels use only the line.
                                    let line_gap = if plot.size.height >= px(24.0) {
                                        px(5.0)
                                    } else {
                                        px(0.0)
                                    };
                                    for (top, bottom) in [
                                        (cursor_top, cursor_center - line_gap),
                                        (cursor_center + line_gap, cursor_bottom),
                                    ] {
                                        if bottom > top {
                                            window.paint_quad(fill(
                                                Bounds::new(
                                                    point(line_left, top),
                                                    size(line_width, bottom - top),
                                                ),
                                                water.alpha(0.7),
                                            ));
                                        }
                                    }
                                    if plot.size.height >= px(24.0) {
                                        if glow > 0.0 {
                                            let mut halo = fill(
                                                Bounds::new(
                                                    point(x - px(5.5), cursor_center - px(6.0)),
                                                    size(px(11.0), px(12.0)),
                                                ),
                                                water.alpha(0.08 * glow),
                                            );
                                            halo.corner_radii = px(5.5).into();
                                            window.paint_quad(halo);
                                        }
                                        window.paint_quad(fill(
                                            Bounds::new(
                                                point(x - px(1.0), cursor_center - px(5.0)),
                                                size(px(2.0), px(2.0)),
                                            ),
                                            water,
                                        ));
                                        let mut droplet = fill(
                                            Bounds::new(
                                                point(x - px(3.5), cursor_center - px(4.0)),
                                                size(px(7.0), px(8.0)),
                                            ),
                                            water,
                                        );
                                        droplet.corner_radii = px(3.5).into();
                                        window.paint_quad(droplet);
                                    }
                                }
                            },
                        )
                        .size_full(),
                    ),
            )
            .when(labels, |panel| {
                panel.child(
                    rivu_ui::row()
                        .absolute()
                        .bottom_0()
                        .left_0()
                        .w_full()
                        .px_2()
                        .justify_between()
                        .text_xs()
                        .text_color(rgb(0xb1bbc5))
                        .child("0:00")
                        .child(
                            label_duration.map_or_else(|| "—".into(), super::super::format_time),
                        ),
                )
            })
            .children(status.map(|message| {
                super::visualization_status(message)
                    .absolute()
                    .top_0()
                    .left_0()
            }))
            .into_any_element()
    }
}

impl Drop for Waveform {
    fn drop(&mut self) {
        self.cancel_full();
    }
}

#[cfg(test)]
mod tests {
    use crate::gui::layout::interaction::{DragCommit, LayoutInteraction, SeekCommit};
    #[test]
    fn removed_panels_retire_without_repainting() {
        let mut plot = WaveformPlot::default();
        let data = frame(vec![Some(0.25); 100]);
        let palette = super::super::visuals::active_palette_lut(
            crate::config::VisualizationPalette::Deadbeef,
        );
        plot.columns(1, 100, &data);
        let scene = plot
            .publication(
                1,
                crate::config::VisualizationPalette::Deadbeef,
                true,
                &palette,
            )
            .unwrap()
            .unwrap();
        let publication = Arc::downgrade(&scene);
        plot.retain_panels(|_| false);
        assert!(plot.columns.is_empty());
        assert!(
            publication.upgrade().is_some(),
            "a captured scene must survive panel removal"
        );
        drop(scene);
        assert!(publication.upgrade().is_none());
    }

    #[test]
    fn uncached_waveform_releases_publications_after_scene_completion() {
        let mut plot = WaveformPlot::default();
        let data = frame(vec![Some(0.25); 100]);
        let palette = super::super::visuals::active_palette_lut(
            crate::config::VisualizationPalette::Deadbeef,
        );
        for _ in 0..32 {
            plot.columns_with_cache(
                1,
                100,
                &data,
                false,
                WaveformMapping {
                    rms_gain: 1.0,
                    peak_gain: 1.0,
                    peak_gamma: 1.0,
                },
            );
            let scene = plot
                .publication(
                    1,
                    crate::config::VisualizationPalette::Deadbeef,
                    false,
                    &palette,
                )
                .unwrap()
                .unwrap();
            let weak = Arc::downgrade(&scene);
            assert!(plot.columns[&1].publication.is_none());
            assert_eq!(plot.columns[&1].values.len(), 100);
            assert_eq!(data.peaks, vec![Some(0.25); 100]);
            assert!(weak.upgrade().is_some());
            drop(scene);
            assert!(weak.upgrade().is_none());
        }
    }

    #[test]
    fn unchanged_columns_reuse_publication_across_resize_and_revisions() {
        let palette_id = crate::config::VisualizationPalette::Deadbeef;
        let palette = super::super::visuals::active_palette_lut(palette_id);
        let mut data = frame(vec![Some(0.25), None, Some(0.0)]);
        let mut plot = WaveformPlot::default();
        plot.columns(1, 100, &data);
        let initial = plot
            .publication(1, palette_id, true, &palette)
            .unwrap()
            .unwrap();
        // Widths above the source bin count, height and position only change placement.
        plot.columns(1, 200, &data);
        let resized = plot
            .publication(1, palette_id, true, &palette)
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&initial, &resized));
        data.revision += 1;
        plot.columns(1, 200, &data);
        let revised = plot
            .publication(1, palette_id, true, &palette)
            .unwrap()
            .unwrap();
        assert!(
            Arc::ptr_eq(&initial, &revised),
            "missing values must compare as unchanged"
        );
        data.left_peaks[1] = Some(0.0);
        data.revision += 1;
        plot.columns(1, 200, &data);
        let silence = plot
            .publication(1, palette_id, true, &palette)
            .unwrap()
            .unwrap();
        assert!(
            !Arc::ptr_eq(&initial, &silence),
            "known silence differs from missing"
        );
        plot.columns(1, 1, &data);
        let pooled = plot
            .publication(1, palette_id, true, &palette)
            .unwrap()
            .unwrap();
        assert!(!Arc::ptr_eq(&silence, &pooled));
        plot.clear_cache();
        plot.columns(1, 1, &data);
        let switched = plot
            .publication(1, palette_id, true, &palette)
            .unwrap()
            .unwrap();
        assert!(
            !Arc::ptr_eq(&pooled, &switched),
            "source/full switches invalidate"
        );
    }

    #[test]
    fn unknown_silence_and_partially_known_transitions_control_publication() {
        let palette_id = crate::config::VisualizationPalette::Deadbeef;
        let palette = super::super::visuals::active_palette_lut(palette_id);
        for cache_enabled in [true, false] {
            let mut plot = WaveformPlot::default();
            let mut data = frame(vec![None; 2]);
            let mapping = WaveformMapping {
                rms_gain: 1.0,
                peak_gain: 1.0,
                peak_gamma: 1.0,
            };
            for known in [None, Some(0.0), None, Some(0.5), None] {
                data.right_peaks[1] = known;
                data.revision += 1;
                plot.columns_with_cache(1, 2, &data, cache_enabled, mapping);
                let published = plot
                    .publication(1, palette_id, cache_enabled, &palette)
                    .unwrap();
                assert_eq!(published.is_some(), known.is_some());
                if let Some(published) = published {
                    let gpui::GpuContent::Waveform { columns, .. } = published.content() else {
                        panic!("expected waveform");
                    };
                    assert!(columns[0].levels.iter().all(|level| level.is_nan()));
                    assert!(columns[1].levels[..3].iter().all(|level| level.is_nan()));
                    assert_eq!(columns[1].levels[3], known.unwrap());
                    let repeated = plot
                        .publication(1, palette_id, cache_enabled, &palette)
                        .unwrap()
                        .unwrap();
                    assert_eq!(Arc::ptr_eq(&published, &repeated), cache_enabled);
                } else {
                    assert!(plot.columns[&1].publication.is_none());
                    assert!(
                        plot.publication(1, palette_id, cache_enabled, &palette)
                            .unwrap()
                            .is_none()
                    );
                }
            }
        }
    }

    #[test]
    fn numerical_publication_preserves_asymmetric_stereo_and_mono_duplication() {
        let palette_id = crate::config::VisualizationPalette::Deadbeef;
        let palette = super::super::visuals::active_palette_lut(palette_id);
        let mut data = frame(vec![Some(0.25), None]);
        data.left_rms[0] = Some(0.2);
        data.left_peaks[0] = Some(0.4);
        data.right_rms[0] = Some(0.6);
        data.right_peaks[0] = Some(0.8);
        let mut plot = WaveformPlot::default();
        plot.columns(1, 2, &data);
        let stereo = plot
            .publication(1, palette_id, true, &palette)
            .unwrap()
            .unwrap();
        let gpui::GpuContent::Waveform { columns, params } = stereo.content() else {
            panic!("expected waveform");
        };
        assert_eq!(
            columns[0].levels,
            [
                waveform_level(0.2, 1.0, 0.86),
                0.4,
                waveform_level(0.6, 1.0, 0.86),
                0.8,
            ]
        );
        assert!(columns[1].levels.iter().all(|value| value.is_nan()));
        assert_eq!(params.palette_start, 0.06);

        data.left_rms.clear();
        data.left_peaks.clear();
        data.left_rms_counts.clear();
        data.right_rms.clear();
        data.right_peaks.clear();
        data.right_rms_counts.clear();
        data.revision += 1;
        plot.columns(1, 2, &data);
        let mono = plot
            .publication(1, palette_id, true, &palette)
            .unwrap()
            .unwrap();
        let gpui::GpuContent::Waveform { columns, .. } = mono.content() else {
            panic!("expected waveform");
        };
        assert_eq!(
            columns[0].levels,
            [
                waveform_level(0.25, 1.0, 0.86),
                0.25,
                waveform_level(0.25, 1.0, 0.86),
                0.25,
            ]
        );
    }

    use super::*;
    #[test]
    fn seek_seconds_clamps_drag_to_timeline_bounds() {
        assert_eq!(seek_seconds(px(8.0), px(8.0), px(100.0), 10.0), 0.0);
        assert_eq!(seek_seconds(px(58.0), px(8.0), px(100.0), 10.0), 5.0);
        assert_eq!(seek_seconds(px(108.0), px(8.0), px(100.0), 10.0), 10.0);
        assert_eq!(seek_seconds(px(200.0), px(8.0), px(100.0), 10.0), 10.0);
        assert_eq!(seek_seconds(px(-20.0), px(8.0), px(100.0), 10.0), 0.0);
    }

    #[test]
    fn cursor_wobble_is_slow_periodic_and_finite() {
        assert_eq!(cursor_wobble(f64::NAN), 0.0);
        assert!((cursor_wobble(0.0) - cursor_wobble(40.0)).abs() < 0.001);
        assert!((cursor_wobble(2.0) - cursor_wobble(0.0)).abs() > 0.01);
        assert!(cursor_wobble(4.0).is_finite());
    }

    fn frame(peaks: Vec<Option<f32>>) -> WaveformFrame {
        let max_peak = peaks.iter().flatten().copied().fold(1.0, f32::max);
        WaveformFrame {
            revision: 1,
            path: Some(PathBuf::from("track.flac")),
            range: None,
            duration: Some(10.0),
            span_seconds: 10.0,
            bin_frames: 80_000,
            bin_count: peaks.len() as u64,
            identity: Some(crate::audio::WaveformIdentity::FullScan {
                duration: Some(10.0),
                sample_rate: 8_000,
            }),
            max_peak,
            bin_revisions: peaks
                .iter()
                .map(|value| u64::from(value.is_some()))
                .collect(),
            peaks: peaks.clone(),
            rms: peaks.clone(),
            rms_counts: peaks
                .iter()
                .map(|value| u64::from(value.is_some()))
                .collect(),
            left_peaks: peaks.clone(),
            right_peaks: peaks.clone(),
            left_rms: peaks.clone(),
            right_rms: peaks.clone(),
            left_rms_counts: peaks
                .iter()
                .map(|value| u64::from(value.is_some()))
                .collect(),
            right_rms_counts: peaks
                .iter()
                .map(|value| u64::from(value.is_some()))
                .collect(),
            channel_weights: Vec::new(),
            complete: false,
            ..WaveformFrame::default()
        }
    }

    fn geometry_frame(
        peaks: Vec<Option<f32>>,
        bin_frames: u128,
        bin_count: u64,
        sample_rate: u32,
    ) -> WaveformFrame {
        let mut result = frame(peaks);
        result.bin_frames = bin_frames;
        result.bin_count = bin_count;
        result.span_seconds = result.peaks.len() as f64 * bin_frames as f64
            / bin_count as f64
            / f64::from(sample_rate);
        result.duration = Some(result.span_seconds);
        result.identity = Some(crate::audio::WaveformIdentity::FullScan {
            duration: result.duration,
            sample_rate,
        });
        result
    }

    fn amplitudes(columns: &[WaveformColumn]) -> Vec<Option<f32>> {
        columns.iter().map(|column| column.left_peak).collect()
    }

    #[test]
    fn resized_columns_preserve_transients_and_track_wide_peak_ratios() {
        let frame = frame(vec![Some(0.0), Some(0.5), Some(2.0), Some(0.25), Some(0.0)]);
        let mut plot = WaveformPlot::default();
        let fine = amplitudes(plot.columns(2, 5, &frame));
        assert_eq!(fine[0], Some(0.0));
        assert_eq!(fine[2], Some(1.0));
        assert_eq!(fine[4], Some(0.0));
        assert!(fine[3].unwrap() < fine[1].unwrap());
        assert!(fine[1].unwrap() < fine[2].unwrap());
        assert_eq!(
            amplitudes(plot.columns(1, 2, &frame)),
            vec![fine[1], fine[2]]
        );
        assert_eq!(plot.columns(1, 1, &frame)[0].left_peak, Some(1.0));
        assert_eq!(amplitudes(plot.columns(2, 5, &frame)), fine);
    }

    #[test]
    fn pooled_rms_weights_boundary_counts_and_matching_channel_fallbacks() {
        let mut data = frame(vec![Some(0.0), Some(1.0), None, Some(0.0), Some(1.0)]);
        data.rms_counts = vec![3, 1, 100, 2, 0];
        data.left_rms.clear();
        // A missing channel RMS uses the mono values and their counts together.
        data.left_rms_counts = vec![999; 5];
        data.right_rms = vec![Some(1.0), Some(0.0), None, Some(0.0), Some(1.0)];
        data.right_rms_counts = vec![1, 3, 100, 2, 0];
        let mut plot = WaveformPlot::default();
        let columns = plot.columns(1, 2, &data);
        let expected = waveform_level(0.5, 1.0, 0.86);
        assert_eq!(columns[0].left_rms, Some(expected));
        assert_eq!(columns[0].right_rms, Some(expected));
        // Unknown and zero-count bins contribute no weight; known silence does.
        assert_eq!(columns[1].left_rms, Some(0.0));
        assert_eq!(columns[1].right_rms, Some(0.0));
        assert!(columns.iter().all(|column| column.left_peak == Some(1.0)));
        assert!(columns.iter().all(|column| column.right_peak == Some(1.0)));
    }

    #[test]
    fn low_amplitude_rms_stays_within_peak_after_display_mapping() {
        for scale in [1.0, 0.5] {
            for (rms, peak) in [(0.1, 0.15), (0.01, 0.02), (0.00025, 0.0005), (0.2, 0.2)] {
                let (rms_height, peak_height) = pooled_side(
                    &[Some(peak)],
                    &[Some(rms)],
                    &[1],
                    0,
                    1,
                    scale,
                    WaveformMapping {
                        rms_gain: 1.0,
                        peak_gain: 1.0,
                        peak_gamma: 0.86,
                    },
                );
                let rms_height = rms_height.unwrap();
                let peak_height = peak_height.unwrap();
                assert!(rms_height > 0.0);
                assert!(peak_height > 0.0);
                assert!(rms_height <= peak_height);
                for half_height in [px(8.0), px(100.0), px(400.0)] {
                    assert!(
                        waveform_height(rms_height, half_height)
                            <= waveform_height(peak_height, half_height)
                    );
                }
                if rms == peak {
                    assert_eq!(rms_height, peak_height);
                }
            }
        }
    }

    #[test]
    fn unknown_intervals_stay_blank_and_known_silence_stays_known() {
        let frame = frame(vec![None, None, Some(0.0), None, Some(0.25), None]);
        let mut plot = WaveformPlot::default();
        let fine = amplitudes(plot.columns(1, 6, &frame));
        assert_eq!(fine[..4], [None, None, Some(0.0), None]);
        assert!(fine[4].unwrap() > 0.0);
        assert_eq!(fine[5], None);
        assert_eq!(
            amplitudes(plot.columns(1, 3, &frame)),
            vec![None, Some(0.0), fine[4]]
        );
        assert_eq!(plot.columns(2, 1, &frame)[0].left_peak, fine[4]);
    }

    #[test]
    fn revised_peaks_update_same_width_and_rescale_existing_columns() {
        let mut frame = frame(vec![Some(0.25), None]);
        let mut plot = WaveformPlot::default();
        let original = amplitudes(plot.columns(1, 2, &frame));
        assert!(original[0].unwrap() > 0.0);
        assert_eq!(original[1], None);
        frame.peaks[1] = Some(2.0);
        frame.left_peaks[1] = Some(2.0);
        frame.right_peaks[1] = Some(2.0);
        frame.max_peak = 2.0;
        frame.revision += 1;
        let revised = amplitudes(plot.columns(1, 2, &frame));
        assert!(revised[0].unwrap() < original[0].unwrap());
        assert_eq!(revised[1], Some(1.0));
    }

    #[test]
    fn zero_and_extreme_widths_are_bounded_and_empty_data_has_no_columns() {
        let data = frame(vec![Some(0.0), None, Some(0.5)]);
        let mut plot = WaveformPlot::default();
        let fine = amplitudes(plot.columns(1, usize::MAX, &data));
        assert_eq!(fine.len(), 3);
        assert_eq!(plot.columns(1, 0, &data).len(), 1);
        assert_eq!(plot.columns(1, 0, &data)[0].left_peak, fine[2]);
        let empty = frame(Vec::new());
        assert!(plot.columns(1, 0, &empty).is_empty());
        assert!(plot.columns(1, usize::MAX, &empty).is_empty());
    }

    #[test]
    fn unavailable_or_different_sources_cannot_reuse_old_peaks() {
        let mut frame = frame(vec![Some(0.0)]);
        let source = Source {
            path: PathBuf::from("track.flac"),
            range: None,
            fingerprint: None,
            decoder_hint: DecoderHint::Auto,
        };
        assert!(preview_available(&source, &frame));
        frame.decoder_hint = DecoderHint::Ffmpeg;
        assert!(preview_available(&source, &frame));
        frame.decoder_hint = DecoderHint::Auto;
        frame.path = Some(PathBuf::from("other.flac"));
        assert!(!preview_available(&source, &frame));
        frame.path = Some(source.path.clone());
        frame.range = Some(PlaybackRange {
            start_seconds: 2.0,
            end_seconds: Some(4.0),
        });
        assert!(!preview_available(&source, &frame));
        frame.range = None;
        frame.peaks[0] = None;
        assert!(!preview_available(&source, &frame));
    }

    #[test]
    fn waveform_seek_keeps_press_duration_and_cancellation_clears_capture() {
        let mut waveform = preview(WaveformFrame::default());
        {
            let mut interaction = waveform.interaction.borrow_mut();
            let bounds = Bounds::new(
                gpui::point(px(10.0), px(0.0)),
                gpui::size(px(100.0), px(20.0)),
            );
            interaction.panels.insert(1, (bounds, Some(20.0)));
            interaction.active_panel = Some(1);
            interaction.seek_duration = Some(20.0);
            interaction.panels.insert(1, (bounds, Some(200.0)));
            assert_eq!(
                interaction.seek_seconds(gpui::point(px(60.0), px(5.0)), None),
                Some(10.0)
            );
        }
        waveform.dragging = true;
        let generation = waveform.seek_generation;
        assert!(waveform.accepts_seek_event(generation));
        waveform.cancel_seek();
        assert!(!waveform.accepts_seek_event(generation));
        assert!(!waveform.dragging);
        assert_eq!(
            waveform
                .interaction
                .borrow()
                .seek_seconds(gpui::point(px(60.0), px(5.0)), None),
            None
        );
    }

    fn captured_seek() -> (Waveform, LayoutInteraction) {
        let mut waveform = preview(WaveformFrame::default());
        waveform.seek_generation = 7;
        waveform.dragging = true;
        {
            let mut interaction = waveform.interaction.borrow_mut();
            interaction
                .panels
                .insert(1, (Bounds::default(), Some(20.0)));
            interaction.active_panel = Some(1);
            interaction.seek_duration = Some(20.0);
        }
        let mut owner = LayoutInteraction::default();
        owner.waveform_preview(10., Some(11), 7);
        (waveform, owner)
    }

    #[test]
    fn hidden_and_unavailable_surfaces_cancel_both_owners_without_committing() {
        for hidden in [true, false] {
            let (mut waveform, mut owner) = captured_seek();
            waveform
                .interaction
                .borrow_mut()
                .panels
                .insert(2, (Bounds::default(), Some(40.0)));
            assert_eq!(waveform.remove_interaction_panel(2), None);
            assert_eq!(waveform.retain_interaction_panels(|id| id == 1), None);
            assert!(waveform.accepts_seek_event(7));
            let cancelled = if hidden {
                waveform.retain_interaction_panels(|_| false)
            } else {
                // This is the same transition used by source/status/timeline early exits.
                waveform.remove_interaction_panel(1)
            }
            .unwrap();
            assert_eq!(cancelled, 7);
            assert!(!waveform.accepts_seek_event(7));
            assert!(!waveform.dragging);
            assert!(owner.cancel_waveform(cancelled));
            assert_eq!(owner.seek_preview(), None);
            assert_eq!(owner.finish_drag(), DragCommit::None);
            assert_eq!(owner.finish_waveform(12., 7), None);
            assert_eq!(waveform.remove_interaction_panel(1), None);
            assert_eq!(waveform.cancel_unreleased_seek(), None);
            assert!(owner.waveform_preview(20., Some(12), 8));
            assert!(!owner.cancel_waveform(cancelled));
            assert_eq!(
                owner.finish_waveform(21., 8),
                Some(SeekCommit {
                    queue_id: 12,
                    seconds: 21.
                })
            );
        }
    }

    #[test]
    fn normal_release_keeps_queued_seek_but_missing_release_is_cancelled() {
        let (mut waveform, mut owner) = captured_seek();
        // Root release may run before the entity listener and must not commit it.
        assert_eq!(owner.finish_drag(), DragCommit::None);
        waveform.release_seek_capture();
        assert_eq!(waveform.cancel_unreleased_seek(), None);
        assert!(waveform.accepts_seek_event(7));
        assert_eq!(
            owner.finish_waveform(12., 7),
            Some(SeekCommit {
                queue_id: 11,
                seconds: 12.
            })
        );
        assert_eq!(owner.finish_waveform(12., 7), None);
        let (mut waveform, mut owner) = captured_seek();
        let cancelled = waveform.cancel_unreleased_seek().unwrap();
        assert!(owner.cancel_waveform(cancelled));
        assert!(!waveform.accepts_seek_event(7));
        assert_eq!(owner.finish_waveform(12., 7), None);
    }

    #[test]
    fn unknown_duration_cursor_uses_the_progressive_span_and_clamps_endpoints() {
        let mut frame = frame(vec![Some(0.25), None]);
        frame.duration = None;
        frame.span_seconds = 20.0;
        assert_eq!(progress(5.0, timeline(&frame, Some(10.0))), Some(0.25));
        assert_eq!(progress(-1.0, timeline(&frame, None)), Some(0.0));
        assert_eq!(progress(25.0, timeline(&frame, None)), Some(1.0));
        assert_eq!(progress(f64::NAN, timeline(&frame, None)), None);
        assert_eq!(progress(1.0, Some(0.0)), None);
        assert_eq!(progress(1.0, Some(f64::INFINITY)), None);
    }

    fn preview(data: WaveformFrame) -> Waveform {
        let source = Source::from_frame(&data).map(Rc::new);
        let mut waveform = Waveform::new(Arc::new(RwLock::new(data)));
        waveform.source = source;
        waveform
    }

    #[test]
    fn disabling_cancels_full_scan_and_retires_display_before_resume() {
        let data = frame(vec![Some(0.25), None]);
        let mut waveform = preview(data);
        waveform.revision = Some(1);
        let (generation, cancelled) = waveform.start_full(DecoderHint::Auto);
        let palette = super::super::visuals::active_palette_lut(
            crate::config::VisualizationPalette::Deadbeef,
        );
        let cached = {
            let mut plot = waveform.plot.borrow_mut();
            plot.columns(1, 100, &waveform.shared.read());
            let frame = plot
                .publication(
                    1,
                    crate::config::VisualizationPalette::Deadbeef,
                    true,
                    &palette,
                )
                .unwrap()
                .unwrap();
            Arc::downgrade(&frame)
        };
        waveform.dragging = true;
        {
            let mut interaction = waveform.interaction.borrow_mut();
            interaction
                .panels
                .insert(1, (Bounds::default(), Some(10.0)));
            interaction.active_panel = Some(1);
            interaction.seek_duration = Some(10.0);
        }
        waveform.set_enabled(false);
        waveform.set_enabled(false);
        assert!(cancelled.load(Ordering::Relaxed));
        assert!(waveform.manual.is_none());
        assert!(!waveform.dragging);
        assert!(waveform.interaction.borrow().panels.is_empty());
        assert!(waveform.interaction.borrow().active_panel.is_none());
        assert!(waveform.interaction.borrow().seek_duration.is_none());
        assert!(waveform.revision.is_none());
        assert!(waveform.source.is_none());
        {
            let plot = waveform.plot.borrow();
            assert_eq!(plot.columns.capacity(), 0);
            assert!(cached.upgrade().is_none());
        }
        waveform.set_enabled(true);
        assert!(waveform.manual.is_none());
        assert!(!waveform.finish_full(generation, Ok(frame(vec![Some(1.0), Some(1.0)]))));
    }

    #[test]
    fn backend_still_transition_rejects_manual_results_before_gui_cancellation() {
        for (resume_before_completion, decoder_cancelled) in
            [(false, false), (false, true), (true, false), (true, true)]
        {
            let mut waveform = preview(frame(vec![Some(0.25), None]));
            let (request, cancelled) = waveform.start_full(DecoderHint::Auto);
            waveform.shared.write().set_still_mode(true);
            assert!(cancelled.load(Ordering::Relaxed));
            if resume_before_completion {
                waveform.shared.write().set_still_mode(false);
            }
            let revision = waveform.shared.read().revision;
            let result = if decoder_cancelled {
                Err(anyhow::anyhow!("Waveform loading cancelled"))
            } else {
                Ok(frame(vec![Some(1.0), Some(1.0)]))
            };
            assert!(waveform.finish_full(request, result));
            assert!(waveform.manual.is_none());
            assert!(waveform.manual_error.is_none());
            assert_eq!(waveform.shared.read().peaks.capacity(), 0);
            assert_eq!(waveform.shared.read().revision, revision);
        }
    }

    #[test]
    fn backend_still_on_off_retires_completed_full_preview_without_rescanning() {
        let mut waveform = preview(frame(vec![Some(0.25), None]));
        let track = track("track.flac", DecoderHint::Auto);
        assert!(waveform.sync_state(Some(&track)));
        let (request, _) = waveform.start_full(DecoderHint::Auto);
        assert!(waveform.finish_full(request, Ok(frame(vec![Some(0.5), Some(0.75)]))));
        let completed = Rc::downgrade(waveform.full.as_ref().unwrap());
        let palette = super::super::visuals::active_palette_lut(
            crate::config::VisualizationPalette::Deadbeef,
        );
        let cached = {
            let shared = waveform.shared.read();
            let full = waveform.display_frame(&shared);
            assert!(full.complete);
            assert_eq!(full.peaks, vec![Some(0.5), Some(0.75)]);
            let mut plot = waveform.plot.borrow_mut();
            plot.columns(1, 100, full);
            let frame = plot
                .publication(
                    1,
                    crate::config::VisualizationPalette::Deadbeef,
                    true,
                    &palette,
                )
                .unwrap()
                .unwrap();
            Arc::downgrade(&frame)
        };
        {
            let mut shared = waveform.shared.write();
            shared.set_still_mode(true);
            shared.set_still_mode(false);
        }
        assert!(
            waveform.enabled,
            "GUI did not observe the intermediate Still Mode"
        );
        {
            let shared = waveform.shared.read();
            assert!(std::ptr::eq(waveform.display_frame(&shared), &*shared));
        }
        assert!(waveform.sync_state(Some(&track)));
        assert!(completed.upgrade().is_none());
        assert!(waveform.full.is_none());
        assert!(waveform.manual.is_none());
        assert!(waveform.manual_error.is_none());
        {
            let plot = waveform.plot.borrow();
            assert!(plot.columns[&1].values.is_empty());
            assert!(plot.columns[&1].publication.is_none());
            assert!(cached.upgrade().is_none());
        }
        assert!(!waveform.sync_state(Some(&track)));
        assert!(waveform.manual.is_none(), "ordinary sync must not rescan");
        assert!(waveform.full.is_none());
    }

    #[test]
    fn scan_registered_while_suspended_stays_cancelled_after_resume() {
        for decoder_cancelled in [false, true] {
            let mut waveform = preview(frame(vec![Some(0.25), None]));
            waveform.shared.write().set_still_mode(true);
            let (request, cancelled) = waveform.start_full(DecoderHint::Auto);
            assert!(cancelled.load(Ordering::Relaxed));
            waveform.shared.write().set_still_mode(false);
            let revision = waveform.shared.read().revision;
            let result = if decoder_cancelled {
                Err(anyhow::anyhow!("Waveform loading cancelled"))
            } else {
                Ok(frame(vec![Some(1.0), Some(1.0)]))
            };
            assert!(waveform.finish_full(request, result));
            assert!(waveform.manual.is_none());
            assert!(waveform.manual_error.is_none());
            assert_eq!(waveform.shared.read().peaks.capacity(), 0);
            assert_eq!(waveform.shared.read().revision, revision);
        }
    }

    fn track(path: &str, decoder_hint: DecoderHint) -> Track {
        Track {
            id: 1,
            path: PathBuf::from(path),
            fingerprint: None,
            cue: None,
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            duration: Some(10.0),
            codec: String::new(),
            channels: 2,
            sample_rate: 44100,
            bitrate_bps: None,
            bitrate_exact: false,
            track_number: None,
            disc_number: None,
            bits_per_sample: None,
            release_date: None,
            favorite: false,
            decoder_hint,
            missing: false,
            play_count: 0,
            last_played: None,
        }
    }

    fn cue_track() -> Track {
        let mut track = track("track.flac", DecoderHint::Ffmpeg);
        track.cue = Some(crate::model::CueSegment {
            sheet: PathBuf::from("track.cue"),
            number: 2,
            start_frame: 150,
            end_frame: Some(300),
        });
        track
    }

    #[test]
    fn explicit_reload_replaces_pending_request_with_latest_hint() {
        let mut waveform = preview(frame(vec![Some(0.25), None]));
        let (first, cancelled) = waveform.start_full(DecoderHint::Auto);
        let (latest, _) = waveform.start_full(DecoderHint::Ffmpeg);
        assert!(cancelled.load(Ordering::Relaxed));
        assert_eq!(
            waveform.manual.as_ref().unwrap().source.decoder_hint,
            DecoderHint::Ffmpeg
        );
        assert!(!waveform.finish_full(first, Ok(frame(vec![Some(1.0), Some(1.0)]))));
        assert!(waveform.manual.is_some());
        let mut result = frame(vec![Some(0.5), Some(0.75)]);
        result.decoder_hint = DecoderHint::Ffmpeg;
        assert!(waveform.finish_full(latest, Ok(result)));
        assert_eq!(waveform.shared.read().peaks, vec![Some(0.25), None]);
        assert_eq!(
            waveform.full.as_ref().unwrap().frame.peaks,
            vec![Some(0.5), Some(0.75)]
        );
        assert!(!waveform.finish_full(latest, Ok(frame(vec![Some(1.0), Some(1.0)]))));
        assert_eq!(
            waveform.full.as_ref().unwrap().frame.peaks,
            vec![Some(0.5), Some(0.75)]
        );
    }

    #[test]
    fn loading_and_failures_preserve_progressive_frame() {
        let mut waveform = preview(frame(vec![Some(0.25), None]));
        let (generation, _) = waveform.start_full(DecoderHint::Auto);
        assert_eq!(waveform.shared.read().peaks, vec![Some(0.25), None]);
        assert!(waveform.manual_message(&Localizer::new("en-US")).is_some());
        assert!(waveform.finish_full(generation, Err(anyhow::anyhow!("decode failed"))));
        assert!(
            waveform
                .manual_error
                .as_ref()
                .unwrap()
                .contains("decode failed")
        );
        assert!(waveform.full.is_none());
        let (generation, _) = waveform.start_full(DecoderHint::Auto);
        waveform.shared.write().revision = 40;
        assert!(waveform.finish_full(generation, Ok(frame(vec![Some(0.5), Some(0.75)]))));
        let shared = waveform.shared.read();
        assert_eq!(shared.revision, 40);
        assert!(!shared.complete);
        assert_eq!(shared.peaks, vec![Some(0.25), None]);
        assert!(waveform.full.as_ref().unwrap().frame.complete);
        assert!(waveform.manual_error.is_none());
    }

    #[test]
    fn late_full_scan_preserves_new_pcm_and_remains_progressively_overwritable() {
        let mut waveform = preview(frame(vec![Some(1.0), None]));
        let (request, _) = waveform.start_full(DecoderHint::Auto);
        let mut fresh = frame(vec![Some(0.25), None]);
        fresh.revision = 2;
        fresh.bin_revisions = vec![2, 0];
        *waveform.shared.write() = fresh;
        assert!(waveform.finish_full(request, Ok(frame(vec![Some(1.0), Some(0.75)]))));
        {
            let shared = waveform.shared.read();
            let display = waveform.display_frame(&shared);
            assert_eq!(display.peaks, [Some(0.25), Some(0.75)]);
            assert_eq!(display.rms, [Some(0.25), Some(0.75)]);
            assert_eq!(display.left_peaks, [Some(0.25), Some(0.75)]);
        }
        let mut replay = frame(vec![Some(0.125), Some(0.0625)]);
        replay.revision = 3;
        replay.bin_revisions = vec![2, 3];
        *waveform.shared.write() = replay;
        waveform.sync_state(Some(&track("track.flac", DecoderHint::Auto)));
        let shared = waveform.shared.read();
        let display = waveform.display_frame(&shared);
        // Bin zero was not revisited in revision three.
        assert_eq!(display.peaks, [Some(0.25), Some(0.0625)]);
        assert_eq!(display.rms, [Some(0.25), Some(0.0625)]);
        assert_eq!(display.right_rms_counts, [1, 1]);
    }

    #[test]
    fn new_full_request_and_failure_keep_the_existing_rolling_display() {
        let mut waveform = preview(frame(vec![Some(0.25), None]));
        let (first, _) = waveform.start_full(DecoderHint::Auto);
        assert!(waveform.finish_full(first, Ok(frame(vec![Some(0.5), Some(0.75)]))));
        let (obsolete, cancelled) = waveform.start_full(DecoderHint::Auto);
        let (latest, _) = waveform.start_full(DecoderHint::Auto);
        assert!(cancelled.load(Ordering::Relaxed));
        assert!(!waveform.finish_full(obsolete, Ok(frame(vec![Some(1.0); 2]))));
        assert!(waveform.finish_full(latest, Err(anyhow::anyhow!("decode failed"))));
        let shared = waveform.shared.read();
        assert_eq!(
            waveform.display_frame(&shared).peaks,
            [Some(0.5), Some(0.75)]
        );
        assert!(
            waveform
                .manual_error
                .as_ref()
                .unwrap()
                .contains("decode failed")
        );
    }

    #[test]
    fn full_after_progressive_installs_baseline_then_maps_different_geometries() {
        let mut waveform = preview(frame(vec![Some(0.25), None]));
        let (request, _) = waveform.start_full(DecoderHint::Auto);
        assert!(waveform.finish_full(request, Ok(frame(vec![Some(1.0); 4]))));
        let mut progress = frame(vec![Some(0.125), None]);
        progress.revision = 2;
        progress.bin_revisions = vec![2, 0];
        *waveform.shared.write() = progress;
        waveform.sync_state(Some(&track("track.flac", DecoderHint::Auto)));
        let shared = waveform.shared.read();
        let display = waveform.display_frame(&shared);
        assert_eq!(
            display.peaks,
            [Some(0.125), Some(0.125), Some(1.0), Some(1.0)]
        );
        assert_eq!(display.left_rms, display.peaks);
        assert_eq!(display.span_seconds, 10.0);
    }

    #[test]
    fn coarse_full_bins_pool_new_packets_but_do_not_mix_traversals() {
        let mut display = frame(vec![Some(1.0); 2]);
        let mut progress = frame(vec![Some(0.25), None, None, None]);
        progress.bin_revisions = vec![2, 0, 0, 0];
        progress.bin_epochs = vec![2, 0, 0, 0];
        merge_progressive(&mut display, &progress, 1, 1);
        assert_eq!(display.left_rms[0], Some(0.25));
        let mut next = frame(vec![Some(0.25), Some(0.75), None, None]);
        next.bin_revisions = vec![2, 3, 0, 0];
        next.bin_epochs = vec![2, 2, 0, 0];
        merge_progressive(&mut display, &next, 2, 1);
        assert_eq!(display.peaks[0], Some(0.75));
        assert!((display.left_rms[0].unwrap() - (0.3125_f32).sqrt()).abs() < 1e-6);
        assert_eq!(display.left_rms_counts[0], 2);
        let mut replay = frame(vec![Some(0.125), Some(0.75), None, None]);
        replay.bin_revisions = vec![4, 3, 0, 0];
        replay.bin_epochs = vec![3, 2, 0, 0];
        merge_progressive(&mut display, &replay, 3, 1);
        assert_eq!(display.peaks, [Some(0.125), Some(1.0)]);
        assert_eq!(display.left_rms[0], Some(0.125));
        assert_eq!(display.left_rms_counts[0], 1);
    }

    #[test]
    fn identical_geometry_preserves_the_unvisited_neighbor_at_point_three_seconds() {
        for source_bins in [1024, 4] {
            let mut display = geometry_frame(vec![Some(1.0); 1024], 1_024_000, 1024, 10_000);
            let mut progress =
                geometry_frame(vec![Some(0.125); source_bins], 1_024_000, 1024, 10_000);
            progress.bin_revisions[3] = 2;
            progress.bin_epochs = vec![2; source_bins];
            progress.rms_spans = vec![None; source_bins];
            progress.rms_spans[3] = Some(3_000..4_000);
            progress.rms_counts[3] = 700;
            progress.left_rms_counts[3] = 700;
            progress.right_rms_counts[3] = 700;
            merge_progressive(&mut display, &progress, 1, 1);

            let mut expected = vec![Some(1.0); 1024];
            expected[3] = Some(0.125);
            for values in [
                &display.peaks,
                &display.rms,
                &display.left_peaks,
                &display.right_peaks,
                &display.left_rms,
                &display.right_rms,
            ] {
                assert_eq!(values, &expected);
            }
            let mut expected_counts = vec![1; 1024];
            expected_counts[3] = 700;
            for counts in [
                &display.rms_counts,
                &display.left_rms_counts,
                &display.right_rms_counts,
            ] {
                assert_eq!(counts, &expected_counts);
            }
            assert_eq!(display.span_seconds, 102.4);
        }
    }

    #[test]
    fn rational_different_geometries_exclude_adjacent_bins_and_weight_partial_overlap() {
        let mut display = geometry_frame(vec![Some(1.0); 512], 512_000, 512, 10_000);
        // Source bins are 1/15 second at another sample rate; display bins are 1/10.
        let mut progress = geometry_frame(vec![Some(0.25); 768], 768_000, 768, 15_000);
        progress.bin_epochs = vec![1; 768];
        progress.bin_revisions[6] = 2;
        progress.bin_epochs[6] = 2;
        progress.rms_counts.fill(600);
        progress.left_rms_counts.fill(600);
        progress.right_rms_counts.fill(600);
        merge_progressive(&mut display, &progress, 1, 1);
        assert_eq!(display.peaks[3], Some(1.0));
        assert_eq!(display.peaks[4], Some(0.25));
        assert_eq!(display.left_rms_counts[4], 600);

        for values in [
            &mut progress.peaks,
            &mut progress.rms,
            &mut progress.left_peaks,
            &mut progress.right_peaks,
            &mut progress.left_rms,
            &mut progress.right_rms,
        ] {
            values[7] = Some(0.75);
        }
        progress.bin_revisions[7] = 3;
        progress.bin_epochs[7] = 2;
        merge_progressive(&mut display, &progress, 2, 1);
        // Display bin four includes all 600 samples of source bin six and half
        // of source bin seven's 600 samples, not the old scan or another epoch.
        let expected_rms = ((0.25_f32.powi(2) * 600.0 + 0.75_f32.powi(2) * 300.0) / 900.0).sqrt();
        for values in [&display.rms, &display.left_rms, &display.right_rms] {
            assert!((values[4].unwrap() - expected_rms).abs() < 1e-6);
            assert_eq!(values[3], Some(1.0));
            assert_eq!(values[5], Some(0.75));
            assert_eq!(values[6], Some(1.0));
        }
        for counts in [
            &display.rms_counts,
            &display.left_rms_counts,
            &display.right_rms_counts,
        ] {
            assert_eq!(&counts[3..7], &[1, 900, 300, 1]);
        }
        assert_eq!(
            &display.peaks[3..7],
            &[Some(1.0), Some(0.75), Some(0.75), Some(1.0)]
        );
    }

    #[test]
    fn geometry_growth_remaps_without_spilling_peaks_then_accepts_direct_updates() {
        let mut old = vec![Some(0.125); 1024];
        old[6] = Some(1.0);
        let mut display = geometry_frame(old, 409_600, 1024, 8_000);
        let mut progress = geometry_frame(vec![Some(0.25); 1024], 1_024_000, 1024, 10_000);
        progress.bin_epochs = vec![1; 1024];
        progress.bin_revisions[600] = 2;
        progress.bin_epochs[600] = 2;
        progress.rms_spans = vec![None; 1024];
        progress.rms_spans[600] = Some(600_000..601_000);
        merge_progressive(&mut display, &progress, 1, 1);
        assert_eq!(display.span_seconds, 102.4);
        for values in [&display.peaks, &display.left_peaks, &display.right_peaks] {
            assert_eq!(values[2], Some(0.125));
            assert_eq!(values[3], Some(1.0));
            assert_eq!(values[600], Some(0.25));
        }
        let pooled_rms = ((1.0_f32 + 0.125_f32.powi(2)) / 2.0).sqrt();
        for values in [&display.rms, &display.left_rms, &display.right_rms] {
            assert_eq!(values[2], Some(0.125));
            assert!((values[3].unwrap() - pooled_rms).abs() < 1e-6);
        }
        assert_eq!(&display.left_rms_counts[2..4], &[2, 2]);

        for values in [
            &mut progress.peaks,
            &mut progress.rms,
            &mut progress.left_peaks,
            &mut progress.right_peaks,
            &mut progress.left_rms,
            &mut progress.right_rms,
        ] {
            values[601] = Some(0.375);
        }
        progress.bin_revisions[601] = 3;
        progress.bin_epochs[601] = 2;
        progress.rms_spans[601] = Some(601_000..602_000);
        merge_progressive(&mut display, &progress, 2, 1);
        assert_eq!(display.peaks[2], Some(0.125));
        assert_eq!(display.peaks[3], Some(1.0));
        assert_eq!(&display.peaks[600..602], &[Some(0.25), Some(0.375)]);
        assert_eq!(&display.left_rms_counts[600..602], &[1, 1]);
    }

    #[test]
    fn scan_provenance_retires_a_preplay_baseline_when_actual_source_changes() {
        let original = tempfile::tempfile().unwrap();
        let replacement = tempfile::tempfile().unwrap();
        let original = crate::audio::SourceIdentity::from_file(&original).unwrap();
        let replacement = crate::audio::SourceIdentity::from_file(&replacement).unwrap();
        let mut waveform = preview(frame(vec![None; 2]));
        let (request, _) = waveform.start_full(DecoderHint::Auto);
        let mut baseline = frame(vec![Some(0.75); 2]);
        baseline.scan_source_identity = Some(original);
        assert!(waveform.finish_full(request, Ok(baseline)));
        {
            let mut shared = waveform.shared.write();
            shared.identity = Some(crate::audio::WaveformIdentity::Progressive {
                content: replacement,
                ffmpeg: false,
                duration: Some(10.0),
                sample_rate: 8_000,
                channels: 1,
            });
            shared.revision += 1;
        }
        assert!(waveform.sync_state(Some(&track("track.flac", DecoderHint::Auto))));
        assert!(waveform.full.is_none());
        let shared = waveform.shared.read();
        assert!(std::ptr::eq(waveform.display_frame(&shared), &*shared));
    }

    #[test]
    fn full_completion_rejects_wrong_path_cue_and_decoder_hint() {
        for wrong in 0..3 {
            let mut waveform = preview(frame(vec![Some(0.25), None]));
            let (generation, _) = waveform.start_full(DecoderHint::Auto);
            let mut result = frame(vec![Some(1.0), Some(1.0)]);
            match wrong {
                0 => result.path = Some(PathBuf::from("other.flac")),
                1 => result.range = track_range(&cue_track()),
                _ => result.decoder_hint = DecoderHint::Ffmpeg,
            }
            assert!(waveform.finish_full(generation, Ok(result)));
            assert!(waveform.full.is_none());
            assert!(waveform.manual_error.is_some());
            assert_eq!(waveform.shared.read().peaks, vec![Some(0.25), None]);
        }
    }

    #[test]
    fn selected_unplayed_preview_does_not_replace_playback_frame() {
        let mut waveform = preview(frame(vec![Some(0.25), None]));
        assert!(waveform.sync_source(Some(&track("selected.flac", DecoderHint::Ffmpeg))));
        let (generation, _) = waveform.start_full(DecoderHint::Ffmpeg);
        let mut selected = frame(vec![Some(0.5), Some(0.75)]);
        selected.path = Some(PathBuf::from("selected.flac"));
        selected.decoder_hint = DecoderHint::Ffmpeg;
        assert!(waveform.finish_full(generation, Ok(selected)));
        let shared = waveform.shared.read();
        assert_eq!(
            shared.path.as_deref(),
            Some(std::path::Path::new("track.flac"))
        );
        assert_eq!(shared.peaks, vec![Some(0.25), None]);
        assert!(preview_available(
            waveform.source.as_deref().unwrap(),
            waveform.display_frame(&shared)
        ));
        assert_eq!(
            waveform.display_frame(&shared).peaks,
            vec![Some(0.5), Some(0.75)]
        );
    }

    #[test]
    fn saved_hint_change_cancels_matching_scan_and_restores_progressive_display() {
        for complete in [false, true] {
            let mut waveform = preview(frame(vec![Some(0.25), None]));
            let (generation, cancelled) = waveform.start_full(DecoderHint::Auto);
            let completed = if complete {
                assert!(waveform.finish_full(generation, Ok(frame(vec![Some(0.5), Some(0.75)]))));
                Some(Rc::downgrade(waveform.full.as_ref().unwrap()))
            } else {
                None
            };
            assert!(!waveform.invalidate_saved_hint(&track("other.flac", DecoderHint::Ffmpeg)));
            assert!(!waveform.invalidate_saved_hint(&cue_track()));
            assert!(waveform.invalidate_saved_hint(&track("track.flac", DecoderHint::Ffmpeg)));
            assert!(complete || cancelled.load(Ordering::Relaxed));
            assert!(waveform.manual.is_none());
            assert!(waveform.full.is_none());
            assert!(waveform.manual_error.is_none());
            assert!(completed.is_none_or(|completed| completed.upgrade().is_none()));
            assert!(!waveform.finish_full(generation, Ok(frame(vec![Some(1.0), Some(1.0)]))));
            let shared = waveform.shared.read();
            assert_eq!(shared.decoder_hint, DecoderHint::Auto);
            assert_eq!(shared.revision, 1);
            assert!(std::ptr::eq(waveform.display_frame(&shared), &*shared));
        }
    }

    #[test]
    fn ordinary_hint_sync_does_not_change_pending_or_completed_preview() {
        let mut waveform = preview(frame(vec![Some(0.25), None]));
        let saved = track("track.flac", DecoderHint::Ffmpeg);
        let (generation, cancelled) = waveform.start_full(DecoderHint::Auto);
        assert!(!waveform.sync_source(Some(&saved)));
        assert!(!cancelled.load(Ordering::Relaxed));
        assert!(waveform.finish_full(generation, Ok(frame(vec![Some(0.5), Some(0.75)]))));
        let completed = Rc::clone(waveform.full.as_ref().unwrap());
        assert!(!waveform.sync_source(Some(&saved)));
        assert!(Rc::ptr_eq(waveform.full.as_ref().unwrap(), &completed));
        assert!(waveform.invalidate_saved_hint(&saved));
        assert!(!waveform.sync_source(Some(&saved)));
        let shared = waveform.shared.read();
        assert!(preview_available(
            waveform.source.as_deref().unwrap(),
            waveform.display_frame(&shared)
        ));
        assert_eq!(
            waveform.source.as_ref().unwrap().decoder_hint,
            DecoderHint::Auto
        );
    }

    #[test]
    fn changed_content_cancels_pending_and_completed_previews_at_the_same_path() {
        for complete in [false, true] {
            for (before, after) in [
                (Some("old"), Some("new")),
                (None, Some("new")),
                (Some("old"), None),
            ] {
                let mut original = track("track.flac", DecoderHint::Auto);
                original.fingerprint = before.map(str::to_owned);
                let mut waveform = preview(frame(vec![Some(0.25), None]));
                waveform.sync_state(Some(&original));
                let mut query = crate::gui::FullWaveformRequest::default();
                let query_generation = query.begin(&original);
                let (generation, cancelled) = waveform.start_full(DecoderHint::Auto);
                let completed = if complete {
                    assert!(waveform.finish_full(generation, Ok(frame(vec![Some(0.5); 2]))));
                    Some(Rc::downgrade(waveform.full.as_ref().unwrap()))
                } else {
                    None
                };
                let mut replaced = original.clone();
                replaced.fingerprint = after.map(str::to_owned);
                assert!(!query.accepts(query_generation, Some(&replaced)));
                query.retain_target(Some(&replaced), false);
                assert!(!query.accepts(query_generation, Some(&original)));
                assert!(waveform.sync_state(Some(&replaced)));
                assert!(complete || cancelled.load(Ordering::Relaxed));
                assert!(waveform.manual.is_none());
                assert!(waveform.full.is_none());
                assert!(waveform.manual_error.is_none());
                assert!(completed.is_none_or(|completed| completed.upgrade().is_none()));
                assert!(!waveform.finish_full(generation, Ok(frame(vec![Some(1.0); 2]))));
                let shared = waveform.shared.read();
                assert_eq!(shared.peaks, vec![Some(0.25), None]);
                assert!(std::ptr::eq(waveform.display_frame(&shared), &*shared));
            }
        }
    }

    #[test]
    fn content_identity_is_checked_at_completion_and_display() {
        for complete in [false, true] {
            let mut original = track("track.flac", DecoderHint::Auto);
            original.fingerprint = Some("old".into());
            let mut waveform = preview(frame(vec![Some(0.25), None]));
            waveform.sync_state(Some(&original));
            let (generation, _) = waveform.start_full(DecoderHint::Auto);
            if complete {
                assert!(waveform.finish_full(generation, Ok(frame(vec![Some(0.5); 2]))));
            }
            // Exercise the install/display guards independently of sync's cancellation.
            Rc::make_mut(waveform.source.as_mut().unwrap()).fingerprint = Some("new".into());
            if !complete {
                assert!(waveform.finish_full(generation, Ok(frame(vec![Some(1.0); 2]))));
                assert!(waveform.full.is_none());
            }
            let shared = waveform.shared.read();
            assert!(std::ptr::eq(waveform.display_frame(&shared), &*shared));
            assert!(waveform.full_preview(&shared).is_none());
        }
    }

    #[gpui::test]
    fn presentation_hiding_retains_pending_and_completed_full_previews(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::gui::presentation::{Presentation, PresentationState};

        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        Config {
            volume: 0.0,
            ffmpeg_enabled: false,
            mpris_enabled: false,
            ..Config::default()
        }
        .save(&config_path)
        .unwrap();
        let runtime = crate::core::Runtime::start(directory.path(), &config_path).unwrap();
        let config = runtime.handle.config_snapshot();
        let window = cx.add_empty_window();
        let waveform = window.update(|_, cx| cx.new(|_| preview(frame(vec![Some(0.25), None]))));
        let owner = window.update(|_, cx| {
            cx.new(|cx| {
                Presentation::new(
                    runtime.handle.clone(),
                    Arc::new(Localizer::new("en-US")),
                    waveform.clone(),
                    cx,
                )
            })
        });
        let track = track("track.flac", DecoderHint::Auto);
        let panel = crate::gui::layout::Panel {
            id: 1,
            kind: "waveform".into(),
            show_tab_bar: true,
            audio_visualizer: None,
            album_artwork: None,
        };
        window.update(|_window, cx| {
            let panels = [&panel];
            let state = |visible, has_panel| PresentationState {
                config: Arc::clone(&config),
                panels: if has_panel { &panels } else { &[] },
                status: crate::model::PlaybackStatus::Paused,
                visible,
                shutting_down: false,
                track: Some(&track),
                position: 0.0,
                duration: track.duration,
            };
            owner.update(cx, |owner, cx| owner.sync(state(true, true), cx));
            for complete in [false, true] {
                let (generation, cancelled) =
                    waveform.update(cx, |waveform, _| waveform.start_full(DecoderHint::Auto));
                if complete {
                    waveform.update(cx, |waveform, _| {
                        assert!(waveform.finish_full(generation, Ok(frame(vec![Some(0.5); 2]))));
                    });
                }
                for (visible, has_panel, release) in [
                    (true, false, false),
                    (true, true, false),
                    (false, true, false),
                    (false, true, true),
                    (true, true, false),
                ] {
                    owner.update(cx, |owner, cx| {
                        owner.sync(state(visible, has_panel), cx);
                        if release {
                            owner.release_display_resources(cx);
                        }
                    });
                    assert!(!cancelled.load(Ordering::Relaxed));
                    let waveform = waveform.read(cx);
                    if complete {
                        assert_eq!(
                            waveform.full.as_ref().unwrap().frame.peaks,
                            vec![Some(0.5); 2]
                        );
                    } else {
                        assert!(waveform.manual.is_some());
                    }
                }
                if !complete {
                    waveform.update(cx, |waveform, _| {
                        assert!(waveform.finish_full(generation, Ok(frame(vec![Some(0.5); 2]))));
                    });
                }
            }
            let mut still = config.as_ref().clone();
            still.still_mode = true;
            owner.update(cx, |owner, cx| {
                let mut state = state(true, true);
                state.config = Arc::new(still);
                owner.sync(state, cx);
            });
            assert!(waveform.read(cx).full.is_none());
            assert!(waveform.read(cx).manual.is_none());
        });
    }

    #[test]
    fn same_content_metadata_and_hidden_panels_retain_manual_preview() {
        let mut original = track("track.flac", DecoderHint::Auto);
        original.fingerprint = Some("same-content".into());
        let mut waveform = preview(frame(vec![Some(0.25), None]));
        waveform.sync_state(Some(&original));
        let source = Rc::clone(waveform.source.as_ref().unwrap());
        let mut query = crate::gui::FullWaveformRequest::default();
        let query_generation = query.begin(&original);
        let (generation, cancelled) = waveform.start_full(DecoderHint::Auto);
        let mut edited = original.clone();
        edited.title = "renamed".into();
        edited.favorite = true;
        edited.play_count += 1;
        edited.decoder_hint = DecoderHint::Ffmpeg;
        for complete in [false, true] {
            query.retain_target(Some(&edited), false);
            assert!(query.accepts(query_generation, Some(&edited)));
            assert!(!waveform.sync_state(Some(&edited)));
            // These are the same owners retained by sync when no panel is visible.
            waveform.plot.borrow_mut().retain_panels(|_| false);
            assert_eq!(waveform.retain_interaction_panels(|_| false), None);
            assert!(!cancelled.load(Ordering::Relaxed));
            assert!(Rc::ptr_eq(waveform.source.as_ref().unwrap(), &source));
            if !complete {
                assert!(waveform.manual.is_some());
                assert!(waveform.finish_full(generation, Ok(frame(vec![Some(0.5); 2]))));
            } else {
                let shared = waveform.shared.read();
                assert_eq!(waveform.display_frame(&shared).peaks, vec![Some(0.5); 2]);
            }
        }
        query.saved_hint_changed(&edited);
        assert!(!query.accepts(query_generation, Some(&original)));
        assert!(waveform.invalidate_saved_hint(&edited));
        let shared = waveform.shared.read();
        assert_eq!(shared.decoder_hint, DecoderHint::Auto);
        assert!(preview_available(
            waveform.source.as_deref().unwrap(),
            &shared
        ));
        assert!(std::ptr::eq(waveform.display_frame(&shared), &*shared));
    }

    #[test]
    fn target_path_or_cue_switch_cancels_pending_and_clears_completed_preview() {
        for complete in [false, true] {
            for changed in [track("other.flac", DecoderHint::Auto), cue_track()] {
                let mut waveform = preview(frame(vec![Some(0.25), None]));
                let (generation, cancelled) = waveform.start_full(DecoderHint::Auto);
                if complete {
                    assert!(
                        waveform.finish_full(generation, Ok(frame(vec![Some(0.5), Some(0.75)])))
                    );
                }
                assert!(waveform.sync_source(Some(&changed)));
                assert!(complete || cancelled.load(Ordering::Relaxed));
                assert!(waveform.manual.is_none());
                assert!(waveform.full.is_none());
                assert!(!waveform.finish_full(generation, Ok(frame(vec![Some(1.0), Some(1.0)]))));
            }
        }
    }

    #[test]
    fn disabling_releases_completed_preview_without_touching_playback() {
        let mut waveform = preview(frame(vec![Some(0.25), None]));
        let (generation, _) = waveform.start_full(DecoderHint::Auto);
        assert!(waveform.finish_full(generation, Ok(frame(vec![Some(0.5), Some(0.75)]))));
        let completed = Rc::downgrade(waveform.full.as_ref().unwrap());
        waveform.set_enabled(false);
        assert!(completed.upgrade().is_none());
        assert_eq!(waveform.shared.read().peaks, vec![Some(0.25), None]);
        waveform.set_enabled(true);
        assert!(waveform.full.is_none());
    }

    #[test]
    fn dropping_preview_cancels_an_explicit_scan() {
        let mut waveform = preview(frame(vec![Some(0.25), None]));
        let (_, cancelled) = waveform.start_full(DecoderHint::Auto);
        drop(waveform);
        assert!(cancelled.load(Ordering::Relaxed));
    }
}
