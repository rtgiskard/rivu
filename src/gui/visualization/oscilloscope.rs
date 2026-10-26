//! Triggered, unwindowed stereo PCM, not a waveform synthesized from FFT bins.
use crate::{
    config::{AudioVisualizerOptions, Config},
    i18n::Localizer,
};
use gpui::{
    AnyElement, PathBuilder, SharedString, TextAlign, canvas, div, point, prelude::*, px, rgb,
};
use rivu_audio::analysis::{AnalysisFrame, AnalysisSettings, PcmSnapshot};
use std::{cell::RefCell, ops::Range, rc::Rc, sync::Arc};

#[derive(Clone)]
pub(super) struct Oscilloscope(Rc<RefCell<Data>>);

struct Data {
    visible: bool,
    settings: AnalysisSettings,
    samples: PcmSnapshot,
    range: Range<usize>,
    background: u32,
    colors: [u32; 2],
    label: Option<SharedString>,
}

impl Data {
    fn clear(&mut self) {
        self.samples = PcmSnapshot::default();
        self.range = 0..0;
    }
}

impl Oscilloscope {
    pub(super) fn new() -> Self {
        let config = Config::default();
        Self(Rc::new(RefCell::new(Data {
            visible: false,
            settings: AnalysisSettings::from(&config),
            samples: PcmSnapshot::default(),
            range: 0..0,
            background: config.visual_background.rgb(),
            colors: [
                config.audio_visualizer_primary_color.rgb(),
                config.audio_visualizer_secondary_color.rgb(),
            ],
            label: None,
        })))
    }

    pub(super) fn configure(&self, config: &Config, visible: bool) -> bool {
        let mut data = self.0.borrow_mut();
        let settings = AnalysisSettings::from(config);
        let colors = [
            config.audio_visualizer_primary_color.rgb(),
            config.audio_visualizer_secondary_color.rgb(),
        ];
        let changed = data.visible != visible
            || data.settings != settings
            || data.background != config.visual_background.rgb()
            || data.colors != colors;
        if data.visible != visible || data.settings != settings {
            data.clear();
        }
        data.visible = visible;
        data.settings = settings;
        data.background = config.visual_background.rgb();
        data.colors = colors;
        changed
    }

    // Presentation rejects retired display tokens before dispatching a frame.
    // A reset or unavailable PCM must also erase the last visible sweep.
    pub(super) fn update(&self, frame: &AnalysisFrame) {
        let mut data = self.0.borrow_mut();
        if !data.visible {
            return;
        }
        if frame.settings != Some(data.settings)
            || !frame.visualizer.demand.pcm
            || frame.sample_rate == 0
            || frame.visualizer.pcm.len() < 2
        {
            data.clear();
            return;
        }
        data.range = triggered_range(&frame.visualizer.pcm, frame.sample_rate);
        data.samples.clone_from(&frame.visualizer.pcm);
    }

    pub(super) fn view(
        &self,
        i18n: &Arc<Localizer>,
        panel_id: u64,
        options: AudioVisualizerOptions,
    ) -> AnyElement {
        let data = Rc::clone(&self.0);
        if options.labels {
            let label = i18n.text("visual-oscilloscope-scale");
            let mut data = data.borrow_mut();
            if data.label.as_deref() != Some(label) {
                data.label = Some(label.to_owned().into());
            }
        }
        let has_samples = !data.borrow().range.is_empty();
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
                        let samples = &data.samples[data.range.clone()];
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
                        if options.reference_lines {
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
                        if options.labels
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
            .when(!has_samples, |panel| {
                panel.child(
                    div()
                        .absolute()
                        .top_6()
                        .left_2()
                        .text_sm()
                        .text_color(rgb(0xb1bbc5))
                        .child(i18n.text("visual-awaiting-audio").to_owned()),
                )
            })
            .into_any_element()
    }
}

// Prefer a rising zero crossing on L; right-only audio triggers on R. A full
// 10 ms sweep needs an equally sized trigger-search prefix. Short FFT windows
// show their available half-window instead of stretching or inventing samples.
fn triggered_range(samples: &[[f32; 2]], sample_rate: u32) -> Range<usize> {
    let maximum = sample_rate as usize / 100;
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
        .map(|value| value.clamp(-1.0, 1.0))
        .fold(None, |range, value| {
            Some(range.map_or((value, value), |(low, high): (f32, f32)| {
                (low.min(value), high.max(value))
            }))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_triggers_on_real_stereo_pcm_and_bounds_short_windows() {
        let samples: Vec<_> = (0..1024)
            .map(|i| {
                let value = (std::f32::consts::TAU * (i as f32 - 7.0) / 32.0).sin();
                [value, -value]
            })
            .collect();
        let range = triggered_range(&samples, 48_000);
        assert_eq!(range.len(), 480);
        assert!(samples[range.start][0] > 0.0);
        assert!(samples[range.start - 1][0] <= 0.0);
        assert_eq!(triggered_range(&[], 48_000), 0..0);
        assert_eq!(triggered_range(&[[0.0; 2]; 2], 192_000), 0..2);
        assert_eq!(triggered_range(&samples[..512], 192_000).len(), 256);
        assert_eq!(triggered_range(&samples, 0), 0..0);
        let right: Vec<_> = samples.iter().map(|sample| [0.0, sample[0]]).collect();
        assert_eq!(triggered_range(&right, 48_000), range);
    }

    #[test]
    fn trigger_ignores_nonfinite_crossings_and_uses_last_complete_sweep() {
        let mut samples = vec![[0.0, -0.5]; 1024];
        samples[0][0] = f32::NAN;
        samples[1][1] = f32::INFINITY;
        samples[2][1] = 0.5;
        samples[20][1] = 0.5;
        assert_eq!(triggered_range(&samples, 48_000), 20..500);
        samples[20][1] = -0.5;
        assert_eq!(triggered_range(&samples, 48_000), 544..1024);
        samples[544][1] = 0.5;
        assert_eq!(triggered_range(&samples, 48_000), 544..1024);
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
        assert_eq!(extrema(&samples, 0), Some((-0.75, 1.0)));
        assert_eq!(extrema(&samples, 1), Some((-1.0, 0.8)));
        assert_eq!(extrema(&[[f32::NAN, f32::INFINITY]], 0), None);
        for columns in [1, 2, 700, 1024] {
            let mut next = 0;
            let mut low = 1.0_f32;
            let mut high = -1.0_f32;
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
            assert_eq!((low, high), (-0.75, 1.0));
        }
        assert_eq!(column_fraction(0, 1), 0.5);
        assert_eq!(column_fraction(699, 700), 1.0);
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
    fn hidden_settings_and_invalid_publications_release_pcm() {
        let scope = Oscilloscope::new();
        let mut config = Config::default();
        scope.configure(&config, true);
        let frame = frame(&config);
        scope.update(&frame);
        assert_eq!(scope.0.borrow().samples[0], [0.25, -0.5]);
        assert_eq!(
            scope.0.borrow().samples.as_ptr(),
            frame.visualizer.pcm.as_ptr()
        );
        scope.configure(&config, false);
        assert!(scope.0.borrow().samples.is_empty());
        scope.update(&frame);
        assert!(scope.0.borrow().samples.is_empty());
        scope.configure(&config, true);
        assert!(scope.0.borrow().samples.is_empty());
        scope.update(&frame);
        config.analysis_fft_size = 512;
        scope.configure(&config, true);
        assert!(scope.0.borrow().samples.is_empty());
        scope.update(&frame);
        assert!(scope.0.borrow().samples.is_empty());
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
            assert!(scope.0.borrow().samples.is_empty());
            assert!(scope.0.borrow().range.is_empty());
        }
        scope.update(&valid);
        config.audio_visualizer_primary_color = "#123456".parse().unwrap();
        scope.configure(&config, true);
        assert_eq!(
            scope.0.borrow().samples.as_ptr(),
            valid.visualizer.pcm.as_ptr()
        );
    }
}
