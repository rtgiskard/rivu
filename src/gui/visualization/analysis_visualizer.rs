//! Fixed-scale, analysis-backed stereo plots. Texture storage is independent of panel size.
use crate::{
    analysis::{AnalysisFrame, AnalysisSettings, DENSITY_SIDE, DensitySnapshot, SpatialBin},
    config::{AudioVisualizerOptions, AudioVisualizerStyle, Config},
    i18n::Localizer,
};
use gpui::{
    AnyElement, App, Bounds, Hsla, Pixels, Point, RenderImage, SharedString, TextAlign, Window,
    canvas, div, fill, point, prelude::*, px, rgb, size,
};
use std::{cell::RefCell, rc::Rc, sync::Arc, time::Duration};

const SPATIAL_WIDTH: usize = 256;
const SPATIAL_HEIGHT: usize = 128;
const SPATIAL_DECAY_SECONDS: f32 = 0.5;
const FIELD_COLOR_COUNT: usize = 256;
// The analysis backend supports FFTs up to 32768 samples. Retain one axis,
// shared by every spatial panel, never a history of axes or panel-local copies.
const MAX_SPATIAL_BINS: usize = 32768 / 2;

#[derive(Default)]
struct SpatialMapping {
    sample_rate: u32,
    fft_size: u32,
    rows: Vec<(u32, Option<u8>)>,
    #[cfg(test)]
    rebuilds: usize,
}

impl SpatialMapping {
    fn prepare(&mut self, sample_rate: u32, fft_size: u32, bins: &[SpatialBin]) {
        let bins = &bins[..bins.len().min(MAX_SPATIAL_BINS)];
        if self.sample_rate == sample_rate
            && self.fft_size == fft_size
            && self.rows.len() == bins.len()
            && self
                .rows
                .iter()
                .zip(bins)
                .all(|((frequency, _), bin)| *frequency == bin.frequency_hz.to_bits())
        {
            return;
        }
        self.sample_rate = sample_rate;
        self.fft_size = fft_size;
        self.rows.clear();
        let high = high_frequency(sample_rate);
        self.rows.extend(bins.iter().map(|bin| {
            (
                bin.frequency_hz.to_bits(),
                spatial_row(bin.frequency_hz, high).map(|row| row as u8),
            )
        }));
        #[cfg(test)]
        {
            self.rebuilds += 1;
        }
    }
}

#[derive(Clone)]
pub(super) struct AnalysisVisualizer {
    data: Rc<RefCell<PlotData>>,
}

struct PlotData {
    labels: Option<[SharedString; 4]>,
    style: AudioVisualizerStyle,
    visible: bool,
    background: u32,
    primary: u32,
    secondary: u32,
    colors: [[u8; 4]; FIELD_COLOR_COUNT],
    db_range: f32,
    settings: AnalysisSettings,
    generation: Option<u64>,
    sample_rate: u32,
    sample_time: Option<Duration>,
    sequence: Option<u64>,
    // Density borrows the immutable worker snapshot; spatial owns its decaying field.
    density: DensitySnapshot,
    field: Vec<f32>,
    spatial_next: Vec<f32>,
    spatial_mapping: SpatialMapping,
    image: Option<Arc<RenderImage>>,
    image_painted: bool,
    image_error_reported: bool,
    retired: Vec<Arc<RenderImage>>,
    pending: Vec<Arc<RenderImage>>,
    stereo_metrics: [Option<f32>; 3],
    readout: SharedString,
    range_label: SharedString,
    high_label: SharedString,
}

pub(super) fn analysis_style(style: AudioVisualizerStyle) -> bool {
    matches!(
        style,
        AudioVisualizerStyle::StereoDensity | AudioVisualizerStyle::SpatialSpectrum
    )
}

impl AnalysisVisualizer {
    pub(super) fn new(style: AudioVisualizerStyle) -> Self {
        assert!(analysis_style(style));
        let config = Config::default();
        Self {
            data: Rc::new(RefCell::new(PlotData {
                labels: None,
                style,
                visible: true,
                background: config.visual_background.rgb(),
                primary: config.audio_visualizer_primary_color.rgb(),
                secondary: config.audio_visualizer_secondary_color.rgb(),
                colors: field_colors(
                    style,
                    config.audio_visualizer_primary_color.rgb(),
                    config.audio_visualizer_secondary_color.rgb(),
                ),
                db_range: config.audio_visualizer_db_range(),
                settings: AnalysisSettings::from(&config),
                generation: None,
                sample_rate: 0,
                sample_time: None,
                sequence: None,
                density: DensitySnapshot::default(),
                field: Vec::new(),
                spatial_next: Vec::new(),
                spatial_mapping: SpatialMapping::default(),
                image: None,
                image_painted: false,
                image_error_reported: false,
                retired: Vec::new(),
                pending: Vec::new(),
                stereo_metrics: [None; 3],
                readout: "".into(),
                range_label: "".into(),
                high_label: "".into(),
            })),
        }
    }

    pub(super) fn configure(&self, config: &Config) -> bool {
        let mut data = self.data.borrow_mut();
        let settings = AnalysisSettings::from(config);
        let reset =
            settings.fft_size != data.settings.fft_size || settings.window != data.settings.window;
        let colors_changed = data.primary != config.audio_visualizer_primary_color.rgb()
            || data.secondary != config.audio_visualizer_secondary_color.rgb();
        let recolor = colors_changed || data.db_range != config.audio_visualizer_db_range();
        let changed = reset || recolor || data.background != config.visual_background.rgb();
        if data.db_range != config.audio_visualizer_db_range() {
            data.range_label = "".into();
        }
        data.settings = settings;
        data.primary = config.audio_visualizer_primary_color.rgb();
        data.secondary = config.audio_visualizer_secondary_color.rgb();
        if colors_changed {
            data.colors = field_colors(data.style, data.primary, data.secondary);
        }
        data.background = config.visual_background.rgb();
        data.db_range = config.audio_visualizer_db_range();
        if reset {
            data.clear();
        } else if recolor && !data.field().is_empty() {
            data.rasterize();
        }
        changed
    }

    pub(super) fn set_visible(&self, visible: bool) -> bool {
        let mut data = self.data.borrow_mut();
        let restored = visible && !data.visible;
        if data.visible != visible {
            data.visible = visible;
            data.clear();
        }
        restored
    }

    pub(super) fn update(&self, frame: &AnalysisFrame) {
        self.data.borrow_mut().update(frame);
    }

    pub(super) fn release_retired_images(&self, window: &mut Window) {
        let mut data = self.data.borrow_mut();
        data.release_images(window);
        if !data.retired.is_empty() {
            let data = Rc::clone(&self.data);
            window.on_next_frame(move |window, _| {
                data.borrow_mut().release_images(window);
            });
        }
    }

    pub(super) fn release_all_images(&self, window: &mut Window) {
        let mut data = self.data.borrow_mut();
        data.clear();
        let PlotData {
            retired, pending, ..
        } = &mut *data;
        for image in retired.drain(..).chain(pending.drain(..)) {
            if let Err(error) = window.drop_image(image) {
                eprintln!("rivu analysis visualizer image release: {error}");
            }
        }
    }

    pub(super) fn view(
        &self,
        i18n: &Arc<Localizer>,
        panel_id: u64,
        options: AudioVisualizerOptions,
    ) -> AnyElement {
        let data = Rc::clone(&self.data);
        self.data.borrow_mut().labels.get_or_insert_with(|| {
            [
                "visual-density-title",
                "visual-spatial-title",
                "visual-mid",
                "visual-side",
            ]
            .map(|id| i18n.text(id).to_owned().into())
        });
        let i18n = Arc::clone(i18n);
        div()
            .id(("analysis-visualizer", panel_id))
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, cx| {
                        data.borrow_mut().paint(&i18n, bounds, options, window, cx);
                    },
                )
                .size_full(),
            )
            .into_any_element()
    }
}

impl PlotData {
    fn release_images(&mut self, window: &mut Window) {
        for image in self.retired.drain(..) {
            if let Err(error) = window.drop_image(image) {
                eprintln!("rivu analysis visualizer image release: {error}");
            }
        }
        // Advance pending images without requesting a content redraw. The image
        // in the last submitted scene remains retained until it is replaced.
        self.retired.append(&mut self.pending);
    }

    fn retire_image(&mut self) {
        if let Some(image) = self.image.take() {
            // Images never painted have no GPU allocation to release.
            if self.image_painted {
                self.pending.push(image);
            }
        }
        self.image_painted = false;
        self.image_error_reported = false;
    }

    fn clear(&mut self) {
        self.density = DensitySnapshot::default();
        self.retire_image();
        if self.visible {
            self.field.clear();
            self.spatial_next.clear();
        } else {
            self.field = Vec::new();
            self.spatial_next = Vec::new();
            self.spatial_mapping = SpatialMapping::default();
        }
        self.generation = None;
        self.sample_rate = 0;
        self.sample_time = None;
        self.sequence = None;
        self.stereo_metrics = [None; 3];
        self.readout = "".into();
        self.high_label = "".into();
    }

    fn update(&mut self, frame: &AnalysisFrame) {
        if !self.visible {
            return;
        }
        if self.generation != Some(frame.generation) || self.sample_rate != frame.sample_rate {
            self.clear();
            self.generation = Some(frame.generation);
            self.sample_rate = frame.sample_rate;
        }
        let demanded = match self.style {
            AudioVisualizerStyle::StereoDensity => frame.visualizer.demand.stereo_density,
            AudioVisualizerStyle::SpatialSpectrum => frame.visualizer.demand.spatial_spectrum,
            _ => false,
        };
        if !demanded
            || !frame.settings.is_some_and(|settings| {
                settings.fft_size == self.settings.fft_size
                    && settings.window == self.settings.window
            })
        {
            return;
        }
        if self.sequence == Some(frame.sequence) || self.sample_time == Some(frame.sample_time) {
            return;
        }
        if self
            .sample_time
            .is_some_and(|last| frame.sample_time < last)
        {
            self.clear();
            self.generation = Some(frame.generation);
            self.sample_rate = frame.sample_rate;
        }
        let mut changed = self.image.is_none();
        match self.style {
            AudioVisualizerStyle::StereoDensity => {
                if frame.visualizer.density.len() != DENSITY_SIDE * DENSITY_SIDE {
                    return;
                }
                if self.density != frame.visualizer.density {
                    changed = true;
                }
                // Also replace equal-valued snapshots so obsolete slots can be reused.
                self.density.clone_from(&frame.visualizer.density);
                let metrics = [
                    frame.visualizer.correlation,
                    frame.visualizer.balance,
                    frame.visualizer.side_ratio,
                ];
                if self.stereo_metrics != metrics {
                    self.stereo_metrics = metrics;
                    self.readout = "".into();
                }
            }
            AudioVisualizerStyle::SpatialSpectrum => {
                let high = high_frequency(frame.sample_rate);
                if high <= 20.0 {
                    return;
                }
                self.field.resize(SPATIAL_WIDTH * SPATIAL_HEIGHT, 0.0);
                self.spatial_next
                    .resize(SPATIAL_WIDTH * SPATIAL_HEIGHT, 0.0);
                let elapsed = self
                    .sample_time
                    .map(|last| frame.sample_time.saturating_sub(last).as_secs_f32())
                    .unwrap_or(0.0);
                decay_spatial(&self.field, &mut self.spatial_next, elapsed);
                self.spatial_mapping.prepare(
                    frame.sample_rate,
                    self.settings.fft_size,
                    &frame.visualizer.spatial,
                );
                for (index, bin) in frame.visualizer.spatial.iter().enumerate() {
                    // Oversized, non-backend axes still retain the old numeric behavior
                    // without growing the cache beyond the largest supported FFT.
                    let y = self
                        .spatial_mapping
                        .rows
                        .get(index)
                        .map(|(_, row)| row.map(usize::from))
                        .unwrap_or_else(|| spatial_row(bin.frequency_hz, high));
                    let Some(y) = y else {
                        continue;
                    };
                    let Some(x) = spatial_column(bin.pan) else {
                        continue;
                    };
                    if !bin.power_db.is_finite() {
                        continue;
                    }
                    // Peak hold per cell avoids making louder colors simply because
                    // more FFT bins share a logarithmic row. It retains calibrated power.
                    let power = 10.0_f32.powf(bin.power_db.min(0.0) * 0.1);
                    let cell = &mut self.spatial_next[y * SPATIAL_WIDTH + x];
                    *cell = cell.max(power);
                }
                if self.field != self.spatial_next {
                    std::mem::swap(&mut self.field, &mut self.spatial_next);
                    changed = true;
                }
            }
            _ => return,
        }
        self.sample_time = Some(frame.sample_time);
        self.sequence = Some(frame.sequence);
        if changed {
            self.rasterize();
        }
    }

    fn field(&self) -> &[f32] {
        if self.style == AudioVisualizerStyle::StereoDensity {
            &self.density
        } else {
            &self.field
        }
    }

    fn rasterize(&mut self) {
        let (width, height) = match self.style {
            AudioVisualizerStyle::StereoDensity => (DENSITY_SIDE, DENSITY_SIDE),
            AudioVisualizerStyle::SpatialSpectrum => (SPATIAL_WIDTH, SPATIAL_HEIGHT),
            _ => return,
        };
        let mut pixels = image::RgbaImage::new(width as u32, height as u32);
        for (pixel, value) in pixels.pixels_mut().zip(self.field()) {
            let strength = field_strength(*value, self.db_range);
            let index = (strength * (FIELD_COLOR_COUNT - 1) as f32).round() as usize;
            *pixel = image::Rgba(self.colors[index]);
        }
        self.retire_image();
        self.image = Some(Arc::new(RenderImage::new([image::Frame::new(pixels)])));
    }

    fn paint(
        &mut self,
        i18n: &Localizer,
        bounds: Bounds<Pixels>,
        options: AudioVisualizerOptions,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.paint_quad(fill(bounds, rgb(self.background)));
        let width = bounds.size.width / px(1.0);
        let height = bounds.size.height / px(1.0);
        if width < 24.0 || height < 24.0 || !self.visible {
            return;
        }
        let labels = options.labels && width >= 250.0 && height >= 155.0;
        let density = self.style == AudioVisualizerStyle::StereoDensity;
        let left = if labels { 49.0 } else { 6.0 };
        let top = if labels { 25.0 } else { 6.0 };
        let bottom = if labels {
            if density { 72.0 } else { 51.0 }
        } else {
            6.0
        };
        let plot_width = (width - left - 12.0).max(1.0);
        let plot_height = (height - top - bottom).max(1.0);
        let (plot_width, plot_height) = if density {
            let side = plot_width.min(plot_height);
            (side, side)
        } else {
            (plot_width, plot_height)
        };
        let plot = Bounds::new(
            point(
                bounds.left() + px(left + (width - left - 12.0 - plot_width) * 0.5),
                bounds.top() + px(top),
            ),
            size(px(plot_width), px(plot_height)),
        );
        window.paint_quad(fill(
            plot,
            rgb(mix_color(self.background, self.primary, 0.035)),
        ));
        if let Some(image) = &self.image {
            self.image_painted = true;
            if let Err(error) =
                window.paint_image(plot, plot, px(0.0).into(), Arc::clone(image), 0, false)
                && !self.image_error_reported
            {
                self.image_error_reported = true;
                eprintln!("rivu analysis visualizer image paint: {error}");
            }
        }
        if options.reference_lines {
            for fraction in [0.0, 0.25, 0.5, 0.75, 1.0] {
                let alpha = if fraction == 0.5 { 0.26 } else { 0.10 };
                window.paint_quad(fill(
                    Bounds::new(
                        point(
                            plot.left() + (plot.size.width - px(1.0)) * fraction,
                            plot.top(),
                        ),
                        size(px(1.0), plot.size.height),
                    ),
                    rgb(0xb1bbc5).alpha(alpha),
                ));
                if density || fraction == 0.0 || fraction == 1.0 {
                    window.paint_quad(fill(
                        Bounds::new(
                            point(
                                plot.left(),
                                plot.top() + (plot.size.height - px(1.0)) * fraction,
                            ),
                            size(plot.size.width, px(1.0)),
                        ),
                        rgb(0xb1bbc5).alpha(alpha),
                    ));
                }
            }
        }
        if !density && (options.reference_lines || labels) {
            let high = high_frequency(if self.sample_rate == 0 {
                48_000
            } else {
                self.sample_rate
            });
            let log_span = (high / 20.0).ln();
            for (hz, text) in [
                (100.0_f32, "100 Hz"),
                (1000.0, "1 kHz"),
                (10_000.0, "10 kHz"),
            ] {
                if hz >= high {
                    continue;
                }
                let fraction = 1.0 - (hz / 20.0).ln() / log_span;
                let y = plot.top() + plot.size.height * fraction;
                if options.reference_lines {
                    window.paint_quad(fill(
                        Bounds::new(point(plot.left(), y), size(plot.size.width, px(1.0))),
                        rgb(0xb1bbc5).alpha(0.10),
                    ));
                }
                if labels && plot_height >= 150.0 {
                    label(
                        text.into(),
                        point(plot.left() - px(5.0), y - px(6.0)),
                        1.0,
                        44.0,
                        window,
                        cx,
                    );
                }
            }
        }
        if !labels {
            return;
        }
        if self.range_label.is_empty() {
            self.range_label = i18n
                .format(
                    if density {
                        "visual-density-range"
                    } else {
                        "visual-power-range"
                    },
                    &[("range", format!("{:.0}", self.db_range).into())],
                )
                .into();
        }
        let title = self.labels.as_ref().expect("localized plot labels")
            [if density { 0 } else { 1 }]
        .clone();
        label(
            title,
            point(bounds.left() + px(10.0), bounds.top() + px(5.0)),
            0.0,
            width - 20.0,
            window,
            cx,
        );
        let axis_labels = if density {
            ["−1", "0", "+1"]
        } else {
            ["L", "C", "R"]
        };
        for (index, text) in axis_labels.into_iter().enumerate() {
            let fraction = index as f32 * 0.5;
            label(
                text.into(),
                point(
                    plot.left() + plot.size.width * fraction,
                    plot.bottom() + px(3.0),
                ),
                fraction,
                35.0,
                window,
                cx,
            );
        }
        if density {
            if self.readout.is_empty() {
                self.readout = stereo_readout(i18n, self.stereo_metrics).into();
            }
            for (text, fraction) in [("+1", 0.0), ("0", 0.5), ("−1", 1.0)] {
                label(
                    text.into(),
                    point(
                        plot.left() - px(6.0),
                        plot.top() + (plot.size.height - px(12.0)) * fraction,
                    ),
                    1.0,
                    30.0,
                    window,
                    cx,
                );
            }
            label(
                self.labels.as_ref().expect("localized plot labels")[2].clone(),
                point(bounds.left() + px(5.0), plot.top() + px(16.0)),
                0.0,
                35.0,
                window,
                cx,
            );
            label(
                self.labels.as_ref().expect("localized plot labels")[3].clone(),
                point(
                    plot.left() + plot.size.width * 0.5,
                    plot.bottom() + px(18.0),
                ),
                0.5,
                plot_width,
                window,
                cx,
            );
            label(
                self.readout.clone(),
                point(
                    bounds.left() + bounds.size.width * 0.5,
                    plot.bottom() + px(38.0),
                ),
                0.5,
                width - 16.0,
                window,
                cx,
            );
            label(
                self.range_label.clone(),
                point(
                    bounds.left() + bounds.size.width * 0.5,
                    plot.bottom() + px(53.0),
                ),
                0.5,
                width - 16.0,
                window,
                cx,
            );
        } else {
            if self.high_label.is_empty() {
                let high = high_frequency(if self.sample_rate == 0 {
                    48_000
                } else {
                    self.sample_rate
                });
                self.high_label = frequency_label(high).into();
            }
            label(
                self.high_label.clone(),
                point(plot.left() - px(5.0), plot.top()),
                1.0,
                44.0,
                window,
                cx,
            );
            label(
                "20 Hz".into(),
                point(plot.left() - px(5.0), plot.bottom() - px(12.0)),
                1.0,
                44.0,
                window,
                cx,
            );
            label(
                self.range_label.clone(),
                point(
                    plot.left() + plot.size.width * 0.5,
                    plot.bottom() + px(20.0),
                ),
                0.5,
                plot_width,
                window,
                cx,
            );
        }
    }
}

fn high_frequency(sample_rate: u32) -> f32 {
    (sample_rate as f32 * 0.5).min(20_000.0)
}

fn spatial_column(pan: f32) -> Option<usize> {
    pan.is_finite().then(|| {
        (((pan.clamp(-1.0, 1.0) + 1.0) * 0.5) * (SPATIAL_WIDTH - 1) as f32).round() as usize
    })
}

fn spatial_row(frequency: f32, high: f32) -> Option<usize> {
    if !frequency.is_finite() || high <= 20.0 || frequency < 20.0 || frequency > high {
        return None;
    }
    Some(
        ((1.0 - (frequency / 20.0).ln() / (high / 20.0).ln()).clamp(0.0, 1.0)
            * (SPATIAL_HEIGHT - 1) as f32)
            .round() as usize,
    )
}

// Keep the original combined transform as an independent numeric reference.
#[cfg(test)]
fn spatial_cell(frequency: f32, pan: f32, high: f32) -> Option<(usize, usize)> {
    if !frequency.is_finite()
        || !pan.is_finite()
        || high <= 20.0
        || frequency < 20.0
        || frequency > high
    {
        return None;
    }
    let x = (((pan.clamp(-1.0, 1.0) + 1.0) * 0.5) * (SPATIAL_WIDTH - 1) as f32).round() as usize;
    let y = ((1.0 - (frequency / 20.0).ln() / (high / 20.0).ln()).clamp(0.0, 1.0)
        * (SPATIAL_HEIGHT - 1) as f32)
        .round() as usize;
    Some((x, y))
}

fn decay_spatial(source: &[f32], target: &mut [f32], elapsed: f32) {
    let decay = (-elapsed.max(0.0) / SPATIAL_DECAY_SECONDS).exp();
    for (target, source) in target.iter_mut().zip(source) {
        *target = *source * decay;
    }
}

fn field_strength(value: f32, db_range: f32) -> f32 {
    // Fixed references: density occupancy=1; spatial spectral power=1 (0 dBFS).
    // Brightness never depends on the loudest cell in the current field.
    if !value.is_finite() || value <= 0.0 {
        0.0
    } else {
        ((10.0 * value.log10() + db_range) / db_range).clamp(0.0, 1.0)
    }
}

fn stereo_readout(i18n: &Localizer, metrics: [Option<f32>; 3]) -> String {
    let value = |value: Option<f32>| value.filter(|value| value.is_finite());
    let correlation = value(metrics[0])
        .map(|v| format!("{:+.2}", v.clamp(-1.0, 1.0)))
        .unwrap_or_else(|| "—".into());
    let balance = value(metrics[1])
        .map(|v| {
            let v = v.clamp(-1.0, 1.0);
            format!("{:.0}/{:.0}%", (1.0 - v) * 50.0, (1.0 + v) * 50.0)
        })
        .unwrap_or_else(|| "—".into());
    let side = value(metrics[2])
        .map(|v| format!("{:.0}%", v.clamp(0.0, 1.0) * 100.0))
        .unwrap_or_else(|| "—".into());
    i18n.format(
        "visual-stereo-readout",
        &[
            ("correlation", correlation.into()),
            ("balance", balance.into()),
            ("side", side.into()),
        ],
    )
}

fn frequency_label(hz: f32) -> String {
    if hz >= 1000.0 {
        format!("{:.0} kHz", hz / 1000.0)
    } else {
        format!("{hz:.0} Hz")
    }
}

fn mix_color(first: u32, second: u32, fraction: f32) -> u32 {
    let channel = |shift: u32| {
        let first = ((first >> shift) & 0xff) as f32;
        let second = ((second >> shift) & 0xff) as f32;
        (first + (second - first) * fraction.clamp(0.0, 1.0)).round() as u32
    };
    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

fn field_colors(
    style: AudioVisualizerStyle,
    primary: u32,
    secondary: u32,
) -> [[u8; 4]; FIELD_COLOR_COUNT] {
    // Keep configured hues, with a cyanward shoulder for density. Lightness
    // separates occupancy/power levels; only the very strongest cells go pale.
    let shade = |color: u32, lightness: f32, hue_shift: f32| {
        let mut color = Hsla::from(rgb(color));
        color.h = (color.h + hue_shift).rem_euclid(1.0);
        color.l = lightness;
        u32::from(color.to_rgb()) >> 8
    };
    let stops = if style == AudioVisualizerStyle::StereoDensity {
        [
            (0.0, shade(primary, 0.18, 0.0)),
            (0.25, shade(primary, 0.40, 0.0)),
            (0.65, shade(primary, 0.58, -1.0 / 12.0)),
            (0.88, shade(primary, 0.80, -1.0 / 12.0)),
            (1.0, shade(secondary, 0.95, 0.0)),
        ]
    } else {
        [
            (0.0, shade(primary, 0.14, 0.0)),
            (0.30, shade(primary, 0.38, 0.0)),
            (0.65, shade(secondary, 0.62, 0.0)),
            (0.88, shade(secondary, 0.80, 0.0)),
            (1.0, shade(secondary, 0.95, 0.0)),
        ]
    };
    // Build only on color changes, not per frame or per cell. GPUI consumes
    // BGRA bytes; image::RgbaImage does not perform the channel conversion.
    std::array::from_fn(|index| {
        let strength = index as f32 / (FIELD_COLOR_COUNT - 1) as f32;
        let interval = stops.windows(2).find(|pair| strength <= pair[1].0).unwrap();
        let t = (strength - interval[0].0) / (interval[1].0 - interval[0].0);
        let color = mix_color(interval[0].1, interval[1].1, t);
        [
            color as u8,
            (color >> 8) as u8,
            (color >> 16) as u8,
            (strength.sqrt() * 255.0).round() as u8,
        ]
    })
}

fn label(
    text: SharedString,
    anchor: Point<Pixels>,
    alignment: f32,
    available_width: f32,
    window: &mut Window,
    cx: &mut App,
) {
    let mut style = window.text_style();
    style.color = rgb(0xb1bbc5).into();
    let run = style.to_run(text.len());
    let line = window
        .text_system()
        .shape_line(text, px(10.0), &[run], None);
    // Tiny panels omit labels rather than overlap or paint out of bounds.
    if line.width() > px(available_width) {
        return;
    }
    if let Err(error) = line.paint(
        point(anchor.x - line.width() * alignment, anchor.y),
        px(14.0),
        TextAlign::Left,
        None,
        window,
        cx,
    ) {
        eprintln!("rivu analysis visualizer label: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::{SpatialBin, VisualizerDemand, VisualizerFrame};

    fn frame(style: AudioVisualizerStyle) -> AnalysisFrame {
        AnalysisFrame {
            sequence: 1,
            generation: 1,
            sample_time: Duration::from_secs(1),
            sample_rate: 48_000,
            settings: Some(AnalysisSettings::from(&Config::default())),
            visualizer: VisualizerFrame {
                demand: VisualizerDemand {
                    stereo_density: style == AudioVisualizerStyle::StereoDensity,
                    spatial_spectrum: style == AudioVisualizerStyle::SpatialSpectrum,
                    band_energy: false,
                },
                spatial: vec![SpatialBin {
                    frequency_hz: 1000.0,
                    pan: 0.0,
                    power_db: -30.0,
                }],
                density: if style == AudioVisualizerStyle::StereoDensity {
                    vec![0.0; DENSITY_SIDE * DENSITY_SIDE].into()
                } else {
                    DensitySnapshot::default()
                },
                ..VisualizerFrame::default()
            },
            ..AnalysisFrame::default()
        }
    }

    fn set_density_cell(frame: &mut AnalysisFrame, index: usize, value: f32) {
        let mut cells = frame.visualizer.density.to_vec();
        cells[index] = value;
        frame.visualizer.density = cells.into();
    }

    fn plot(style: AudioVisualizerStyle) -> AnalysisVisualizer {
        let plot = AnalysisVisualizer::new(style);
        plot.configure(&Config::default());
        plot
    }
    #[test]
    fn cached_spatial_rows_match_original_transform_at_edges_and_fft_axes() {
        for sample_rate in [0, 40, 41, 8_000, 44_100, 48_000, 192_000] {
            let high = high_frequency(sample_rate);
            for fft_size in [512, 8192, 32768] {
                let mut bins: Vec<_> = (1..=fft_size / 2)
                    .map(|bin| SpatialBin {
                        frequency_hz: bin as f32 * sample_rate as f32 / fft_size as f32,
                        pan: 0.0,
                        power_db: -30.0,
                    })
                    .collect();
                let mut mapping = SpatialMapping::default();
                mapping.prepare(sample_rate, fft_size, &bins);
                for (bin, (_, row)) in bins.iter().zip(&mapping.rows) {
                    for pan in [-2.0, -1.0, -0.0, 0.0, 1.0, 2.0, f32::NAN, f32::INFINITY] {
                        assert_eq!(
                            spatial_column(pan).zip(row.map(usize::from)),
                            spatial_cell(bin.frequency_hz, pan, high),
                            "rate={sample_rate}, FFT={fft_size}, hz={}, pan={pan}",
                            bin.frequency_hz,
                        );
                    }
                }
                bins.clear();
                bins.extend(
                    [
                        f32::NAN,
                        f32::NEG_INFINITY,
                        f32::INFINITY,
                        -20.0,
                        -0.0,
                        f32::from_bits(20.0_f32.to_bits() - 1),
                        20.0,
                        f32::from_bits(20.0_f32.to_bits() + 1),
                        high,
                        f32::from_bits(high.to_bits() + 1),
                    ]
                    .map(|frequency_hz| SpatialBin {
                        frequency_hz,
                        pan: 0.0,
                        power_db: -30.0,
                    }),
                );
                // Include logarithmic row-rounding boundaries and their adjacent floats.
                if high > 20.0 {
                    for row in 0..SPATIAL_HEIGHT - 1 {
                        let frequency = 20.0
                            * ((1.0 - (row as f32 + 0.5) / (SPATIAL_HEIGHT - 1) as f32)
                                * (high / 20.0).ln())
                            .exp();
                        bins.extend([-1_i32, 0, 1].map(|offset| SpatialBin {
                            frequency_hz: f32::from_bits(
                                (frequency.to_bits() as i64 + offset as i64) as u32,
                            ),
                            pan: 0.0,
                            power_db: -30.0,
                        }));
                    }
                }
                mapping.prepare(sample_rate, fft_size, &bins);
                for (bin, (_, row)) in bins.iter().zip(&mapping.rows) {
                    assert_eq!(
                        spatial_column(bin.pan).zip(row.map(usize::from)),
                        spatial_cell(bin.frequency_hz, bin.pan, high)
                    );
                }
                mapping.prepare(sample_rate, fft_size, &bins);
                assert_eq!(
                    mapping.rebuilds, 2,
                    "unchanged NaN bits must reuse the axis"
                );
            }
        }
    }

    #[test]
    fn spatial_mapping_reuses_static_axis_and_bounds_retained_rows() {
        let mut bins: Vec<_> = (1..=4096)
            .map(|bin| SpatialBin {
                frequency_hz: bin as f32 * 48_000.0 / 8192.0,
                pan: 0.0,
                power_db: -30.0,
            })
            .collect();
        let mut mapping = SpatialMapping::default();
        for tick in 0..120 {
            bins[0].pan = tick as f32 / 120.0;
            bins[0].power_db = -(tick as f32);
            mapping.prepare(48_000, 8192, &bins);
        }
        assert_eq!(mapping.rebuilds, 1);
        bins[0].frequency_hz = 1000.0;
        mapping.prepare(48_000, 8192, &bins);
        assert_eq!(mapping.rebuilds, 2);
        mapping.prepare(44_100, 8192, &bins);
        assert_eq!(mapping.rebuilds, 3);
        mapping.prepare(44_100, 16384, &bins);
        assert_eq!(mapping.rebuilds, 4);
        bins.resize(MAX_SPATIAL_BINS + 1, bins[0]);
        mapping.prepare(44_100, 16384, &bins);
        assert_eq!(mapping.rows.len(), MAX_SPATIAL_BINS);
    }

    #[test]
    fn rasterized_fields_preserve_channels_silence_and_fixed_brightness() {
        for style in [
            AudioVisualizerStyle::StereoDensity,
            AudioVisualizerStyle::SpatialSpectrum,
        ] {
            let plot = plot(style);
            let mut config = Config {
                audio_visualizer_primary_color: "#ff3300".parse().unwrap(),
                audio_visualizer_secondary_color: "#ff7700".parse().unwrap(),
                ..Config::default()
            };
            plot.configure(&config);
            let mut data = plot.data.borrow_mut();
            let cells = if style == AudioVisualizerStyle::StereoDensity {
                DENSITY_SIDE * DENSITY_SIDE
            } else {
                SPATIAL_WIDTH * SPATIAL_HEIGHT
            };
            let mut field = vec![0.0; cells];
            field[1] = 0.001;
            if style == AudioVisualizerStyle::StereoDensity {
                data.density = field.into();
            } else {
                data.field = field;
            }
            data.rasterize();
            let image = data.image.as_ref().unwrap();
            let bytes = image.as_bytes(0).unwrap();
            assert_eq!(bytes[3], 0, "silence must remain transparent");
            assert!(
                bytes[6] > bytes[4],
                "red tint must be red in GPUI's BGRA texture"
            );
            let moderate: [u8; 4] = bytes[4..8].try_into().unwrap();
            if style == AudioVisualizerStyle::StereoDensity {
                let mut field = data.density.to_vec();
                field[2] = 1.0;
                data.density = field.into();
            } else {
                data.field[2] = 1.0;
            }
            data.rasterize();
            let bytes = data.image.as_ref().unwrap().as_bytes(0).unwrap();
            assert_eq!(
                &bytes[4..8],
                &moderate,
                "another peak cannot renormalize this cell"
            );
            drop(data);
            config.audio_visualizer_primary_color = "#0033ff".parse().unwrap();
            config.audio_visualizer_secondary_color = "#0077ff".parse().unwrap();
            plot.configure(&config);
            let data = plot.data.borrow();
            assert_eq!(
                data.field()[1],
                0.001,
                "recoloring must preserve measured power"
            );
            let bytes = data.image.as_ref().unwrap().as_bytes(0).unwrap();
            assert!(
                bytes[4] > bytes[6],
                "blue tint must remain blue after recoloring"
            );
        }
    }

    #[test]
    fn frequency_and_energy_balance_have_fixed_coordinates() {
        assert_eq!(spatial_cell(20.0, -1.0, 20_000.0), Some((0, 127)));
        assert_eq!(spatial_cell(20_000.0, 1.0, 20_000.0), Some((255, 0)));
        let (center_x, center_y) =
            spatial_cell((20.0_f32 * 20_000.0).sqrt(), 0.0, 20_000.0).unwrap();
        assert_eq!(center_x, 128);
        assert!(center_y == 63 || center_y == 64);
        assert_eq!(spatial_cell(20_001.0, 0.0, 20_000.0), None);
        assert_eq!(spatial_cell(19.0, 0.0, 20_000.0), None);
        assert_eq!(spatial_cell(1000.0, f32::NAN, 20_000.0), None);
        assert_eq!(spatial_cell(3000.0, 0.0, 2000.0), None);
    }

    #[test]
    fn trail_decays_only_in_accepted_sample_time_and_does_not_sum_bins() {
        let plot = plot(AudioVisualizerStyle::SpatialSpectrum);
        let mut frame = frame(AudioVisualizerStyle::SpatialSpectrum);
        frame.visualizer.spatial.push(frame.visualizer.spatial[0]);
        plot.update(&frame);
        let (x, y) = spatial_cell(1000.0, 0.0, 20_000.0).unwrap();
        let index = y * SPATIAL_WIDTH + x;
        assert!((plot.data.borrow().field[index] - 0.001).abs() < 1e-7);
        let image = Arc::clone(plot.data.borrow().image.as_ref().unwrap());
        frame.sequence += 1;
        plot.update(&frame);
        assert!(Arc::ptr_eq(
            &image,
            plot.data.borrow().image.as_ref().unwrap()
        ));
        frame.sample_time += Duration::from_millis(50);
        frame.sequence += 1;
        plot.update(&frame);
        assert!(Arc::ptr_eq(
            &image,
            plot.data.borrow().image.as_ref().unwrap()
        ));
        frame.sample_time += Duration::from_millis(500);
        frame.sequence += 1;
        frame.visualizer.spatial.clear();
        plot.update(&frame);
        assert!((plot.data.borrow().field[index] - 0.001 / std::f32::consts::E).abs() < 1e-7);
    }

    #[test]
    fn generation_demand_settings_and_visibility_invalidate_old_fields() {
        let plot = plot(AudioVisualizerStyle::SpatialSpectrum);
        let mut frame = frame(AudioVisualizerStyle::SpatialSpectrum);
        plot.update(&frame);
        frame.generation += 1;
        frame.visualizer.demand.spatial_spectrum = false;
        plot.update(&frame);
        assert!(plot.data.borrow().image.is_none());
        frame.visualizer.demand.spatial_spectrum = true;
        plot.update(&frame);
        assert!(plot.data.borrow().image.is_some());
        plot.configure(&Config {
            audio_visualizer_style: AudioVisualizerStyle::SpatialSpectrum,
            analysis_fft_size: 16384,
            ..Config::default()
        });
        plot.update(&frame);
        assert!(plot.data.borrow().image.is_none());
        frame.settings.as_mut().unwrap().fft_size = 16384;
        plot.update(&frame);
        plot.set_visible(false);
        plot.update(&frame);
        assert!(plot.data.borrow().field.is_empty());
        plot.set_visible(true);
        plot.configure(&Config {
            audio_visualizer_style: AudioVisualizerStyle::StereoDensity,
            ..Config::default()
        });
        plot.update(&frame);
        assert!(plot.data.borrow().image.is_none());
        let density = self::frame(AudioVisualizerStyle::StereoDensity);
        plot.update(&density);
        assert!(plot.data.borrow().field.is_empty());
        plot.update(&self::frame(AudioVisualizerStyle::SpatialSpectrum));
        assert_eq!(
            plot.data.borrow().field.len(),
            SPATIAL_WIDTH * SPATIAL_HEIGHT
        );
    }

    #[test]
    fn density_is_not_auto_normalized_and_unchanged_data_reuses_texture() {
        assert_eq!(field_strength(0.0, 70.0), 0.0);
        assert_eq!(field_strength(1.0, 70.0), 1.0);
        assert!((field_strength(0.001, 60.0) - 0.5).abs() < 1e-6);
        let plot = plot(AudioVisualizerStyle::StereoDensity);
        let mut frame = frame(AudioVisualizerStyle::StereoDensity);
        set_density_cell(&mut frame, 0, 0.001);
        plot.update(&frame);
        assert_eq!(plot.data.borrow().field()[0], 0.001);
        let image = Arc::clone(plot.data.borrow().image.as_ref().unwrap());
        frame.sample_time += Duration::from_millis(100);
        frame.sequence += 1;
        plot.update(&frame);
        assert!(Arc::ptr_eq(
            &image,
            plot.data.borrow().image.as_ref().unwrap()
        ));
        set_density_cell(&mut frame, DENSITY_SIDE * DENSITY_SIDE - 1, 1.0);
        frame.sample_time += Duration::from_millis(100);
        frame.sequence += 1;
        plot.update(&frame);
        assert_eq!(plot.data.borrow().field()[0], 0.001);
        assert_eq!(
            plot.data.borrow().field()[DENSITY_SIDE * DENSITY_SIDE - 1],
            1.0
        );
        assert!(!Arc::ptr_eq(
            &image,
            plot.data.borrow().image.as_ref().unwrap()
        ));
    }

    #[test]
    fn density_panels_share_snapshots_and_release_them_independently() {
        let first = plot(AudioVisualizerStyle::StereoDensity);
        let second = plot(AudioVisualizerStyle::StereoDensity);
        let mut frame = frame(AudioVisualizerStyle::StereoDensity);
        set_density_cell(&mut frame, 0, 0.25);
        first.update(&frame);
        second.update(&frame);
        let old = frame.visualizer.density.clone();
        for plot in [&first, &second] {
            assert_eq!(plot.data.borrow().density.as_ptr(), old.as_ptr());
            assert_eq!(plot.data.borrow().field.capacity(), 0);
        }
        first.set_visible(false);
        assert!(first.data.borrow().density.is_empty());
        assert_eq!(second.data.borrow().density.as_ptr(), old.as_ptr());
        set_density_cell(&mut frame, 0, 0.5);
        frame.sequence += 1;
        frame.sample_time += Duration::from_millis(100);
        first.update(&frame);
        second.update(&frame);
        assert!(first.data.borrow().density.is_empty());
        assert_eq!(second.data.borrow().field()[0], 0.5);
        assert_eq!(
            second.data.borrow().density.as_ptr(),
            frame.visualizer.density.as_ptr(),
        );
        assert_eq!(old[0], 0.25);
    }

    #[test]
    fn only_uploaded_images_need_delayed_retirement() {
        let plot = plot(AudioVisualizerStyle::StereoDensity);
        let mut frame = frame(AudioVisualizerStyle::StereoDensity);
        plot.update(&frame);
        let old = Arc::downgrade(plot.data.borrow().image.as_ref().unwrap());
        set_density_cell(&mut frame, 0, 1.0);
        frame.sample_time += Duration::from_millis(100);
        frame.sequence += 1;
        plot.update(&frame);
        assert!(old.upgrade().is_none());
        let uploaded = Arc::downgrade(plot.data.borrow().image.as_ref().unwrap());
        plot.data.borrow_mut().image_painted = true;
        plot.set_visible(false);
        assert!(uploaded.upgrade().is_some());
        assert_eq!(plot.data.borrow().pending.len(), 1);
        assert!(plot.data.borrow().image.is_none());
    }

    #[test]
    fn hidden_analysis_releases_fields_without_recoloring_or_consuming_audio() {
        for style in [
            AudioVisualizerStyle::StereoDensity,
            AudioVisualizerStyle::SpatialSpectrum,
        ] {
            let plot = plot(style);
            plot.update(&frame(style));
            assert!(!plot.data.borrow().field().is_empty());
            plot.set_visible(false);
            assert_eq!(plot.data.borrow().field.capacity(), 0);
            assert!(plot.data.borrow().density.is_empty());
            assert_eq!(plot.data.borrow().spatial_next.capacity(), 0);
            assert_eq!(plot.data.borrow().spatial_mapping.rows.capacity(), 0);
            plot.configure(&Config {
                audio_visualizer_style: style,
                audio_visualizer_db_range: Some(50.0),
                ..Config::default()
            });
            plot.update(&frame(style));
            assert!(plot.data.borrow().image.is_none());
            assert_eq!(plot.data.borrow().field.capacity(), 0);
            plot.set_visible(true);
            plot.update(&frame(style));
            assert!(plot.data.borrow().image.is_some());
        }
    }

    #[test]
    fn simultaneous_analysis_styles_keep_independent_fields_and_retire_only_hidden_images() {
        let density = plot(AudioVisualizerStyle::StereoDensity);
        let spatial = plot(AudioVisualizerStyle::SpatialSpectrum);
        let mut frame = frame(AudioVisualizerStyle::StereoDensity);
        frame.visualizer.demand.spatial_spectrum = true;
        frame.visualizer.demand.band_energy = true;
        set_density_cell(&mut frame, 0, 0.25);
        density.update(&frame);
        spatial.update(&frame);
        assert_eq!(density.data.borrow().field()[0], 0.25);
        assert!(spatial.data.borrow().field.iter().any(|value| *value > 0.0));
        let density_image = Arc::downgrade(density.data.borrow().image.as_ref().unwrap());
        let spatial_image = Arc::clone(spatial.data.borrow().image.as_ref().unwrap());
        density.data.borrow_mut().image_painted = true;
        spatial.data.borrow_mut().image_painted = true;
        let config = Config {
            audio_visualizer_style: AudioVisualizerStyle::FractalFlow,
            ..Config::default()
        };
        assert!(!density.configure(&config));
        assert!(!spatial.configure(&config));
        density.set_visible(false);
        assert!(density_image.upgrade().is_some());
        assert_eq!(density.data.borrow().pending.len(), 1);
        assert_eq!(density.data.borrow().field.capacity(), 0);
        assert!(spatial.data.borrow().pending.is_empty());
        assert!(Arc::ptr_eq(
            &spatial_image,
            spatial.data.borrow().image.as_ref().unwrap()
        ));
        frame.sequence += 1;
        frame.sample_time += Duration::from_millis(50);
        frame.visualizer.spatial[0].power_db = -10.0;
        density.update(&frame);
        spatial.update(&frame);
        assert!(density.data.borrow().image.is_none());
        assert!(!Arc::ptr_eq(
            &spatial_image,
            spatial.data.borrow().image.as_ref().unwrap()
        ));
        assert_eq!(spatial.data.borrow().pending.len(), 1);
        assert_eq!(density.data.borrow().pending.len(), 1);
    }
}
