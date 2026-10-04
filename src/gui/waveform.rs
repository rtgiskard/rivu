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
    AnyElement, Bounds, Context, SharedString, canvas, div, fill, point, prelude::*, px, rgb, size,
};
use parking_lot::RwLock;

use super::visuals::palette_color;
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
    palette: fn(f32) -> u32,
    values: Vec<(Option<f32>, u32)>,
}

#[derive(Default)]
struct WaveformPlot {
    columns: HashMap<u64, Columns>,
}

impl WaveformPlot {
    fn columns(
        &mut self,
        panel_id: u64,
        width: usize,
        frame: &WaveformFrame,
        palette: fn(f32) -> u32,
    ) -> &[(Option<f32>, u32)] {
        let columns = self.columns.entry(panel_id).or_insert_with(|| Columns {
            revision: frame.revision,
            width,
            palette,
            values: Vec::new(),
        });
        let count = width.max(1).min(frame.peaks.len());
        if columns.revision != frame.revision
            || columns.width != width
            || columns.values.len() != count
            || !std::ptr::fn_addr_eq(columns.palette, palette)
        {
            columns.values.clear();
        }
        if columns.values.len() != count {
            // Pool at most 1600 bins while reading the shared frame, never PCM.
            // Preserve track-wide peak ratios without amplifying quiet tracks.
            let scale = frame.max_peak.max(1.0).recip();
            for column in 0..count {
                let first = column * frame.peaks.len() / count;
                let end = (column + 1) * frame.peaks.len() / count;
                let amplitude = frame.peaks[first..end]
                    .iter()
                    .flatten()
                    .copied()
                    .reduce(f32::max)
                    .map(|peak| peak * scale);
                let fraction = column as f32 / count.saturating_sub(1).max(1) as f32;
                columns.values.push((amplitude, palette(fraction)));
            }
        }
        columns.revision = frame.revision;
        columns.width = width;
        columns.palette = palette;
        &columns.values
    }
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

pub(super) struct Waveform {
    shared: Arc<RwLock<WaveformFrame>>,
    source: Option<Rc<Source>>,
    revision: Option<u64>,
    plot: Rc<RefCell<WaveformPlot>>,
    generation: u64,
    manual: Option<ManualRequest>,
    manual_error: Option<SharedString>,
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
        }
    }

    pub(super) fn retain_panels(&mut self, panels: &[&super::layout::Panel]) {
        self.plot
            .borrow_mut()
            .columns
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

    /// Position and duration are relative to the selected track/CUE segment.
    /// Pass position zero for a selection preview rather than another track's
    /// playback position. The caller observes this entity for data changes.
    pub(super) fn view(
        &self,
        panel_id: u64,
        position: f64,
        duration: Option<f64>,
        config: &Config,
    ) -> AnyElement {
        let panel = div()
            .id(("waveform", panel_id))
            .relative()
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .bg(rgb(config.visual_background.rgb()));
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
        let water = rgb(config.waveform_cursor_color.rgb());
        let background = rgb(config.visual_background.rgb());
        let glow = config.waveform_glow;
        let labels = config.waveform_labels;
        let label_duration = timeline(&self.shared.read(), duration);
        panel
            .child(
                canvas(
                    |_, _, _| (),
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
                        let center = plot.center().y;
                        let mut waveform = waveform.borrow_mut();
                        let progress = {
                            let frame = shared.read();
                            // Playback may replace its source between layout and paint.
                            if !frame.matches(&source.path, source.range) {
                                return;
                            }
                            waveform.columns(
                                panel_id,
                                (plot.size.width / px(1.0)) as usize,
                                &frame,
                                palette_color,
                            );
                            progress(position, timeline(&frame, duration))
                        };
                        // No shared read lock is held while submitting draw commands.
                        let columns = &waveform.columns[&panel_id].values;
                        if columns.is_empty() {
                            return;
                        }
                        // Max-pooling preserves narrow transients. Unknown columns
                        // have neither a bar nor a silence baseline.
                        let width = plot.size.width / columns.len() as f32;
                        for (column, &(amplitude, color)) in columns.iter().enumerate() {
                            let Some(amplitude) = amplitude else {
                                continue;
                            };
                            let height = (plot.size.height * amplitude).max(px(1.0));
                            window.paint_quad(fill(
                                Bounds::new(
                                    point(
                                        plot.left() + width * column as f32,
                                        center - height * 0.5,
                                    ),
                                    size(width, height),
                                ),
                                rgb(color),
                            ));
                        }
                        if let Some(fraction) = progress {
                            // The plot inset leaves room for the droplet at both endpoints.
                            let x = plot.left() + plot.size.width * fraction;
                            if glow > 0.0 {
                                for (width, opacity) in [(12.0, 0.035), (6.0, 0.08), (2.0, 0.28)] {
                                    window.paint_quad(fill(
                                        Bounds::new(
                                            point(x - px(width * 0.5), plot.top()),
                                            size(px(width), plot.size.height),
                                        ),
                                        water.alpha(opacity * glow),
                                    ));
                                }
                            }
                            window.paint_quad(fill(
                                Bounds::new(
                                    point(x - px(0.5), plot.top()),
                                    size(px(1.0), plot.size.height),
                                ),
                                water.alpha(0.9),
                            ));
                            if plot.size.height >= px(24.0) {
                                if glow > 0.0 {
                                    let mut halo = fill(
                                        Bounds::new(
                                            point(x - px(7.0), plot.top()),
                                            size(px(14.0), px(14.0)),
                                        ),
                                        water.alpha(0.12 * glow),
                                    );
                                    halo.corner_radii = px(7.0).into();
                                    window.paint_quad(halo);
                                }
                                for (width, y) in [(6.0, 10.0), (4.0, 12.0), (2.0, 14.0)] {
                                    window.paint_quad(fill(
                                        Bounds::new(
                                            point(x - px(width * 0.5), plot.top() + px(y)),
                                            size(px(width), px(2.0)),
                                        ),
                                        water,
                                    ));
                                }
                                let mut droplet = fill(
                                    Bounds::new(
                                        point(x - px(4.5), plot.top() + px(2.0)),
                                        size(px(9.0), px(10.0)),
                                    ),
                                    water,
                                );
                                droplet.corner_radii = px(4.5).into();
                                window.paint_quad(droplet);
                                let mut glint = fill(
                                    Bounds::new(
                                        point(x - px(2.5), plot.top() + px(4.0)),
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

    fn frame(peaks: Vec<Option<f32>>) -> WaveformFrame {
        let max_peak = peaks.iter().flatten().copied().fold(1.0, f32::max);
        WaveformFrame {
            revision: 1,
            path: Some(PathBuf::from("track.flac")),
            range: None,
            duration: Some(10.0),
            span_seconds: 10.0,
            max_peak,
            complete: false,
            peaks,
        }
    }

    fn amplitudes(columns: &[(Option<f32>, u32)]) -> Vec<Option<f32>> {
        columns.iter().map(|column| column.0).collect()
    }

    #[test]
    fn resized_columns_preserve_transients_and_track_wide_peak_ratios() {
        let frame = frame(vec![Some(0.0), Some(0.5), Some(2.0), Some(0.25), Some(0.0)]);
        let mut plot = WaveformPlot::default();
        assert_eq!(
            amplitudes(plot.columns(1, 2, &frame, palette_color)),
            vec![Some(0.25), Some(1.0)]
        );
        assert_eq!(
            amplitudes(plot.columns(2, 5, &frame, palette_color)),
            vec![Some(0.0), Some(0.25), Some(1.0), Some(0.125), Some(0.0)]
        );
        assert_eq!(plot.columns(1, 1, &frame, palette_color)[0].0, Some(1.0));
        assert_eq!(
            amplitudes(plot.columns(2, 5, &frame, palette_color)),
            vec![Some(0.0), Some(0.25), Some(1.0), Some(0.125), Some(0.0)]
        );
    }

    #[test]
    fn unknown_intervals_stay_blank_and_known_silence_stays_known() {
        let frame = frame(vec![None, None, Some(0.0), None, Some(0.25), None]);
        let mut plot = WaveformPlot::default();
        assert_eq!(
            amplitudes(plot.columns(1, 3, &frame, palette_color)),
            vec![None, Some(0.0), Some(0.25)]
        );
        assert_eq!(
            amplitudes(plot.columns(1, 6, &frame, palette_color)),
            frame.peaks
        );
        assert_eq!(plot.columns(2, 1, &frame, palette_color)[0].0, Some(0.25));
    }

    #[test]
    fn revised_peaks_update_same_width_and_rescale_existing_columns() {
        let mut frame = frame(vec![Some(0.25), None]);
        let mut plot = WaveformPlot::default();
        assert_eq!(
            amplitudes(plot.columns(1, 2, &frame, palette_color)),
            vec![Some(0.25), None]
        );
        frame.peaks[1] = Some(2.0);
        frame.max_peak = 2.0;
        frame.revision += 1;
        assert_eq!(
            amplitudes(plot.columns(1, 2, &frame, palette_color)),
            vec![Some(0.125), Some(1.0)]
        );
    }

    fn grayscale(fraction: f32) -> u32 {
        let level = (fraction * 255.0).round() as u32;
        level * 0x010101
    }

    #[test]
    fn columns_follow_palette_endpoints_and_palette_changes() {
        let frame = frame(vec![Some(0.25), Some(0.5), Some(0.75)]);
        let mut plot = WaveformPlot::default();
        let columns = plot.columns(1, 3, &frame, palette_color);
        assert_eq!(columns[0].1, palette_color(0.0));
        assert_eq!(columns[1].1, palette_color(0.5));
        assert_eq!(columns[2].1, palette_color(1.0));
        let columns = plot.columns(1, 3, &frame, grayscale);
        assert_eq!(columns[0].1, 0x000000);
        assert_eq!(columns[1].1, 0x808080);
        assert_eq!(columns[2].1, 0xffffff);
        assert_eq!(columns[1].0, Some(0.5));
        assert_eq!(
            plot.columns(1, 1, &frame, palette_color)[0].1,
            palette_color(0.0)
        );
    }

    #[test]
    fn zero_and_extreme_widths_are_bounded_and_empty_data_has_no_columns() {
        let data = frame(vec![Some(0.0), None, Some(0.5)]);
        let mut plot = WaveformPlot::default();
        assert_eq!(plot.columns(1, 0, &data, palette_color).len(), 1);
        assert_eq!(plot.columns(1, 0, &data, palette_color)[0].0, Some(0.5));
        assert_eq!(plot.columns(1, usize::MAX, &data, palette_color).len(), 3);
        let empty = frame(Vec::new());
        assert!(plot.columns(1, 0, &empty, palette_color).is_empty());
        assert!(
            plot.columns(1, usize::MAX, &empty, palette_color)
                .is_empty()
        );
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
