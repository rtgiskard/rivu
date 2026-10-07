//! Progressive whole-track amplitude previews from the playback decoder.
//!
//! The audio worker owns the bounded peak bins. Unknown intervals remain blank.
//! Only an explicit full-preview request opens a separate decoder; hiding and
//! metadata/display changes never cancel or restart it.

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
    AnyElement, Bounds, Context, EventEmitter, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, SharedString, Window, canvas, div, fill, linear_color_stop,
    linear_gradient, point, prelude::*, px, rgb, size,
};
use parking_lot::RwLock;

use crate::{
    audio::{PlaybackRange, WaveformFrame},
    config::Config,
    model::Track,
};

#[derive(PartialEq)]
struct Source {
    path: PathBuf,
    range: Option<PlaybackRange>,
}

impl Source {
    fn from_frame(frame: &WaveformFrame) -> Option<Self> {
        frame.path.as_ref().map(|path| Self {
            path: path.clone(),
            range: frame.range,
        })
    }

    fn matches(&self, frame: &WaveformFrame) -> bool {
        frame.matches(&self.path, self.range)
    }
}

struct ManualRequest {
    source: Rc<Source>,
    previous_source: Option<Source>,
    cancelled: Arc<AtomicBool>,
}

fn track_range(track: &Track) -> Option<PlaybackRange> {
    track.cue.as_ref().map(|cue| PlaybackRange {
        start_seconds: cue.start_seconds(),
        end_seconds: cue.end_seconds(),
    })
}

struct Columns {
    revision: u64,
    width: usize,
    mapping: Option<(f32, f32, f32)>,
    values: Vec<(Option<f32>, Option<f32>, Option<f32>, Option<f32>)>,
}

#[derive(Default)]
struct WaveformPlot {
    columns: HashMap<u64, Columns>,
}

fn waveform_level(value: f32, gain: f32, gamma: f32) -> f32 {
    (value * gain).clamp(0.0, 1.0).powf(gamma)
}

fn waveform_height(level: f32, half_height: Pixels) -> Pixels {
    (half_height * level).max(px(1.0))
}

const WAVEFORM_PEAK_PALETTE_MAX: f32 = 0.76;
const WAVEFORM_RMS_PALETTE_MAX: f32 = 0.68;

fn pooled_side(
    peaks: &[Option<f32>],
    rms: &[Option<f32>],
    first: usize,
    end: usize,
    scale: f32,
    rms_gain: f32,
    peak_gain: f32,
    peak_gamma: f32,
) -> (Option<f32>, Option<f32>) {
    let peak = peaks[first..end]
        .iter()
        .flatten()
        .copied()
        .reduce(f32::max)
        .map(|value| waveform_level(value * scale, peak_gain, peak_gamma));
    let mut energy = 0.0_f32;
    let mut known = 0_u32;
    for value in rms[first..end].iter().flatten().copied() {
        energy += value * value;
        known += 1;
    }
    let rms = (known != 0)
        .then(|| waveform_level((energy / known as f32).sqrt() * scale, rms_gain, 0.86));
    (rms, peak)
}

impl WaveformPlot {
    fn clear_cache(&mut self) {
        for columns in self.columns.values_mut() {
            columns.values.clear();
        }
    }
    #[cfg(test)]
    fn columns(
        &mut self,
        panel_id: u64,
        width: usize,
        frame: &WaveformFrame,
    ) -> &[(Option<f32>, Option<f32>, Option<f32>, Option<f32>)] {
        self.columns_with_cache(panel_id, width, frame, true, 1.0, 1.0, 1.0)
    }

    fn columns_with_cache(
        &mut self,
        panel_id: u64,
        width: usize,
        frame: &WaveformFrame,
        cache_enabled: bool,
        rms_gain: f32,
        peak_gain: f32,
        peak_gamma: f32,
    ) -> &[(Option<f32>, Option<f32>, Option<f32>, Option<f32>)] {
        let columns = self.columns.entry(panel_id).or_insert_with(|| Columns {
            revision: frame.revision,
            width,
            mapping: None,
            values: Vec::new(),
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
        let mapping = (rms_gain, peak_gain, peak_gamma);
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
                let (left_rms, left_peak) = pooled_side(
                    left_peaks, left_rms, first, end, scale, rms_gain, peak_gain, peak_gamma,
                );
                let (right_rms, right_peak) = pooled_side(
                    right_peaks,
                    right_rms,
                    first,
                    end,
                    scale,
                    rms_gain,
                    peak_gain,
                    peak_gamma,
                );
                columns
                    .values
                    .push((left_rms, left_peak, right_rms, right_peak));
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

fn preview_message(source: Option<&Source>, frame: &WaveformFrame) -> Option<&'static str> {
    let Some(source) = source else {
        return Some("Select a track to preview its waveform");
    };
    if !frame.matches(&source.path, source.range) || !frame.peaks.iter().any(Option::is_some) {
        return Some("Play or buffer this track to build its waveform");
    }
    None
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
}

pub(super) enum WaveformEvent {
    Preview(f64),
    Seek(f64),
}
impl EventEmitter<WaveformEvent> for Waveform {}

pub(super) struct Waveform {
    shared: Arc<RwLock<WaveformFrame>>,
    source: Option<Rc<Source>>,
    revision: Option<u64>,
    plot: Rc<RefCell<WaveformPlot>>,
    generation: u64,
    manual: Option<ManualRequest>,
    manual_error: Option<SharedString>,
    interaction: Rc<RefCell<WaveformInteraction>>,
    dragging: bool,
    visualization_cache: bool,
}

impl Waveform {
    pub(super) fn new(shared: Arc<RwLock<WaveformFrame>>) -> Self {
        Self {
            shared,
            source: None,
            revision: None,
            plot: Rc::new(RefCell::new(WaveformPlot::default())),
            generation: 0,
            manual: None,
            manual_error: None,
            interaction: Rc::new(RefCell::new(WaveformInteraction::default())),
            dragging: false,
            visualization_cache: true,
        }
    }

    pub(super) fn retain_panels(&mut self, panels: &[&super::layout::Panel]) {
        self.plot
            .borrow_mut()
            .columns
            .retain(|id, _| panels.iter().any(|panel| panel.id == *id));
        self.interaction
            .borrow_mut()
            .panels
            .retain(|id, _| panels.iter().any(|panel| panel.id == *id));
    }
    /// The caller chooses the playing track, or the selected track before play.
    /// Metadata, display settings and visibility do not change source identity.
    pub(super) fn sync(&mut self, track: Option<&Track>, cx: &mut Context<Self>) {
        let source_changed = match (&self.source, track) {
            (Some(source), Some(track)) => {
                source.path != track.path || source.range != track_range(track)
            }
            (None, None) => false,
            _ => true,
        };
        if source_changed {
            self.cancel_full();
            self.source = track.map(|track| {
                Rc::new(Source {
                    path: track.path.clone(),
                    range: track_range(track),
                })
            });
        }
        let revision = self.shared.read().revision;
        if source_changed || self.revision != Some(revision) {
            self.revision = Some(revision);
            cx.notify();
        }
    }

    /// Explicit context-menu action; ordinary sync never opens a decoder.
    pub(super) fn load_full(
        &mut self,
        track: Option<&Track>,
        config: &Config,
        cx: &mut Context<Self>,
    ) {
        self.sync(track, cx);
        let Some(source) = self.source.as_ref() else {
            return;
        };
        if self.manual.is_some() {
            return;
        }
        let path = source.path.clone();
        let range = source.range;
        let buffer_mb = config.media_read_buffer_mb;
        let ffmpeg_enabled = config.ffmpeg_enabled;
        let (generation, cancelled) = self.start_full();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    crate::audio::waveform(&path, range, buffer_mb, ffmpeg_enabled, &cancelled)
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

    fn start_full(&mut self) -> (u64, Arc<AtomicBool>) {
        self.generation = self.generation.wrapping_add(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        self.manual = Some(ManualRequest {
            source: Rc::clone(
                self.source
                    .as_ref()
                    .expect("full preview source is present"),
            ),
            previous_source: Source::from_frame(&self.shared.read()),
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
        self.manual_error = None;
    }

    fn finish_full(&mut self, generation: u64, result: anyhow::Result<WaveformFrame>) -> bool {
        let Some(request) = self.manual.as_ref() else {
            return false;
        };
        if generation != self.generation {
            return false;
        }
        let mut installed_revision = None;
        let result = result.and_then(|result| {
            if self.source.as_deref() != Some(request.source.as_ref())
                || !request.source.matches(&result)
            {
                anyhow::bail!("Full waveform source changed before completion");
            }
            let mut shared = self.shared.write();
            let previous_unchanged = match &request.previous_source {
                Some(previous) => previous.matches(&shared),
                None => shared.path.is_none(),
            };
            if !request.source.matches(&shared) && !previous_unchanged {
                anyhow::bail!("Playback source changed before full waveform completion");
            }
            shared.install_full(result);
            installed_revision = Some(shared.revision);
            Ok(())
        });
        if let Some(revision) = installed_revision {
            self.revision = Some(revision);
        }
        self.manual = None;
        self.manual_error = result
            .err()
            .map(|error| format!("Cannot load full waveform: {error:#}").into());
        true
    }

    fn manual_message(&self) -> Option<SharedString> {
        if self.manual.is_some() {
            Some("Loading full waveform…".into())
        } else {
            self.manual_error.clone()
        }
    }

    fn seek_from_position(
        &mut self,
        position: Point<Pixels>,
        panel_id: Option<u64>,
        preview: bool,
        cx: &mut Context<Self>,
    ) {
        let interaction = self.interaction.borrow();
        let panel_id = panel_id.or(interaction.active_panel);
        let Some((bounds, Some(duration))) = panel_id.and_then(|id| interaction.panels.get(&id))
        else {
            return;
        };
        let seconds = seek_seconds(
            position.x,
            bounds.left(),
            bounds.size.width.max(px(1.0)),
            *duration,
        );
        cx.emit(if preview {
            WaveformEvent::Preview(seconds)
        } else {
            WaveformEvent::Seek(seconds)
        });
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let panel_id = self
            .interaction
            .borrow()
            .panels
            .iter()
            .find(|(_, (bounds, _))| bounds.contains(&event.position))
            .map(|(panel_id, _)| *panel_id);
        let Some(panel_id) = panel_id else {
            return;
        };
        self.dragging = true;
        self.interaction.borrow_mut().active_panel = Some(panel_id);
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
        self.dragging = false;
        self.interaction.borrow_mut().active_panel = None;
    }

    /// Position and duration are relative to the selected track/CUE segment.
    /// Pass position zero for a selection preview rather than another track's
    /// playback position. The caller observes this entity for data changes.
    pub(super) fn view(
        &mut self,
        panel_id: u64,
        position: f64,
        duration: Option<f64>,
        config: &Config,
        cx: &mut Context<Self>,
    ) -> AnyElement {
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
        let status = self.manual_message();
        let message = preview_message(self.source.as_deref(), &self.shared.read());
        if let Some(message) = message {
            return panel
                .child(super::components::visualization_status(
                    status.unwrap_or_else(|| message.into()),
                ))
                .into_any_element();
        }
        let source = Rc::clone(self.source.as_ref().expect("preview source is present"));
        let shared = Arc::clone(&self.shared);
        let waveform = Rc::clone(&self.plot);
        let palette = super::visuals::active_palette_lut(config.visual_palette);
        let water = rgb(config.waveform_cursor_color.rgb());
        let background = rgb(config.visual_background.rgb());
        let glow = config.waveform_glow;
        let labels = config.waveform_labels;
        let rms_gain = config.waveform_rms_gain;
        let peak_gain = config.waveform_peak_gain;
        let peak_gamma = config.waveform_peak_gamma;
        let cache_enabled = config.visualization_cache;
        let label_duration = timeline(&self.shared.read(), duration);
        let interaction = Rc::clone(&self.interaction);
        interaction
            .borrow_mut()
            .panels
            .insert(panel_id, (Bounds::default(), label_duration));
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
                                let progress = {
                                    let frame = shared.read();
                                    // Playback may replace its source between layout and paint.
                                    if !frame.matches(&source.path, source.range) {
                                        return;
                                    }
                                    waveform.columns_with_cache(
                                        panel_id,
                                        (plot.size.width / px(1.0)) as usize,
                                        &frame,
                                        cache_enabled,
                                        rms_gain,
                                        peak_gain,
                                        peak_gamma,
                                    );
                                    progress(position, timeline(&frame, duration))
                                };
                                // No shared read lock is held while submitting draw commands.
                                if waveform.columns[&panel_id].values.is_empty() {
                                    return;
                                }
                                let columns = &waveform.columns[&panel_id].values;
                                let width = plot.size.width / columns.len() as f32;
                                let center = plot.center().y;
                                let half_height = plot.size.height * 0.5;
                                let low = rgb(palette.lookup_mapped(0.0));
                                for (column, &(left_rms, left_peak, right_rms, right_peak)) in
                                    columns.iter().enumerate()
                                {
                                    let x = plot.left() + width * column as f32;
                                    for (level, upper) in [(left_rms, true), (right_rms, false)] {
                                        let Some(level) = level else { continue };
                                        let height = waveform_height(level, half_height);
                                        let high = rgb(palette
                                            .lookup_mapped(level.min(WAVEFORM_RMS_PALETTE_MAX)));
                                        let (start, end) =
                                            if upper { (low, high) } else { (high, low) };
                                        window.paint_quad(fill(
                                            Bounds::new(
                                                point(
                                                    x,
                                                    if upper { center - height } else { center },
                                                ),
                                                size(width, height),
                                            ),
                                            linear_gradient(
                                                0.0,
                                                linear_color_stop(start, 0.0),
                                                linear_color_stop(end, 1.0),
                                            ),
                                        ));
                                    }
                                    for (level, upper) in [(left_peak, true), (right_peak, false)] {
                                        let Some(level) = level else { continue };
                                        let height = waveform_height(level, half_height);
                                        let y = if upper {
                                            center - height
                                        } else {
                                            center + height
                                        };
                                        window.paint_quad(fill(
                                            Bounds::new(
                                                point(x, y - px(0.5)),
                                                size(width, px(1.0)),
                                            ),
                                            rgb(palette.lookup_mapped(
                                                level.min(WAVEFORM_PEAK_PALETTE_MAX),
                                            )),
                                        ));
                                    }
                                }
                                if !cache_enabled {
                                    waveform.clear_cache();
                                }
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
                    super::row()
                        .absolute()
                        .bottom_0()
                        .left_0()
                        .w_full()
                        .px_2()
                        .justify_between()
                        .child(super::panels::caption("0:00"))
                        .child(super::panels::caption(
                            label_duration.map_or_else(|| "—".into(), super::format_time),
                        )),
                )
            })
            .children(status.map(|message| {
                super::components::visualization_status(message)
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
            last_push_start: None,
            complete: false,
        }
    }

    fn amplitudes(
        columns: &[(Option<f32>, Option<f32>, Option<f32>, Option<f32>)],
    ) -> Vec<Option<f32>> {
        columns.iter().map(|column| column.1).collect()
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
        assert_eq!(plot.columns(1, 1, &frame)[0].1, Some(1.0));
        assert_eq!(amplitudes(plot.columns(2, 5, &frame)), fine);
    }

    #[test]
    fn low_amplitude_rms_stays_within_peak_after_display_mapping() {
        for scale in [1.0, 0.5] {
            for (rms, peak) in [(0.1, 0.15), (0.01, 0.02), (0.00025, 0.0005), (0.2, 0.2)] {
                let (rms_height, peak_height) =
                    pooled_side(&[Some(peak)], &[Some(rms)], 0, 1, scale, 1.0, 1.0, 0.86);
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
        assert_eq!(plot.columns(2, 1, &frame)[0].1, fine[4]);
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
        assert_eq!(plot.columns(1, 0, &data)[0].1, fine[2]);
        let empty = frame(Vec::new());
        assert!(plot.columns(1, 0, &empty).is_empty());
        assert!(plot.columns(1, usize::MAX, &empty).is_empty());
    }

    #[test]
    fn unavailable_or_different_sources_prompt_playback_instead_of_using_old_peaks() {
        let mut frame = frame(vec![Some(0.0)]);
        let source = Source {
            path: PathBuf::from("track.flac"),
            range: None,
        };
        assert!(preview_message(None, &frame).is_some());
        assert!(preview_message(Some(&source), &frame).is_none());
        frame.path = Some(PathBuf::from("other.flac"));
        assert!(preview_message(Some(&source), &frame).is_some());
        frame.path = Some(source.path.clone());
        frame.range = Some(PlaybackRange {
            start_seconds: 2.0,
            end_seconds: Some(4.0),
        });
        assert!(preview_message(Some(&source), &frame).is_some());
        frame.range = None;
        frame.peaks[0] = None;
        assert!(preview_message(Some(&source), &frame).is_some());
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
    fn cancelled_and_stale_full_results_cannot_replace_the_current_request() {
        let mut waveform = preview(frame(vec![Some(0.25), None]));
        let (first, cancelled) = waveform.start_full();
        waveform.cancel_full();
        assert!(cancelled.load(Ordering::Relaxed));
        let (latest, _) = waveform.start_full();
        assert!(!waveform.finish_full(first, Ok(frame(vec![Some(1.0), Some(1.0)]))));
        assert!(waveform.manual.is_some());
        assert_eq!(waveform.shared.read().peaks, vec![Some(0.25), None]);
        assert!(waveform.finish_full(latest, Ok(frame(vec![Some(0.5), Some(0.75)]))));
        assert_eq!(waveform.shared.read().peaks, vec![Some(0.5), Some(0.75)]);
        assert!(!waveform.finish_full(latest, Ok(frame(vec![Some(1.0), Some(1.0)]))));
        assert_eq!(waveform.shared.read().peaks, vec![Some(0.5), Some(0.75)]);
    }

    #[test]
    fn loading_and_failures_preserve_buffered_peaks_and_success_uses_current_revision() {
        let mut waveform = preview(frame(vec![Some(0.25), None]));
        let (generation, _) = waveform.start_full();
        assert_eq!(waveform.shared.read().peaks, vec![Some(0.25), None]);
        assert!(waveform.manual_message().is_some());
        assert!(waveform.finish_full(generation, Err(anyhow::anyhow!("decode failed"))));
        assert!(
            waveform
                .manual_error
                .as_ref()
                .unwrap()
                .contains("decode failed")
        );
        assert_eq!(waveform.shared.read().peaks, vec![Some(0.25), None]);
        let (generation, _) = waveform.start_full();
        waveform.shared.write().revision = 40;
        assert!(waveform.finish_full(generation, Ok(frame(vec![Some(0.5), Some(0.75)]))));
        let shared = waveform.shared.read();
        assert_eq!(shared.revision, 41);
        assert!(shared.complete);
        assert_eq!(shared.peaks, vec![Some(0.5), Some(0.75)]);
        assert!(waveform.manual_error.is_none());
    }

    #[test]
    fn full_completion_rejects_playback_switches_even_before_gui_source_sync() {
        let mut waveform = preview(frame(vec![Some(0.25), None]));
        let (generation, _) = waveform.start_full();
        {
            let mut shared = waveform.shared.write();
            shared.path = Some(PathBuf::from("other.flac"));
            shared.peaks = vec![Some(0.1), None];
            shared.revision += 1;
        }
        assert!(waveform.finish_full(generation, Ok(frame(vec![Some(1.0), Some(1.0)]))));
        let shared = waveform.shared.read();
        assert_eq!(
            shared.path.as_deref(),
            Some(std::path::Path::new("other.flac"))
        );
        assert_eq!(shared.peaks, vec![Some(0.1), None]);
        assert_eq!(shared.revision, 2);
        assert!(!shared.complete);
        assert!(waveform.manual_error.is_some());
    }

    #[test]
    fn full_completion_rejects_cue_switches_and_wrong_result_sources() {
        let mut waveform = preview(frame(vec![Some(0.25), None]));
        let (generation, _) = waveform.start_full();
        waveform.shared.write().range = Some(PlaybackRange {
            start_seconds: 2.0,
            end_seconds: Some(4.0),
        });
        assert!(waveform.finish_full(generation, Ok(frame(vec![Some(1.0), Some(1.0)]))));
        assert_eq!(waveform.shared.read().peaks, vec![Some(0.25), None]);
        waveform.shared.write().range = None;
        let (generation, _) = waveform.start_full();
        let mut wrong_source = frame(vec![Some(1.0), Some(1.0)]);
        wrong_source.path = Some(PathBuf::from("other.flac"));
        assert!(waveform.finish_full(generation, Ok(wrong_source)));
        assert_eq!(waveform.shared.read().peaks, vec![Some(0.25), None]);
        assert!(!waveform.shared.read().complete);
    }

    #[test]
    fn selected_unplayed_source_can_replace_unchanged_previous_preview() {
        let mut waveform = preview(frame(vec![Some(0.25), None]));
        waveform.source = Some(Rc::new(Source {
            path: PathBuf::from("selected.flac"),
            range: None,
        }));
        let (generation, _) = waveform.start_full();
        let mut selected = frame(vec![Some(0.5), Some(0.75)]);
        selected.path = Some(PathBuf::from("selected.flac"));
        assert!(waveform.finish_full(generation, Ok(selected)));
        let shared = waveform.shared.read();
        assert_eq!(
            shared.path.as_deref(),
            Some(std::path::Path::new("selected.flac"))
        );
        assert!(shared.complete);
        assert_eq!(shared.peaks, vec![Some(0.5), Some(0.75)]);
    }

    #[test]
    fn dropping_preview_cancels_an_explicit_scan() {
        let mut waveform = preview(frame(vec![Some(0.25), None]));
        let (_, cancelled) = waveform.start_full();
        drop(waveform);
        assert!(cancelled.load(Ordering::Relaxed));
    }
}
