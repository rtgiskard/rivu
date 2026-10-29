//! Triggered, unwindowed stereo PCM, not a waveform synthesized from FFT bins.
use crate::{VisualLabels, VisualParameters};
use gpui::{
    AnyElement, PathBuilder, SharedString, TextAlign, canvas, div, point, prelude::*, px, rgb,
};
use rivu_audio::analysis::{AnalysisFrame, AnalysisSettings};
use rivu_format::config::AudioVisualizerOptions;
use std::{cell::RefCell, ops::Range, rc::Rc, sync::Arc};

// Retain normalized sweep geometry, never the worker's full PCM snapshot.
const MAX_SWEEP_POINTS: usize = 2048;

#[derive(Clone)]
pub(super) struct OscilloscopeRenderer(Rc<RefCell<OscilloscopeState>>);

struct OscilloscopeState {
    visible: bool,
    settings: AnalysisSettings,
    source: Option<(u64, u32)>,
    points: Vec<[f32; 2]>,
    time_span_ms: f32,
    gain: f32,
    sweep_span_ms: Option<f32>,
    background: u32,
    colors: [u32; 2],
    label: Option<SharedString>,
    label_template: Option<SharedString>,
}

impl OscilloscopeState {
    fn clear(&mut self) {
        self.points.clear();
        self.source = None;
        self.sweep_span_ms = None;
        self.label = None;
    }
}

impl OscilloscopeRenderer {
    pub(super) fn new() -> Self {
        let config = VisualParameters::default();
        Self(Rc::new(RefCell::new(OscilloscopeState {
            visible: false,
            settings: config.analysis_settings(),
            source: None,
            points: Vec::new(),
            time_span_ms: config.oscilloscope_time_span_ms,
            gain: config.oscilloscope_gain,
            sweep_span_ms: None,
            background: config.visual_background.rgb(),
            colors: [
                config.audio_visualizer_primary_color.rgb(),
                config.audio_visualizer_secondary_color.rgb(),
            ],
            label: None,
            label_template: None,
        })))
    }

    pub(super) fn configure(&self, config: &VisualParameters, visible: bool) -> bool {
        let mut data = self.0.borrow_mut();
        let settings = config.analysis_settings();
        let colors = [
            config.audio_visualizer_primary_color.rgb(),
            config.audio_visualizer_secondary_color.rgb(),
        ];
        let changed = data.visible != visible
            || data.settings != settings
            || data.background != config.visual_background.rgb()
            || data.colors != colors
            || data.time_span_ms != config.oscilloscope_time_span_ms
            || data.gain != config.oscilloscope_gain;
        if data.settings != settings {
            data.clear();
        }
        if data.gain != config.oscilloscope_gain {
            data.label = None;
        }
        data.time_span_ms = config.oscilloscope_time_span_ms;
        data.gain = config.oscilloscope_gain;
        data.visible = visible;
        data.settings = settings;
        data.background = config.visual_background.rgb();
        data.colors = colors;
        changed
    }

    pub(super) fn clear(&self) {
        self.0.borrow_mut().clear();
    }

    // Presentation rejects retired display tokens and freezes updates on pause.
    // Restoration keeps the compact sweep until usable PCM replaces it.
    pub(super) fn update(&self, frame: &AnalysisFrame) {
        let mut data = self.0.borrow_mut();
        if frame.settings != Some(data.settings) || frame.sample_rate == 0 {
            data.clear();
            return;
        }
        if data
            .source
            .is_some_and(|source| source != (frame.generation, frame.sample_rate))
        {
            data.clear();
        }
        if !data.visible || !frame.visualizer.demand.pcm || frame.visualizer.pcm.len() < 2 {
            return;
        }
        let samples = &frame.visualizer.pcm;
        let range = triggered_range(samples, frame.sample_rate, data.time_span_ms);
        let span_ms = sweep_span_ms(range.len(), frame.sample_rate);
        if data.sweep_span_ms != span_ms {
            data.sweep_span_ms = span_ms;
            data.label = None;
        }
        compact_sweep(&samples[range], &mut data.points);
        data.source = Some((frame.generation, frame.sample_rate));
    }

    pub(super) fn view(
        &self,
        i18n: &Arc<VisualLabels>,
        panel_id: u64,
        options: AudioVisualizerOptions,
    ) -> AnyElement {
        let data = Rc::clone(&self.0);
        if options.label {
            let template = &i18n.oscilloscope_scale;
            let mut data = data.borrow_mut();
            if data.label_template.as_ref() != Some(template) || data.label.is_none() {
                data.label = Some(scale_label(template, data.gain, data.sweep_span_ms).into());
                data.label_template = Some(template.clone());
            }
        }
        div()
            .id(("oscilloscope", panel_id))
            .relative()
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, cx| {
                        let data = data.borrow();
                        let width = f32::from(bounds.size.width);
                        let height = f32::from(bounds.size.height);
                        let samples = &data.points;
                        let columns =
                            physical_columns(width, height, window.scale_factor(), samples.len());
                        if !f32::from(bounds.left()).is_finite()
                            || !f32::from(bounds.top()).is_finite()
                            || !width.is_finite()
                            || !height.is_finite()
                            || width <= 0.0
                            || height <= 0.0
                        {
                            return;
                        }
                        window.paint_quad(gpui::fill(bounds, rgb(data.background)));
                        if options.grid {
                            for y in [0.05, 0.25, 0.45, 0.55, 0.75, 0.95] {
                                window.paint_quad(gpui::fill(
                                    gpui::Bounds::new(
                                        point(bounds.left(), bounds.top() + bounds.size.height * y),
                                        gpui::size(bounds.size.width, px(1.0)),
                                    ),
                                    rgb(0x414956).alpha(0.5),
                                ));
                            }
                        }
                        // Every sample belongs to one physical-pixel bucket. Extrema
                        // survive even in a one-pixel panel; geometry never follows
                        // the FFT size when the panel is narrower than the sweep.
                        for channel in 0..2 {
                            let center = if channel == 0 { 0.25 } else { 0.75 };
                            let mut path = PathBuilder::stroke(px(1.2));
                            let mut connected = false;
                            let mut has_points = false;
                            for bucket in 0..columns {
                                let group = &samples[bucket_range(samples.len(), columns, bucket)];
                                let Some((low, high)) = extrema(group, channel) else {
                                    connected = false;
                                    continue;
                                };
                                let x = bounds.left()
                                    + bounds.size.width * column_fraction(bucket, columns);
                                for value in [low, high] {
                                    let value = display_amplitude(value, data.gain);
                                    let p = point(
                                        x,
                                        bounds.top() + bounds.size.height * (center - value * 0.2),
                                    );
                                    if connected {
                                        path.line_to(p);
                                    } else {
                                        path.move_to(p);
                                        connected = true;
                                    }
                                    has_points = true;
                                }
                            }
                            if has_points && let Ok(path) = path.build() {
                                window.paint_path(path, rgb(data.colors[channel]));
                            }
                        }
                        // Measure the actual localized label rather than imposing a
                        // guessed minimum width on small or translated panels.
                        if options.label
                            && height >= 100.0
                            && let Some(label) = &data.label
                        {
                            let mut style = window.text_style();
                            style.color = rgb(0xb1bbc5).into();
                            let run = style.to_run(label.len());
                            let line = window.text_system().shape_line(
                                label.clone(),
                                px(10.0),
                                &[run],
                                None,
                            );
                            if line.width() + px(16.0) <= bounds.size.width {
                                let origin = point(bounds.left() + px(8.0), bounds.top() + px(2.0));
                                if let Err(error) =
                                    line.paint(origin, px(14.0), TextAlign::Left, None, window, cx)
                                {
                                    eprintln!("rivu visualization label: {error}");
                                }
                            }
                        }
                    },
                )
                .size_full(),
            )
            .into_any_element()
    }
}

fn compact_sweep(samples: &[[f32; 2]], points: &mut Vec<[f32; 2]>) {
    points.clear();
    points.reserve_exact(samples.len().min(MAX_SWEEP_POINTS));
    if samples.len() <= MAX_SWEEP_POINTS {
        points.extend_from_slice(samples);
        return;
    }
    let columns = MAX_SWEEP_POINTS / 2;
    for bucket in 0..columns {
        let group = &samples[bucket_range(samples.len(), columns, bucket)];
        let ranges = [extrema(group, 0), extrema(group, 1)];
        points.push(ranges.map(|range| range.map_or(f32::NAN, |(low, _)| low)));
        points.push(ranges.map(|range| range.map_or(f32::NAN, |(_, high)| high)));
    }
}

// Prefer a rising zero crossing on L; right-only audio triggers on R.
// Each sweep reserves an equally sized trigger-search prefix. Short snapshots
// show the available half-window; the axis reports the actual source sample span.
fn triggered_range(samples: &[[f32; 2]], sample_rate: u32, time_span_ms: f32) -> Range<usize> {
    let maximum = (sample_rate as f64 * time_span_ms as f64 / 1000.0).round() as usize;
    let span = maximum.min(samples.len() / 2);
    if span < 2 {
        return 0..samples.len().min(maximum);
    }
    let channel = usize::from(
        !samples
            .iter()
            .any(|sample| sample[0].is_finite() && sample[0].abs() > 0.0001),
    );
    let search = samples.len() - span;
    let start = samples[..=search]
        .windows(2)
        .position(|pair| {
            let previous = pair[0][channel];
            let next = pair[1][channel];
            previous.is_finite() && next.is_finite() && previous <= 0.0 && next > 0.0
        })
        .map_or(search, |index| index + 1);
    start..start + span
}

fn sweep_span_ms(samples: usize, sample_rate: u32) -> Option<f32> {
    (samples >= 2 && sample_rate != 0)
        .then(|| ((samples - 1) as f64 * 1000.0 / sample_rate as f64) as f32)
}

fn display_amplitude(value: f32, gain: f32) -> f32 {
    (value * gain).clamp(-1.0, 1.0)
}

fn scale_label(template: &str, gain: f32, span_ms: Option<f32>) -> String {
    let amplitude = format!("{:.2}", 1.0 / gain);
    let span = span_ms.map_or_else(|| "—".into(), |span| format!("{span:.2}"));
    template
        .replace("{amplitude}", &amplitude)
        .replace("{span}", &span)
}

fn physical_columns(width: f32, height: f32, scale: f32, samples: usize) -> usize {
    if samples < 2
        || !width.is_finite()
        || !height.is_finite()
        || !scale.is_finite()
        || width <= 0.0
        || height <= 0.0
        || scale <= 0.0
    {
        return 0;
    }
    // f64 prevents finite f32 dimensions from overflowing during multiplication.
    ((width as f64 * scale as f64).ceil().max(1.0) as usize).min(samples)
}

fn bucket_range(samples: usize, columns: usize, bucket: usize) -> Range<usize> {
    bucket * samples / columns..(bucket + 1) * samples / columns
}

fn column_fraction(bucket: usize, columns: usize) -> f32 {
    if columns == 1 {
        0.5
    } else {
        bucket as f32 / (columns - 1) as f32
    }
}

fn extrema(samples: &[[f32; 2]], channel: usize) -> Option<(f32, f32)> {
    samples
        .iter()
        .map(|sample| sample[channel])
        .filter(|value| value.is_finite())
        .fold(None, |range, value| {
            Some(range.map_or((value, value), |(low, high): (f32, f32)| {
                (low.min(value), high.max(value))
            }))
        })
}

#[cfg(test)]
impl OscilloscopeRenderer {
    fn configure_config(&self, config: &rivu_format::config::Config, visible: bool) -> bool {
        self.configure(&crate::VisualParameters::from(config), visible)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use rivu_audio::analysis::PcmSnapshot;
    use rivu_format::config::Config;

    #[test]
    fn scope_triggers_on_real_stereo_pcm_and_bounds_short_windows() {
        let samples: Vec<_> = (0..1024)
            .map(|i| {
                let value = (std::f32::consts::TAU * (i as f32 - 7.0) / 32.0).sin();
                [value, -value]
            })
            .collect();
        let range = triggered_range(&samples, 48_000, 10.0);
        assert_eq!(range.len(), 480);
        assert!(samples[range.start][0] > 0.0);
        assert!(samples[range.start - 1][0] <= 0.0);
        assert_eq!(triggered_range(&[], 48_000, 10.0), 0..0);
        assert_eq!(triggered_range(&[[0.0; 2]; 2], 192_000, 10.0), 0..2);
        assert_eq!(triggered_range(&samples[..512], 192_000, 10.0).len(), 256);
        assert_eq!(triggered_range(&samples, 0, 10.0), 0..0);
        let right: Vec<_> = samples.iter().map(|sample| [0.0, sample[0]]).collect();
        assert_eq!(triggered_range(&right, 48_000, 10.0), range);
        for span_ms in [1.0, 50.0] {
            for rate in [44_100, 192_000] {
                let range = triggered_range(&samples, rate, span_ms);
                assert!(samples[range.start][0] > 0.0);
                assert!(samples[range.start - 1][0] <= 0.0);
                assert_eq!(triggered_range(&right, rate, span_ms), range);
            }
        }
    }

    #[test]
    fn trigger_ignores_nonfinite_crossings_and_uses_last_complete_sweep() {
        let mut samples = vec![[0.0, -0.5]; 1024];
        samples[0][0] = f32::NAN;
        samples[1][1] = f32::INFINITY;
        samples[2][1] = 0.5;
        samples[20][1] = 0.5;
        assert_eq!(triggered_range(&samples, 48_000, 10.0), 20..500);
        samples[20][1] = -0.5;
        assert_eq!(triggered_range(&samples, 48_000, 10.0), 544..1024);
        samples[544][1] = 0.5;
        assert_eq!(triggered_range(&samples, 48_000, 10.0), 544..1024);
    }

    #[test]
    fn pixel_buckets_preserve_peaks_and_bound_narrow_geometry() {
        let mut samples = vec![[0.0; 2]; 1024];
        samples[17] = [4.0, -4.0];
        samples[1023] = [-0.75, 0.8];
        samples[100][0] = f32::NAN;
        assert_eq!(physical_columns(0.25, 10.0, 1.0, samples.len()), 1);
        assert_eq!(physical_columns(100.0, 10.0, 2.0, samples.len()), 200);
        assert_eq!(
            physical_columns(f32::MAX, 10.0, f32::MAX, samples.len()),
            1024
        );
        for (width, height, scale) in [
            (0.0, 1.0, 1.0),
            (1.0, 0.0, 1.0),
            (f32::NAN, 1.0, 1.0),
            (1.0, f32::INFINITY, 1.0),
            (1.0, 1.0, -1.0),
        ] {
            assert_eq!(physical_columns(width, height, scale, samples.len()), 0);
        }
        assert_eq!(extrema(&samples, 0), Some((-0.75, 4.0)));
        assert_eq!(extrema(&samples, 1), Some((-4.0, 0.8)));
        assert_eq!(extrema(&[[f32::NAN, f32::INFINITY]], 0), None);
        for columns in [1, 2, 700, 1024] {
            let mut next = 0;
            let mut low = f32::INFINITY;
            let mut high = f32::NEG_INFINITY;
            for bucket in 0..columns {
                let range = bucket_range(samples.len(), columns, bucket);
                assert_eq!(range.start, next);
                assert!(!range.is_empty());
                next = range.end;
                if let Some((min, max)) = extrema(&samples[range], 0) {
                    low = low.min(min);
                    high = high.max(max);
                }
                assert!((0.0..=1.0).contains(&column_fraction(bucket, columns)));
            }
            assert_eq!(next, samples.len());
            assert_eq!((low, high), (-0.75, 4.0));
        }
        assert_eq!(column_fraction(0, 1), 0.5);
        assert_eq!(column_fraction(699, 700), 1.0);
    }

    #[test]
    fn configured_spans_clip_to_pcm_and_report_source_sample_duration() {
        for rate in [8_000, 44_100, 48_000, 96_000, 192_000] {
            for span_ms in [1.0, 10.0, 50.0] {
                for available in [2, 64, 512, 4096] {
                    let samples = vec![[-0.5; 2]; available];
                    let range = triggered_range(&samples, rate, span_ms);
                    let requested = (rate as f64 * span_ms as f64 / 1000.0).round() as usize;
                    let span = requested.min(available / 2);
                    let expected = if span < 2 {
                        requested.min(available)
                    } else {
                        span
                    };
                    assert_eq!(range.len(), expected);
                    assert!(range.end <= available);
                    if expected >= 2 {
                        let actual = sweep_span_ms(range.len(), rate).unwrap();
                        let expected = (expected - 1) as f32 * 1000.0 / rate as f32;
                        assert!((actual - expected).abs() < 0.0001);
                    }
                }
            }
        }
        assert_eq!(sweep_span_ms(1, 48_000), None);
        assert_eq!(sweep_span_ms(480, 0), None);
    }

    #[test]
    fn span_changes_reselect_pcm_while_gain_changes_only_drawing() {
        let scope = OscilloscopeRenderer::new();
        let mut config = Config {
            oscilloscope_time_span_ms: 1.0,
            ..Config::default()
        };
        scope.configure_config(&config, true);
        let mut frame = frame(&config);
        frame.sample_rate = 192_000;
        frame.visualizer.pcm = vec![[2.0, -3.0]; 4096].into();
        scope.update(&frame);
        assert_eq!(scope.0.borrow().points.len(), 192);
        config.oscilloscope_time_span_ms = 50.0;
        assert!(scope.configure_config(&config, true));
        scope.update(&frame);
        let points = scope.0.borrow().points.clone();
        let span_ms = scope.0.borrow().sweep_span_ms;
        assert_eq!(points.len(), 2048);
        assert_eq!(points[0], [2.0, -3.0]);
        assert_eq!(span_ms, sweep_span_ms(2048, 192_000));
        assert!(span_ms.unwrap() < config.oscilloscope_time_span_ms);
        for gain in [0.1, 1.0, 10.0] {
            config.oscilloscope_gain = gain;
            scope.configure_config(&config, true);
            scope.update(&frame);
            assert_eq!(scope.0.borrow().points, points);
            assert_eq!(scope.0.borrow().sweep_span_ms, span_ms);
            assert_eq!(frame.visualizer.pcm[0], [2.0, -3.0]);
            assert_eq!(frame.visualizer.pcm.len(), 4096);
            assert!((-1.0..=1.0).contains(&display_amplitude(points[0][0], gain)));
            assert!((-1.0..=1.0).contains(&display_amplitude(points[0][1], gain)));
        }
        assert!((display_amplitude(2.0, 0.1) - 0.2).abs() < 0.00001);
        assert!((display_amplitude(-3.0, 0.1) + 0.3).abs() < 0.00001);
        assert_eq!(display_amplitude(2.0, 1.0), 1.0);
        assert_eq!(display_amplitude(-3.0, 1.0), -1.0);
    }

    #[test]
    fn scale_axis_uses_input_amplitude_and_actual_source_span() {
        let template = "±{amplitude} · {span} ms";
        assert_eq!(
            scale_label(template, 2.0, sweep_span_ms(480, 48_000)),
            "±0.50 · 9.98 ms"
        );
        assert_eq!(
            scale_label(template, 0.1, sweep_span_ms(2048, 192_000)),
            "±10.00 · 10.66 ms"
        );
        assert_eq!(scale_label(template, 1.0, None), "±1.00 · — ms");
    }

    fn frame(config: &Config) -> AnalysisFrame {
        let mut frame = AnalysisFrame {
            generation: 1,
            sample_rate: 48_000,
            settings: Some(AnalysisSettings::from(config)),
            ..Default::default()
        };
        frame.visualizer.demand.pcm = true;
        frame.visualizer.pcm = vec![[0.25, -0.5]; 512].into();
        frame
    }

    #[test]
    fn compact_sweeps_bound_storage_and_preserve_channel_extrema() {
        let mut samples = vec![[0.0; 2]; MAX_SWEEP_POINTS * 8];
        samples[17] = [-0.8, 0.9];
        samples[18] = [0.7, -0.6];
        samples[20] = [f32::NAN, f32::INFINITY];
        let mut points = Vec::new();
        compact_sweep(&samples, &mut points);
        assert_eq!(points.len(), MAX_SWEEP_POINTS);
        assert_eq!(extrema(&points, 0), Some((-0.8, 0.7)));
        assert_eq!(extrema(&points, 1), Some((-0.6, 0.9)));
    }

    #[test]
    fn hiding_retains_compact_points_and_semantic_changes_clear() {
        let scope = OscilloscopeRenderer::new();
        let mut config = Config::default();
        scope.configure_config(&config, true);
        let frame = frame(&config);
        scope.update(&frame);
        assert_eq!(scope.0.borrow().points[0], [0.25, -0.5]);
        assert_ne!(
            scope.0.borrow().points.as_ptr(),
            frame.visualizer.pcm.as_ptr()
        );
        let points = scope.0.borrow().points.clone();
        scope.configure_config(&config, false);
        assert_eq!(scope.0.borrow().points, points);
        scope.update(&frame);
        assert_eq!(scope.0.borrow().points, points);
        scope.configure_config(&config, true);
        assert_eq!(scope.0.borrow().points, points);
        let mut unavailable = frame.clone();
        unavailable.visualizer.pcm = PcmSnapshot::default();
        scope.update(&unavailable);
        assert_eq!(scope.0.borrow().points, points);
        scope.update(&frame);
        config.analysis_fft_size = 512;
        scope.configure_config(&config, true);
        assert!(scope.0.borrow().points.is_empty());
        scope.update(&frame);
        assert!(scope.0.borrow().points.is_empty());
        let valid = self::frame(&config);
        for invalid in 0..5 {
            scope.update(&valid);
            let mut rejected = valid.clone();
            match invalid {
                0 => rejected.settings = None,
                1 => rejected.visualizer.demand.pcm = false,
                2 => rejected.sample_rate = 0,
                3 => rejected.visualizer.pcm = PcmSnapshot::default(),
                _ => rejected.visualizer.pcm = vec![[0.0; 2]].into(),
            }
            scope.update(&rejected);
            assert_eq!(
                scope.0.borrow().points.is_empty(),
                invalid == 0 || invalid == 2
            );
        }
        scope.update(&valid);
        config.audio_visualizer_primary_color = "#123456".parse().unwrap();
        scope.configure_config(&config, true);
        assert_eq!(scope.0.borrow().points[0], [0.25, -0.5]);
        scope.clear();
        assert!(scope.0.borrow().points.is_empty());
    }

    #[test]
    fn inactive_sweeps_are_cleared_by_continuity_loss_even_without_pcm() {
        let scope = OscilloscopeRenderer::new();
        let config = Config::default();
        scope.configure_config(&config, true);
        let mut frame = frame(&config);
        scope.update(&frame);
        scope.configure_config(&config, false);
        frame.generation += 1;
        frame.visualizer.pcm = PcmSnapshot::default();
        scope.update(&frame);
        assert!(scope.0.borrow().points.is_empty());
    }
}
