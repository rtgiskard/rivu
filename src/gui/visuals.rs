//! GPU-backed analysis views. The audio worker owns the 8192-point FFT; these
//! views retain every musical band supplied by `AnalysisFrame`.
//!
//! GPUI's public `RenderImage` is immutable, so the spectrogram caches one
//! one-pixel-wide BGRA image per history column rather than rebuilding a whole
//! texture. At 126 bands this uploads 504 bytes per new visible frame, retains
//! at most 180 image columns (90,720 pixel bytes), and submits at most 180 image
//! primitives, not 22,680 widgets. These are payload counts, not benchmark
//! measurements; atlas padding and renderer bookkeeping are additional costs.
//! Replaced images are explicitly removed from the window's atlas. Hidden
//! spectrograms retain reusable CPU columns but create no new GPU images.

use std::{cell::RefCell, rc::Rc, sync::Arc};

use gpui::{
    AnyElement, App, Bounds, PathBuilder, Pixels, Point, RenderImage, SharedString, TextAlign,
    Window, canvas, div, fill, linear_color_stop, linear_gradient, point, prelude::*, px, rgb,
    size,
};

use crate::analysis::AnalysisFrame;

const HISTORY_COLUMNS: usize = 180;
const SPECTRUM_STOPS: &[(f32, [u8; 3])] = &[
    (0.0, [190, 35, 45]),
    (0.2, [235, 90, 30]),
    (0.4, [245, 205, 55]),
    (0.6, [90, 205, 100]),
    (0.8, [40, 190, 210]),
    (1.0, [55, 90, 220]),
];
const HEAT_STOPS: &[(f32, [u8; 3])] = &[
    (0.0, [0, 0, 8]),
    (0.18, [15, 45, 150]),
    (0.38, [15, 190, 210]),
    (0.58, [55, 200, 100]),
    (0.76, [240, 215, 40]),
    (0.9, [245, 105, 20]),
    (1.0, [235, 30, 25]),
];

#[derive(Default)]
struct Column {
    levels: Vec<f32>,
    image: Option<Arc<RenderImage>>,
}

struct VisualData {
    frequencies: Vec<f32>,
    frequency_labels: [SharedString; 3],
    peaks: Vec<f32>,
    columns: Vec<Column>,
    head: usize,
    len: usize,
    // Only columns painted since the last cleanup can enter this queue. Thus
    // even while hidden it cannot grow past HISTORY_COLUMNS.
    retired: Vec<Arc<RenderImage>>,
    error: Option<String>,
}

pub(super) struct Visuals {
    // GPUI prepaint/paint closures are 'static. Share the UI-thread state rather
    // than copying the full history into each panel's render closure.
    data: Rc<RefCell<VisualData>>,
}

impl Visuals {
    pub(super) fn new() -> Self {
        Self {
            data: Rc::new(RefCell::new(VisualData {
                frequencies: Vec::new(),
                frequency_labels: ["8 Hz".into(), "300 Hz".into(), "11 kHz".into()],
                peaks: Vec::new(),
                columns: (0..HISTORY_COLUMNS).map(|_| Column::default()).collect(),
                head: 0,
                len: 0,
                retired: Vec::with_capacity(HISTORY_COLUMNS),
                error: None,
            })),
        }
    }

    pub(super) fn update(&mut self, frame: &AnalysisFrame) {
        let mut data = self.data.borrow_mut();
        if frame.spectrum_db.len() != frame.frequencies_hz.len() {
            data.report_error("Analysis frequency and level counts differ".into());
            return;
        }
        if data.frequencies != frame.frequencies_hz {
            for index in 0..HISTORY_COLUMNS {
                if let Some(image) = data.columns[index].image.take() {
                    data.retired.push(image);
                }
            }
            data.head = 0;
            data.len = 0;
            data.frequencies.clone_from(&frame.frequencies_hz);
            data.peaks.clear();
            data.peaks.resize(frame.spectrum_db.len(), -70.0);
            if let (Some(low), Some(high)) = (data.frequencies.first(), data.frequencies.last()) {
                let middle = (*low * *high).sqrt();
                data.frequency_labels = [
                    frequency_label(*low),
                    frequency_label(middle),
                    frequency_label(*high),
                ];
            }
        }
        if frame.spectrum_db.is_empty() {
            return;
        }
        let head = data.head;
        if let Some(image) = data.columns[head].image.take() {
            data.retired.push(image);
        }
        data.columns[head].levels.clone_from(&frame.spectrum_db);
        for (peak, value) in data.peaks.iter_mut().zip(&frame.spectrum_db) {
            *peak = (*peak - 1.5).max(finite_level(*value));
        }
        data.head = (head + 1) % HISTORY_COLUMNS;
        data.len = (data.len + 1).min(HISTORY_COLUMNS);
    }

    pub(super) fn spectrum(&self, panel_id: u64) -> AnyElement {
        self.view(panel_id, false)
    }

    pub(super) fn spectrogram(&self, panel_id: u64) -> AnyElement {
        self.view(panel_id, true)
    }

    fn view(&self, panel_id: u64, spectrogram: bool) -> AnyElement {
        let data = Rc::clone(&self.data);
        div()
            .id((
                if spectrogram {
                    "spectrogram"
                } else {
                    "spectrum"
                },
                panel_id,
            ))
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .bg(rgb(0x000000))
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, cx| {
                        let mut data = data.borrow_mut();
                        data.release_retired(window);
                        window.paint_quad(fill(bounds, rgb(0x000000)));
                        if bounds.size.width <= px(64.0) || bounds.size.height <= px(40.0) {
                            return;
                        }
                        if data.len == 0 {
                            paint_label(
                                "Analysis waiting for playback".into(),
                                point(bounds.left() + px(8.0), bounds.top() + px(8.0)),
                                0x8794a4,
                                window,
                                cx,
                            );
                        } else {
                            let result = if spectrogram {
                                data.paint_spectrogram(bounds, window, cx)
                            } else {
                                data.paint_spectrum(bounds, window, cx)
                            };
                            if let Err(error) = result {
                                data.report_error(error);
                            }
                        }
                        if let Some(error) = &data.error {
                            window.paint_quad(fill(
                                Bounds::new(bounds.origin, size(bounds.size.width, px(22.0))),
                                rgb(0x481b20),
                            ));
                            paint_label(
                                format!("Visualization error: {error}").into(),
                                point(bounds.left() + px(6.0), bounds.top() + px(3.0)),
                                0xffb5b5,
                                window,
                                cx,
                            );
                        }
                    },
                )
                .size_full(),
            )
            .into_any_element()
    }
}

impl VisualData {
    fn report_error(&mut self, error: String) {
        if self.error.as_ref() != Some(&error) {
            eprintln!("rivu visualization: {error}");
            self.error = Some(error);
        }
    }

    fn release_retired(&mut self, window: &mut Window) {
        // Both visual canvases clean up, even when only the spectrum is shown.
        while let Some(image) = self.retired.pop() {
            if let Err(error) = window.drop_image(image) {
                self.report_error(format!("Cannot release spectrogram image: {error}"));
            }
        }
    }

    fn paint_spectrum(
        &self,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<(), String> {
        let plot = Bounds::new(
            point(bounds.left() + px(8.0), bounds.top() + px(10.0)),
            size(bounds.size.width - px(16.0), bounds.size.height - px(32.0)),
        );
        let latest = (self.head + HISTORY_COLUMNS - 1) % HISTORY_COLUMNS;
        let levels = &self.columns[latest].levels;
        let position = |index: usize, value: f32| {
            point(
                plot.left() + plot.size.width * (index as f32 + 0.5) / levels.len() as f32,
                plot.bottom() - plot.size.height * spectrum_height(value),
            )
        };
        for db in [-15.0, -30.0, -45.0] {
            let y = plot.bottom() - plot.size.height * spectrum_height(db);
            window.paint_quad(fill(
                Bounds::new(point(plot.left(), y), size(plot.size.width, px(1.0))),
                rgb(0x151a20),
            ));
        }
        if levels.len() == 1 {
            let top = position(0, levels[0]);
            window.paint_quad(fill(
                Bounds::new(
                    point(top.x - px(1.0), top.y),
                    size(px(2.0), plot.bottom() - top.y),
                ),
                rgb(0x64b9ae),
            ));
        } else {
            // Five gradient sections, not one path/widget per frequency band.
            // The shared endpoint keeps the curve and filled area continuous.
            for section in 0..SPECTRUM_STOPS.len() - 1 {
                let first = section * (levels.len() - 1) / (SPECTRUM_STOPS.len() - 1);
                let last = (section + 1) * (levels.len() - 1) / (SPECTRUM_STOPS.len() - 1);
                if first == last {
                    continue;
                }
                let mut area = PathBuilder::fill();
                let mut line = PathBuilder::stroke(px(1.7));
                let start = position(first, levels[first]);
                area.move_to(point(start.x, plot.bottom()));
                area.line_to(start);
                line.move_to(start);
                for (index, value) in levels.iter().enumerate().take(last + 1).skip(first + 1) {
                    let next = position(index, *value);
                    area.line_to(next);
                    line.line_to(next);
                }
                area.line_to(point(position(last, levels[last]).x, plot.bottom()));
                area.close();
                let from = color_rgb(gradient(
                    first as f32 / (levels.len() - 1) as f32,
                    SPECTRUM_STOPS,
                ));
                let to = color_rgb(gradient(
                    last as f32 / (levels.len() - 1) as f32,
                    SPECTRUM_STOPS,
                ));
                let area = area
                    .build()
                    .map_err(|error| format!("Spectrum fill: {error}"))?;
                let line = line
                    .build()
                    .map_err(|error| format!("Spectrum curve: {error}"))?;
                window.paint_path(
                    area,
                    linear_gradient(
                        90.0,
                        linear_color_stop(from.alpha(0.25), 0.0),
                        linear_color_stop(to.alpha(0.25), 1.0),
                    ),
                );
                window.paint_path(
                    line,
                    linear_gradient(
                        90.0,
                        linear_color_stop(from, 0.0),
                        linear_color_stop(to, 1.0),
                    ),
                );
            }
        }
        let band_width = plot.size.width / levels.len() as f32;
        for (index, peak) in self.peaks.iter().enumerate() {
            let point = position(index, *peak);
            window.paint_quad(fill(
                Bounds::new(
                    point - gpui::point(band_width * 0.3, px(0.0)),
                    size((band_width * 0.6).max(px(1.0)), px(1.0)),
                ),
                rgb(0xc5ced7),
            ));
        }
        paint_label("0 dB".into(), plot.origin, 0xb1bbc5, window, cx);
        paint_label(
            "−60 dB".into(),
            point(plot.left(), plot.bottom() - px(14.0)),
            0x8794a4,
            window,
            cx,
        );
        let labels_y = bounds.bottom() - px(17.0);
        paint_label(
            self.frequency_labels[0].clone(),
            point(plot.left(), labels_y),
            0xb1bbc5,
            window,
            cx,
        );
        paint_aligned_label(
            self.frequency_labels[1].clone(),
            point(plot.center().x, labels_y),
            0.5,
            window,
            cx,
        );
        paint_aligned_label(
            self.frequency_labels[2].clone(),
            point(plot.right(), labels_y),
            1.0,
            window,
            cx,
        );
        Ok(())
    }

    fn paint_spectrogram(
        &mut self,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<(), String> {
        let plot = Bounds::new(
            point(bounds.left() + px(8.0), bounds.top() + px(6.0)),
            size(bounds.size.width - px(16.0), bounds.size.height - px(28.0)),
        );
        let column_width = plot.size.width / HISTORY_COLUMNS as f32;
        // head is the next write slot. Traversing from it puts unused columns
        // at the left until full, then oldest left and newest right forever.
        for display_index in HISTORY_COLUMNS - self.len..HISTORY_COLUMNS {
            let index = (self.head + display_index) % HISTORY_COLUMNS;
            let column = &mut self.columns[index];
            if column.image.is_none() {
                let height = u32::try_from(column.levels.len()).map_err(|_| {
                    "Spectrogram image height exceeds GPU image dimensions".to_string()
                })?;
                let mut pixels = image::RgbaImage::new(1, height);
                for (band, level) in column.levels.iter().enumerate() {
                    let [red, green, blue] = gradient(
                        ((finite_level(*level) + 70.0) / 70.0).clamp(0.0, 1.0),
                        HEAT_STOPS,
                    );
                    // RenderImage expects BGRA, despite image::Frame's RGBA type.
                    // Flip frequency only: high bands top, low bands bottom.
                    pixels.put_pixel(
                        0,
                        height - 1 - band as u32,
                        image::Rgba([blue, green, red, 255]),
                    );
                }
                column.image = Some(Arc::new(RenderImage::new([image::Frame::new(pixels)])));
            }
            let column_bounds = Bounds::new(
                point(
                    plot.left() + column_width * display_index as f32,
                    plot.top(),
                ),
                size(column_width, plot.size.height),
            );
            if let Some(image) = &column.image {
                window
                    .paint_image(
                        column_bounds,
                        column_bounds,
                        px(0.0).into(),
                        Arc::clone(image),
                        0,
                        false,
                    )
                    .map_err(|error| format!("Cannot upload/paint spectrogram image: {error}"))?;
            }
        }
        paint_label(
            self.frequency_labels[2].clone(),
            point(plot.left() + px(3.0), plot.top() + px(2.0)),
            0xe4ebf2,
            window,
            cx,
        );
        paint_label(
            self.frequency_labels[0].clone(),
            point(plot.left() + px(3.0), plot.bottom() - px(16.0)),
            0xe4ebf2,
            window,
            cx,
        );
        let labels_y = bounds.bottom() - px(17.0);
        paint_label(
            "older".into(),
            point(plot.left(), labels_y),
            0x8794a4,
            window,
            cx,
        );
        paint_aligned_label(
            "now →".into(),
            point(plot.right(), labels_y),
            1.0,
            window,
            cx,
        );
        Ok(())
    }
}

fn finite_level(value: f32) -> f32 {
    if value.is_finite() { value } else { -70.0 }
}

fn spectrum_height(value: f32) -> f32 {
    (finite_level(value).clamp(-60.0, 0.0) + 60.0) / 60.0
}

fn frequency_label(frequency: f32) -> SharedString {
    if frequency >= 1000.0 {
        format!("{:.1} kHz", frequency / 1000.0).into()
    } else {
        format!("{frequency:.0} Hz").into()
    }
}

fn gradient(value: f32, stops: &[(f32, [u8; 3])]) -> [u8; 3] {
    let value = value.clamp(0.0, 1.0);
    for pair in stops.windows(2) {
        if value <= pair[1].0 {
            let fraction = (value - pair[0].0) / (pair[1].0 - pair[0].0);
            return std::array::from_fn(|channel| {
                (pair[0].1[channel] as f32
                    + fraction * (pair[1].1[channel] as f32 - pair[0].1[channel] as f32))
                    as u8
            });
        }
    }
    stops[stops.len() - 1].1
}

fn color_rgb([red, green, blue]: [u8; 3]) -> gpui::Rgba {
    rgb((u32::from(red) << 16) | (u32::from(green) << 8) | u32::from(blue))
}

fn paint_label(
    text: SharedString,
    origin: Point<Pixels>,
    color: u32,
    window: &mut Window,
    cx: &mut App,
) {
    let mut style = window.text_style();
    style.color = rgb(color).into();
    let run = style.to_run(text.len());
    let line = window
        .text_system()
        .shape_line(text, px(10.0), &[run], None);
    if let Err(error) = line.paint(origin, px(14.0), TextAlign::Left, None, window, cx) {
        eprintln!("rivu visualization label: {error}");
    }
}

fn paint_aligned_label(
    text: SharedString,
    anchor: Point<Pixels>,
    alignment: f32,
    window: &mut Window,
    cx: &mut App,
) {
    let mut style = window.text_style();
    style.color = rgb(0xb1bbc5).into();
    let run = style.to_run(text.len());
    let line = window
        .text_system()
        .shape_line(text, px(10.0), &[run], None);
    let origin = point(anchor.x - line.width() * alignment, anchor.y);
    if let Err(error) = line.paint(origin, px(14.0), TextAlign::Left, None, window, cx) {
        eprintln!("rivu visualization label: {error}");
    }
}
