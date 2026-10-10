use super::{
    BORDER, ButtonTooltip, DropdownItem, DropdownState, ERROR, GuiApp, Measured,
    POPOVER_MAX_HEIGHT, StatusKind, UI_INSET, active_control, button, caption, column,
    components::DROPDOWN_ROW_REMS,
    copyable_message, dropdown_container, dropdown_row, dropdown_trigger, form_field, icon_button,
    input::{Input, InputEvent},
    row, section_group, status_message, toggle_button, two_column,
};
use crate::{
    config::{
        AudioVisualizerOptions, Config, LogLevel, RadialSpectrumStyle, RgbColor, SpectrumStyle,
        SpectrumWindow, VisualizationPalette,
    },
    model::{Command, RepeatMode},
};
use anyhow::{Context as _, Result};
use gpui::{prelude::*, *};
use std::{collections::HashMap, path::PathBuf, str::FromStr};

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingChoice {
    Device(usize),
    Font(usize),
    CustomFont,
    SpectrumStyle(SpectrumStyle),
    Fft(u32),
    RadialSpectrumStyle(RadialSpectrumStyle),
    Palette(VisualizationPalette),
    LogLevel(LogLevel),
    Window(SpectrumWindow),
}

impl SettingChoice {
    fn label(self) -> String {
        match self {
            Self::Device(_) | Self::Font(_) | Self::CustomFont => String::new(),
            Self::SpectrumStyle(value) => spectrum_style_label(value).to_owned(),
            Self::RadialSpectrumStyle(value) => radial_spectrum_style_label(value).to_owned(),
            Self::Palette(value) => visualization_palette_label(value).to_owned(),
            Self::Fft(value) => value.to_string(),
            Self::LogLevel(value) => match value {
                LogLevel::Debug => "Debug".to_owned(),
                LogLevel::Info => "Info".to_owned(),
                LogLevel::Warning => "Warning".to_owned(),
                LogLevel::Error => "Error".to_owned(),
            },
            Self::Window(SpectrumWindow::Hann) => "Hann".to_owned(),
            Self::Window(SpectrumWindow::BlackmanHarris) => "Blackman–Harris".to_owned(),
            Self::Window(SpectrumWindow::None) => "None".to_owned(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Field {
    Roots,
    ScanMaxDepth,
    Volume,
    PlayCountThreshold,
    QueueLimit,
    LogRetentionWeeks,
    MediaReadBuffer,
    Font,
    Scale,
    Fps,
    Background,
    VisualizationDb,
    RadialSpectrumSensitivity,
    RadialSpectrumRotationSpeed,
    RadialSpectrumBarWidth,
    RadialSpectrumBarGlowLayers,
    RadialSpectrumRingOpacity,
    RadialSpectrumBloomIntensity,
    RadialSpectrumInnerDiameter,
    RadialSpectrumPrimaryColor,
    RadialSpectrumSecondaryColor,
    RadialSpectrumDb,
    SpectrumDb,
    SpectrumBarWidth,
    SpectrumBars,
    SpectrumGap,
    SpectrumHold,
    SpectrumGravity,
    SpectrumBarHold,
    SpectrumBarGravity,
    SpectrumSmoothing,
    SpectrogramDb,
    SpectrogramHistory,
    SpectrogramSamplingPointsScale,
    SpectrogramInterpolationPoints,
    CursorColor,
    Glow,
    WaveformRmsGain,
    WaveformPeakGain,
    WaveformPeakGamma,
}
impl Field {
    fn label(self) -> &'static str {
        match self {
            Self::Roots => "Library folders",
            Self::ScanMaxDepth => "Maximum scan depth",
            Self::Volume => "Volume (%)",
            Self::PlayCountThreshold => "Play count threshold (%)",
            Self::QueueLimit => "Queue capacity (tracks)",
            Self::LogRetentionWeeks => "Log retention (weeks)",
            Self::MediaReadBuffer => "Media read buffer (MiB)",
            Self::Font => "Font family",
            Self::Scale => "Interface scale",
            Self::Fps => "Analysis rate (Hz)",
            Self::Background => "Background color",
            Self::VisualizationDb => "Default display range (dB)",
            Self::RadialSpectrumSensitivity => "Sensitivity",
            Self::RadialSpectrumRotationSpeed => "Rotation speed",
            Self::RadialSpectrumBarWidth => "Bar width",
            Self::RadialSpectrumBarGlowLayers => "Bar glow layers",
            Self::RadialSpectrumRingOpacity => "Ring opacity",
            Self::RadialSpectrumBloomIntensity => "Bloom intensity",
            Self::RadialSpectrumInnerDiameter => "Inner diameter",
            Self::RadialSpectrumPrimaryColor => "Primary color",
            Self::RadialSpectrumSecondaryColor => "Secondary color",
            Self::SpectrumDb | Self::RadialSpectrumDb => "Display range (dB)",
            Self::SpectrogramDb => "Color range (dB)",
            Self::SpectrumBarWidth => "Automatic bar width (px)",
            Self::SpectrumBars => "Bar count",
            Self::SpectrumGap => "Bar gap (px)",
            Self::SpectrumHold => "Peak hold (ms)",
            Self::SpectrumGravity => "Peak gravity (dB/s²)",
            Self::SpectrumBarHold => "Bar hold (ms)",
            Self::SpectrumBarGravity => "Bar gravity (dB/s²)",
            Self::SpectrumSmoothing => "Release smoothing (ms)",
            Self::SpectrogramHistory => "History limit (s)",
            Self::SpectrogramSamplingPointsScale => "Source row scale",
            Self::SpectrogramInterpolationPoints => "Maximum frequency rows",
            Self::CursorColor => "Waterline color",
            Self::Glow => "Glow strength",
            Self::WaveformRmsGain => "RMS gain",
            Self::WaveformPeakGain => "Peak gain",
            Self::WaveformPeakGamma => "Peak gamma",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::Roots => "Separate paths with semicolons.",
            Self::ScanMaxDepth => "Minimum 1; starting folder = depth 0.",
            Self::Volume => "0–100.",
            Self::PlayCountThreshold => "0 ≤ value < 100; based on time heard.",
            Self::QueueLimit => "1–4096.",
            Self::LogRetentionWeeks => "1–520.",
            Self::MediaReadBuffer => "1, 2, 4, 8, or 16. Applies when media is next opened.",
            Self::Font => "",
            Self::Scale => "0.75–2.",
            Self::Fps => "5–60.",
            Self::Background
            | Self::RadialSpectrumPrimaryColor
            | Self::RadialSpectrumSecondaryColor
            | Self::CursorColor => "#RRGGBB.",
            Self::VisualizationDb => "1–160.",
            // The group supplies the live inherited range for these fields.
            Self::SpectrumDb | Self::RadialSpectrumDb | Self::SpectrogramDb => "",
            Self::RadialSpectrumSensitivity => "0–5.",
            Self::RadialSpectrumRotationSpeed => "0–10.",
            Self::RadialSpectrumBarWidth
            | Self::RadialSpectrumBloomIntensity
            | Self::WaveformPeakGain => "0–2.",
            Self::RadialSpectrumBarGlowLayers => "0–4.",
            Self::RadialSpectrumRingOpacity => "0–1.",
            Self::RadialSpectrumInnerDiameter => "0–2; relative to panel size.",
            Self::SpectrumBarWidth => "1–20; used with automatic bar count.",
            Self::SpectrumBars => "1–512; 0 = automatic.",
            Self::SpectrumGap => "0–8.",
            Self::SpectrumHold | Self::SpectrumBarHold => "0–2000.",
            Self::SpectrumGravity | Self::SpectrumBarGravity => "0–500.",
            Self::SpectrumSmoothing => "0–1000.",
            Self::SpectrogramHistory => "1–120; limited by panel width and analysis rate.",
            Self::SpectrogramSamplingPointsScale => {
                "0.4–1.4; relative to the tallest visible panel."
            }
            Self::SpectrogramInterpolationPoints => {
                "64–4096; applies with or without interpolation."
            }
            Self::Glow => "0–2; 0 = off.",
            Self::WaveformRmsGain => "0–3.",
            Self::WaveformPeakGamma => "0.25–3; above 1 reduces low peaks, below 1 lifts them.",
        }
    }

    fn form(self, control: impl IntoElement) -> Div {
        let hint = self.hint();
        form_field(self.label(), control).when(!hint.is_empty(), |field| field.child(caption(hint)))
    }
}

fn dropdown_button(
    id: &'static str,
    label: impl Into<SharedString>,
    cx: &mut Context<GuiApp>,
    action: impl Fn(&mut GuiApp, &mut Window, &mut Context<GuiApp>) + 'static,
) -> Stateful<Div> {
    dropdown_trigger(id, label).on_click(cx.listener(move |this, _, window, cx| {
        action(this, window, cx);
        this.settings_focus.focus(window, cx);
        cx.notify();
        cx.stop_propagation();
    }))
}
fn visual_switch(
    id: &'static str,
    icon: &'static str,
    label: &'static str,
    enabled: bool,
    cx: &mut Context<GuiApp>,
    toggle: impl Fn(&mut Config) + 'static,
) -> Stateful<Div> {
    toggle_button(id, icon, label, enabled, cx, move |this, _| {
        toggle(&mut this.settings.draft);
    })
}

fn settings_group(title: &'static str, content: impl IntoElement) -> Div {
    section_group(title, content)
}

fn spectrum_style_label(style: SpectrumStyle) -> &'static str {
    match style {
        SpectrumStyle::Bars => "Bars",
        SpectrumStyle::Outline => "Outline",
        SpectrumStyle::Led => "LED",
        SpectrumStyle::Line => "Line",
        SpectrumStyle::Solid => "Solid",
    }
}
fn radial_spectrum_style_label(style: RadialSpectrumStyle) -> &'static str {
    match style {
        RadialSpectrumStyle::Bars => "Bars",
        RadialSpectrumStyle::Rings => "Rings",
        RadialSpectrumStyle::BarsRings => "Bars + Rings",
        RadialSpectrumStyle::StereoDensity => "Stereo Density",
        RadialSpectrumStyle::SpatialSpectrum => "Spatial Spectrum",
        RadialSpectrumStyle::FractalBeams => "Fractal Beams",
        RadialSpectrumStyle::FractalFlow => "Fractal Flow",
    }
}

fn visualization_palette_label(palette: VisualizationPalette) -> &'static str {
    match palette {
        VisualizationPalette::TokyoNight => "Tokyo Night",
        VisualizationPalette::Deadbeef => "Deadbeef",
    }
}

fn ffmpeg_hint(status: &str) -> String {
    let mut text = String::with_capacity(status.len() + 24);
    text.push_str("FFmpeg audio decoding");
    if let Some(libraries) = status
        .strip_prefix("FFmpeg runtime available (")
        .and_then(|detail| detail.strip_suffix(')'))
    {
        for library in libraries
            .split(';')
            .next()
            .unwrap_or(libraries)
            .split(" / ")
        {
            text.push('\n');
            text.push_str(library);
        }
    } else {
        text.push('\n');
        text.push_str(status);
    }
    text
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsPage {
    General,
    Visualizations,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum VisualPage {
    Common,
    Spectrum,
    Spectrogram,
    AudioVisualizer,
    Waveform,
}

struct FontOption {
    family: String,
    label: String,
    search: String,
}

impl FontOption {
    fn new(family: String) -> Self {
        let label = if family.is_empty() {
            "System default".to_owned()
        } else {
            family.clone()
        };
        let search = label.to_lowercase();
        Self {
            family,
            label,
            search,
        }
    }
}

/// Unsaved preferences stay independent from live playback snapshots.
pub(super) struct Settings {
    draft: Config,
    inputs: HashMap<Field, Entity<Input>>,
    initialized: bool,
    dropdown: DropdownState<SettingChoice>,
    font_options: Vec<FontOption>,
    font_value: String,
    device_scroll: UniformListScrollHandle,
    dropdown_anchor: Measured,
    applying: bool,
    session: u64,
    feedback: Option<std::result::Result<(), String>>,
    page: SettingsPage,
    visual_page: VisualPage,
    visualizer_panel: Option<u64>,
}

impl Settings {
    pub(super) fn new(config: &Config) -> Self {
        Self {
            draft: config.clone(),
            inputs: HashMap::new(),
            initialized: false,
            dropdown: DropdownState::default(),
            font_options: Vec::new(),
            font_value: config.ui_font.clone(),
            device_scroll: UniformListScrollHandle::new(),
            dropdown_anchor: Measured::Device,
            applying: false,
            session: 0,
            feedback: None,
            page: SettingsPage::General,
            visual_page: VisualPage::Common,
            visualizer_panel: None,
        }
    }

    fn initialize(&mut self, cx: &mut App) {
        if self.initialized {
            return;
        }
        self.inputs = [
            Field::Roots,
            Field::ScanMaxDepth,
            Field::Volume,
            Field::PlayCountThreshold,
            Field::QueueLimit,
            Field::LogRetentionWeeks,
            Field::MediaReadBuffer,
            Field::Font,
            Field::Scale,
            Field::Fps,
            Field::Background,
            Field::VisualizationDb,
            Field::RadialSpectrumSensitivity,
            Field::RadialSpectrumRotationSpeed,
            Field::RadialSpectrumBarWidth,
            Field::RadialSpectrumBarGlowLayers,
            Field::RadialSpectrumRingOpacity,
            Field::RadialSpectrumBloomIntensity,
            Field::RadialSpectrumInnerDiameter,
            Field::RadialSpectrumPrimaryColor,
            Field::RadialSpectrumSecondaryColor,
            Field::RadialSpectrumDb,
            Field::SpectrumDb,
            Field::SpectrumBarWidth,
            Field::SpectrumBars,
            Field::SpectrumGap,
            Field::SpectrumHold,
            Field::SpectrumGravity,
            Field::SpectrumBarHold,
            Field::SpectrumBarGravity,
            Field::SpectrumSmoothing,
            Field::SpectrogramDb,
            Field::SpectrogramHistory,
            Field::SpectrogramSamplingPointsScale,
            Field::SpectrogramInterpolationPoints,
            Field::CursorColor,
            Field::Glow,
            Field::WaveformRmsGain,
            Field::WaveformPeakGain,
            Field::WaveformPeakGamma,
        ]
        .into_iter()
        .map(|field| {
            let input = cx.new(|cx| {
                if field == Field::Font {
                    Input::new("", "System default — search fonts", cx).with_trailing_space(px(28.))
                } else {
                    Input::new("", field.label(), cx)
                }
            });
            (field, input)
        })
        .collect();
        let generic = ["", "sans-serif", "serif", "monospace"];
        self.font_options = generic
            .iter()
            .map(|family| FontOption::new((*family).to_owned()))
            .collect();
        self.font_options.extend(
            cx.text_system()
                .all_font_names()
                .into_iter()
                .filter(|family| !generic.contains(&family.as_str()))
                .map(FontOption::new),
        );
        self.initialized = true;
        self.fill(cx);
    }

    fn font_items(&self, query: &str) -> Vec<DropdownItem<SettingChoice>> {
        let query = query.trim();
        let search = query.to_lowercase();
        let mut items: Vec<_> = self
            .font_options
            .iter()
            .enumerate()
            .filter(|(_, option)| option.search.contains(&search))
            .map(|(index, option)| {
                DropdownItem::new(SettingChoice::Font(index), option.label.clone())
            })
            .collect();
        if !query.is_empty()
            && !self
                .font_options
                .iter()
                .any(|option| option.family.eq_ignore_ascii_case(query))
        {
            items.push(DropdownItem::new(
                SettingChoice::CustomFont,
                format!("Use \"{query}\""),
            ));
        }
        items
    }

    fn field(&self, field: Field) -> Div {
        field.form(self.inputs[&field].clone())
    }

    fn pair(&self, first: Field, second: Field) -> Div {
        two_column(
            self.field(first).min_w(px(180.)),
            self.field(second).min_w(px(180.)),
        )
    }

    fn single(&self, field: Field) -> Div {
        row().child(self.field(field).w(relative(0.5)).min_w(px(180.)).pr_2())
    }

    fn db_range_hint(&self, cx: &App) -> Div {
        let range = self
            .number::<f32>(Field::VisualizationDb, cx)
            .ok()
            .filter(|range| range.is_finite() && (1.0..=160.0).contains(range))
            .unwrap_or(self.draft.visualization_db_range);
        caption(format!("1–160 dB; empty = Common ({range} dB)."))
    }

    fn field_state(&self, field: Field, enabled: bool, reason: &'static str, cx: &App) -> Div {
        if enabled {
            return self.field(field).min_w(px(180.));
        }
        // Keep the draft value, but unmount the editor so disabled fields cannot take focus or edits.
        let control = div()
            .id(("inactive-setting", field as usize))
            .h(px(36.))
            .p(px(7.))
            .line_height(px(20.))
            .text_size(px(14.))
            .bg(rgb(super::PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .rounded(px(5.))
            .cursor_default()
            .child(self.value(field, cx))
            .tooltip(move |_, cx| {
                cx.new(|_| ButtonTooltip {
                    text: reason.into(),
                })
                .into()
            });
        field.form(control).min_w(px(180.)).opacity(0.45)
    }

    fn value(&self, field: Field, cx: &App) -> String {
        self.inputs[&field].read(cx).text().to_owned()
    }

    fn number<T: FromStr>(&self, field: Field, cx: &App) -> Result<T>
    where
        T::Err: std::fmt::Display,
    {
        self.value(field, cx)
            .trim()
            .parse()
            .map_err(|error| anyhow::anyhow!("{}: {error}", field.label()))
    }

    fn optional_number<T: FromStr>(&self, field: Field, cx: &App) -> Result<Option<T>>
    where
        T::Err: std::fmt::Display,
    {
        let value = self.value(field, cx);
        if value.trim().is_empty() {
            Ok(None)
        } else {
            value
                .trim()
                .parse()
                .map(Some)
                .map_err(|error| anyhow::anyhow!("{}: {error}", field.label()))
        }
    }

    fn set_value(&mut self, field: Field, value: impl Into<SharedString>, cx: &mut App) {
        self.inputs[&field].update(cx, |input, cx| input.set_text(value.into(), cx));
    }

    fn reset(&mut self, config: &Config, cx: &mut App) {
        self.session = self.session.wrapping_add(1);
        self.visualizer_panel = None;
        self.draft = config.clone();
        self.applying = false;
        self.feedback = None;
        self.fill(cx);
    }

    fn fill(&mut self, cx: &mut App) {
        self.set_value(
            Field::Roots,
            self.draft
                .library_roots
                .iter()
                .map(|path| path.to_string_lossy())
                .collect::<Vec<_>>()
                .join(";"),
            cx,
        );
        self.set_value(
            Field::ScanMaxDepth,
            self.draft.scan_max_depth.to_string(),
            cx,
        );
        self.set_value(Field::Volume, (self.draft.volume * 100.).to_string(), cx);
        self.set_value(
            Field::PlayCountThreshold,
            self.draft.play_count_threshold_percent.to_string(),
            cx,
        );
        self.set_value(Field::QueueLimit, self.draft.queue_limit.to_string(), cx);
        self.set_value(
            Field::LogRetentionWeeks,
            self.draft.log_retention_weeks.to_string(),
            cx,
        );
        self.set_value(
            Field::MediaReadBuffer,
            self.draft.media_read_buffer_mb.to_string(),
            cx,
        );
        self.font_value = self.draft.ui_font.clone();
        self.set_value(Field::Font, self.draft.ui_font.clone(), cx);
        self.set_value(Field::Scale, self.draft.ui_scale.to_string(), cx);
        self.set_value(Field::Fps, self.draft.analysis_fps.to_string(), cx);
        self.set_value(
            Field::Background,
            self.draft.visual_background.to_string(),
            cx,
        );
        self.set_value(
            Field::VisualizationDb,
            self.draft.visualization_db_range.to_string(),
            cx,
        );
        self.set_value(
            Field::RadialSpectrumSensitivity,
            self.draft.radial_spectrum_sensitivity.to_string(),
            cx,
        );
        self.set_value(
            Field::RadialSpectrumRotationSpeed,
            self.draft.radial_spectrum_rotation_speed.to_string(),
            cx,
        );
        self.set_value(
            Field::RadialSpectrumBarWidth,
            self.draft.radial_spectrum_bar_width.to_string(),
            cx,
        );
        self.set_value(
            Field::RadialSpectrumBarGlowLayers,
            self.draft.radial_spectrum_bar_glow_layers.to_string(),
            cx,
        );
        self.set_value(
            Field::RadialSpectrumRingOpacity,
            self.draft.radial_spectrum_ring_opacity.to_string(),
            cx,
        );
        self.set_value(
            Field::RadialSpectrumBloomIntensity,
            self.draft.radial_spectrum_bloom_intensity.to_string(),
            cx,
        );
        self.set_value(
            Field::RadialSpectrumInnerDiameter,
            self.draft.radial_spectrum_inner_diameter.to_string(),
            cx,
        );
        self.set_value(
            Field::RadialSpectrumPrimaryColor,
            self.draft.radial_spectrum_primary_color.to_string(),
            cx,
        );
        self.set_value(
            Field::RadialSpectrumSecondaryColor,
            self.draft.radial_spectrum_secondary_color.to_string(),
            cx,
        );
        self.set_value(
            Field::RadialSpectrumDb,
            self.draft
                .radial_spectrum_db_range
                .map(|range| range.to_string())
                .unwrap_or_default(),
            cx,
        );
        self.set_value(
            Field::SpectrumDb,
            self.draft
                .spectrum_db_range
                .map(|range| range.to_string())
                .unwrap_or_default(),
            cx,
        );
        self.set_value(Field::SpectrumGap, self.draft.spectrum_gap.to_string(), cx);
        self.set_value(
            Field::SpectrumHold,
            self.draft.spectrum_peak_hold_ms.to_string(),
            cx,
        );
        self.set_value(
            Field::SpectrumGravity,
            self.draft.spectrum_peak_gravity.to_string(),
            cx,
        );
        self.set_value(
            Field::SpectrumBarHold,
            self.draft.spectrum_bar_hold_ms.to_string(),
            cx,
        );
        self.set_value(
            Field::SpectrumBarGravity,
            self.draft.spectrum_bar_gravity.to_string(),
            cx,
        );
        self.set_value(
            Field::SpectrumSmoothing,
            self.draft.spectrum_smoothing_ms.to_string(),
            cx,
        );
        self.set_value(
            Field::SpectrumBarWidth,
            self.draft.spectrum_bar_width.to_string(),
            cx,
        );
        self.set_value(
            Field::SpectrumBars,
            self.draft.spectrum_bars.to_string(),
            cx,
        );
        self.set_value(
            Field::SpectrogramDb,
            self.draft
                .spectrogram_db_range
                .map(|range| range.to_string())
                .unwrap_or_default(),
            cx,
        );
        self.set_value(
            Field::SpectrogramHistory,
            self.draft.spectrogram_history_seconds.to_string(),
            cx,
        );
        self.set_value(
            Field::SpectrogramSamplingPointsScale,
            self.draft.spectrogram_sampling_points_scale.to_string(),
            cx,
        );
        self.set_value(
            Field::SpectrogramInterpolationPoints,
            self.draft.spectrogram_interpolation_points.to_string(),
            cx,
        );
        self.set_value(
            Field::CursorColor,
            self.draft.waveform_cursor_color.to_string(),
            cx,
        );
        self.set_value(Field::Glow, self.draft.waveform_glow.to_string(), cx);
        self.set_value(
            Field::WaveformRmsGain,
            self.draft.waveform_rms_gain.to_string(),
            cx,
        );
        self.set_value(
            Field::WaveformPeakGain,
            self.draft.waveform_peak_gain.to_string(),
            cx,
        );
        self.set_value(
            Field::WaveformPeakGamma,
            self.draft.waveform_peak_gamma.to_string(),
            cx,
        );
    }

    fn parse(&self, cx: &App) -> Result<Config> {
        let mut config = self.draft.clone();
        config.library_roots = self
            .value(Field::Roots, cx)
            .split(';')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .collect();
        config.scan_max_depth = self.number(Field::ScanMaxDepth, cx)?;
        config.volume = self.number::<f32>(Field::Volume, cx)? / 100.;
        config.play_count_threshold_percent = self.number(Field::PlayCountThreshold, cx)?;
        config.queue_limit = self.number(Field::QueueLimit, cx)?;
        config.log_retention_weeks = self.number(Field::LogRetentionWeeks, cx)?;
        config.media_read_buffer_mb = self.number(Field::MediaReadBuffer, cx)?;
        config.ui_font = self.value(Field::Font, cx).trim().to_owned();
        config.ui_scale = self.number(Field::Scale, cx)?;
        config.analysis_fps = self.number(Field::Fps, cx)?;
        config.visualization_db_range = self.number(Field::VisualizationDb, cx)?;
        config.visual_background = self
            .value(Field::Background, cx)
            .trim()
            .parse::<RgbColor>()
            .context("Visualization background")?;
        config.radial_spectrum_sensitivity = self.number(Field::RadialSpectrumSensitivity, cx)?;
        config.radial_spectrum_rotation_speed =
            self.number(Field::RadialSpectrumRotationSpeed, cx)?;
        config.radial_spectrum_bar_width = self.number(Field::RadialSpectrumBarWidth, cx)?;
        config.radial_spectrum_bar_glow_layers =
            self.number(Field::RadialSpectrumBarGlowLayers, cx)?;
        config.radial_spectrum_ring_opacity = self.number(Field::RadialSpectrumRingOpacity, cx)?;
        config.radial_spectrum_bloom_intensity =
            self.number(Field::RadialSpectrumBloomIntensity, cx)?;
        config.radial_spectrum_inner_diameter =
            self.number(Field::RadialSpectrumInnerDiameter, cx)?;
        config.radial_spectrum_primary_color = self
            .value(Field::RadialSpectrumPrimaryColor, cx)
            .trim()
            .parse::<RgbColor>()
            .context("Audio Visualizer primary color")?;
        config.radial_spectrum_secondary_color = self
            .value(Field::RadialSpectrumSecondaryColor, cx)
            .trim()
            .parse::<RgbColor>()
            .context("Audio Visualizer secondary color")?;
        config.radial_spectrum_db_range = self.optional_number(Field::RadialSpectrumDb, cx)?;
        config.spectrum_db_range = self.optional_number(Field::SpectrumDb, cx)?;
        config.spectrum_gap = self.number(Field::SpectrumGap, cx)?;
        config.spectrum_peak_hold_ms = self.number(Field::SpectrumHold, cx)?;
        config.spectrum_peak_gravity = self.number(Field::SpectrumGravity, cx)?;
        config.spectrum_bar_hold_ms = self.number(Field::SpectrumBarHold, cx)?;
        config.spectrum_bar_gravity = self.number(Field::SpectrumBarGravity, cx)?;
        config.spectrum_smoothing_ms = self.number(Field::SpectrumSmoothing, cx)?;
        config.spectrum_bar_width = self.number(Field::SpectrumBarWidth, cx)?;
        config.spectrum_bars = self.number(Field::SpectrumBars, cx)?;
        config.spectrogram_db_range = self.optional_number(Field::SpectrogramDb, cx)?;
        config.spectrogram_history_seconds = self.number(Field::SpectrogramHistory, cx)?;
        config.spectrogram_sampling_points_scale =
            self.number(Field::SpectrogramSamplingPointsScale, cx)?;
        config.spectrogram_interpolation_points =
            self.number(Field::SpectrogramInterpolationPoints, cx)?;
        config.waveform_cursor_color = self
            .value(Field::CursorColor, cx)
            .trim()
            .parse::<RgbColor>()
            .context("Waveform waterline color")?;
        config.waveform_glow = self.number(Field::Glow, cx)?;
        config.waveform_rms_gain = self.number(Field::WaveformRmsGain, cx)?;
        config.waveform_peak_gain = self.number(Field::WaveformPeakGain, cx)?;
        config.waveform_peak_gamma = self.number(Field::WaveformPeakGamma, cx)?;
        config.validate()?;
        Ok(config)
    }

    fn tab_button(
        &self,
        id: &'static str,
        label: &'static str,
        active: bool,
        cx: &mut Context<GuiApp>,
        select: impl Fn(&mut Settings) + 'static,
    ) -> Stateful<Div> {
        active_control(
            button(id, label, cx, move |this, _, cx| {
                this.settings.dropdown.close();
                select(&mut this.settings);
                cx.notify();
            }),
            active,
        )
        .flex_shrink_0()
    }
}

impl GuiApp {
    fn initialize_settings(&mut self, cx: &mut Context<Self>) {
        if self.settings.initialized {
            return;
        }
        self.settings.initialize(cx);
        self._subscriptions.push(cx.subscribe(
            &self.settings.inputs[&Field::Font],
            |this, _, event, cx| {
                if matches!(event, InputEvent::Submit) {
                    if this.settings.dropdown.is_open()
                        && this.settings.dropdown_anchor == Measured::SettingsFont
                        && let Some(index) = this.settings.dropdown.selected_index()
                    {
                        this.choose_device(index, cx);
                    }
                    return;
                }
                let value = this.settings.value(Field::Font, cx);
                if value == this.settings.font_value {
                    return;
                }
                this.settings.font_value = value;
                if this.settings_open {
                    this.open_font_dropdown(true, cx);
                    cx.notify();
                }
            },
        ));
    }

    fn open_font_dropdown(&mut self, filtered: bool, cx: &App) {
        let value = self.settings.value(Field::Font, cx);
        let items = self.settings.font_items(if filtered { &value } else { "" });
        let selected = items
            .iter()
            .position(|item| match item.value {
                SettingChoice::Font(index) => self.settings.font_options[index]
                    .family
                    .eq_ignore_ascii_case(value.trim()),
                _ => false,
            })
            .unwrap_or(0);
        self.open_setting_dropdown(items, selected);
    }

    pub(super) fn settings_font_input_focused(&self, window: &Window, cx: &App) -> bool {
        self.settings.dropdown_anchor == Measured::SettingsFont
            && self.settings.inputs[&Field::Font]
                .read(cx)
                .is_focused(window)
    }

    fn font_control(&self, cx: &mut Context<Self>) -> Div {
        let input = self.settings.inputs[&Field::Font].clone();
        let control = div()
            .id("settings-font")
            .relative()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if !this.settings.dropdown.is_open()
                        || this.settings.dropdown_anchor != Measured::SettingsFont
                    {
                        this.open_font_dropdown(false, cx);
                        cx.notify();
                    }
                }),
            )
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let modifiers = event.keystroke.modifiers;
                if modifiers.control
                    || modifiers.platform
                    || modifiers.alt
                    || modifiers.shift
                    || modifiers.function
                {
                    return;
                }
                let key = event.keystroke.key.as_str();
                let open = this.settings.dropdown.is_open()
                    && this.settings.dropdown_anchor == Measured::SettingsFont;
                if !open {
                    if matches!(key, "up" | "down") {
                        this.open_font_dropdown(false, cx);
                        cx.stop_propagation();
                        cx.notify();
                    }
                } else if key == "escape" {
                    this.settings.dropdown.close();
                    cx.stop_propagation();
                    cx.notify();
                } else if matches!(key, "up" | "down" | "pageup" | "pagedown") {
                    this.settings_dropdown_key(key, window, cx);
                }
            }))
            .child(input)
            .child(
                div()
                    .id("settings-font-arrow")
                    .absolute()
                    .right(px(1.))
                    .top(px(1.))
                    .w(px(27.))
                    .h(px(34.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .child("▾")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            if this.settings.dropdown.is_open()
                                && this.settings.dropdown_anchor == Measured::SettingsFont
                            {
                                this.settings.dropdown.close();
                            } else {
                                this.open_font_dropdown(false, cx);
                            }
                            this.settings.inputs[&Field::Font]
                                .focus_handle(cx)
                                .focus(window, cx);
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    ),
            )
            .child(self.measurement(Measured::SettingsFont));
        Field::Font.form(control)
    }

    pub(super) fn load_settings(&mut self, cx: &mut Context<Self>) {
        let config = self.handle.config_snapshot();
        self.initialize_settings(cx);
        self.settings.dropdown.close();
        self.settings.reset(&config, cx);
        cx.notify();
    }

    pub(super) fn load_visualizer_settings(&mut self, panel_id: u64, cx: &mut Context<Self>) {
        let Some(options) = self.layout.audio_visualizer_options(panel_id) else {
            return;
        };
        self.load_settings(cx);
        self.settings.visualizer_panel = Some(panel_id);
        self.settings.page = SettingsPage::Visualizations;
        self.settings.visual_page = VisualPage::AudioVisualizer;
        self.settings.draft.radial_spectrum_style = options.style;
        self.settings.draft.radial_spectrum_grid = options.reference_lines;
        self.settings.draft.radial_spectrum_labels = options.labels;
        self.settings_open = true;
        cx.notify();
    }

    fn apply_settings(&mut self, cx: &mut Context<Self>) {
        if self.settings.applying {
            return;
        }
        if let Some(panel_id) = self.settings.visualizer_panel {
            let options = AudioVisualizerOptions::from(&self.settings.draft);
            self.settings.feedback = Some(
                if self.layout.set_audio_visualizer_options(panel_id, options) {
                    self.layout
                        .save(&self.layout_path)
                        .map_err(|error| format!("Saving layout: {error:#}"))
                } else {
                    Err("This visualizer instance no longer exists".into())
                },
            );
            self.sync_analysis(cx);
            cx.notify();
            return;
        }
        let config = match self.settings.parse(cx) {
            Ok(config) => config,
            Err(error) => {
                self.settings.feedback = Some(Err(format!("Invalid settings: {error:#}")));
                cx.notify();
                return;
            }
        };
        self.settings.applying = true;
        self.settings.feedback = None;
        let session = self.settings.session;
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            let response = cx
                .background_executor()
                .spawn(async move { handle.request_ack(Command::Configure { config }) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.settings.session != session {
                    return;
                }
                this.settings.applying = false;
                this.settings.feedback = Some(if response.ok {
                    Ok(())
                } else {
                    Err(response
                        .error
                        .unwrap_or_else(|| "Could not save settings".into()))
                });
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn select_font(&mut self, value: String, cx: &mut Context<Self>) {
        // set_text emits Changed too; remember explicit writes so selecting or
        // resetting a value does not reopen the autocomplete popup.
        self.settings.font_value = value.clone();
        self.settings.draft.ui_font = value.clone();
        self.settings.inputs[&Field::Font].update(cx, |input, cx| input.set_text(value, cx));
    }

    fn choose_device(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some((_, item)) = self.settings.dropdown.filtered_item(index) else {
            return;
        };
        match item.value {
            SettingChoice::Device(device_index) => {
                if device_index == 0 {
                    self.settings.draft.output_device = None;
                } else if let Some(name) = self.state.system.devices.get(device_index - 1) {
                    self.settings.draft.output_device = Some(name.clone());
                } else {
                    return;
                }
            }
            SettingChoice::Font(index) => {
                let value = self.settings.font_options[index].family.clone();
                self.select_font(value, cx);
            }
            SettingChoice::CustomFont => {
                let value = self.settings.value(Field::Font, cx).trim().to_owned();
                self.select_font(value, cx);
            }
            SettingChoice::SpectrumStyle(value) => self.settings.draft.spectrum_style = value,
            SettingChoice::RadialSpectrumStyle(value) => {
                self.settings.draft.radial_spectrum_style = value
            }
            SettingChoice::Palette(value) => self.settings.draft.visual_palette = value,
            SettingChoice::Fft(value) => self.settings.draft.spectrum_fft_size = value,
            SettingChoice::LogLevel(value) => self.settings.draft.log_level = value,
            SettingChoice::Window(value) => self.settings.draft.spectrum_window = value,
        }
        self.settings.dropdown.select_index(index);
        self.settings.dropdown.close();
        cx.notify();
    }

    fn open_setting_dropdown(&mut self, items: Vec<DropdownItem<SettingChoice>>, selected: usize) {
        if let Some(item) = items.first() {
            self.settings.dropdown_anchor = match item.value {
                SettingChoice::Font(_) | SettingChoice::CustomFont => Measured::SettingsFont,
                SettingChoice::SpectrumStyle(_) => Measured::SettingsStyle,
                SettingChoice::Fft(_) => Measured::SettingsFft,
                SettingChoice::LogLevel(_) => Measured::SettingsLogLevel,
                SettingChoice::Window(_) => Measured::SettingsWindow,
                SettingChoice::RadialSpectrumStyle(_) => Measured::SettingsStyle,
                SettingChoice::Palette(_) => Measured::SettingsStyle,
                SettingChoice::Device(_) => Measured::Device,
            };
        }
        self.settings.dropdown.open(items);
        self.settings.dropdown.select_index(selected);
        if let Some(index) = self.settings.dropdown.selected_index() {
            self.settings
                .device_scroll
                .scroll_to_item(index, ScrollStrategy::Nearest);
        }
    }

    pub(super) fn settings_dropdown_is_open(&self) -> bool {
        self.settings.dropdown.is_open()
    }

    pub(super) fn close_settings_dropdown(&mut self) {
        self.settings.dropdown.close();
    }

    pub(super) fn settings_dropdown_key(
        &mut self,
        key: &str,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        match key {
            "up" => self.settings.dropdown.move_previous(),
            "down" => self.settings.dropdown.move_next(),
            "pageup" | "pagedown" => {
                if let Some(index) = self.settings.dropdown.selected_index() {
                    let viewport_height = self
                        .settings
                        .device_scroll
                        .0
                        .borrow()
                        .base_handle
                        .bounds()
                        .size
                        .height;
                    let row_height = DROPDOWN_ROW_REMS * f32::from(window.rem_size());
                    let visible_rows =
                        ((f32::from(viewport_height) / row_height).floor() as usize).max(1);
                    let next = if key == "pagedown" {
                        index.saturating_add(visible_rows)
                    } else {
                        index.saturating_sub(visible_rows)
                    };
                    self.settings.dropdown.select_index(next);
                }
            }
            "enter" | "space" => {
                if let Some(index) = self.settings.dropdown.selected_index() {
                    self.choose_device(index, cx);
                }
                cx.stop_propagation();
                return;
            }
            _ => return,
        }
        if let Some(index) = self.settings.dropdown.selected_index() {
            self.settings
                .device_scroll
                .scroll_to_item(index, ScrollStrategy::Nearest);
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn general_settings(&mut self, cx: &mut Context<Self>) -> Div {
        let chosen_device: SharedString = self
            .settings
            .draft
            .output_device
            .clone()
            .unwrap_or_else(|| "System default".into())
            .into();
        let device = column()
            .flex_shrink_0()
            .gap_1()
            .child(caption("Output device"))
            .child(
                dropdown_button(
                    "output-device-selector",
                    chosen_device.clone(),
                    cx,
                    |this, _, _| {
                        let open = this.settings.dropdown.is_open();
                        if open {
                            this.settings.dropdown.close();
                        } else {
                            let selected = this
                                .settings
                                .draft
                                .output_device
                                .as_ref()
                                .and_then(|name| {
                                    this.state
                                        .system
                                        .devices
                                        .iter()
                                        .position(|device| device == name)
                                })
                                .map_or(0, |index| index + 1);
                            let items = std::iter::once(DropdownItem::new(
                                SettingChoice::Device(0),
                                "System default",
                            ))
                            .chain(
                                this.state.system.devices.iter().enumerate().map(
                                    |(index, device)| {
                                        DropdownItem::new(
                                            SettingChoice::Device(index + 1),
                                            device.clone(),
                                        )
                                    },
                                ),
                            );
                            this.settings.dropdown.open(items);
                            this.settings.dropdown.select_index(selected);
                        }
                        if let Some(index) = this.settings.dropdown.selected_index() {
                            this.settings
                                .device_scroll
                                .scroll_to_item(index, ScrollStrategy::Nearest);
                        }
                    },
                )
                .relative()
                .tooltip({
                    let chosen_device = chosen_device.clone();
                    move |_, cx| {
                        cx.new(|_| ButtonTooltip {
                            text: chosen_device.clone(),
                        })
                        .into()
                    }
                })
                .child(self.measurement(Measured::Device)),
            );
        let draft = &self.settings.draft;
        let (shuffle, repeat, mpris, tray, ffmpeg, remix) = (
            draft.shuffle,
            draft.repeat,
            draft.mpris_enabled,
            draft.tray_enabled,
            draft.ffmpeg_enabled,
            draft.pipewire_auto_mix,
        );
        let log_level_dropdown = dropdown_button(
            "settings-log-level",
            SettingChoice::LogLevel(draft.log_level).label(),
            cx,
            |this, _, _| {
                let values = [
                    LogLevel::Debug,
                    LogLevel::Info,
                    LogLevel::Warning,
                    LogLevel::Error,
                ];
                let selected = values
                    .iter()
                    .position(|value| *value == this.settings.draft.log_level)
                    .unwrap_or(0);
                this.open_setting_dropdown(
                    values
                        .into_iter()
                        .map(|value| {
                            DropdownItem::new(
                                SettingChoice::LogLevel(value),
                                SettingChoice::LogLevel(value).label(),
                            )
                        })
                        .collect(),
                    selected,
                );
            },
        )
        .relative()
        .child(self.measurement(Measured::SettingsLogLevel));
        let logging = settings_group(
            "Logging",
            row()
                .items_center()
                .flex_wrap()
                .gap_3()
                .child(
                    column()
                        .flex_1()
                        .min_w(px(180.))
                        .gap_1()
                        .child(caption("Log level"))
                        .child(log_level_dropdown),
                )
                .child(
                    self.settings
                        .field(Field::LogRetentionWeeks)
                        .flex_1()
                        .min_w(px(180.)),
                )
                .child(visual_switch(
                    "settings-log-file",
                    "▤",
                    "Write logs to disk",
                    draft.log_to_file,
                    cx,
                    |draft| draft.log_to_file = !draft.log_to_file,
                )),
        );
        let library = settings_group(
            "Library",
            row()
                .items_start()
                .flex_wrap()
                .gap_3()
                .child(self.settings.field(Field::Roots).flex_1().min_w(px(260.)))
                .child(
                    self.settings
                        .field(Field::ScanMaxDepth)
                        .flex_1()
                        .min_w(px(220.)),
                ),
        );
        let playback = settings_group(
            "Playback",
            column()
                .gap_2()
                .child(self.settings.pair(Field::Volume, Field::PlayCountThreshold))
                .child(self.settings.single(Field::QueueLimit))
                .child(
                    row()
                        .flex_wrap()
                        .gap_2()
                        .child(visual_switch(
                            "settings-shuffle",
                            "󰒟",
                            "Shuffle",
                            shuffle,
                            cx,
                            |draft| draft.shuffle = !draft.shuffle,
                        ))
                        .child(active_control(
                            icon_button(
                                "settings-repeat",
                                if repeat == RepeatMode::One {
                                    "󰑘"
                                } else {
                                    "󰑖"
                                },
                                match repeat {
                                    RepeatMode::Off => "Repeat: Off; click for All",
                                    RepeatMode::All => "Repeat: All; click for One",
                                    RepeatMode::One => "Repeat: One; click for Off",
                                },
                                cx,
                                |this, _, cx| {
                                    this.settings.draft.repeat = match this.settings.draft.repeat {
                                        RepeatMode::Off => RepeatMode::All,
                                        RepeatMode::All => RepeatMode::One,
                                        RepeatMode::One => RepeatMode::Off,
                                    };
                                    cx.notify();
                                },
                            ),
                            repeat != RepeatMode::Off,
                        )),
                ),
        );
        let audio = settings_group(
            "Audio",
            column()
                .gap_2()
                .child(
                    row()
                        .items_start()
                        .flex_wrap()
                        .gap_3()
                        .child(device.flex_1().min_w(px(260.)))
                        .child(
                            self.settings
                                .field(Field::MediaReadBuffer)
                                .flex_1()
                                .min_w(px(240.)),
                        ),
                )
                .child(
                    row()
                        .flex_wrap()
                        .gap_2()
                        .child(active_control(
                            icon_button(
                                "settings-ffmpeg",
                                "\u{f384}",
                                format!(
                                    "FFmpeg decoding: {}\n{}",
                                    if ffmpeg { "On" } else { "Off" },
                                    ffmpeg_hint(&self.state.system.ffmpeg_status)
                                ),
                                cx,
                                |this, _, cx| {
                                    this.settings.draft.ffmpeg_enabled =
                                        !this.settings.draft.ffmpeg_enabled;
                                    cx.notify();
                                },
                            )
                            .text_size(rems(1.)),
                            ffmpeg,
                        ))
                        .child(visual_switch(
                            "settings-pipewire-auto-mix",
                            "󱀞",
                            "PipeWire channel remix",
                            remix,
                            cx,
                            |draft| draft.pipewire_auto_mix = !draft.pipewire_auto_mix,
                        )),
                ),
        );
        let interface = settings_group(
            "Interface",
            column()
                .gap_2()
                .child(row().child(self.font_control(cx).flex_1().min_w(px(260.))))
                .child(self.settings.single(Field::Scale))
                .child(row().child(visual_switch(
                    "settings-nerd-symbols",
                    "Nf",
                    "Nerd Font symbols",
                    draft.nerd_symbols,
                    cx,
                    |draft| draft.nerd_symbols = !draft.nerd_symbols,
                )))
                .child(row().child(visual_switch(
                    "settings-library-tree-lines",
                    "├",
                    "Library tree lines",
                    draft.library_tree_lines,
                    cx,
                    |draft| draft.library_tree_lines = !draft.library_tree_lines,
                )))
                .child(caption("Tree lines off = disclosure arrows.")),
        );
        let desktop = settings_group(
            "Desktop integration",
            column()
                .gap_2()
                .child(
                    row()
                        .flex_wrap()
                        .gap_2()
                        .child(active_control(
                            icon_button(
                                "settings-mpris",
                                "󰐹",
                                format!(
                                    "Media controls (MPRIS): {}\n{}",
                                    if mpris { "On" } else { "Off" },
                                    self.state.system.mpris_status
                                ),
                                cx,
                                |this, _, cx| {
                                    this.settings.draft.mpris_enabled =
                                        !this.settings.draft.mpris_enabled;
                                    cx.notify();
                                },
                            )
                            .text_size(rems(1.)),
                            mpris,
                        ))
                        .child(visual_switch(
                            "settings-tray",
                            "▣",
                            "System tray",
                            tray,
                            cx,
                            |draft| draft.tray_enabled = !draft.tray_enabled,
                        )),
                )
                .child(caption("Requires restarting the graphical host.")),
        );

        column()
            .gap_3()
            .child(library)
            .child(playback)
            .child(audio)
            .child(interface)
            .child(desktop)
            .child(logging)
    }

    fn visual_settings_tabs(&self, cx: &mut Context<Self>) -> Div {
        let selected = self.settings.visual_page;
        row()
            .flex_wrap()
            .flex_shrink_0()
            .gap_1()
            .px(gpui::px(UI_INSET))
            .py_2()
            .child(self.settings.tab_button(
                "visual-common",
                "Common",
                selected == VisualPage::Common,
                cx,
                |settings| settings.visual_page = VisualPage::Common,
            ))
            .child(self.settings.tab_button(
                "visual-spectrogram",
                "Spectrogram",
                selected == VisualPage::Spectrogram,
                cx,
                |settings| settings.visual_page = VisualPage::Spectrogram,
            ))
            .child(self.settings.tab_button(
                "visual-spectrum",
                "Spectrum",
                selected == VisualPage::Spectrum,
                cx,
                |settings| settings.visual_page = VisualPage::Spectrum,
            ))
            .child(self.settings.tab_button(
                "visual-waveform",
                "Waveform",
                selected == VisualPage::Waveform,
                cx,
                |settings| settings.visual_page = VisualPage::Waveform,
            ))
            .child(self.settings.tab_button(
                "visual-audio-visualizer",
                "Audio Visualizer",
                selected == VisualPage::AudioVisualizer,
                cx,
                |settings| settings.visual_page = VisualPage::AudioVisualizer,
            ))
    }

    fn visual_settings(&mut self, cx: &mut Context<GuiApp>) -> Div {
        let settings = &self.settings;
        let draft = &settings.draft;
        match settings.visual_page {
            VisualPage::Common => {
                let palette = dropdown_button(
                    "visual-palette",
                    visualization_palette_label(draft.visual_palette),
                    cx,
                    |this, _, _| {
                        let values = [
                            VisualizationPalette::TokyoNight,
                            VisualizationPalette::Deadbeef,
                        ];
                        let selected = values
                            .iter()
                            .position(|value| *value == this.settings.draft.visual_palette)
                            .unwrap_or(0);
                        this.open_setting_dropdown(
                            values
                                .into_iter()
                                .map(|value| {
                                    DropdownItem::new(
                                        SettingChoice::Palette(value),
                                        visualization_palette_label(value),
                                    )
                                })
                                .collect(),
                            selected,
                        );
                    },
                )
                .relative()
                .child(self.measurement(Measured::SettingsStyle));
                let fft = dropdown_button(
                    "spectrum-fft",
                    draft.spectrum_fft_size.to_string(),
                    cx,
                    |this, _, _| {
                        let values = [512, 1024, 2048, 4096, 8192, 16384, 32768];
                        let selected = values
                            .iter()
                            .position(|value| *value == this.settings.draft.spectrum_fft_size)
                            .unwrap_or(0);
                        this.open_setting_dropdown(
                            values
                                .into_iter()
                                .map(|value| {
                                    DropdownItem::new(SettingChoice::Fft(value), value.to_string())
                                })
                                .collect(),
                            selected,
                        );
                    },
                )
                .relative()
                .child(self.measurement(Measured::SettingsFft));
                let window = dropdown_button(
                    "spectrum-window",
                    SettingChoice::Window(draft.spectrum_window).label(),
                    cx,
                    |this, _, _| {
                        let values = [
                            SpectrumWindow::Hann,
                            SpectrumWindow::BlackmanHarris,
                            SpectrumWindow::None,
                        ];
                        let selected = values
                            .iter()
                            .position(|value| *value == this.settings.draft.spectrum_window)
                            .unwrap_or(0);
                        this.open_setting_dropdown(
                            values
                                .into_iter()
                                .map(|value| {
                                    DropdownItem::new(
                                        SettingChoice::Window(value),
                                        SettingChoice::Window(value).label(),
                                    )
                                })
                                .collect(),
                            selected,
                        );
                    },
                )
                .relative()
                .child(self.measurement(Measured::SettingsWindow));
                column().gap_3()
                    .child(settings_group("Appearance", column().gap_2()
                        .child(two_column(settings.field(Field::Background).min_w(px(180.)), form_field("Color scheme", palette).min_w(px(180.))))
                        .child(settings.single(Field::VisualizationDb))
                        .child(caption("Background: all views. Color scheme: Spectrum, Spectrogram, and Waveform."))
                        .child(caption("Display range: Spectrum, Spectrogram, and Audio Visualizer. Fractal styles use it for energy response; playback volume is unchanged."))))
                    .child(settings_group("Frequency analysis", column().gap_2()
                        .child(two_column(form_field("FFT size (samples)", fft).min_w(px(180.)), form_field("FFT window", window).min_w(px(180.))))
                        .child(settings.single(Field::Fps))
                        .child(caption("Applies to Spectrum, Spectrogram, and Audio Visualizer. Changing FFT size or window clears visualization history."))))
                    .child(settings_group("Rendering", column().gap_2()
                        .child(row().child(visual_switch("settings-still-mode", "≈", "Still mode", draft.still_mode, cx, |draft| draft.still_mode = !draft.still_mode)))
                        .child(caption("Stops visualization processing and discards results; playback and interface effects are unchanged."))
                        .child(row().child(visual_switch("visual-cache", "▤", "Rendering cache", draft.visualization_cache, cx, |draft| draft.visualization_cache = !draft.visualization_cache)))
                        .child(caption("Reduces CPU use at the cost of memory in Spectrum, Spectrogram, and Waveform."))))
            }
            VisualPage::Spectrum => {
                let solid = draft.spectrum_style == SpectrumStyle::Solid;
                let style = dropdown_button(
                    "spectrum-style",
                    spectrum_style_label(draft.spectrum_style),
                    cx,
                    |this, _, _| {
                        let values = [
                            SpectrumStyle::Bars,
                            SpectrumStyle::Outline,
                            SpectrumStyle::Led,
                            SpectrumStyle::Line,
                            SpectrumStyle::Solid,
                        ];
                        let selected = values
                            .iter()
                            .position(|value| *value == this.settings.draft.spectrum_style)
                            .unwrap_or(0);
                        this.open_setting_dropdown(
                            values
                                .into_iter()
                                .map(|value| {
                                    DropdownItem::new(
                                        SettingChoice::SpectrumStyle(value),
                                        spectrum_style_label(value),
                                    )
                                })
                                .collect(),
                            selected,
                        );
                    },
                )
                .relative()
                .child(self.measurement(Measured::SettingsStyle));
                column().gap_3()
                    .child(settings_group("Appearance", column().gap_2()
                        .child(two_column(form_field("Style", style).min_w(px(180.)), settings.field_state(Field::SpectrumBars, !solid, "Unavailable with Solid; uses pixel density.", cx)))
                        .child(two_column(
                            settings.field_state(Field::SpectrumBarWidth, !solid, "Unavailable with Solid; uses pixel density.", cx),
                            settings.field_state(Field::SpectrumGap, !solid, "Unavailable with Solid; has no bar gaps.", cx),
                        ))
                        .child(settings.single(Field::SpectrumDb))
                        .child(settings.db_range_hint(cx))
                        .child(row().flex_wrap().gap_2()
                            .child(visual_switch("spectrum-interpolate", "≈", "Frequency interpolation", draft.spectrum_interpolate, cx, |draft| draft.spectrum_interpolate = !draft.spectrum_interpolate))
                            .child(visual_switch("spectrum-grid", "#", "Grid lines", draft.spectrum_grid, cx, |draft| draft.spectrum_grid = !draft.spectrum_grid))
                            .child(visual_switch("spectrum-labels", "T", "Axis labels", draft.spectrum_labels, cx, |draft| draft.spectrum_labels = !draft.spectrum_labels)))
                        .child(caption("Automatic bar count uses bar width and gap."))))
                    .child(settings_group("Bar motion", column().gap_2()
                        .child(settings.pair(Field::SpectrumBarHold, Field::SpectrumBarGravity))
                        .child(settings.single(Field::SpectrumSmoothing))
                        .child(caption("Gravity 0 snaps after hold, bypassing smoothing. Attacks stay immediate."))))
                    .child(settings_group("Peak markers", column().gap_2()
                        .child(row().child(visual_switch("spectrum-peaks", "∧", "Peak markers", draft.spectrum_peaks, cx, |draft| draft.spectrum_peaks = !draft.spectrum_peaks)))
                        .child(two_column(
                            settings.field_state(Field::SpectrumHold, draft.spectrum_peaks, "Enable peak markers to edit.", cx),
                            settings.field_state(Field::SpectrumGravity, draft.spectrum_peaks, "Enable peak markers to edit.", cx),
                        ))))
            }
            VisualPage::Spectrogram => column()
                .gap_3()
                .child(settings_group(
                    "Display",
                    column()
                        .gap_2()
                        .child(settings.pair(Field::SpectrogramDb, Field::SpectrogramHistory))
                        .child(settings.db_range_hint(cx))
                        .child(row().child(visual_switch(
                            "spectrogram-labels",
                            "T",
                            "Axis labels",
                            draft.spectrogram_labels,
                            cx,
                            |draft| draft.spectrogram_labels = !draft.spectrogram_labels,
                        ))),
                ))
                .child(settings_group(
                    "Frequency sampling",
                    column()
                        .gap_2()
                        .child(settings.pair(
                            Field::SpectrogramSamplingPointsScale,
                            Field::SpectrogramInterpolationPoints,
                        ))
                        .child(row().child(visual_switch(
                            "spectrogram-interpolate",
                            "≈",
                            "Frequency interpolation",
                            draft.spectrogram_interpolate,
                            cx,
                            |draft| draft.spectrogram_interpolate = !draft.spectrogram_interpolate,
                        )))
                        .child(caption("More frequency rows do not add FFT detail.")),
                )),
            VisualPage::AudioVisualizer => {
                let style = dropdown_button(
                    "audio-visualizer-style",
                    radial_spectrum_style_label(draft.radial_spectrum_style),
                    cx,
                    |this, _, _| {
                        let values = [
                            RadialSpectrumStyle::Bars,
                            RadialSpectrumStyle::Rings,
                            RadialSpectrumStyle::BarsRings,
                            RadialSpectrumStyle::StereoDensity,
                            RadialSpectrumStyle::SpatialSpectrum,
                            RadialSpectrumStyle::FractalBeams,
                            RadialSpectrumStyle::FractalFlow,
                        ];
                        let selected = values
                            .iter()
                            .position(|value| *value == this.settings.draft.radial_spectrum_style)
                            .unwrap_or(0);
                        this.open_setting_dropdown(
                            values
                                .into_iter()
                                .map(|value| {
                                    DropdownItem::new(
                                        SettingChoice::RadialSpectrumStyle(value),
                                        radial_spectrum_style_label(value),
                                    )
                                })
                                .collect(),
                            selected,
                        );
                    },
                )
                .relative()
                .child(self.measurement(Measured::SettingsStyle));
                let hint = match draft.radial_spectrum_style {
                    RadialSpectrumStyle::StereoDensity => {
                        "Fixed Mid/Side axes. Color shows occupancy, not dBFS."
                    }
                    RadialSpectrumStyle::SpatialSpectrum => {
                        "Frequency versus left/right energy balance, not source direction. Color shows dBFS."
                    }
                    RadialSpectrumStyle::FractalBeams => {
                        "Bands deform branches; energy rises send outward pulses."
                    }
                    RadialSpectrumStyle::FractalFlow => {
                        "Bands steer light filaments; energy rises brighten their trails."
                    }
                    _ => "",
                };
                let instance = settings.visualizer_panel.is_some();
                let display = settings_group(
                    if instance { "This instance" } else { "New instance defaults" },
                    column()
                        .gap_2()
                        .child(form_field("Style", style))
                        .when(!hint.is_empty(), |group| group.child(caption(hint)))
                        .child(row().flex_wrap().gap_2()
                            .child(visual_switch(
                                "audio-visualizer-grid", "#", "Reference lines",
                                draft.radial_spectrum_grid, cx,
                                |draft| draft.radial_spectrum_grid = !draft.radial_spectrum_grid,
                            ))
                            .child(visual_switch(
                                "audio-visualizer-labels", "T", "Labels",
                                draft.radial_spectrum_labels, cx,
                                |draft| draft.radial_spectrum_labels = !draft.radial_spectrum_labels,
                            )))
                        .child(caption(if instance {
                            "Only this visualizer changes. These controls are also in its right-click menu."
                        } else {
                            "Existing instances keep their choices. Right-click a visualizer to edit it."
                        }))
                        .child(caption("Bars and rings guides show normalized levels, not dB or frequency.")),
                );
                if instance {
                    return column().child(display);
                }
                let content = column().gap_3()
                    .child(display)
                    .child(caption("Effect parameters below are shared by all instances, regardless of the default style."))
                    .child(settings_group("Response & glow", column().gap_2()
                        .child(settings.single(Field::RadialSpectrumDb))
                        .child(settings.db_range_hint(cx))
                        .child(settings.pair(Field::RadialSpectrumSensitivity, Field::RadialSpectrumBloomIntensity))
                        .child(caption("Fractal styles: silence fades the light; pause freezes it. Pulses follow energy rises, not detected beats."))))
                    .child(settings_group("Shape & motion", column().gap_2()
                        .child(settings.pair(Field::RadialSpectrumInnerDiameter, Field::RadialSpectrumRotationSpeed))
                        .child(row().child(visual_switch("audio-visualizer-fade-idle", "◌", "Fade when idle", draft.radial_spectrum_fade_when_idle, cx, |draft| draft.radial_spectrum_fade_when_idle = !draft.radial_spectrum_fade_when_idle)))
                        .child(caption("Controls the Bars, Rings and Bars + Rings styles."))))
                    .child(settings_group("Bars & rings", column().gap_2()
                        .child(settings.pair(Field::RadialSpectrumBarWidth, Field::RadialSpectrumBarGlowLayers))
                        .child(settings.single(Field::RadialSpectrumRingOpacity))
                        .child(caption("Bloom affects bars and rings; bar glow requires bloom above zero."))));
                content.child(settings_group(
                    "Colors",
                    column()
                        .gap_2()
                        .child(settings.pair(
                            Field::RadialSpectrumPrimaryColor,
                            Field::RadialSpectrumSecondaryColor,
                        ))
                        .child(caption("Independent of the shared color scheme.")),
                ))
            }
            VisualPage::Waveform => column()
                .gap_3()
                .child(settings_group(
                    "Appearance & interaction",
                    column()
                        .gap_2()
                        .child(settings.pair(Field::CursorColor, Field::Glow))
                        .child(
                            row()
                                .flex_wrap()
                                .gap_2()
                                .child(visual_switch(
                                    "waveform-labels",
                                    "T",
                                    "Time labels",
                                    draft.waveform_labels,
                                    cx,
                                    |draft| draft.waveform_labels = !draft.waveform_labels,
                                ))
                                .child(visual_switch(
                                    "waveform-seek",
                                    "󱀞",
                                    "Click and drag to seek",
                                    draft.waveform_seek_enabled,
                                    cx,
                                    |draft| {
                                        draft.waveform_seek_enabled = !draft.waveform_seek_enabled
                                    },
                                )),
                        )
                        .child(caption("The waterline stays still while paused.")),
                ))
                .child(settings_group(
                    "Amplitude display",
                    column()
                        .gap_2()
                        .child(settings.pair(Field::WaveformRmsGain, Field::WaveformPeakGain))
                        .child(settings.single(Field::WaveformPeakGamma))
                        .child(caption("Display only; does not affect playback volume.")),
                )),
        }
    }

    pub(super) fn settings_panel(
        &mut self,
        panel_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.initialize_settings(cx);
        let category = row()
            .flex_wrap()
            .flex_shrink_0()
            .gap_1()
            .px(gpui::px(UI_INSET))
            .py(gpui::px(UI_INSET))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(self.settings.tab_button(
                "settings-general",
                "General",
                self.settings.page == SettingsPage::General,
                cx,
                |settings| settings.page = SettingsPage::General,
            ))
            .child(self.settings.tab_button(
                "settings-visualizations",
                "Visualizations",
                self.settings.page == SettingsPage::Visualizations,
                cx,
                |settings| settings.page = SettingsPage::Visualizations,
            ));
        let mut panel = column()
            .id(("settings-panel", panel_id))
            .relative()
            .w_full()
            .flex_1()
            .min_h_0()
            .gap_0()
            .when(self.settings.visualizer_panel.is_none(), |panel| {
                panel.child(category)
            });
        let page_id = match (self.settings.page, self.settings.visual_page) {
            (SettingsPage::General, _) => "settings-general-scroll",
            (_, VisualPage::Common) => "settings-common-scroll",
            (_, VisualPage::Spectrum) => "settings-spectrum-scroll",
            (_, VisualPage::Spectrogram) => "settings-spectrogram-scroll",
            (_, VisualPage::Waveform) => "settings-waveform-scroll",
            (_, VisualPage::AudioVisualizer) => "settings-audio-visualizer-scroll",
        };
        let content = match self.settings.page {
            SettingsPage::General => self.general_settings(cx),
            SettingsPage::Visualizations => {
                if self.settings.visualizer_panel.is_none() {
                    panel = panel.child(self.visual_settings_tabs(cx));
                }
                self.visual_settings(cx)
            }
        };
        let scroll = self.scroll_views.content((page_id, panel_id));
        panel = panel.child(
            div()
                .id(page_id)
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .track_scroll(&scroll)
                .child(content.flex_shrink_0().p(gpui::px(UI_INSET))),
        );
        let feedback = if self.settings.applying {
            status_message("Applying settings…", StatusKind::Info).into_any_element()
        } else {
            match &self.settings.feedback {
                Some(Err(error)) => copyable_message("copy-settings-error", error.clone(), cx)
                    .text_color(rgb(ERROR))
                    .into_any_element(),
                Some(Ok(())) => {
                    status_message("Last apply succeeded", StatusKind::Success).into_any_element()
                }
                None => div().into_any_element(),
            }
        };
        panel = panel.child(
            row()
                .gap_3()
                .flex_shrink_0()
                .px(gpui::px(UI_INSET))
                .py(gpui::px(UI_INSET))
                .min_h(rems(3.))
                .border_t_1()
                .border_color(rgb(BORDER))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_sm()
                        .child(feedback),
                )
                .child(
                    icon_button(
                        "settings-apply",
                        "✓",
                        "Apply settings",
                        cx,
                        |this, _, cx| this.apply_settings(cx),
                    )
                    .opacity(if self.settings.applying { 0.4 } else { 1.0 }),
                )
                .child(icon_button(
                    "settings-discard",
                    "×",
                    "Discard changes and close",
                    cx,
                    |this, _, cx| {
                        this.load_settings(cx);
                        this.settings_open = false;
                        cx.notify();
                    },
                )),
        );
        if self.settings.dropdown.is_open() {
            let bounds = self
                .measured
                .borrow()
                .get(&self.settings.dropdown_anchor)
                .copied();
            if let Some(bounds) = bounds {
                let selected = self.settings.dropdown.selected_index().unwrap_or(0);
                let item_count = self.settings.dropdown.filtered().count();
                let row_height = DROPDOWN_ROW_REMS * f32::from(window.rem_size());
                let height = (item_count as f32 * row_height)
                    .min(POPOVER_MAX_HEIGHT * self.state.system.config.ui_scale)
                    .min(f32::from(window.viewport_size().height) * 0.45);
                let choices = uniform_list(
                    "settings-dropdown",
                    item_count,
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .filter_map(|index| {
                                let (item_index, item) =
                                    this.settings.dropdown.filtered_item(index)?;
                                let label: SharedString = item.label.clone().into();
                                let hint = label.clone();
                                Some(
                                    dropdown_row(
                                        ("settings-choice", item_index),
                                        item_index == selected,
                                        label,
                                    )
                                    .tooltip(move |_, cx| {
                                        cx.new(|_| ButtonTooltip { text: hint.clone() }).into()
                                    })
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| this.choose_device(item_index, cx),
                                    )),
                                )
                            })
                            .collect()
                    }),
                )
                .track_scroll(&self.settings.device_scroll)
                .h(px(height))
                .w_full();
                panel = panel.child(
                    deferred(
                        anchored()
                            .position(bounds.bottom_left())
                            .snap_to_window()
                            .child(
                                dropdown_container("settings-dropdown", bounds.size.width)
                                    .on_mouse_down_out(cx.listener(
                                        |this, event: &MouseDownEvent, _, cx| {
                                            if this.settings.dropdown_anchor
                                                == Measured::SettingsFont
                                                && this
                                                    .measured
                                                    .borrow()
                                                    .get(&Measured::SettingsFont)
                                                    .is_some_and(|bounds| {
                                                        bounds.contains(&event.position)
                                                    })
                                            {
                                                return;
                                            }
                                            this.settings.dropdown.close();
                                            cx.stop_propagation();
                                            cx.notify();
                                        },
                                    ))
                                    .child(choices),
                            ),
                    )
                    .with_priority(2),
                );
            }
        }
        panel.into_any_element()
    }
}
