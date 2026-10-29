//! GPUI audio visualizations consuming borrowed analysis frames and shared waveform snapshots.
mod fractal_beams;
mod fractal_flow;
mod fractal_renderer;
mod frequency_views;
mod labels;
mod oscilloscope_renderer;
mod palette;
mod parameters;
mod radial_renderer;
mod spectrogram_flow;
mod stereo_renderer;
mod style_coordinator;
mod waveform;

pub use frequency_views::FrequencyViews;
pub use labels::VisualLabels;
pub use parameters::{VisualParameters, WaveformScanParameters};
pub use style_coordinator::StyleCoordinator;
pub use waveform::{Waveform, WaveformEvent, WaveformViewState};

use gpui::{Div, SharedString, div, prelude::*, rgb};

fn visualization_status(message: impl Into<SharedString>) -> Div {
    div()
        .w_full()
        .p_2()
        .text_sm()
        .text_color(rgb(0xb1bbc5))
        .child(message.into())
}

fn format_time(seconds: f64) -> String {
    let seconds = if seconds.is_finite() {
        seconds.max(0.0) as u64
    } else {
        0
    };
    format!("{}:{:02}", seconds / 60, seconds % 60)
}
