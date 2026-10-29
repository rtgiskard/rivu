//! Host-localized strings, named `{placeholder}` templates, and shared text painting.
use gpui::{App, Pixels, Point, SharedString, TextAlign, Window, point, px, rgb};

#[derive(Clone, Debug, Default)]
pub struct VisualLabels {
    pub density_title: SharedString,
    pub spatial_title: SharedString,
    pub mid: SharedString,
    pub side: SharedString,
    pub level_zero: SharedString,
    pub level_half: SharedString,
    pub level_full: SharedString,
    pub scene: SharedString,
    pub oscilloscope_scale: SharedString,
    pub no_spectrum: SharedString,
    pub no_spectrogram: SharedString,
    pub waveform_loading: SharedString,
    pub visual_error: SharedString,
    pub waveform_error: SharedString,
    pub density_range: SharedString,
    pub power_range: SharedString,
    pub stereo_readout: SharedString,
}

impl VisualLabels {
    pub fn visual_error(&self, error: &str) -> String {
        self.visual_error.replace("{error}", error)
    }

    pub fn waveform_error(&self, error: &str) -> String {
        self.waveform_error.replace("{error}", error)
    }

    pub fn range(&self, density: bool, range: f32) -> String {
        let template = if density {
            &self.density_range
        } else {
            &self.power_range
        };
        template.replace("{range}", &format!("{range:.0}"))
    }

    pub fn stereo_readout(&self, correlation: &str, balance: &str, side: &str) -> String {
        self.stereo_readout
            .replace("{correlation}", correlation)
            .replace("{balance}", balance)
            .replace("{side}", side)
    }
}

pub(super) fn paint_label(
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

pub(super) fn paint_aligned_label(
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
