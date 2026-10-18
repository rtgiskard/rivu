//! Progressive whole-track amplitude previews from the playback decoder.
//!
//! The audio worker owns bounded progressive bins. Explicit full previews own
//! separate bounded results, never replacing playback's frame. Content changes,
//! saved decoder hint edits or target switches cancel previews; hiding does not.
//! Still Mode cancels scans and releases both owners' display data.
//!
//! Pointer captures require a live interactive panel, not just cached peaks.
//! Losing that surface cancels its generation and the matching window seek;
//! ordinary MouseUp keeps the queued final Seek valid until consumption.
use crate::i18n::Localizer;

use std::{
    cell::RefCell,
    collections::HashMap,
    hash::{Hash, Hasher},
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use gpui::{
    AnyElement, Bounds, Context, EventEmitter, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, RenderImage, SharedString, Window, canvas, div, fill, point,
    prelude::*, px, rgb, size,
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
}

struct FullPreview {
    source: Rc<Source>,
    suspension_generation: u64,
    frame: WaveformFrame,
}

fn track_range(track: &Track) -> Option<PlaybackRange> {
    track.cue.as_ref().map(|cue| PlaybackRange {
        start_seconds: cue.start_seconds(),
        end_seconds: cue.end_seconds(),
    })
}

const WAVEFORM_TILE_WIDTH: u32 = 32;

struct Tile {
    x_start: u32,
    x_end: u32,
    fingerprint: u64,
    image: Arc<RenderImage>,
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
    width: usize,
    mapping: Option<WaveformMapping>,
    values: Vec<WaveformColumn>,
    layer_key: Option<LayerKey>,
    tiles: Vec<Tile>,
    tile_revision: Option<u64>,
}

#[derive(PartialEq)]
struct LayerKey {
    source: Rc<Source>,
    width: u32,
    height: u32,
    rms_gain: u32,
    peak_gain: u32,
    peak_gamma: u32,
    palette: crate::config::VisualizationPalette,
}

#[derive(Default)]
struct WaveformPlot {
    columns: HashMap<u64, Columns>,
    retired: Vec<Arc<RenderImage>>,
    pending_images: Vec<Arc<RenderImage>>,
    paint_tiles: Vec<(u32, u32, Arc<RenderImage>)>,
    retire_scratch: Vec<Arc<RenderImage>>,
}

fn waveform_level(value: f32, gain: f32, gamma: f32) -> f32 {
    (value * gain).clamp(0.0, 1.0).powf(gamma)
}

#[cfg(test)]
fn waveform_height(level: f32, half_height: Pixels) -> Pixels {
    (half_height * level).max(px(1.0))
}

fn waveform_color_level(level: f32) -> f32 {
    level.clamp(0.0, 1.0).powf(1.35)
}

fn pooled_side(
    peaks: &[Option<f32>],
    rms: &[Option<f32>],
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
    let mut energy = 0.0_f32;
    let mut known = 0_u32;
    for value in rms[first..end].iter().flatten().copied() {
        energy += value * value;
        known += 1;
    }
    let rms = (known != 0).then(|| {
        waveform_level(
            (energy / known as f32).sqrt() * scale,
            mapping.rms_gain,
            0.86,
        )
    });
    (rms, peak)
}

fn tile_columns(x_start: u32, x_end: u32, width: u32, count: usize) -> std::ops::Range<usize> {
    if count == 0 || width == 0 || x_start >= x_end {
        return 0..0;
    }
    let width = u64::from(width);
    let count64 = count as u64;
    let first = ((u64::from(x_start) + 1) * count64 - 1) / width;
    let end = (u64::from(x_end) * count64).div_ceil(width);
    (first as usize).min(count)..(end as usize).min(count)
}

/// Called once at the start of a window frame, even when no waveform is visible.
/// Uncached images survive a full subsequent frame in the pending queue.
/// Removed cached tiles are released at the next window-frame boundary.
#[derive(Clone)]
pub(in crate::gui) struct WaveformImageRelease(Rc<RefCell<WaveformPlot>>);

impl WaveformImageRelease {
    pub(in crate::gui) fn release_retired(&self, window: &mut Window) {
        let mut plot = self.0.borrow_mut();
        plot.release_images(|image| {
            let _ = window.drop_image(image);
        });
        if !plot.retired.is_empty() {
            let plot = Rc::clone(&self.0);
            window.on_next_frame(move |window, _| {
                plot.borrow_mut().release_images(|image| {
                    let _ = window.drop_image(image);
                });
            });
        }
    }

    pub(in crate::gui) fn release_all_images(&self, window: &mut Window) {
        let mut plot = self.0.borrow_mut();
        plot.paint_tiles.clear();
        let WaveformPlot {
            columns,
            retired,
            pending_images,
            retire_scratch,
            ..
        } = &mut *plot;
        for image in columns
            .values_mut()
            .flat_map(|columns| columns.tiles.drain(..).map(|tile| tile.image))
            .chain(retired.drain(..))
            .chain(pending_images.drain(..))
            .chain(retire_scratch.drain(..))
        {
            let _ = window.drop_image(image);
        }
        columns.clear();
    }
}

fn tile_fingerprint(values: &[WaveformColumn], start: usize, end: usize) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for column in &values[start..end] {
        for value in [
            column.left_rms,
            column.left_peak,
            column.right_rms,
            column.right_peak,
        ] {
            value.map(f32::to_bits).hash(&mut hasher);
        }
    }
    hasher.finish()
}

impl WaveformPlot {
    fn retain_panels(&mut self, mut keep: impl FnMut(u64) -> bool) {
        let retired = &mut self.retired;
        self.columns.retain(|id, columns| {
            if keep(*id) {
                return true;
            }
            retired.extend(columns.tiles.drain(..).map(|tile| tile.image));
            false
        });
        self.paint_tiles.clear();
    }

    fn clear_cache(&mut self) {
        self.paint_tiles.clear();
        for columns in self.columns.values_mut() {
            columns.values.clear();
            columns.layer_key = None;
            for tile in columns.tiles.drain(..) {
                self.retired.push(tile.image);
            }
        }
    }

    fn finish_paint(&mut self, cache_enabled: bool) {
        if !cache_enabled {
            self.pending_images
                .extend(self.paint_tiles.drain(..).map(|(_, _, image)| image));
            self.clear_cache();
        } else {
            self.paint_tiles.clear();
        }
    }

    fn release_images(&mut self, mut release: impl FnMut(Arc<RenderImage>)) {
        while let Some(image) = self.retired.pop() {
            release(image);
        }
        self.retired.append(&mut self.pending_images);
    }

    fn update_tiles(
        &mut self,
        panel_id: u64,
        key: LayerKey,
        cache_enabled: bool,
        palette: &super::visuals::PaletteLut,
    ) {
        self.paint_tiles.clear();
        let columns = self.columns.get_mut(&panel_id).unwrap();
        if columns.values.is_empty() {
            return;
        }
        let unchanged = cache_enabled
            && columns.layer_key.as_ref() == Some(&key)
            && columns.tile_revision == Some(columns.revision);
        if !cache_enabled || columns.layer_key.as_ref() != Some(&key) {
            self.retire_scratch
                .extend(columns.tiles.drain(..).map(|tile| tile.image));
        }
        let tile_count = key.width.div_ceil(WAVEFORM_TILE_WIDTH) as usize;
        for tile_index in 0..tile_count {
            let x_start = tile_index as u32 * WAVEFORM_TILE_WIDTH;
            let x_end = (x_start + WAVEFORM_TILE_WIDTH).min(key.width);
            let fingerprint = if unchanged {
                columns.tiles[tile_index].fingerprint
            } else {
                let range = tile_columns(x_start, x_end, key.width, columns.values.len());
                tile_fingerprint(&columns.values, range.start, range.end)
            };
            let retained = cache_enabled
                && columns.tiles.get(tile_index).is_some_and(|tile| {
                    tile.x_start == x_start
                        && tile.x_end == x_end
                        && tile.fingerprint == fingerprint
                });
            if retained {
                self.paint_tiles.push((
                    x_start,
                    x_end,
                    Arc::clone(&columns.tiles[tile_index].image),
                ));
                continue;
            }
            let image = Self::build_tile(
                &columns.values,
                x_start,
                x_end,
                key.width,
                key.height,
                palette,
            );
            if cache_enabled {
                let tile = Tile {
                    x_start,
                    x_end,
                    fingerprint,
                    image: Arc::clone(&image),
                };
                if tile_index < columns.tiles.len() {
                    self.retire_scratch
                        .push(std::mem::replace(&mut columns.tiles[tile_index], tile).image);
                } else {
                    columns.tiles.push(tile);
                }
            }
            self.paint_tiles.push((x_start, x_end, image));
        }
        columns.layer_key = cache_enabled.then_some(key);
        columns.tile_revision = cache_enabled.then_some(columns.revision);
        self.retired.append(&mut self.retire_scratch);
    }

    fn build_tile(
        values: &[WaveformColumn],
        x_start: u32,
        x_end: u32,
        width: u32,
        height: u32,
        palette: &super::visuals::PaletteLut,
    ) -> Arc<RenderImage> {
        let mut pixels = image::RgbaImage::new(x_end - x_start, height);
        let center = height as f32 * 0.5;
        let range = tile_columns(x_start, x_end, width, values.len());
        for column in range {
            let WaveformColumn {
                left_rms,
                left_peak,
                right_rms,
                right_peak,
            } = values[column];
            let x0 = (column as u32 * width / values.len() as u32).max(x_start);
            let x1 = (((column + 1) as u32 * width) / values.len() as u32).min(x_end);
            if x0 >= x1 {
                continue;
            }
            for (level, upper) in [(left_rms, true), (right_rms, false)] {
                let Some(level) = level else { continue };
                let height_px = (center * level).max(1.0);
                let start = if upper { center - height_px } else { center };
                let end = if upper { center } else { center + height_px };
                let low_color = palette.lookup_mapped_rgb(0.0);
                let rms_color = palette.lookup_mapped_rgb(waveform_color_level(level));
                for y in (start.floor() as u32)..(end.ceil() as u32).min(height) {
                    let t = ((y as f32 + 0.5 - start) / (end - start)).clamp(0.0, 1.0);
                    let t = if upper { 1.0 - t } else { t };
                    let color = image::Rgba([
                        (low_color[2] as f32 + (rms_color[2] as f32 - low_color[2] as f32) * t)
                            as u8,
                        (low_color[1] as f32 + (rms_color[1] as f32 - low_color[1] as f32) * t)
                            as u8,
                        (low_color[0] as f32 + (rms_color[0] as f32 - low_color[0] as f32) * t)
                            as u8,
                        255,
                    ]);
                    for x in x0..x1 {
                        pixels.put_pixel(x - x_start, y, color);
                    }
                }
            }
            for (level, upper) in [(left_peak, true), (right_peak, false)] {
                let Some(level) = level else { continue };
                let y = if upper {
                    center - (center * level).max(1.0)
                } else {
                    center + (center * level).max(1.0)
                };
                if height != 0 {
                    let y = (y.round() as u32).min(height - 1);
                    let [red, green, blue] = palette.lookup_mapped_rgb(waveform_color_level(level));
                    for x in x0..x1 {
                        pixels.put_pixel(x - x_start, y, image::Rgba([blue, green, red, 255]));
                    }
                }
            }
        }
        Arc::new(RenderImage::new([image::Frame::new(pixels)]))
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
            width,
            mapping: None,
            values: Vec::new(),
            layer_key: None,
            tiles: Vec::new(),
            tile_revision: None,
        });
        let left_peaks = if frame.left_peaks.is_empty() {
            &frame.peaks
        } else {
            &frame.left_peaks
        };
        let left_rms = if frame.left_rms.is_empty() {
            &frame.rms
        } else {
            &frame.left_rms
        };
        let right_peaks = if frame.right_peaks.is_empty() {
            &frame.peaks
        } else {
            &frame.right_peaks
        };
        let right_rms = if frame.right_rms.is_empty() {
            &frame.rms
        } else {
            &frame.right_rms
        };
        let count = width.max(1).min(left_peaks.len());
        if !cache_enabled
            || columns.revision != frame.revision
            || columns.width != width
            || columns.mapping != Some(mapping)
            || columns.values.len() != count
        {
            columns.values.clear();
        }
        if columns.values.len() != count {
            let scale = frame.max_peak.max(1.0).recip();
            for column in 0..count {
                let first = column * left_peaks.len() / count;
                let end = (column + 1) * left_peaks.len() / count;
                let (left_rms, left_peak) =
                    pooled_side(left_peaks, left_rms, first, end, scale, mapping);
                let (right_rms, right_peak) =
                    pooled_side(right_peaks, right_rms, first, end, scale, mapping);
                columns.values.push(WaveformColumn {
                    left_rms,
                    left_peak,
                    right_rms,
                    right_peak,
                });
            }
        }
        columns.revision = frame.revision;
        columns.width = width;
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
    full: Option<Rc<FullPreview>>,
    manual_error: Option<SharedString>,
    interaction: Rc<RefCell<WaveformInteraction>>,
    dragging: bool,
    seek_generation: u64,
    seek_enabled: bool,
    visualization_cache: bool,
}

impl Waveform {
    pub(in crate::gui) fn image_releaser(&self) -> WaveformImageRelease {
        WaveformImageRelease(Rc::clone(&self.plot))
    }

    pub(in crate::gui) fn new(shared: Arc<RwLock<WaveformFrame>>) -> Self {
        Self {
            enabled: true,
            shared,
            source: None,
            revision: None,
            plot: Rc::new(RefCell::new(WaveformPlot::default())),
            generation: 0,
            manual: None,
            full: None,
            manual_error: None,
            interaction: Rc::new(RefCell::new(WaveformInteraction::default())),
            dragging: false,
            seek_generation: 0,
            seek_enabled: true,
            visualization_cache: true,
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
        plot.paint_tiles = Vec::new();
        let mut scratch = std::mem::take(&mut plot.retire_scratch);
        plot.retired.append(&mut scratch);
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
        let (revision, suspension_generation) = {
            let shared = self.shared.read();
            (shared.revision, shared.suspension_generation())
        };
        let suspended = self
            .manual
            .as_ref()
            .is_some_and(|request| request.suspension_generation != suspension_generation)
            || self
                .full
                .as_ref()
                .is_some_and(|full| full.suspension_generation != suspension_generation);
        if suspended {
            self.cancel_full();
        }
        let changed = source_changed || suspended || self.revision != Some(revision);
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

    /// Cancel immediately while the caller queries the latest saved track.
    pub(in crate::gui) fn cancel_full_request(&mut self, cx: &mut Context<Self>) {
        self.cancel_full();
        cx.notify();
    }

    /// Explicit context-menu action; ordinary sync never opens a decoder.
    pub(in crate::gui) fn load_full(
        &mut self,
        track: Option<&Track>,
        config: &Config,
        cx: &mut Context<Self>,
    ) {
        if !self.enabled || config.still_mode {
            return;
        }
        let Some(track) = track else {
            return;
        };
        self.sync(Some(track), cx);
        let Some(source) = self.source.as_ref() else {
            return;
        };
        // Every explicit action replaces its predecessor, using the freshly
        // queried saved hint rather than the active playback decoder.
        let path = source.path.clone();
        let range = source.range;
        let buffer_mb = config.media_read_buffer_mb;
        let ffmpeg_enabled = config.ffmpeg_enabled;
        let decoder_hint = track.decoder_hint;
        let (generation, cancelled) = self.start_full(decoder_hint);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    crate::audio::waveform(
                        &path,
                        range,
                        decoder_hint,
                        buffer_mb,
                        ffmpeg_enabled,
                        &cancelled,
                    )
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
        self.cancel_full();
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
        });
        self.manual_error = None;
        (self.generation, cancelled)
    }

    fn cancel_full(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if let Some(request) = self.manual.take() {
            request.cancelled.store(true, Ordering::Relaxed);
        }
        if self.full.take().is_some() {
            self.plot.borrow_mut().clear_cache();
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
            {
                anyhow::bail!("Full waveform source changed before completion");
            }
            result.complete = true;
            completed = Some(Rc::new(FullPreview {
                source: Rc::clone(&request.source),
                suspension_generation: request.suspension_generation,
                frame: result,
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
        if self.visualization_cache != config.visualization_cache {
            self.plot.borrow_mut().clear_cache();
            self.visualization_cache = config.visualization_cache;
        }
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
                .children(status.map(super::super::components::visualization_status))
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
                            move |bounds, _, window, _| {
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
                                let width_px = (plot.size.width / px(1.0)).ceil().max(1.0) as u32;
                                let height_px = (plot.size.height / px(1.0)).ceil().max(1.0) as u32;
                                let key = LayerKey {
                                    source: Rc::clone(&source),
                                    width: width_px,
                                    height: height_px,
                                    rms_gain: rms_gain.to_bits(),
                                    peak_gain: peak_gain.to_bits(),
                                    peak_gamma: peak_gamma.to_bits(),
                                    palette: config_palette,
                                };
                                waveform.update_tiles(panel_id, key, cache_enabled, &palette);
                                for (x_start, x_end, image) in &waveform.paint_tiles {
                                    let tile_bounds = Bounds::new(
                                        point(plot.left() + px(*x_start as f32), plot.top()),
                                        size(px((*x_end - *x_start) as f32), plot.size.height),
                                    );
                                    window
                                        .paint_image(
                                            tile_bounds,
                                            tile_bounds,
                                            px(0.0).into(),
                                            Arc::clone(image),
                                            0,
                                            false,
                                        )
                                        .map_err(|_| ())
                                        .ok();
                                }
                                waveform.finish_paint(cache_enabled);
                                if let Some(fraction) = progress {
                                    // Slow periodic easing gives the droplet a deliberate rhythm;
                                    // it never snaps at a cycle boundary.
                                    let jitter = cursor_wobble(position) * 0.8;
                                    let cursor_center = plot.center().y + px(jitter);
                                    let x = plot.left() + plot.size.width * fraction;
                                    if glow > 0.0 {
                                        for (width, opacity) in
                                            [(10.0, 0.025), (5.0, 0.05), (2.0, 0.14)]
                                        {
                                            window.paint_quad(fill(
                                                Bounds::new(
                                                    point(x - px(width * 0.5), plot.top()),
                                                    size(px(width), plot.size.height),
                                                ),
                                                water.alpha(opacity * glow),
                                            ));
                                        }
                                    }
                                    // Leave the droplet readable: the playhead is continuous
                                    // above and below it, but does not cut through its body.
                                    let line_gap = if plot.size.height >= px(24.0) {
                                        px(10.0)
                                    } else {
                                        px(0.0)
                                    };
                                    let upper_height = cursor_center - line_gap - plot.top();
                                    if upper_height > px(0.0) {
                                        window.paint_quad(fill(
                                            Bounds::new(
                                                point(x - px(0.5), plot.top()),
                                                size(px(1.0), upper_height),
                                            ),
                                            water.alpha(0.62),
                                        ));
                                    }
                                    let lower_top = cursor_center + line_gap;
                                    if lower_top < plot.bottom() {
                                        window.paint_quad(fill(
                                            Bounds::new(
                                                point(x - px(0.5), lower_top),
                                                size(px(1.0), plot.bottom() - lower_top),
                                            ),
                                            water.alpha(0.62),
                                        ));
                                    }
                                    if plot.size.height >= px(24.0) {
                                        if glow > 0.0 {
                                            let mut halo = fill(
                                                Bounds::new(
                                                    point(x - px(7.0), cursor_center - px(7.0)),
                                                    size(px(14.0), px(14.0)),
                                                ),
                                                water.alpha(0.12 * glow),
                                            );
                                            halo.corner_radii = px(7.0).into();
                                            window.paint_quad(halo);
                                        }
                                        for (width, offset) in
                                            [(6.0, -4.0), (4.0, -2.0), (2.0, 0.0)]
                                        {
                                            window.paint_quad(fill(
                                                Bounds::new(
                                                    point(
                                                        x - px(width * 0.5),
                                                        cursor_center + px(offset),
                                                    ),
                                                    size(px(width), px(2.0)),
                                                ),
                                                water,
                                            ));
                                        }
                                        let mut droplet = fill(
                                            Bounds::new(
                                                point(x - px(4.5), cursor_center - px(5.0)),
                                                size(px(9.0), px(10.0)),
                                            ),
                                            water,
                                        );
                                        droplet.corner_radii = px(4.5).into();
                                        window.paint_quad(droplet);
                                        let mut glint = fill(
                                            Bounds::new(
                                                point(x - px(2.5), cursor_center - px(3.0)),
                                                size(px(2.5), px(3.0)),
                                            ),
                                            rgb(0xc4f5ee).alpha(0.8),
                                        );
                                        glint.corner_radii = px(1.25).into();
                                        window.paint_quad(glint);
                                    }
                                }
                            },
                        )
                        .size_full(),
                    ),
            )
            .when(labels, |panel| {
                panel.child(
                    crate::gui::components::row()
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
                super::super::components::visualization_status(message)
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
    fn waveform_tiles_preserve_palette_channels_and_dark_center() {
        let palette = super::super::visuals::active_palette_lut(
            crate::config::VisualizationPalette::Deadbeef,
        );
        let image = WaveformPlot::build_tile(
            &[
                WaveformColumn {
                    left_rms: Some(0.5),
                    left_peak: Some(0.9),
                    right_rms: Some(0.5),
                    right_peak: Some(0.9),
                },
                WaveformColumn {
                    left_rms: None,
                    left_peak: Some(0.9),
                    right_rms: None,
                    right_peak: Some(0.9),
                },
            ],
            0,
            2,
            2,
            100,
            &palette,
        );
        let bytes = image.as_bytes(0).unwrap();
        // GPUI consumes BGRA, even though image::RgbaImage names its channels RGBA.
        let pixel = |x: usize, y: usize| &bytes[(y * 2 + x) * 4..(y * 2 + x + 1) * 4];
        for (edge_y, center_y, peak_y) in [(26, 49, 5), (73, 50, 95)] {
            let edge = pixel(0, edge_y);
            let center = pixel(0, center_y);
            assert!(edge[0] > edge[2], "moderate RMS must remain blue, not red");
            let brightness = |p: &[u8]| p[..3].iter().map(|&c| u32::from(c)).sum::<u32>();
            assert!(
                brightness(center) < brightness(edge),
                "gradient must darken toward the center"
            );
            for x in 0..2 {
                let peak = pixel(x, peak_y);
                assert!(
                    peak[2] > peak[0],
                    "strong Peak must retain its warm palette color"
                );
                assert_eq!(peak[3], 255);
            }
        }
        assert_eq!(pixel(1, 49)[3], 0, "Peak-only data must not invent RMS");
    }

    #[test]
    fn full_and_near_full_stereo_peaks_reach_valid_edge_pixels() {
        let palette = super::super::visuals::active_palette_lut(
            crate::config::VisualizationPalette::Deadbeef,
        );
        let levels = [1.0, 0.999];
        let columns = levels.map(|level| WaveformColumn {
            left_rms: None,
            left_peak: Some(level),
            right_rms: None,
            right_peak: Some(level),
        });
        for height in [1, 2, 99, 100] {
            let image = WaveformPlot::build_tile(&columns, 0, 2, 2, height, &palette);
            let bytes = image.as_bytes(0).unwrap();
            for (x, level) in levels.into_iter().enumerate() {
                let [red, green, blue] = palette.lookup_mapped_rgb(waveform_color_level(level));
                for y in [0, height as usize - 1] {
                    let offset = (y * 2 + x) * 4;
                    assert_eq!(&bytes[offset..offset + 4], &[blue, green, red, 255]);
                }
                if height > 2 {
                    let center = (height as usize / 2 * 2 + x) * 4;
                    assert_eq!(bytes[center + 3], 0, "Peak-only data must not invent RMS");
                }
            }
        }
    }

    #[test]
    fn removed_panels_retire_without_repainting() {
        let mut plot = WaveformPlot::default();
        let data = frame(vec![Some(0.25); 100]);
        let palette = super::super::visuals::active_palette_lut(
            crate::config::VisualizationPalette::Deadbeef,
        );
        plot.columns(1, 100, &data);
        plot.update_tiles(
            1,
            LayerKey {
                source: Rc::new(Source::from_frame(&data).unwrap()),
                width: 101,
                height: 40,
                rms_gain: 1.0_f32.to_bits(),
                peak_gain: 1.0_f32.to_bits(),
                peak_gamma: 1.0_f32.to_bits(),
                palette: crate::config::VisualizationPalette::Deadbeef,
            },
            true,
            &palette,
        );
        let tile = Arc::downgrade(&plot.columns[&1].tiles[0].image);
        plot.retain_panels(|_| false);
        assert!(plot.columns.is_empty());
        assert!(tile.upgrade().is_some());
        let mut released = 0;
        plot.release_images(|_| released += 1);
        assert_eq!(released, 4);
        assert!(tile.upgrade().is_none());
    }

    #[test]
    fn uncached_waveform_images_survive_one_boundary_then_retire() {
        let mut plot = WaveformPlot::default();
        let data = frame(vec![Some(0.25); 100]);
        let palette = super::super::visuals::active_palette_lut(
            crate::config::VisualizationPalette::Deadbeef,
        );
        let mut previous = std::collections::HashSet::new();
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
            plot.update_tiles(
                1,
                LayerKey {
                    source: Rc::new(Source::from_frame(&data).unwrap()),
                    width: 101,
                    height: 40,
                    rms_gain: 1.0_f32.to_bits(),
                    peak_gain: 1.0_f32.to_bits(),
                    peak_gamma: 1.0_f32.to_bits(),
                    palette: crate::config::VisualizationPalette::Deadbeef,
                },
                false,
                &palette,
            );
            let painted: std::collections::HashSet<_> = plot
                .paint_tiles
                .iter()
                .map(|(_, _, image)| image.id)
                .collect();
            assert_eq!(painted.len(), 4);
            plot.finish_paint(false);
            let mut released = std::collections::HashSet::new();
            plot.release_images(|image| assert!(released.insert(image.id)));
            assert_eq!(released, previous);
            previous = painted;
        }
        let mut released = std::collections::HashSet::new();
        plot.release_images(|image| assert!(released.insert(image.id)));
        assert_eq!(released, previous);
        plot.release_images(|_| panic!("image retired twice"));
    }

    #[test]
    fn boundary_columns_refresh_all_and_only_intersecting_tiles() {
        let palette = super::super::visuals::active_palette_lut(
            crate::config::VisualizationPalette::Deadbeef,
        );
        for (count, changed_column, changed_tiles) in [(100, 95, vec![2]), (70, 22, vec![0, 1])] {
            let mut data = frame(vec![Some(0.25); count]);
            let mut plot = WaveformPlot::default();
            let key = |data: &WaveformFrame| LayerKey {
                source: Rc::new(Source::from_frame(data).unwrap()),
                width: 101,
                height: 40,
                rms_gain: 1.0_f32.to_bits(),
                peak_gain: 1.0_f32.to_bits(),
                peak_gamma: 1.0_f32.to_bits(),
                palette: crate::config::VisualizationPalette::Deadbeef,
            };
            plot.columns(1, count, &data);
            plot.update_tiles(1, key(&data), true, &palette);
            let before: Vec<_> = plot.columns[&1]
                .tiles
                .iter()
                .map(|tile| Arc::clone(&tile.image))
                .collect();
            data.peaks[changed_column] = Some(0.75);
            data.rms[changed_column] = Some(0.75);
            data.left_peaks[changed_column] = Some(0.75);
            data.left_rms[changed_column] = Some(0.75);
            data.revision += 1;
            plot.columns(1, count, &data);
            plot.update_tiles(1, key(&data), true, &palette);
            for (index, image) in before.iter().enumerate() {
                assert_eq!(
                    Arc::ptr_eq(image, &plot.columns[&1].tiles[index].image),
                    !changed_tiles.contains(&index)
                );
            }
            let before: Vec<_> = plot.columns[&1]
                .tiles
                .iter()
                .map(|tile| Arc::clone(&tile.image))
                .collect();
            plot.update_tiles(1, key(&data), true, &palette);
            assert!(
                before
                    .iter()
                    .zip(&plot.columns[&1].tiles)
                    .all(|(image, tile)| Arc::ptr_eq(image, &tile.image))
            );
        }
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
            max_peak,
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
    fn low_amplitude_rms_stays_within_peak_after_display_mapping() {
        for scale in [1.0, 0.5] {
            for (rms, peak) in [(0.1, 0.15), (0.01, 0.02), (0.00025, 0.0005), (0.2, 0.2)] {
                let (rms_height, peak_height) = pooled_side(
                    &[Some(peak)],
                    &[Some(rms)],
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
        let source = Rc::clone(waveform.source.as_ref().unwrap());
        let (generation, cancelled) = waveform.start_full(DecoderHint::Auto);
        let palette = super::super::visuals::active_palette_lut(
            crate::config::VisualizationPalette::Deadbeef,
        );
        let cached = {
            let mut plot = waveform.plot.borrow_mut();
            plot.columns(1, 100, &waveform.shared.read());
            plot.update_tiles(
                1,
                LayerKey {
                    source: Rc::clone(&source),
                    width: 101,
                    height: 40,
                    rms_gain: 1.0_f32.to_bits(),
                    peak_gain: 1.0_f32.to_bits(),
                    peak_gamma: 1.0_f32.to_bits(),
                    palette: crate::config::VisualizationPalette::Deadbeef,
                },
                true,
                &palette,
            );
            Arc::downgrade(&plot.columns[&1].tiles[0].image)
        };
        let image = WaveformPlot::build_tile(
            &[WaveformColumn {
                left_rms: Some(0.2),
                left_peak: Some(0.3),
                right_rms: None,
                right_peak: None,
            }],
            0,
            1,
            1,
            10,
            &palette,
        );
        let pending = Arc::downgrade(&image);
        waveform.plot.borrow_mut().pending_images.push(image);
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
            let mut plot = waveform.plot.borrow_mut();
            assert_eq!(plot.columns.capacity(), 0);
            assert_eq!(plot.paint_tiles.capacity(), 0);
            assert!(cached.upgrade().is_some());
            plot.release_images(drop);
            assert!(cached.upgrade().is_none());
            assert!(pending.upgrade().is_some());
            plot.release_images(drop);
            assert!(pending.upgrade().is_none());
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
            plot.update_tiles(
                1,
                LayerKey {
                    source: Rc::clone(&waveform.full.as_ref().unwrap().source),
                    width: 100,
                    height: 40,
                    rms_gain: 1.0_f32.to_bits(),
                    peak_gain: 1.0_f32.to_bits(),
                    peak_gamma: 1.0_f32.to_bits(),
                    palette: crate::config::VisualizationPalette::Deadbeef,
                },
                true,
                &palette,
            );
            Arc::downgrade(&plot.columns[&1].tiles[0].image)
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
            let mut plot = waveform.plot.borrow_mut();
            assert!(plot.columns[&1].values.is_empty());
            assert!(plot.columns[&1].tiles.is_empty());
            assert!(plot.paint_tiles.is_empty());
            assert!(
                cached.upgrade().is_some(),
                "images must survive until the frame boundary"
            );
            let mut released = 0;
            plot.release_images(|_| released += 1);
            assert_eq!(released, 4);
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
    fn full_result_is_independent_of_playback_changes_before_and_after_completion() {
        let mut waveform = preview(frame(vec![Some(0.25), None]));
        let (generation, _) = waveform.start_full(DecoderHint::Ffmpeg);
        {
            let mut shared = waveform.shared.write();
            shared.path = Some(PathBuf::from("other.flac"));
            shared.range = track_range(&cue_track());
            shared.decoder_hint = DecoderHint::Native;
            shared.peaks = vec![Some(0.1), None];
            shared.revision += 1;
        }
        let mut result = frame(vec![Some(0.5), Some(0.75)]);
        result.decoder_hint = DecoderHint::Ffmpeg;
        assert!(waveform.finish_full(generation, Ok(result)));
        assert!(waveform.manual_error.is_none());
        {
            let shared = waveform.shared.read();
            assert_eq!(
                shared.path.as_deref(),
                Some(std::path::Path::new("other.flac"))
            );
            assert_eq!(shared.peaks, vec![Some(0.1), None]);
            assert_eq!(shared.revision, 2);
            assert_eq!(
                waveform.display_frame(&shared).peaks,
                vec![Some(0.5), Some(0.75)]
            );
        }
        *waveform.shared.write() = frame(vec![Some(0.2), Some(0.3)]);
        let shared = waveform.shared.read();
        assert_eq!(
            waveform.display_frame(&shared).peaks,
            vec![Some(0.5), Some(0.75)]
        );
        assert_eq!(
            waveform.display_frame(&shared).decoder_hint,
            DecoderHint::Ffmpeg
        );
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
        window.update(|window, cx| {
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
                            owner.release_render_images(window, cx);
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
