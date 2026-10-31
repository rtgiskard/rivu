#[cfg(test)]
use crate::config::ConfigPersistence;
mod library_stats;
mod model;
mod pages;

use model::{Completion, PanelApply, SettingsModel, Submission, Target};

use super::{album_artwork::album_artwork_style_label, visualization::visualization_style_label};
use crate::{
    artwork::{ArtworkCacheReport, ArtworkManager},
    config::{
        AlbumArtworkDirection, AlbumArtworkMode, AlbumArtworkOptions, AlbumArtworkStyle,
        AnalysisWindow, AudioVisualizerStyle, Config, ConfigPatch, LogLevel, SpectrumBarLayout,
        SpectrumStyle, VisualizationPalette, VisualizationSignal,
    },
    core::CoreHandle,
    i18n::Localizer,
    theme::Theme,
};
use anyhow::Result;
use gpui::{prelude::*, *};
use rivu_ui::{
    ButtonTooltip, ChoiceAction, ChoiceItem, ChoiceMode, ChoicePopup, ChoiceState, Dialog,
    FocusScope, FormField, IconButton, IconButtonDensity, IconButtonGroup, IconButtonStyle,
    StatusKind, activate_control, active_control, column, copyable_message, dropdown_trigger,
    input::{Input, InputEvent},
    input_aligned_control, row, section_group, status_message,
    theme::{Palette, palette},
    two_column,
};
use std::{cell::RefCell, collections::HashMap, rc::Rc, str::FromStr, sync::Arc, time::Duration};

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingChoice {
    Language(usize),
    Theme(Theme),
    Device(usize),
    Font(usize),
    CustomFont,
    SpectrumStyle(SpectrumStyle),
    SpectrumLayout(SpectrumLayoutMode),
    Fft(u32),
    DefaultVisualizerStyle(AudioVisualizerStyle),
    Palette(VisualizationPalette),
    LogLevel(LogLevel),
    Window(AnalysisWindow),
    Signal(VisualizationSignal),
    AlbumArtworkStyle(AlbumArtworkStyle),
    AlbumArtworkMode(AlbumArtworkMode),
    AlbumArtworkDirection(AlbumArtworkDirection),
}

impl SettingChoice {
    fn label(self, i18n: &Localizer) -> String {
        match self {
            Self::Language(_) | Self::Device(_) | Self::Font(_) | Self::CustomFont => String::new(),
            Self::Theme(value) => value.label().to_owned(),
            Self::AlbumArtworkStyle(value) => album_artwork_style_label(i18n, value).to_owned(),
            Self::AlbumArtworkMode(AlbumArtworkMode::Static) => {
                i18n.text("settings-static").to_owned()
            }
            Self::AlbumArtworkMode(AlbumArtworkMode::Rotating) => {
                i18n.text("settings-rotating").to_owned()
            }
            Self::AlbumArtworkDirection(AlbumArtworkDirection::Clockwise) => {
                i18n.text("settings-clockwise").to_owned()
            }
            Self::AlbumArtworkDirection(AlbumArtworkDirection::Counterclockwise) => {
                i18n.text("settings-counterclockwise").to_owned()
            }
            Self::SpectrumStyle(value) => spectrum_style_label(i18n, value).to_owned(),
            Self::SpectrumLayout(value) => i18n.text(value.label_id()).to_owned(),
            Self::DefaultVisualizerStyle(value) => {
                visualization_style_label(i18n, value).to_owned()
            }
            Self::Palette(value) => visualization_palette_label(i18n, value).to_owned(),
            Self::Fft(value) => value.to_string(),
            Self::LogLevel(value) => match value {
                LogLevel::Debug => i18n.text("settings-debug").to_owned(),
                LogLevel::Info => i18n.text("settings-info").to_owned(),
                LogLevel::Warning => i18n.text("settings-warning").to_owned(),
                LogLevel::Error => i18n.text("settings-error").to_owned(),
            },
            Self::Window(AnalysisWindow::Hann) => "Hann".to_owned(),
            Self::Window(AnalysisWindow::BlackmanHarris) => "Blackman–Harris".to_owned(),
            Self::Window(AnalysisWindow::None) => i18n.text("settings-none").to_owned(),
            Self::Signal(VisualizationSignal::Before) => {
                i18n.text("settings-signal-before").to_owned()
            }
            Self::Signal(VisualizationSignal::After) => {
                i18n.text("settings-signal-after").to_owned()
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SpectrumLayoutMode {
    Automatic,
    Fixed,
}

impl SpectrumLayoutMode {
    fn from_layout(layout: SpectrumBarLayout) -> Self {
        match layout {
            SpectrumBarLayout::Automatic { .. } => Self::Automatic,
            SpectrumBarLayout::Fixed { .. } => Self::Fixed,
        }
    }

    fn label_id(self) -> &'static str {
        match self {
            Self::Automatic => "settings-spectrum-layout-automatic",
            Self::Fixed => "settings-spectrum-layout-fixed",
        }
    }

    fn initial_layout(self) -> SpectrumBarLayout {
        match self {
            Self::Automatic => SpectrumBarLayout::Automatic { width: 3.0 },
            Self::Fixed => SpectrumBarLayout::Fixed { count: 64 },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Field {
    ScanMaxDepth,
    ScanMinDurationSeconds,
    Volume,
    PlayCountThreshold,
    LogRetentionWeeks,
    MediaReadBufferMib,
    ArtworkFileLimitMib,
    ArtworkDecodeBudgetMib,
    GpuMaterialBudgetMib,
    Font,
    FontSizePt,
    Scale,
    LayoutSnapGrid,
    AnalysisRateHz,
    VisualizationAnimationFps,
    VisualizationBackground,
    VisualizationDbRange,
    ReactiveSensitivity,
    RadialRotationSpeed,
    RadialBarWidth,
    RadialRingOpacity,
    ReactiveBloomIntensity,
    RadialInnerDiameter,
    EffectPrimaryColor,
    EffectSecondaryColor,
    ReactiveDbRange,
    SpectrumDbRange,
    SpectrumAutoWidth,
    SpectrumFixedCount,
    SpectrumGap,
    SpectrumHold,
    SpectrumGravity,
    SpectrumBarHold,
    SpectrumBarGravity,
    SpectrumSmoothing,
    SpectrogramDbRange,
    SpectrogramHistory,
    WaveformCursorColor,
    WaveformGlow,
    WaveformRmsGain,
    WaveformPeakGain,
    WaveformPeakGamma,
    OscilloscopeTimeSpanMs,
    OscilloscopeGain,
    AlbumArtworkPeriodSeconds,
    AlbumArtworkFps,
}
#[derive(Debug)]
struct FieldIssue {
    field: Field,
    message: String,
}

impl std::fmt::Display for FieldIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for FieldIssue {}

impl Field {
    fn config_key(self) -> &'static str {
        match self {
            Self::ScanMaxDepth => "library.scan_max_depth",
            Self::ScanMinDurationSeconds => "library.scan_min_duration_seconds",
            Self::Volume => "playback.volume_percent",
            Self::PlayCountThreshold => "playback.play_count_threshold_percent",
            Self::LogRetentionWeeks => "logging.retention_weeks",
            Self::MediaReadBufferMib => "playback.media_read_buffer_mib",
            Self::ArtworkFileLimitMib => "interface.artwork_file_limit_mib",
            Self::ArtworkDecodeBudgetMib => "interface.artwork_decode_budget_mib",
            Self::GpuMaterialBudgetMib => "interface.gpu_material_budget_mib",
            Self::Font => "interface.font",
            Self::FontSizePt => "interface.font_size_pt",
            Self::Scale => "interface.scale",
            Self::LayoutSnapGrid => "interface.layout_snap_grid_px",
            Self::AnalysisRateHz => "visualizations.general.analysis_rate_hz",
            Self::VisualizationAnimationFps => "visualizations.general.animation_fps",
            Self::VisualizationBackground => "visualizations.general.background",
            Self::VisualizationDbRange => "visualizations.general.db_range",
            Self::ReactiveSensitivity => "visualizations.reactive.sensitivity",
            Self::ReactiveBloomIntensity => "visualizations.reactive.bloom_intensity",
            Self::ReactiveDbRange => "visualizations.reactive.db_range",
            Self::EffectPrimaryColor => "visualizations.effects.primary_color",
            Self::EffectSecondaryColor => "visualizations.effects.secondary_color",
            Self::RadialRotationSpeed => "visualizations.radial.rotation_speed",
            Self::RadialBarWidth => "visualizations.radial.bar_width",
            Self::RadialRingOpacity => "visualizations.radial.ring_opacity",
            Self::RadialInnerDiameter => "visualizations.radial.inner_diameter",
            Self::SpectrumDbRange => "visualizations.spectrum.db_range",
            Self::SpectrumAutoWidth => "visualizations.spectrum.bar_layout.width",
            Self::SpectrumFixedCount => "visualizations.spectrum.bar_layout.count",
            Self::SpectrumGap => "visualizations.spectrum.gap",
            Self::SpectrumHold => "visualizations.spectrum.peak_hold_ms",
            Self::SpectrumGravity => "visualizations.spectrum.peak_gravity",
            Self::SpectrumBarHold => "visualizations.spectrum.bar_hold_ms",
            Self::SpectrumBarGravity => "visualizations.spectrum.bar_gravity",
            Self::SpectrumSmoothing => "visualizations.spectrum.smoothing_ms",
            Self::SpectrogramDbRange => "visualizations.spectrogram.db_range",
            Self::SpectrogramHistory => "visualizations.spectrogram.history_seconds",
            Self::WaveformCursorColor => "visualizations.waveform.cursor_color",
            Self::WaveformGlow => "visualizations.waveform.glow",
            Self::WaveformRmsGain => "visualizations.waveform.rms_gain",
            Self::WaveformPeakGain => "visualizations.waveform.peak_gain",
            Self::WaveformPeakGamma => "visualizations.waveform.peak_gamma",
            Self::OscilloscopeTimeSpanMs => "visualizations.oscilloscope.time_span_ms",
            Self::OscilloscopeGain => "visualizations.oscilloscope.gain",
            Self::AlbumArtworkPeriodSeconds => "interface.album_artwork.period_seconds",
            Self::AlbumArtworkFps => "interface.album_artwork.fps",
        }
    }
    fn precision(self) -> Option<usize> {
        match self {
            Self::Volume => Some(0),
            Self::PlayCountThreshold
            | Self::VisualizationDbRange
            | Self::ReactiveDbRange
            | Self::SpectrumDbRange
            | Self::SpectrogramDbRange
            | Self::SpectrumGravity
            | Self::SpectrumBarGravity => Some(1),
            Self::Scale
            | Self::ReactiveSensitivity
            | Self::RadialRotationSpeed
            | Self::RadialBarWidth
            | Self::RadialRingOpacity
            | Self::ReactiveBloomIntensity
            | Self::RadialInnerDiameter
            | Self::SpectrumAutoWidth
            | Self::SpectrumGap
            | Self::WaveformGlow
            | Self::WaveformRmsGain
            | Self::WaveformPeakGain
            | Self::WaveformPeakGamma
            | Self::OscilloscopeTimeSpanMs
            | Self::OscilloscopeGain
            | Self::AlbumArtworkPeriodSeconds => Some(2),
            _ => None,
        }
    }

    fn enabled(self, config: &Config) -> bool {
        match self {
            Self::AlbumArtworkPeriodSeconds => {
                config.album_artwork_mode == AlbumArtworkMode::Rotating
            }
            Self::SpectrumAutoWidth | Self::SpectrumFixedCount | Self::SpectrumGap => {
                config.spectrum_style != SpectrumStyle::Solid
            }
            Self::SpectrumHold | Self::SpectrumGravity => config.spectrum_peaks,
            _ => true,
        }
    }

    fn label_id(self) -> &'static str {
        match self {
            Self::ScanMaxDepth => "settings-maximum-scan-depth",
            Self::ScanMinDurationSeconds => "settings-minimum-scan-duration-secs",
            Self::Volume => "settings-volume",
            Self::PlayCountThreshold => "settings-play-count-threshold",
            Self::LogRetentionWeeks => "settings-log-retention-weeks",
            Self::MediaReadBufferMib => "settings-media-read-buffer-mib",
            Self::ArtworkFileLimitMib => "settings-artwork-file-limit-mib",
            Self::ArtworkDecodeBudgetMib => "settings-artwork-decode-budget-mib",
            Self::GpuMaterialBudgetMib => "settings-gpu-material-budget-mib",
            Self::Font => "settings-font-family",
            Self::FontSizePt => "settings-base-font-size-pt",
            Self::Scale => "settings-interface-scale",
            Self::LayoutSnapGrid => "settings-layout-grid-spacing-px",
            Self::AnalysisRateHz => "settings-analysis-rate-hz",
            Self::VisualizationAnimationFps => "settings-animation-rate-fps",
            Self::VisualizationBackground => "settings-background-color",
            Self::VisualizationDbRange => "settings-default-display-range-db",
            Self::ReactiveSensitivity => "settings-sensitivity",
            Self::RadialRotationSpeed => "settings-rotation-speed",
            Self::RadialBarWidth => "settings-bar-width",
            Self::RadialRingOpacity => "settings-ring-opacity",
            Self::ReactiveBloomIntensity => "settings-bloom-intensity",
            Self::RadialInnerDiameter => "settings-inner-diameter",
            Self::EffectPrimaryColor => "settings-primary-color",
            Self::EffectSecondaryColor => "settings-secondary-color",
            Self::SpectrumDbRange | Self::ReactiveDbRange => "settings-display-range-db",
            Self::SpectrogramDbRange => "settings-color-range-db",
            Self::SpectrumAutoWidth => "settings-automatic-bar-width-px",
            Self::SpectrumFixedCount => "settings-bar-count",
            Self::SpectrumGap => "settings-bar-gap-px",
            Self::SpectrumHold => "settings-peak-hold-ms",
            Self::SpectrumGravity => "settings-peak-gravity-db-s",
            Self::SpectrumBarHold => "settings-bar-hold-ms",
            Self::SpectrumBarGravity => "settings-bar-gravity-db-s",
            Self::SpectrumSmoothing => "settings-release-smoothing-ms",
            Self::SpectrogramHistory => "settings-history-limit-s",
            Self::WaveformCursorColor => "settings-waterline-color",
            Self::WaveformGlow => "settings-glow-strength",
            Self::WaveformRmsGain => "settings-rms-gain",
            Self::WaveformPeakGain => "settings-peak-gain",
            Self::WaveformPeakGamma => "settings-peak-gamma",
            Self::OscilloscopeTimeSpanMs => "settings-oscilloscope-time-span-ms",
            Self::OscilloscopeGain => "settings-oscilloscope-gain-x",
            Self::AlbumArtworkPeriodSeconds => "settings-seconds-per-circle",
            Self::AlbumArtworkFps => "settings-animation-rate-fps",
        }
    }

    fn label(self, i18n: &Localizer) -> &str {
        i18n.text(self.label_id())
    }

    fn hint(self, settings: &Settings) -> SharedString {
        match self {
            Self::AlbumArtworkPeriodSeconds if !self.enabled(&settings.model.draft) => {
                settings.tr(if matches!(settings.model.target(), Target::Artwork(_)) {
                    "settings-artwork-panel-recovery"
                } else {
                    "settings-artwork-recovery"
                })
            }
            Self::SpectrumAutoWidth | Self::SpectrumFixedCount | Self::SpectrumGap
                if !self.enabled(&settings.model.draft) =>
            {
                settings.tr("settings-spectrum-style-recovery")
            }
            Self::SpectrumHold | Self::SpectrumGravity if !self.enabled(&settings.model.draft) => {
                settings.tr("settings-spectrum-peaks-recovery")
            }
            Self::AlbumArtworkPeriodSeconds => settings.tr("settings-artwork-period-hint"),
            Self::AlbumArtworkFps => format!(
                "{} {}",
                settings.tr("settings-animation-fps-hint"),
                settings.tr("settings-artwork-motion-help")
            )
            .into(),
            Self::ScanMaxDepth => settings.tr("settings-scan-depth-hint"),
            Self::ScanMinDurationSeconds => settings.tr("settings-scan-duration-hint"),
            Self::Volume => "0–100.".into(),
            Self::PlayCountThreshold => settings.tr("settings-play-count-threshold-hint"),
            Self::LogRetentionWeeks => "1–520.".into(),
            Self::MediaReadBufferMib => settings.tr("settings-media-buffer-hint"),
            Self::ArtworkFileLimitMib => settings.tr("settings-artwork-file-limit-hint"),
            Self::ArtworkDecodeBudgetMib => settings.tr("settings-artwork-decode-budget-hint"),
            Self::GpuMaterialBudgetMib => settings.tr("settings-gpu-material-budget-hint"),
            Self::Font => settings.tr("settings-font-hint"),
            Self::FontSizePt => "8–18 pt".into(),
            Self::Scale => "0.75–2.".into(),
            Self::LayoutSnapGrid => settings.tr("settings-layout-grid-hint"),
            Self::AnalysisRateHz => settings.tr("settings-analysis-rate-hint"),
            Self::VisualizationAnimationFps => settings.tr("settings-visualization-fps-hint"),
            Self::VisualizationBackground => settings.tr("settings-background-hint"),
            Self::EffectPrimaryColor | Self::EffectSecondaryColor => {
                settings.tr("settings-colors-hint")
            }
            Self::WaveformCursorColor => settings.tr("settings-waterline-hint"),
            Self::VisualizationDbRange => settings.tr("settings-visualizer-db-hint"),
            // These controls attach a live inherited-range tooltip in Settings::field.
            Self::SpectrumDbRange | Self::ReactiveDbRange | Self::SpectrogramDbRange => "".into(),
            Self::ReactiveSensitivity => "0–5.".into(),
            Self::RadialRotationSpeed => "0–10.".into(),
            Self::RadialBarWidth | Self::ReactiveBloomIntensity | Self::WaveformPeakGain => {
                "0–2.".into()
            }
            Self::RadialRingOpacity => "0–1.".into(),
            Self::RadialInnerDiameter => settings.tr("settings-visualizer-diameter-hint"),
            Self::SpectrumAutoWidth => settings.tr("settings-spectrum-bar-width-hint"),
            Self::SpectrumFixedCount => settings.tr("settings-spectrum-bar-count-hint"),
            Self::SpectrumGap => "0–8.".into(),
            Self::SpectrumHold | Self::SpectrumBarHold => "0–2000.".into(),
            Self::SpectrumGravity => "0–500.".into(),
            Self::SpectrumBarGravity => settings.tr("settings-spectrum-bar-gravity-hint"),
            Self::SpectrumSmoothing => settings.tr("settings-spectrum-smoothing-hint"),
            Self::SpectrogramHistory => settings.tr("settings-spectrogram-history-hint"),
            Self::WaveformGlow => settings.tr("settings-waveform-glow-hint"),
            Self::WaveformRmsGain => settings.tr("settings-waveform-rms-hint"),
            Self::WaveformPeakGamma => settings.tr("settings-waveform-gamma-hint"),
            Self::OscilloscopeTimeSpanMs => settings.tr("settings-oscilloscope-time-span-hint"),
            Self::OscilloscopeGain => settings.tr("settings-oscilloscope-gain-hint"),
        }
    }

    fn form(self, theme: &'static Palette, settings: &Settings, control: impl IntoElement) -> Div {
        let hint = self.hint(settings);
        let control = div()
            .id(("settings-field-help", self as usize))
            .w_full()
            .child(control)
            .when(!hint.is_empty(), |control| {
                control.tooltip(rivu_ui::text_tooltip(hint))
            });
        if self == Self::ArtworkDecodeBudgetMib {
            return column()
                .w_full()
                .flex_shrink_0()
                .gap_1()
                .child(settings.info_label(
                    theme,
                    settings.tr(self.label_id()),
                    "settings-artwork-memory-help",
                ))
                .child(control);
        }
        FormField::new(theme, settings.tr(self.label_id()), control).into_div()
    }
}

fn dropdown_button(
    id: &'static str,
    name: impl Into<SharedString>,
    label: impl Into<SharedString>,
    expanded: bool,
    cx: &mut Context<Settings>,
    action: impl Fn(&mut Settings, &mut Window, &mut Context<Settings>) + 'static,
) -> Stateful<Div> {
    input_aligned_control(
        activate_control(
            dropdown_trigger(palette(cx), id, label, expanded, cx)
                .role(Role::ComboBox)
                .aria_label(name),
            cx,
            move |this, window, cx| {
                action(this, window, cx);
                cx.notify();
            },
        ),
        cx,
    )
}
fn visual_switch(
    i18n: &Localizer,
    id: &'static str,
    icon: &'static str,
    label: impl Into<SharedString>,
    enabled: bool,
    cx: &mut Context<Settings>,
    toggle: impl Fn(&mut Config) + 'static,
) -> Stateful<Div> {
    let label = label.into();
    let hint = match id {
        "settings-tray" => i18n.text("settings-restart-hint"),
        "settings-visualization-disabled" => i18n.text("settings-visualization-disabled-hint"),
        "audio-visualizer-fade-idle" => i18n.text("settings-visualizer-fade-hint"),
        "default-visualizer-reference-lines" => i18n.text("settings-visualizer-guides-hint"),
        "oscilloscope-overlay-channels" => i18n.text("settings-oscilloscope-overlay-channels-hint"),
        _ => "",
    };
    let label: SharedString = if hint.is_empty() {
        label
    } else {
        format!("{label} ({hint})").into()
    };
    let hint: SharedString = format!(
        "{label}: {}",
        i18n.text(if enabled {
            "settings-on"
        } else {
            "settings-off"
        })
    )
    .into();
    IconButton::new(id, icon, hint).switch(enabled).build(
        IconButtonStyle::Surface,
        IconButtonDensity::Regular,
        cx,
        move |this, _, cx| {
            toggle(&mut this.model.draft);
            this.model.edited();
            this.sync_field_enabled(cx);
            cx.notify();
        },
    )
}

fn settings_group(
    theme: &'static Palette,
    title: impl IntoElement,
    content: impl IntoElement,
) -> Div {
    section_group(theme, title, content)
}

fn settings_card(
    theme: &'static Palette,
    title: impl IntoElement,
    content: impl IntoElement,
) -> Div {
    settings_group(theme, title, content)
        .border_1()
        .border_color(rgb(theme.structure_border))
        .rounded(rems(5. / 16.))
}

fn spectrum_style_label(i18n: &Localizer, style: SpectrumStyle) -> String {
    match style {
        SpectrumStyle::Bars => i18n.text("settings-bars").to_owned(),
        SpectrumStyle::Outline => i18n.text("settings-outline").to_owned(),
        SpectrumStyle::Led => i18n.text("settings-led").to_owned(),
        SpectrumStyle::Line => i18n.text("settings-line").to_owned(),
        SpectrumStyle::Solid => i18n.text("settings-solid").to_owned(),
    }
}

fn visualization_palette_label(_i18n: &Localizer, palette: VisualizationPalette) -> String {
    match palette {
        VisualizationPalette::TokyoNight => "Tokyo Night".to_owned(),
        VisualizationPalette::Deadbeef => "Deadbeef".to_owned(),
    }
}

fn ffmpeg_hint(i18n: &Localizer, status: &str) -> String {
    let mut text = String::with_capacity(status.len() + 24);
    text.push_str(i18n.text("settings-ffmpeg-audio-decoding"));
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
    Library,
    Playback,
    Interface,
    Visualizations,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VisualPage {
    General,
    Waveform,
    Spectrum,
    Spectrogram,
    BarsRings,
    Oscilloscope,
}

impl VisualPage {
    const ALL: [Self; 6] = [
        Self::General,
        Self::Waveform,
        Self::Spectrum,
        Self::Spectrogram,
        Self::BarsRings,
        Self::Oscilloscope,
    ];

    fn label_id(self) -> &'static str {
        match self {
            Self::General => "settings-general",
            Self::Waveform => "settings-waveform",
            Self::Spectrum => "settings-spectrum",
            Self::Spectrogram => "settings-spectrogram",
            Self::BarsRings => "settings-bars-rings-family",
            Self::Oscilloscope => "settings-oscilloscope",
        }
    }

    fn scroll_id(self) -> &'static str {
        match self {
            Self::General => "settings-general-scroll",
            Self::Waveform => "settings-waveform-scroll",
            Self::Spectrum => "settings-spectrum-scroll",
            Self::Spectrogram => "settings-spectrogram-scroll",
            Self::BarsRings => "settings-bars-rings-scroll",
            Self::Oscilloscope => "settings-oscilloscope-scroll",
        }
    }
}

struct FontOption {
    family: String,
    search: String,
}

impl FontOption {
    fn new(family: String) -> Self {
        let search = family.to_lowercase();
        Self { family, search }
    }
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
enum Measured {
    Language,
    Theme,
    Device,
    SettingsFont,
    SettingsStyle,
    SpectrumLayout,
    DefaultVisualizerStyle,
    SettingsFft,
    SettingsLogLevel,
    SettingsWindow,
    SettingsSignal,
    AlbumArtworkStyle,
    AlbumArtworkMode,
    AlbumArtworkDirection,
}

pub(super) enum SettingsEvent {
    Closed,
    ReviewDuplicates,
    ApplyArtwork {
        panel_id: u64,
        options: AlbumArtworkOptions,
        session: u64,
    },
}

impl EventEmitter<SettingsEvent> for Settings {}

#[derive(Default)]
struct NumericInput {
    // Only genuine edits detach the displayed text from the precise draft.
    displayed: SharedString,
    edited: bool,
    revision: u64,
}

struct SubmittedInput {
    text: String,
    revision: u64,
}

struct SettingsSubmission {
    model: Submission,
    inputs: HashMap<Field, SubmittedInput>,
    layout_revision: u64,
}

/// Unsaved preferences stay independent from live playback snapshots.
pub(super) struct Settings {
    i18n: Arc<Localizer>,
    text: HashMap<String, SharedString>,
    handle: CoreHandle,
    artwork_manager: Arc<ArtworkManager>,
    root_stats: library_stats::RootStats,
    artwork_cache_age: Entity<Input>,
    artwork_cache_age_revision: u64,
    artwork_cache_busy: Option<(u64, bool)>,
    artwork_cache_report: Option<(u64, bool, ArtworkCacheReport)>,
    artwork_cache_error: Option<String>,
    devices: Arc<Vec<String>>,
    ffmpeg_status: String,
    mpris_status: String,
    ui_scale: f32,
    ui_font_size_pt: u32,
    scope: FocusScope,
    active: bool,
    measured: Rc<RefCell<HashMap<Measured, Bounds<Pixels>>>>,
    scrolls: HashMap<&'static str, Rc<ScrollHandle>>,
    active_scroll: Option<ScrollHandle>,
    _subscriptions: Vec<Subscription>,
    model: SettingsModel,
    inputs: HashMap<Field, Entity<Input>>,
    numeric_inputs: HashMap<Field, NumericInput>,
    layout_revision: u64,
    layout_switch_error: Option<String>,
    initialized: bool,
    dropdown: ChoiceState<SettingChoice>,
    font_options: Vec<FontOption>,
    font_value: String,
    dropdown_anchor: Measured,
    page: SettingsPage,
    visual_page: VisualPage,
}

impl Focusable for Settings {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.scope.handle().clone()
    }
}

impl Settings {
    fn tr(&self, id: &str) -> SharedString {
        self.text[id].clone()
    }

    pub(super) fn new(
        i18n: Arc<Localizer>,
        handle: CoreHandle,
        artwork_manager: Arc<ArtworkManager>,
        devices: Arc<Vec<String>>,
        runtime_status: (String, String),
        ui_scale: f32,
        cx: &mut Context<Self>,
    ) -> Self {
        let (ffmpeg_status, mpris_status) = runtime_status;
        let config = handle.config_snapshot();
        let artwork_cache_age = cx.new(|cx| {
            Input::new(
                i18n.text("settings-age-in-days").to_owned(),
                "30",
                i18n.text("settings-age-in-days").to_owned(),
                cx,
            )
        });
        let artwork_cache_subscription = cx.subscribe(&artwork_cache_age, |this, _, event, cx| {
            if matches!(event, InputEvent::Changed { .. }) {
                this.artwork_cache_age_revision = this.artwork_cache_age_revision.wrapping_add(1);
                this.artwork_cache_report = None;
                this.artwork_cache_error = None;
                cx.notify();
            }
        });
        let text = i18n
            .fixed_texts()
            .map(|(id, text)| (id.to_owned(), SharedString::from(text.to_owned())))
            .collect();
        Self {
            text,
            i18n,
            handle,
            artwork_manager,
            root_stats: library_stats::RootStats::default(),
            artwork_cache_age,
            artwork_cache_age_revision: 0,
            artwork_cache_busy: None,
            artwork_cache_report: None,
            artwork_cache_error: None,
            devices,
            ffmpeg_status,
            mpris_status,
            ui_scale,
            ui_font_size_pt: config.ui_font_size_pt,
            scope: FocusScope::new(cx),
            active: false,
            measured: Rc::new(RefCell::new(HashMap::new())),
            scrolls: HashMap::new(),
            active_scroll: None,
            _subscriptions: vec![artwork_cache_subscription],
            model: SettingsModel::new(&config),
            inputs: HashMap::new(),
            numeric_inputs: HashMap::new(),
            layout_revision: 0,
            layout_switch_error: None,
            initialized: false,
            dropdown: ChoiceState::default(),
            font_options: Vec::new(),
            font_value: config.ui_font.clone(),
            dropdown_anchor: Measured::Device,
            page: SettingsPage::Library,
            visual_page: VisualPage::General,
        }
    }

    pub(super) fn sync_runtime(
        &mut self,
        devices: &Arc<Vec<String>>,
        ffmpeg_status: &str,
        mpris_status: &str,
        ui_scale: f32,
        ui_font_size_pt: u32,
        cx: &mut Context<Self>,
    ) {
        let devices_changed = !Arc::ptr_eq(&self.devices, devices) && self.devices != *devices;
        let scale_changed = self.ui_scale != ui_scale || self.ui_font_size_pt != ui_font_size_pt;
        let changed = devices_changed
            || self.ffmpeg_status != ffmpeg_status
            || self.mpris_status != mpris_status
            || scale_changed;
        if !changed {
            return;
        }
        self.devices = Arc::clone(devices);
        self.ffmpeg_status.clear();
        self.ffmpeg_status.push_str(ffmpeg_status);
        self.mpris_status.clear();
        self.mpris_status.push_str(mpris_status);
        self.ui_scale = ui_scale;
        self.ui_font_size_pt = ui_font_size_pt;
        if scale_changed || (devices_changed && self.dropdown_anchor == Measured::Device) {
            self.dropdown.close();
        }
        if self.active {
            cx.notify();
        }
    }

    pub(super) fn complete_panel_apply(
        &mut self,
        session: u64,
        result: std::result::Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        if self.model.complete_panel_apply(session, result) {
            cx.notify();
        }
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.model.applying() {
            return;
        }
        self.active = false;
        self.root_stats.cancel();
        self.dropdown.close();
        self.reset(&self.handle.config_snapshot(), cx);
        cx.emit(SettingsEvent::Closed);
    }

    fn measurement(&self, key: Measured, cx: &Context<Self>) -> AnyElement {
        let measured = Rc::clone(&self.measured);
        let view = cx.entity().downgrade();
        let close_on_move = self.dropdown.is_open() && self.dropdown_anchor == key;
        canvas(
            move |bounds, _, cx| {
                let previous = measured.borrow_mut().insert(key, bounds);
                if close_on_move && previous.is_some_and(|previous| previous != bounds) {
                    let view = view.clone();
                    cx.defer(move |cx| {
                        let _ = view.update(cx, |this, cx| {
                            if this.dropdown.is_open() && this.dropdown_anchor == key {
                                this.dropdown.close();
                                cx.notify();
                            }
                        });
                    });
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .into_any_element()
    }

    fn initialize(&mut self, cx: &mut App) {
        if self.initialized {
            return;
        }
        self.inputs = [
            Field::ScanMaxDepth,
            Field::ScanMinDurationSeconds,
            Field::Volume,
            Field::PlayCountThreshold,
            Field::LogRetentionWeeks,
            Field::MediaReadBufferMib,
            Field::ArtworkFileLimitMib,
            Field::ArtworkDecodeBudgetMib,
            Field::GpuMaterialBudgetMib,
            Field::Font,
            Field::FontSizePt,
            Field::Scale,
            Field::LayoutSnapGrid,
            Field::AnalysisRateHz,
            Field::VisualizationAnimationFps,
            Field::VisualizationBackground,
            Field::VisualizationDbRange,
            Field::ReactiveSensitivity,
            Field::RadialRotationSpeed,
            Field::RadialBarWidth,
            Field::RadialRingOpacity,
            Field::ReactiveBloomIntensity,
            Field::RadialInnerDiameter,
            Field::EffectPrimaryColor,
            Field::EffectSecondaryColor,
            Field::ReactiveDbRange,
            Field::SpectrumDbRange,
            Field::SpectrumAutoWidth,
            Field::SpectrumFixedCount,
            Field::SpectrumGap,
            Field::SpectrumHold,
            Field::SpectrumGravity,
            Field::SpectrumBarHold,
            Field::SpectrumBarGravity,
            Field::SpectrumSmoothing,
            Field::SpectrogramDbRange,
            Field::SpectrogramHistory,
            Field::WaveformCursorColor,
            Field::WaveformGlow,
            Field::WaveformRmsGain,
            Field::WaveformPeakGain,
            Field::WaveformPeakGamma,
            Field::OscilloscopeTimeSpanMs,
            Field::OscilloscopeGain,
            Field::AlbumArtworkPeriodSeconds,
            Field::AlbumArtworkFps,
        ]
        .into_iter()
        .map(|field| {
            let input = cx.new(|cx| {
                let placeholder = self.tr(match field {
                    Field::Font => "settings-font-search-placeholder",
                    Field::SpectrumDbRange | Field::SpectrogramDbRange | Field::ReactiveDbRange => {
                        "settings-inherit-placeholder"
                    }
                    Field::VisualizationBackground
                    | Field::EffectPrimaryColor
                    | Field::EffectSecondaryColor
                    | Field::WaveformCursorColor => "settings-color-placeholder",
                    Field::MediaReadBufferMib => "settings-buffer-placeholder",
                    Field::AlbumArtworkFps | Field::VisualizationAnimationFps => {
                        "settings-display-fps-placeholder"
                    }
                    Field::ScanMinDurationSeconds => "settings-duration-placeholder",
                    _ => field.label_id(),
                });
                let input = Input::new(self.tr(field.label_id()), "", placeholder, cx);
                if field == Field::Font {
                    input.with_trailing_space(px(28.)).with_control_surface()
                } else {
                    input
                }
            });
            (field, input)
        })
        .collect();
        let generic = ["sans-serif", "serif", "monospace"];
        self.font_options = generic
            .iter()
            .map(|family| FontOption::new((*family).to_owned()))
            .collect();
        self.font_options.extend(
            cx.text_system()
                .all_font_names()
                .into_iter()
                .filter(|family| {
                    !generic.contains(&family.as_str())
                        && family != super::appearance::fonts::SYMBOLS_FAMILY
                })
                .map(FontOption::new),
        );
        self.initialized = true;
        self.fill(cx);
    }

    fn font_items(&self, query: &str) -> Vec<ChoiceItem<SettingChoice>> {
        let query = query.trim();
        let search = query.to_lowercase();
        let mut items: Vec<_> = self
            .font_options
            .iter()
            .enumerate()
            .filter(|(_, option)| option.search.contains(&search))
            .map(|(index, option)| {
                ChoiceItem::new(SettingChoice::Font(index), option.family.clone())
            })
            .collect();
        if !query.is_empty()
            && !self
                .font_options
                .iter()
                .any(|option| option.family.eq_ignore_ascii_case(query))
        {
            items.push(ChoiceItem::new(
                SettingChoice::CustomFont,
                self.i18n
                    .format("settings-use-font", &[("font", query.into())]),
            ));
        }
        items
    }

    fn field(&self, field: Field, cx: &App) -> Div {
        let input = self.inputs[&field].clone();
        if matches!(
            field,
            Field::SpectrumDbRange | Field::ReactiveDbRange | Field::SpectrogramDbRange
        ) {
            let common = self.inputs[&Field::VisualizationDbRange].clone();
            let fallback = self.model.draft.visualization_db_range;
            let i18n = Arc::clone(&self.i18n);
            return field.form(
                palette(cx),
                self,
                div()
                    .id(("settings-range-help", field as usize))
                    .w_full()
                    .child(input)
                    .tooltip(move |_, cx| {
                        let range = common
                            .read(cx)
                            .text()
                            .trim()
                            .parse::<f32>()
                            .ok()
                            .filter(|range| range.is_finite() && (1.0..=160.0).contains(range))
                            .unwrap_or(fallback);
                        cx.new(|_| ButtonTooltip {
                            text: i18n
                                .format("settings-inherited-range", &[("range", range.into())])
                                .into(),
                        })
                        .into()
                    }),
            );
        }
        field.form(palette(cx), self, input)
    }

    fn pair(&self, first: Field, second: Field, cx: &App) -> Div {
        two_column(
            self.field(first, cx).min_w(rems(180. / 16.)),
            self.field(second, cx).min_w(rems(180. / 16.)),
        )
    }

    fn single(&self, field: Field, cx: &App) -> Div {
        row().child(
            self.field(field, cx)
                .w(relative(0.5))
                .min_w(rems(180. / 16.))
                .pr_2(),
        )
    }

    fn spectrum_layout_field_state(&self, cx: &mut Context<Self>) -> Div {
        self.field_state(self.spectrum_layout_field(), cx)
            .when_some(self.layout_switch_error.as_ref(), |control, error| {
                control.child(status_message(
                    palette(cx),
                    error.clone(),
                    StatusKind::Error,
                ))
            })
    }

    fn spectrum_layout_field(&self) -> Field {
        match self.model.draft.spectrum_bar_layout {
            SpectrumBarLayout::Automatic { .. } => Field::SpectrumAutoWidth,
            SpectrumBarLayout::Fixed { .. } => Field::SpectrumFixedCount,
        }
    }

    fn fill_spectrum_layout(
        &mut self,
        unchanged: Option<&HashMap<Field, SubmittedInput>>,
        cx: &mut App,
    ) {
        match self.model.draft.spectrum_bar_layout {
            SpectrumBarLayout::Automatic { width } => {
                self.set_float(Field::SpectrumAutoWidth, Some(width), unchanged, cx);
            }
            SpectrumBarLayout::Fixed { count } => {
                self.set_value(Field::SpectrumFixedCount, count.to_string(), unchanged, cx);
            }
        }
    }

    fn parse_spectrum_layout(&self, cx: &App) -> Result<SpectrumBarLayout> {
        let field = self.spectrum_layout_field();
        let layout = match self.model.draft.spectrum_bar_layout {
            SpectrumBarLayout::Automatic { width } => SpectrumBarLayout::Automatic {
                width: self.float(field, width, cx)?,
            },
            SpectrumBarLayout::Fixed { .. } => SpectrumBarLayout::Fixed {
                count: self.number(field, cx)?,
            },
        };
        let valid = match layout {
            SpectrumBarLayout::Automatic { width } => {
                width.is_finite() && (1.0..=20.0).contains(&width)
            }
            SpectrumBarLayout::Fixed { count } => (1..=512).contains(&count),
        };
        if !valid {
            return Err(FieldIssue {
                field,
                message: format!(
                    "{}: {}",
                    field.label(&self.i18n),
                    self.i18n.text(match layout {
                        SpectrumBarLayout::Automatic { .. } => "settings-spectrum-bar-width-hint",
                        SpectrumBarLayout::Fixed { .. } => "settings-spectrum-bar-count-hint",
                    })
                ),
            }
            .into());
        }
        Ok(layout)
    }

    fn select_spectrum_layout(&mut self, mode: SpectrumLayoutMode, cx: &mut Context<Self>) {
        if SpectrumLayoutMode::from_layout(self.model.draft.spectrum_bar_layout) == mode {
            self.dropdown.close();
            cx.notify();
            return;
        }
        if let Err(error) = self.parse_spectrum_layout(cx) {
            self.reveal_field(self.spectrum_layout_field());
            self.layout_switch_error = Some(self.i18n.format(
                "settings-spectrum-layout-invalid",
                &[("error", format!("{error:#}").into())],
            ));
            cx.notify();
            return;
        }
        // Even a switch away and back to identical text is newer than an Apply
        // snapshot. Fence both editors before initializing the new active value.
        for field in [Field::SpectrumAutoWidth, Field::SpectrumFixedCount] {
            let state = self.numeric_inputs.entry(field).or_default();
            state.revision = state.revision.wrapping_add(1);
        }
        self.layout_revision = self.layout_revision.wrapping_add(1);
        self.layout_switch_error = None;
        self.model.draft.spectrum_bar_layout = mode.initial_layout();
        self.fill_spectrum_layout(None, cx);
        self.model.edited();
        self.dropdown.close();
        self.sync_field_enabled(cx);
        cx.notify();
    }

    fn field_enabled(&self, field: Field, _: &App) -> bool {
        field.enabled(&self.model.draft)
    }

    fn field_state(&self, field: Field, cx: &mut Context<Self>) -> Div {
        column()
            .min_w(rems(180. / 16.))
            .gap_1()
            .child(self.field(field, cx))
            .when(
                !self.field_enabled(field, cx)
                    && match field {
                        Field::SpectrumFixedCount => {
                            !matches!(self.model.draft.spectrum_bar_layout,
                                SpectrumBarLayout::Fixed { count }
                                    if self.inputs[&field].read(cx).text().trim().parse::<u32>() == Ok(count))
                        }
                        Field::SpectrumHold => {
                            self.inputs[&field].read(cx).text().trim().parse::<u32>()
                                != Ok(self.model.draft.spectrum_peak_hold_ms)
                        }
                        _ => !self.numeric_untouched(field, cx),
                    },
                |control| {
                    control
                        .child(div().text_xs().child(field.hint(self)))
                        .child(
                            div()
                                .id(("settings-reset-field-help", field as usize))
                                .child(rivu_ui::button(
                                    ("settings-reset-field", field as usize),
                                    self.tr("settings-reset-field-text"),
                                    cx,
                                    move |this, _, cx| this.restore_field_text(field, cx),
                                ))
                                .tooltip(rivu_ui::text_tooltip(
                                    self.tr("settings-reset-field-hint"),
                                )),
                        )
                },
            )
    }

    fn restore_field_text(&mut self, field: Field, cx: &mut Context<Self>) {
        match field {
            Field::AlbumArtworkPeriodSeconds => self.set_float(
                field,
                Some(self.model.draft.album_artwork_period_seconds),
                None,
                cx,
            ),
            Field::SpectrumAutoWidth | Field::SpectrumFixedCount => {
                self.fill_spectrum_layout(None, cx);
                self.layout_switch_error = None;
            }
            Field::SpectrumGap => {
                self.set_float(field, Some(self.model.draft.spectrum_gap), None, cx)
            }
            Field::SpectrumGravity => self.set_float(
                field,
                Some(self.model.draft.spectrum_peak_gravity),
                None,
                cx,
            ),
            Field::SpectrumHold => {
                self.set_value(
                    field,
                    self.model.draft.spectrum_peak_hold_ms.to_string(),
                    None,
                    cx,
                );
            }
            _ => unreachable!("only conditionally disabled fields expose text recovery"),
        }
        self.sync_field_enabled(cx);
        self.model.edited();
        cx.notify();
    }

    fn sync_field_enabled(&self, cx: &mut App) {
        for field in [
            Field::AlbumArtworkPeriodSeconds,
            Field::SpectrumAutoWidth,
            Field::SpectrumFixedCount,
            Field::SpectrumGap,
            Field::SpectrumHold,
            Field::SpectrumGravity,
        ] {
            if let Some(input) = self.inputs.get(&field) {
                let enabled = self.field_enabled(field, cx);
                input.update(cx, |input, cx| input.set_enabled(enabled, cx));
            }
        }
    }

    fn value(&self, field: Field, cx: &App) -> String {
        self.inputs[&field].read(cx).text().to_owned()
    }

    fn numeric_revision(&self, field: Field) -> u64 {
        self.numeric_inputs
            .get(&field)
            .map_or(0, |input| input.revision)
    }

    fn numeric_untouched(&self, field: Field, cx: &App) -> bool {
        // Rounding belongs only to presentation. Parsing an untouched editor
        // must not feed that lossy presentation back into the config.
        self.numeric_inputs.get(&field).is_some_and(|state| {
            !state.edited && self.inputs[&field].read(cx).text() == &*state.displayed
        })
    }

    fn float<T: FromStr>(&self, field: Field, original: T, cx: &App) -> Result<T>
    where
        T::Err: std::fmt::Display,
    {
        if self.numeric_untouched(field, cx) {
            Ok(original)
        } else {
            self.number(field, cx)
        }
    }

    fn optional_float<T: FromStr>(
        &self,
        field: Field,
        original: Option<T>,
        cx: &App,
    ) -> Result<Option<T>>
    where
        T::Err: std::fmt::Display,
    {
        if self.numeric_untouched(field, cx) {
            Ok(original)
        } else {
            self.optional_number(field, cx)
        }
    }

    fn set_float<T: Into<f64>>(
        &mut self,
        field: Field,
        value: Option<T>,
        unchanged: Option<&HashMap<Field, SubmittedInput>>,
        cx: &mut App,
    ) {
        let text: SharedString = value
            .map_or_else(String::new, |value| {
                let value: f64 = value.into();
                let precision = field.precision().expect("floating Settings field");
                let mut text = format!("{value:.precision$}");
                if precision > 0 {
                    let trimmed = text.trim_end_matches('0').trim_end_matches('.').len();
                    text.truncate(trimmed);
                }
                if text == "-0" {
                    text.remove(0);
                }
                text
            })
            .into();
        if self.set_value(field, text.clone(), unchanged, cx) {
            let state = self.numeric_inputs.entry(field).or_default();
            state.displayed = text;
            state.edited = false;
        }
    }

    fn number<T: FromStr>(&self, field: Field, cx: &App) -> Result<T>
    where
        T::Err: std::fmt::Display,
    {
        self.value(field, cx).trim().parse().map_err(|error| {
            FieldIssue {
                field,
                message: format!("{}: {error}", field.label(&self.i18n)),
            }
            .into()
        })
    }

    fn optional_number<T: FromStr>(&self, field: Field, cx: &App) -> Result<Option<T>>
    where
        T::Err: std::fmt::Display,
    {
        let value = self.value(field, cx);
        if value.trim().is_empty() {
            Ok(None)
        } else {
            value.trim().parse().map(Some).map_err(|error| {
                FieldIssue {
                    field,
                    message: format!("{}: {error}", field.label(&self.i18n)),
                }
                .into()
            })
        }
    }

    fn set_value(
        &mut self,
        field: Field,
        value: impl Into<SharedString>,
        unchanged: Option<&HashMap<Field, SubmittedInput>>,
        cx: &mut App,
    ) -> bool {
        if unchanged.is_some_and(|inputs| {
            inputs.get(&field).is_none_or(|submitted| {
                self.inputs[&field].read(cx).text() != submitted.text
                    || self.numeric_revision(field) != submitted.revision
            })
        }) {
            return false;
        }
        self.inputs[&field].update(cx, |input, cx| input.set_text(value.into(), cx));
        true
    }

    fn reset(&mut self, config: &Arc<Config>, cx: &mut App) {
        self.model.reset(config);
        self.artwork_cache_report = None;
        self.artwork_cache_error = None;
        self.fill(cx);
    }

    fn fill(&mut self, cx: &mut App) {
        self.layout_switch_error = None;
        self.fill_unchanged(None, cx);
    }

    fn fill_unchanged(&mut self, unchanged: Option<&HashMap<Field, SubmittedInput>>, cx: &mut App) {
        self.set_value(
            Field::ScanMaxDepth,
            self.model.draft.scan_max_depth.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::ScanMinDurationSeconds,
            self.model.draft.scan_min_duration_seconds.to_string(),
            unchanged,
            cx,
        );
        self.set_float(
            Field::Volume,
            Some(self.model.draft.volume * 100.),
            unchanged,
            cx,
        );
        self.set_float(
            Field::PlayCountThreshold,
            Some(self.model.draft.play_count_threshold_percent),
            unchanged,
            cx,
        );
        self.set_value(
            Field::LogRetentionWeeks,
            self.model.draft.log_retention_weeks.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::MediaReadBufferMib,
            self.model.draft.media_read_buffer_mib.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::ArtworkFileLimitMib,
            self.model.draft.artwork_file_limit_mib.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::ArtworkDecodeBudgetMib,
            self.model.draft.artwork_decode_budget_mib.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::GpuMaterialBudgetMib,
            self.model.draft.gpu_material_budget_mib.to_string(),
            unchanged,
            cx,
        );
        self.font_value = self.model.draft.ui_font.clone();
        self.set_value(Field::Font, self.model.draft.ui_font.clone(), unchanged, cx);
        self.set_value(
            Field::FontSizePt,
            self.model.draft.ui_font_size_pt.to_string(),
            unchanged,
            cx,
        );
        self.set_float(Field::Scale, Some(self.model.draft.ui_scale), unchanged, cx);
        self.set_value(
            Field::LayoutSnapGrid,
            self.model.draft.layout_snap_grid_px.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::AnalysisRateHz,
            self.model.draft.analysis_rate_hz.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::VisualizationAnimationFps,
            self.model.draft.visualization_fps.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::VisualizationBackground,
            self.model.draft.visualization_background.to_string(),
            unchanged,
            cx,
        );
        self.set_float(
            Field::VisualizationDbRange,
            Some(self.model.draft.visualization_db_range),
            unchanged,
            cx,
        );
        self.set_float(
            Field::ReactiveSensitivity,
            Some(self.model.draft.reactive_sensitivity),
            unchanged,
            cx,
        );
        self.set_float(
            Field::RadialRotationSpeed,
            Some(self.model.draft.radial_rotation_speed),
            unchanged,
            cx,
        );
        self.set_float(
            Field::RadialBarWidth,
            Some(self.model.draft.radial_bar_width),
            unchanged,
            cx,
        );
        self.set_float(
            Field::RadialRingOpacity,
            Some(self.model.draft.radial_ring_opacity),
            unchanged,
            cx,
        );
        self.set_float(
            Field::ReactiveBloomIntensity,
            Some(self.model.draft.reactive_bloom_intensity),
            unchanged,
            cx,
        );
        self.set_float(
            Field::RadialInnerDiameter,
            Some(self.model.draft.radial_inner_diameter),
            unchanged,
            cx,
        );
        self.set_value(
            Field::EffectPrimaryColor,
            self.model.draft.effect_primary_color.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::EffectSecondaryColor,
            self.model.draft.effect_secondary_color.to_string(),
            unchanged,
            cx,
        );
        self.set_float(
            Field::ReactiveDbRange,
            self.model.draft.reactive_db_range,
            unchanged,
            cx,
        );
        self.set_float(
            Field::SpectrumDbRange,
            self.model.draft.spectrum_db_range,
            unchanged,
            cx,
        );
        self.set_float(
            Field::SpectrumGap,
            Some(self.model.draft.spectrum_gap),
            unchanged,
            cx,
        );
        self.set_value(
            Field::SpectrumHold,
            self.model.draft.spectrum_peak_hold_ms.to_string(),
            unchanged,
            cx,
        );
        self.set_float(
            Field::SpectrumGravity,
            Some(self.model.draft.spectrum_peak_gravity),
            unchanged,
            cx,
        );
        self.set_value(
            Field::SpectrumBarHold,
            self.model.draft.spectrum_bar_hold_ms.to_string(),
            unchanged,
            cx,
        );
        self.set_float(
            Field::SpectrumBarGravity,
            Some(self.model.draft.spectrum_bar_gravity),
            unchanged,
            cx,
        );
        self.set_value(
            Field::SpectrumSmoothing,
            self.model.draft.spectrum_smoothing_ms.to_string(),
            unchanged,
            cx,
        );
        self.fill_spectrum_layout(unchanged, cx);
        self.set_float(
            Field::SpectrogramDbRange,
            self.model.draft.spectrogram_db_range,
            unchanged,
            cx,
        );
        self.set_value(
            Field::SpectrogramHistory,
            self.model.draft.spectrogram_history_seconds.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::WaveformCursorColor,
            self.model.draft.waveform_cursor_color.to_string(),
            unchanged,
            cx,
        );
        self.set_float(
            Field::WaveformGlow,
            Some(self.model.draft.waveform_glow),
            unchanged,
            cx,
        );
        self.set_float(
            Field::WaveformRmsGain,
            Some(self.model.draft.waveform_rms_gain),
            unchanged,
            cx,
        );
        self.set_float(
            Field::WaveformPeakGain,
            Some(self.model.draft.waveform_peak_gain),
            unchanged,
            cx,
        );
        self.set_float(
            Field::WaveformPeakGamma,
            Some(self.model.draft.waveform_peak_gamma),
            unchanged,
            cx,
        );
        self.set_float(
            Field::OscilloscopeTimeSpanMs,
            Some(self.model.draft.oscilloscope_time_span_ms),
            unchanged,
            cx,
        );
        self.set_float(
            Field::OscilloscopeGain,
            Some(self.model.draft.oscilloscope_gain),
            unchanged,
            cx,
        );
        self.set_float(
            Field::AlbumArtworkPeriodSeconds,
            Some(self.model.draft.album_artwork_period_seconds),
            unchanged,
            cx,
        );
        self.set_value(
            Field::AlbumArtworkFps,
            self.model.draft.album_artwork_fps.to_string(),
            unchanged,
            cx,
        );
        self.sync_field_enabled(cx);
    }

    fn parse(&self, cx: &App) -> Result<Config> {
        let mut config = self.model.draft.clone();
        config.scan_max_depth = self.number(Field::ScanMaxDepth, cx)?;
        config.scan_min_duration_seconds = self.number(Field::ScanMinDurationSeconds, cx)?;
        if !self.numeric_untouched(Field::Volume, cx) {
            config.volume = self.number::<f32>(Field::Volume, cx)? / 100.;
        }
        config.play_count_threshold_percent = self.float(
            Field::PlayCountThreshold,
            self.model.draft.play_count_threshold_percent,
            cx,
        )?;
        config.log_retention_weeks = self.number(Field::LogRetentionWeeks, cx)?;
        config.media_read_buffer_mib = self.number(Field::MediaReadBufferMib, cx)?;
        config.artwork_file_limit_mib = self.number(Field::ArtworkFileLimitMib, cx)?;
        config.artwork_decode_budget_mib = self.number(Field::ArtworkDecodeBudgetMib, cx)?;
        config.gpu_material_budget_mib = self.number(Field::GpuMaterialBudgetMib, cx)?;
        config.ui_font = self.value(Field::Font, cx).trim().to_owned();
        if config.ui_font.is_empty() {
            config.ui_font = "monospace".to_owned();
        }
        config.ui_font_size_pt = self.number(Field::FontSizePt, cx)?;
        config.ui_scale = self.float(Field::Scale, self.model.draft.ui_scale, cx)?;
        config.layout_snap_grid_px = self.number(Field::LayoutSnapGrid, cx)?;
        config.analysis_rate_hz = self.number(Field::AnalysisRateHz, cx)?;
        config.visualization_fps = self.number(Field::VisualizationAnimationFps, cx)?;
        config.visualization_db_range = self.float(
            Field::VisualizationDbRange,
            self.model.draft.visualization_db_range,
            cx,
        )?;
        config.visualization_background = self.number(Field::VisualizationBackground, cx)?;
        config.reactive_sensitivity = self.float(
            Field::ReactiveSensitivity,
            self.model.draft.reactive_sensitivity,
            cx,
        )?;
        config.radial_rotation_speed = self.float(
            Field::RadialRotationSpeed,
            self.model.draft.radial_rotation_speed,
            cx,
        )?;
        config.radial_bar_width =
            self.float(Field::RadialBarWidth, self.model.draft.radial_bar_width, cx)?;
        config.radial_ring_opacity = self.float(
            Field::RadialRingOpacity,
            self.model.draft.radial_ring_opacity,
            cx,
        )?;
        config.reactive_bloom_intensity = self.float(
            Field::ReactiveBloomIntensity,
            self.model.draft.reactive_bloom_intensity,
            cx,
        )?;
        config.radial_inner_diameter = self.float(
            Field::RadialInnerDiameter,
            self.model.draft.radial_inner_diameter,
            cx,
        )?;
        config.effect_primary_color = self.number(Field::EffectPrimaryColor, cx)?;
        config.effect_secondary_color = self.number(Field::EffectSecondaryColor, cx)?;
        config.reactive_db_range = self.optional_float(
            Field::ReactiveDbRange,
            self.model.draft.reactive_db_range,
            cx,
        )?;
        config.spectrum_db_range = self.optional_float(
            Field::SpectrumDbRange,
            self.model.draft.spectrum_db_range,
            cx,
        )?;
        config.spectrum_gap = self.float(Field::SpectrumGap, self.model.draft.spectrum_gap, cx)?;
        config.spectrum_peak_hold_ms = self.number(Field::SpectrumHold, cx)?;
        config.spectrum_peak_gravity = self.float(
            Field::SpectrumGravity,
            self.model.draft.spectrum_peak_gravity,
            cx,
        )?;
        config.spectrum_bar_hold_ms = self.number(Field::SpectrumBarHold, cx)?;
        config.spectrum_bar_gravity = self.float(
            Field::SpectrumBarGravity,
            self.model.draft.spectrum_bar_gravity,
            cx,
        )?;
        config.spectrum_smoothing_ms = self.number(Field::SpectrumSmoothing, cx)?;
        config.spectrum_bar_layout = self.parse_spectrum_layout(cx)?;
        config.spectrogram_db_range = self.optional_float(
            Field::SpectrogramDbRange,
            self.model.draft.spectrogram_db_range,
            cx,
        )?;
        config.spectrogram_history_seconds = self.number(Field::SpectrogramHistory, cx)?;
        config.waveform_cursor_color = self.number(Field::WaveformCursorColor, cx)?;
        config.waveform_glow =
            self.float(Field::WaveformGlow, self.model.draft.waveform_glow, cx)?;
        config.waveform_rms_gain = self.float(
            Field::WaveformRmsGain,
            self.model.draft.waveform_rms_gain,
            cx,
        )?;
        config.waveform_peak_gain = self.float(
            Field::WaveformPeakGain,
            self.model.draft.waveform_peak_gain,
            cx,
        )?;
        config.waveform_peak_gamma = self.float(
            Field::WaveformPeakGamma,
            self.model.draft.waveform_peak_gamma,
            cx,
        )?;
        config.oscilloscope_time_span_ms = self.float(
            Field::OscilloscopeTimeSpanMs,
            self.model.draft.oscilloscope_time_span_ms,
            cx,
        )?;
        config.oscilloscope_gain = self.float(
            Field::OscilloscopeGain,
            self.model.draft.oscilloscope_gain,
            cx,
        )?;
        config.album_artwork_period_seconds = self.float(
            Field::AlbumArtworkPeriodSeconds,
            self.model.draft.album_artwork_period_seconds,
            cx,
        )?;
        config.album_artwork_fps = self.number(Field::AlbumArtworkFps, cx)?;
        config.validate().map_err(|error| {
            let message = error.to_string();
            self.inputs
                .keys()
                .find(|field| {
                    message
                        .strip_prefix(field.config_key())
                        .is_some_and(|rest| rest.starts_with(' '))
                })
                .map_or(error, |&field| FieldIssue { field, message }.into())
        })?;
        Ok(config)
    }

    fn tab_button(
        &self,
        id: &'static str,
        label: impl Into<SharedString>,
        active: bool,
        cx: &mut Context<Settings>,
        select: impl Fn(&mut Settings) + 'static,
    ) -> Stateful<Div> {
        let label: SharedString = label.into();
        let theme = palette(cx);
        active_control(
            theme,
            activate_control(
                div()
                    .id(id)
                    .group("control")
                    .tab_index(0)
                    .focus(|style| style.border_color(rgb(theme.accent)))
                    .flex()
                    .items_center()
                    .w_full()
                    .min_w_0()
                    .min_h_8()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(gpui::transparent_black())
                    .text_sm()
                    .text_color(rgb(theme.text))
                    .cursor_pointer()
                    .group_hover("control", |style| style.bg(rgb(theme.highlight)))
                    .aria_label(label.clone())
                    .tooltip(rivu_ui::text_tooltip(label.clone()))
                    .child(div().flex_1().min_w_0().truncate().child(label)),
                cx,
                move |this, _, cx| {
                    this.dropdown.close();
                    select(this);
                    cx.notify();
                },
            ),
            active,
        )
        .role(Role::Tab)
        .aria_selected(active)
        .flex_shrink_0()
    }
}

impl Settings {
    fn initialize_settings(&mut self, cx: &mut Context<Self>) {
        if self.initialized {
            return;
        }
        self.initialize(cx);
        for (&field, input) in &self.inputs {
            self._subscriptions
                .push(cx.subscribe(input, move |this, _, event, cx| {
                    // Programmatic rebase/reset events must not become user edits.
                    if matches!(event, InputEvent::Changed { user: true, .. }) {
                        if matches!(field, Field::SpectrumAutoWidth | Field::SpectrumFixedCount) {
                            this.layout_switch_error = None;
                        }
                        if field.precision().is_some() {
                            let state = this.numeric_inputs.entry(field).or_default();
                            state.edited = true;
                            // Same-text replacement is intentional and survives Apply.
                            state.revision = state.revision.wrapping_add(1);
                        }
                        this.model.edited();
                        this.sync_field_enabled(cx);
                        cx.notify();
                    }
                }));
        }
        self._subscriptions.push(
            cx.subscribe(&self.inputs[&Field::Font], |this, _, event, cx| {
                if matches!(event, InputEvent::Submit) {
                    if this.dropdown.is_open()
                        && this.dropdown_anchor == Measured::SettingsFont
                        && let Some(index) = this.dropdown.selected_index()
                    {
                        this.choose_setting(index, cx);
                    }
                    return;
                }
                let value = this.value(Field::Font, cx);
                if value == this.font_value {
                    return;
                }
                this.font_value = value;
                if this.active {
                    this.open_font_dropdown(true, cx);
                    cx.notify();
                }
            }),
        );
    }

    fn open_font_dropdown(&mut self, filtered: bool, cx: &App) {
        let value = self.value(Field::Font, cx);
        let items = self.font_items(if filtered { &value } else { "" });
        let selected = items
            .iter()
            .position(|item| match item.value {
                SettingChoice::Font(index) => self.font_options[index]
                    .family
                    .eq_ignore_ascii_case(value.trim()),
                _ => false,
            })
            .unwrap_or(0);
        self.open_setting_dropdown(items, selected);
    }

    fn settings_font_input_focused(&self, window: &Window, cx: &App) -> bool {
        self.dropdown_anchor == Measured::SettingsFont
            && self.inputs[&Field::Font].read(cx).is_focused(window)
    }

    fn toggle_font_dropdown(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dropdown.is_open() && self.dropdown_anchor == Measured::SettingsFont {
            self.dropdown.close();
        } else {
            self.open_font_dropdown(false, cx);
        }
        self.inputs[&Field::Font].focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn font_control(&self, cx: &mut Context<Self>) -> Div {
        let input = self.inputs[&Field::Font].clone();
        let control = div()
            .id("settings-font")
            .relative()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if !this.dropdown.is_open() || this.dropdown_anchor != Measured::SettingsFont {
                        this.open_font_dropdown(false, cx);
                        cx.notify();
                    }
                }),
            )
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                // Input owns IME and Enter submission; only local candidate navigation is captured.
                let composing = this.inputs[&Field::Font].read(cx).is_composing();
                if this.dropdown.is_open() && this.dropdown_anchor != Measured::SettingsFont {
                    return;
                }
                this.choice_key(event, ChoiceMode::Editable, composing, window, cx);
            }))
            .child(input.clone())
            .child(
                div()
                    .id("settings-font-arrow")
                    .role(Role::ComboBox)
                    .aria_label(self.tr(Field::Font.label_id()))
                    .aria_value(SharedString::from(input.read(cx).text().to_owned()))
                    .aria_expanded(
                        self.dropdown.is_open() && self.dropdown_anchor == Measured::SettingsFont,
                    )
                    .on_a11y_action(AccessibleAction::Click, {
                        let owner = cx.entity().downgrade();
                        move |_, window, cx| {
                            let _ =
                                owner.update(cx, |this, cx| this.toggle_font_dropdown(window, cx));
                        }
                    })
                    .absolute()
                    .right(px(1.))
                    .top(px(1.))
                    .bottom(px(1.))
                    .w(rems(27. / 16.))
                    .h_auto()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .font_family(super::appearance::fonts::SYMBOLS_FAMILY)
                    .font_size_scale(1.)
                    .text_size(rems(0.875))
                    .text_color(rgb(palette(cx).muted))
                    .child("󰅀")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            this.toggle_font_dropdown(window, cx);
                            cx.stop_propagation();
                        }),
                    ),
            )
            .child(self.measurement(Measured::SettingsFont, cx));
        Field::Font.form(palette(cx), self, control)
    }

    pub(super) fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.model.reset(&self.handle.config_snapshot());
        self.show_session(window, cx);
    }

    fn show_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.initialize_settings(cx);
        self.dropdown.close();
        self.artwork_cache_report = None;
        self.artwork_cache_error = None;
        self.fill(cx);
        self.active = true;
        self.root_stats.retry();
        self.sync_library_stats(cx);
        self.scope.focus(window, cx);
        cx.notify();
    }

    pub(super) fn open_artwork(
        &mut self,
        panel_id: u64,
        options: AlbumArtworkOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.model
            .reset_artwork(&self.handle.config_snapshot(), panel_id, options);
        self.page = SettingsPage::Interface;
        self.show_session(window, cx);
    }

    fn reveal_field(&mut self, field: Field) {
        let (page, visual_page) = match field {
            Field::ScanMaxDepth
            | Field::ScanMinDurationSeconds
            | Field::ArtworkFileLimitMib
            | Field::ArtworkDecodeBudgetMib => (SettingsPage::Library, None),
            Field::Volume | Field::PlayCountThreshold | Field::MediaReadBufferMib => {
                (SettingsPage::Playback, None)
            }
            Field::Font
            | Field::FontSizePt
            | Field::Scale
            | Field::LayoutSnapGrid
            | Field::LogRetentionWeeks
            | Field::AlbumArtworkPeriodSeconds
            | Field::AlbumArtworkFps => (SettingsPage::Interface, None),
            Field::EffectPrimaryColor
            | Field::EffectSecondaryColor
            | Field::ReactiveDbRange
            | Field::ReactiveSensitivity
            | Field::ReactiveBloomIntensity => {
                (SettingsPage::Visualizations, Some(VisualPage::General))
            }
            Field::RadialRotationSpeed
            | Field::RadialBarWidth
            | Field::RadialRingOpacity
            | Field::RadialInnerDiameter => {
                (SettingsPage::Visualizations, Some(VisualPage::BarsRings))
            }
            Field::SpectrumDbRange
            | Field::SpectrumAutoWidth
            | Field::SpectrumFixedCount
            | Field::SpectrumGap
            | Field::SpectrumHold
            | Field::SpectrumGravity
            | Field::SpectrumBarHold
            | Field::SpectrumBarGravity
            | Field::SpectrumSmoothing => {
                (SettingsPage::Visualizations, Some(VisualPage::Spectrum))
            }
            Field::SpectrogramDbRange | Field::SpectrogramHistory => {
                (SettingsPage::Visualizations, Some(VisualPage::Spectrogram))
            }
            Field::WaveformCursorColor
            | Field::WaveformGlow
            | Field::WaveformRmsGain
            | Field::WaveformPeakGain
            | Field::WaveformPeakGamma => {
                (SettingsPage::Visualizations, Some(VisualPage::Waveform))
            }
            Field::OscilloscopeTimeSpanMs | Field::OscilloscopeGain => {
                (SettingsPage::Visualizations, Some(VisualPage::Oscilloscope))
            }
            Field::GpuMaterialBudgetMib
            | Field::AnalysisRateHz
            | Field::VisualizationAnimationFps
            | Field::VisualizationBackground
            | Field::VisualizationDbRange => {
                (SettingsPage::Visualizations, Some(VisualPage::General))
            }
        };
        self.page = page;
        if let Some(visual_page) = visual_page {
            self.visual_page = visual_page;
        }
        self.dropdown.close();
    }

    fn apply_settings(&mut self, cx: &mut Context<Self>) {
        if self.model.applying() {
            return;
        }
        if matches!(self.model.target(), Target::Artwork(_)) {
            let period = self
                .float(
                    Field::AlbumArtworkPeriodSeconds,
                    self.model.draft.album_artwork_period_seconds,
                    cx,
                )
                .ok();
            match self.model.begin_panel_apply(period) {
                Some(PanelApply::Artwork {
                    panel_id,
                    options,
                    session,
                }) => {
                    cx.emit(SettingsEvent::ApplyArtwork {
                        panel_id,
                        options,
                        session,
                    });
                }
                None => self
                    .model
                    .reject_input(self.i18n.text("settings-artwork-period-hint").to_owned()),
            }
            cx.notify();
            return;
        }
        let config = match self.parse(cx) {
            Ok(config) => config,
            Err(error) => {
                if let Some(issue) = error.downcast_ref::<FieldIssue>() {
                    self.reveal_field(issue.field);
                }
                self.model.reject_input(self.i18n.format(
                    "settings-invalid",
                    &[("error", format!("{error:#}").into())],
                ));
                cx.notify();
                return;
            }
        };
        let Some((patch, submission)) = self.begin_settings_apply(config, cx) else {
            return;
        };
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            let response = handle.apply_gui_settings(patch).await;
            let committed = handle.config_snapshot();
            let _ = this.update(cx, |this, cx| {
                let result = if response.is_success() {
                    Ok(())
                } else {
                    Err(response
                        .error
                        .unwrap_or_else(|| this.tr("settings-save-error").to_string()))
                };
                this.complete_settings_apply(submission, committed, result, cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn begin_settings_apply(
        &mut self,
        config: Config,
        cx: &App,
    ) -> Option<(ConfigPatch, SettingsSubmission)> {
        let (patch, model) = self.model.begin_apply(config)?;
        let submission = SettingsSubmission {
            model,
            layout_revision: self.layout_revision,
            inputs: self
                .inputs
                .iter()
                .map(|(&field, input)| {
                    (
                        field,
                        SubmittedInput {
                            text: input.read(cx).text().to_owned(),
                            revision: self.numeric_revision(field),
                        },
                    )
                })
                .collect(),
        };
        Some((patch, submission))
    }

    fn complete_settings_apply(
        &mut self,
        submission: SettingsSubmission,
        committed: Arc<Config>,
        result: std::result::Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        let newer_layout = (self.layout_revision != submission.layout_revision)
            .then_some(self.model.draft.spectrum_bar_layout);
        match self
            .model
            .complete_apply(submission.model, committed, result)
        {
            Completion::Stale => return,
            Completion::Failed => {}
            Completion::Committed => {
                if let Some(layout) = newer_layout {
                    self.model.draft.spectrum_bar_layout = layout;
                }
                // Preserve invalid/partial input and its caret, not just parsed edits.
                self.fill_unchanged(Some(&submission.inputs), cx);
                self.font_value = self.value(Field::Font, cx);
                if self.inputs.iter().any(|(field, input)| {
                    submission.inputs.get(field).is_some_and(|submitted| {
                        input.read(cx).text() != submitted.text
                            || self.numeric_revision(*field) != submitted.revision
                    })
                }) {
                    self.model.clear_success();
                }
            }
        }
        self.sync_library_stats(cx);
        cx.notify();
    }

    fn select_font(&mut self, value: String, cx: &mut Context<Self>) {
        // set_text emits Changed too; remember explicit writes so selecting or
        // resetting a value does not reopen the autocomplete popup.
        self.font_value = value.clone();
        self.model.draft.ui_font = value.clone();
        self.inputs[&Field::Font].update(cx, |input, cx| input.set_text(value, cx));
    }

    fn choose_setting(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(item) = self.dropdown.item(index) else {
            return;
        };
        match item.value {
            SettingChoice::Theme(value) => self.model.draft.ui_theme = value,
            SettingChoice::Language(index) => {
                self.model.draft.ui_language = ["auto", "en-US", "zh-CN"][index].to_owned()
            }
            SettingChoice::Device(device_index) => {
                if device_index == 0 {
                    self.model.draft.output_device = None;
                } else if let Some(name) = self.devices.get(device_index - 1) {
                    self.model.draft.output_device = Some(name.clone());
                } else {
                    return;
                }
            }
            SettingChoice::Font(index) => {
                let value = self.font_options[index].family.clone();
                self.select_font(value, cx);
            }
            SettingChoice::CustomFont => {
                let value = self.value(Field::Font, cx).trim().to_owned();
                self.select_font(value, cx);
            }
            SettingChoice::SpectrumStyle(value) => self.model.draft.spectrum_style = value,
            SettingChoice::SpectrumLayout(value) => {
                self.select_spectrum_layout(value, cx);
                return;
            }
            SettingChoice::DefaultVisualizerStyle(value) => {
                self.model.draft.default_visualizer_style = value;
            }
            SettingChoice::Palette(value) => self.model.draft.visualization_palette = value,
            SettingChoice::Fft(value) => self.model.draft.analysis_fft_size = value,
            SettingChoice::LogLevel(value) => self.model.draft.log_level = value,
            SettingChoice::Window(value) => self.model.draft.analysis_window = value,
            SettingChoice::Signal(value) => self.model.draft.visualization_signal = value,
            SettingChoice::AlbumArtworkStyle(value) => self.model.draft.album_artwork_style = value,
            SettingChoice::AlbumArtworkMode(value) => self.model.draft.album_artwork_mode = value,
            SettingChoice::AlbumArtworkDirection(value) => {
                self.model.draft.album_artwork_direction = value
            }
        }
        self.model.edited();
        self.dropdown.select_index(index);
        self.dropdown.close();
        self.sync_field_enabled(cx);
        cx.notify();
    }

    fn open_setting_dropdown(&mut self, items: Vec<ChoiceItem<SettingChoice>>, selected: usize) {
        if let Some(item) = items.first() {
            self.dropdown_anchor = match item.value {
                SettingChoice::Language(_) => Measured::Language,
                SettingChoice::Theme(_) => Measured::Theme,
                SettingChoice::Font(_) | SettingChoice::CustomFont => Measured::SettingsFont,
                SettingChoice::SpectrumStyle(_) => Measured::SettingsStyle,
                SettingChoice::SpectrumLayout(_) => Measured::SpectrumLayout,
                SettingChoice::Fft(_) => Measured::SettingsFft,
                SettingChoice::LogLevel(_) => Measured::SettingsLogLevel,
                SettingChoice::Window(_) => Measured::SettingsWindow,
                SettingChoice::Signal(_) => Measured::SettingsSignal,
                SettingChoice::DefaultVisualizerStyle(_) => Measured::DefaultVisualizerStyle,
                SettingChoice::Palette(_) => Measured::SettingsStyle,
                SettingChoice::Device(_) => Measured::Device,
                SettingChoice::AlbumArtworkStyle(_) => Measured::AlbumArtworkStyle,
                SettingChoice::AlbumArtworkMode(_) => Measured::AlbumArtworkMode,
                SettingChoice::AlbumArtworkDirection(_) => Measured::AlbumArtworkDirection,
            };
        }
        self.dropdown.open(items, selected);
    }

    fn choice_key(
        &mut self,
        event: &KeyDownEvent,
        mode: ChoiceMode,
        composing: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let disposition = self.dropdown.key(event, mode, composing, window);
        match disposition.apply(window, cx) {
            ChoiceAction::None => return,
            ChoiceAction::Open => self.open_font_dropdown(false, cx),
            ChoiceAction::Commit(index) => self.choose_setting(index, cx),
            ChoiceAction::Dismiss => self.dropdown.close(),
            ChoiceAction::Navigated => {}
        }
        cx.notify();
    }

    fn maintain_artwork_cache(&mut self, clean: bool, cx: &mut Context<Self>) {
        if self.artwork_cache_busy.is_some() {
            return;
        }
        let age = self.artwork_cache_age.read(cx).text().trim();
        let days = age
            .bytes()
            .all(|byte| byte.is_ascii_digit())
            .then(|| age.parse::<u64>().ok())
            .flatten()
            .filter(|days| *days > 0);
        let Some((days, seconds)) =
            days.and_then(|days| days.checked_mul(86_400).map(|seconds| (days, seconds)))
        else {
            self.artwork_cache_report = None;
            self.artwork_cache_error = Some(self.i18n.format(
                "settings-cache-days",
                &[("max", (u64::MAX / 86_400).to_string().into())],
            ));
            cx.notify();
            return;
        };
        self.artwork_cache_busy = Some((days, clean));
        self.artwork_cache_report = None;
        self.artwork_cache_error = None;
        let session = self.model.session();
        let age_revision = self.artwork_cache_age_revision;
        let manager = Arc::clone(&self.artwork_manager);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { manager.maintain(Duration::from_secs(seconds), clean) })
                .await;
            let _ = this.update(cx, |this, cx| {
                // Keep the busy guard across close/reopen, but never reuse an old session's feedback.
                this.artwork_cache_busy = None;
                if this.active
                    && this.model.session() == session
                    && this.artwork_cache_age_revision == age_revision
                {
                    match result {
                        Ok(report) => this.artwork_cache_report = Some((days, clean, report)),
                        Err(error) => {
                            this.artwork_cache_error = Some(this.i18n.format(
                                "settings-cache-error",
                                &[("error", error.to_string().into())],
                            ));
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn panel(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let colors = palette(cx);
        let panel_id = u64::MAX;
        let mut panel = column()
            .id(("settings-panel", panel_id))
            .relative()
            .w_full()
            .flex_1()
            .min_h_0()
            .gap_0();
        let (page_id, content) = match self.page {
            SettingsPage::Library => ("settings-library-scroll", self.library_settings(cx)),
            SettingsPage::Playback => ("settings-playback-scroll", self.playback_settings(cx)),
            SettingsPage::Interface if matches!(self.model.target(), Target::Artwork(_)) => {
                ("settings-artwork-scroll", self.artwork_settings(cx))
            }
            SettingsPage::Interface => (
                "settings-interface-scroll",
                column()
                    .gap_3()
                    .child(self.basic_settings(cx))
                    .child(settings_card(
                        colors,
                        self.tr("settings-album-artwork"),
                        self.artwork_settings(cx),
                    ))
                    .child(settings_card(
                        colors,
                        self.tr("settings-desktop-integration"),
                        self.desktop_settings(cx),
                    ))
                    .child(self.logging_settings(cx)),
            ),
            SettingsPage::Visualizations => {
                let id = self.visual_page.scroll_id();
                (id, self.visual_settings(cx))
            }
        };
        let scroll = self.scrolls.entry(page_id).or_default().clone();
        self.active_scroll = Some(scroll.as_ref().clone());
        let mut body = row().flex_1().min_h_0().items_start();
        if self.model.target() == Target::Global {
            let sidebar_scroll = self.scrolls.entry("settings-sidebar").or_default().clone();
            body = body.child(
                div()
                    .w(rems(11.))
                    .h_full()
                    .flex_shrink_0()
                    .child(rivu_ui::scrollable_area(
                        "settings-sidebar-surface",
                        div()
                            .id("settings-sidebar")
                            .flex()
                            .flex_col()
                            .w_full()
                            .h_full()
                            .child(self.sidebar(cx).w_full().flex_shrink_0()),
                        &sidebar_scroll,
                    )),
            );
        }
        panel = panel.child(
            body.child(rivu_ui::scrollable_area(
                page_id,
                div()
                    .id(page_id)
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .h_full()
                    .child(content.flex_shrink_0().p_2()),
                &scroll,
            )),
        );
        let feedback = if self.model.applying() {
            status_message(
                colors,
                self.tr("settings-applying-settings"),
                StatusKind::Info,
            )
            .into_any_element()
        } else {
            match self.model.feedback() {
                Some(Err(error)) => copyable_message(
                    "copy-settings-error",
                    self.tr("shell-copy-message"),
                    error.clone(),
                    cx,
                )
                .text_color(rgb(colors.error))
                .into_any_element(),
                Some(Ok(())) => {
                    status_message(colors, self.tr("settings-applied"), StatusKind::Success)
                        .into_any_element()
                }
                None => div().into_any_element(),
            }
        };
        panel = panel.child(
            row()
                .gap_1()
                .flex_shrink_0()
                .px_2()
                .py_2()
                .min_h_12()
                .border_t_1()
                .border_color(rgb(colors.border))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_sm()
                        .child(feedback),
                )
                .child(
                    IconButtonGroup::new(IconButtonStyle::Surface, IconButtonDensity::Regular)
                        .button(
                            IconButton::new(
                                "settings-apply",
                                "",
                                self.tr("settings-apply-settings"),
                            )
                            .enabled(!self.model.applying()),
                            cx,
                            |this, _, cx| this.apply_settings(cx),
                        )
                        .button(
                            IconButton::new(
                                "settings-discard",
                                "",
                                self.tr("settings-discard-changes-and-close"),
                            )
                            .enabled(!self.model.applying()),
                            cx,
                            |this, _, cx| this.cancel(cx),
                        )
                        .into_div(),
                ),
        );
        if self.dropdown.is_open() {
            let bounds = self.measured.borrow().get(&self.dropdown_anchor).copied();
            if let Some(bounds) = bounds {
                panel = panel.child(
                    ChoicePopup::new("settings-dropdown", "settings-dropdown-scrollbar", bounds)
                        .render(
                            self,
                            |this| &this.dropdown,
                            |this, index, _, cx| this.choose_setting(index, cx),
                            |this, event, _, _| {
                                if this.dropdown_anchor == Measured::SettingsFont
                                    && this
                                        .measured
                                        .borrow()
                                        .get(&Measured::SettingsFont)
                                        .is_some_and(|bounds| bounds.contains(&event.position))
                                {
                                    return false;
                                }
                                this.dropdown.close();
                                true
                            },
                            window,
                            cx,
                        ),
                );
            }
        }
        panel.into_any_element()
    }
}

impl Render for Settings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = palette(cx);
        let panel = self.panel(window, cx);
        self.scope.surface(
            Dialog::backdrop("settings-modal")
                .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    if !this.dropdown.is_open() {
                        return;
                    }
                    if this.settings_font_input_focused(window, cx) {
                        return;
                    }
                    this.choice_key(event, ChoiceMode::Select, false, window, cx);
                }))
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    let key = event.keystroke.key.as_str();
                    let modifiers = event.keystroke.modifiers;
                    if matches!(key, "pageup" | "pagedown")
                        && !modifiers.control
                        && !modifiers.platform
                        && !modifiers.alt
                        && !modifiers.shift
                        && !modifiers.function
                        && this.scope.handle().contains_focused(window, cx)
                    {
                        if let Some(scroll) = &this.active_scroll {
                            let mut offset = scroll.offset();
                            let step = scroll.bounds().size.height.max(px(0.));
                            offset.y = (if key == "pagedown" {
                                offset.y - step
                            } else {
                                offset.y + step
                            })
                            .max(-scroll.max_offset().y.max(px(0.)))
                            .min(px(0.));
                            scroll.set_offset(offset);
                            cx.notify();
                        }
                        cx.stop_propagation();
                    } else if key == "escape" && !modifiers.modified() {
                        if this
                            .inputs
                            .values()
                            .any(|input| input.read(cx).is_focused(window))
                        {
                            this.scope.handle().focus(window, cx);
                        } else {
                            this.cancel(cx);
                        }
                        cx.stop_propagation();
                    }
                }))
                .child(
                    Dialog::card(colors)
                        .max_w(rems(860. / 16.))
                        .w(relative(0.9))
                        .h((window.viewport_size().height - px(32.))
                            .max(px(0.))
                            .min(window.rem_size() * (640. / 16.)))
                        .gap_0()
                        .overflow_hidden()
                        .child(
                            row().h_10().flex_shrink_0().px_2().child(
                                div().flex_1().text_lg().child(self.tr("settings-settings")),
                            ),
                        )
                        .child(panel),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::CoreRuntime;
    use rivu_protocol::model::Command;
    // gpui::* also imports its test macro; expansion needs Rust's builtin attribute.
    use core::prelude::v1::test;

    fn fixture(cx: &mut TestAppContext) -> (tempfile::TempDir, CoreRuntime, Entity<Settings>) {
        cx.update(|cx| {
            rivu_ui::theme::set_palette(crate::theme::palette(Theme::default()), cx);
            rivu_ui::theme::set_typography(
                gpui::font("sans-serif"),
                crate::gui::appearance::fonts::SYMBOLS_FAMILY,
                12,
                cx,
            );
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        Config {
            volume: 0.0,
            ffmpeg_enabled: false,
            mpris_enabled: false,
            ..Config::default()
        }
        .save(&path)
        .unwrap();
        let runtime = CoreRuntime::start(directory.path(), &path).unwrap();
        let config = runtime.handle.config_snapshot();
        let settings = cx.new(|cx| {
            Settings::new(
                Arc::new(Localizer::new("en-US")),
                runtime.handle.clone(),
                Arc::new(ArtworkManager::new(directory.path())),
                Arc::new(Vec::new()),
                ("disabled".into(), "disabled".into()),
                1.0,
                cx,
            )
        });
        settings.update(cx, |this, cx| {
            this.initialize_settings(cx);
            this.reset(&config, cx);
            this.active = true;
        });
        (directory, runtime, settings)
    }

    fn submit(
        settings: &Entity<Settings>,
        cx: &mut TestAppContext,
    ) -> (ConfigPatch, SettingsSubmission) {
        settings.update(cx, |this, cx| {
            this.begin_settings_apply(this.parse(cx).unwrap(), cx)
                .unwrap()
        })
    }

    fn execute(
        runtime: &CoreRuntime,
        patch: ConfigPatch,
    ) -> (Arc<Config>, std::result::Result<(), String>) {
        let response = futures::executor::block_on(runtime.handle.apply_gui_settings(patch));
        let result = if response.is_success() {
            Ok(())
        } else {
            Err(response.error.unwrap())
        };
        (runtime.handle.config_snapshot(), result)
    }

    struct SettingsWindow(Entity<Settings>);

    impl Render for SettingsWindow {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.0.clone())
        }
    }

    #[gpui::test]
    fn expanded_sidebar_scrolls_and_clamps_after_collapsing(cx: &mut TestAppContext) {
        let (_directory, _runtime, settings) = fixture(cx);
        settings.update(cx, |this, _| this.page = SettingsPage::Visualizations);
        let window = cx.add_window({
            let settings = settings.clone();
            move |_, _| SettingsWindow(settings)
        });
        cx.update_window(window.into(), |_, window, cx| {
            window.set_rem_size(px(40.));
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let scroll = settings.read_with(cx, |this, _| this.scrolls["settings-sidebar"].clone());
        assert!(scroll.max_offset().y > px(0.));
        scroll.set_offset(gpui::point(px(0.), -scroll.max_offset().y));
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .unwrap();
        assert_eq!(scroll.offset().y, -scroll.max_offset().y);
        settings.update(cx, |this, cx| {
            this.page = SettingsPage::Library;
            cx.notify();
        });
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .unwrap();
        assert_eq!(scroll.max_offset().y, px(0.));
        assert_eq!(scroll.offset().y, px(0.));
    }

    #[gpui::test]
    fn dropdown_ignores_modified_keys_and_escape_closes_only_candidates(cx: &mut TestAppContext) {
        use gpui::InputEvent as _;

        let (_directory, _runtime, settings) = fixture(cx);
        let window = cx.add_window({
            let settings = settings.clone();
            move |_, _| SettingsWindow(settings)
        });
        let handle = window.into();
        cx.update_window(handle, |_, window, cx| {
            settings.update(cx, |this, cx| {
                this.scope.handle().focus(window, cx);
                this.model.draft.album_artwork_mode = AlbumArtworkMode::Static;
                this.open_setting_dropdown(
                    [AlbumArtworkMode::Static, AlbumArtworkMode::Rotating]
                        .into_iter()
                        .map(|value| {
                            let choice = SettingChoice::AlbumArtworkMode(value);
                            ChoiceItem::new(choice, choice.label(&this.i18n))
                        })
                        .collect(),
                    0,
                );
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        for key in [
            "ctrl-down",
            "alt-down",
            "shift-down",
            "ctrl-enter",
            "shift-enter",
            "alt-escape",
        ] {
            cx.update_window(handle, |_, window, cx| {
                let keystroke = Keystroke::parse(key).unwrap();
                window.dispatch_event(
                    KeyDownEvent {
                        keystroke: keystroke.clone(),
                        is_held: false,
                        prefer_character_input: false,
                    }
                    .to_platform_input(),
                    cx,
                );
                window.dispatch_event(KeyUpEvent { keystroke }.to_platform_input(), cx);
            })
            .unwrap();
        }
        settings.update(cx, |this, _| {
            assert!(this.dropdown.is_open());
            assert_eq!(this.dropdown.selected_index(), Some(0));
            assert_eq!(
                this.model.draft.album_artwork_mode,
                AlbumArtworkMode::Static
            );
            assert!(this.active);
        });
        cx.update_window(handle, |_, window, cx| {
            window.dispatch_event(
                KeyDownEvent {
                    keystroke: Keystroke::parse("escape").unwrap(),
                    is_held: false,
                    prefer_character_input: false,
                }
                .to_platform_input(),
                cx,
            );
        })
        .unwrap();
        settings.update(cx, |this, _| {
            assert!(!this.dropdown.is_open());
            assert!(this.active);
        });
    }

    #[gpui::test]
    fn disabling_artwork_period_retains_editor_draft_and_reenables_editing(
        cx: &mut TestAppContext,
    ) {
        let (_directory, _runtime, settings) = fixture(cx);
        let window = cx.add_window({
            let settings = settings.clone();
            move |_, _| SettingsWindow(settings)
        });
        cx.update_window(window.into(), |_, window, cx| {
            settings.update(cx, |this, cx| {
                this.set_value(Field::AlbumArtworkPeriodSeconds, "1e", None, cx);
                let input = this.inputs[&Field::AlbumArtworkPeriodSeconds].clone();
                this.open_setting_dropdown(
                    vec![ChoiceItem::new(
                        SettingChoice::AlbumArtworkMode(AlbumArtworkMode::Static),
                        "Static",
                    )],
                    0,
                );
                this.choose_setting(0, cx);
                input.update(cx, |input, cx| {
                    input.replace_text_in_range(Some(0..2), "99", window, cx);
                });
                assert_eq!(this.value(Field::AlbumArtworkPeriodSeconds, cx), "1e");
                assert_eq!(this.inputs[&Field::AlbumArtworkPeriodSeconds], input);
                this.open_setting_dropdown(
                    vec![ChoiceItem::new(
                        SettingChoice::AlbumArtworkMode(AlbumArtworkMode::Rotating),
                        "Rotating",
                    )],
                    0,
                );
                this.choose_setting(0, cx);
                assert_eq!(this.value(Field::AlbumArtworkPeriodSeconds, cx), "1e");
                input.update(cx, |input, cx| {
                    input.replace_text_in_range(Some(0..2), "12", window, cx);
                });
                assert_eq!(this.value(Field::AlbumArtworkPeriodSeconds, cx), "12");
            });
        })
        .unwrap();
    }

    #[gpui::test]
    fn disabled_retained_fields_still_validate_global_apply(cx: &mut TestAppContext) {
        let (_directory, _runtime, settings) = fixture(cx);
        settings.update(cx, |this, cx| {
            this.model.draft.album_artwork_mode = AlbumArtworkMode::Static;
            this.model.draft.spectrum_style = SpectrumStyle::Solid;
            this.model.draft.spectrum_peaks = false;
            this.sync_field_enabled(cx);
            for field in [
                Field::AlbumArtworkPeriodSeconds,
                Field::SpectrumAutoWidth,
                Field::SpectrumGap,
                Field::SpectrumHold,
                Field::SpectrumGravity,
            ] {
                assert!(!field.enabled(&this.model.draft));
                let retained = this.value(field, cx);
                this.set_value(field, "invalid", None, cx);
                assert!(this.parse(cx).is_err());
                assert_eq!(this.value(field, cx), "invalid");
                this.set_value(field, retained, None, cx);
            }
            assert!(this.parse(cx).is_ok());
        });
    }

    fn precise_config(base: &Config) -> Config {
        Config {
            spectrum_style: SpectrumStyle::Bars,
            volume: 0.1234567,
            play_count_threshold_percent: 23.456789123456,
            ui_scale: 1.234567,
            visualization_db_range: 81.23456,
            reactive_sensitivity: 1.234567,
            radial_rotation_speed: 0.456789,
            radial_bar_width: 0.678912,
            radial_ring_opacity: 0.789123,
            reactive_bloom_intensity: 0.456789,
            radial_inner_diameter: 0.789123,
            reactive_db_range: Some(82.34567),
            spectrum_db_range: Some(83.45678),
            spectrum_gap: 2.345678,
            spectrum_peak_gravity: 81.23456,
            spectrum_bar_gravity: 41.23456,
            spectrum_bar_layout: SpectrumBarLayout::Automatic { width: 4.567891 },
            spectrogram_db_range: Some(84.56789),
            waveform_glow: 0.456789,
            waveform_rms_gain: 1.234567,
            waveform_peak_gain: 1.234567,
            waveform_peak_gamma: 1.234567,
            album_artwork_period_seconds: 23.45678,
            ..base.clone()
        }
    }

    fn numeric_bits(config: &Config) -> Vec<u64> {
        vec![
            u64::from(config.volume.to_bits()),
            config.play_count_threshold_percent.to_bits(),
            u64::from(config.ui_scale.to_bits()),
            u64::from(config.visualization_db_range.to_bits()),
            u64::from(config.reactive_sensitivity.to_bits()),
            u64::from(config.radial_rotation_speed.to_bits()),
            u64::from(config.radial_bar_width.to_bits()),
            u64::from(config.radial_ring_opacity.to_bits()),
            u64::from(config.reactive_bloom_intensity.to_bits()),
            u64::from(config.radial_inner_diameter.to_bits()),
            u64::from(config.reactive_db_range.map_or(0, f32::to_bits)),
            u64::from(config.spectrum_db_range.map_or(0, f32::to_bits)),
            u64::from(config.spectrum_gap.to_bits()),
            u64::from(config.spectrum_peak_gravity.to_bits()),
            u64::from(config.spectrum_bar_gravity.to_bits()),
            u64::from(match config.spectrum_bar_layout {
                SpectrumBarLayout::Automatic { width } => width.to_bits(),
                SpectrumBarLayout::Fixed { .. } => panic!("precision fixture uses automatic width"),
            }),
            u64::from(config.spectrogram_db_range.map_or(0, f32::to_bits)),
            u64::from(config.waveform_glow.to_bits()),
            u64::from(config.waveform_rms_gain.to_bits()),
            u64::from(config.waveform_peak_gain.to_bits()),
            u64::from(config.waveform_peak_gamma.to_bits()),
            u64::from(config.album_artwork_period_seconds.to_bits()),
        ]
    }

    fn numeric_texts(config: &Config) -> Vec<(Field, String)> {
        vec![
            (Field::Volume, (config.volume * 100.).to_string()),
            (
                Field::PlayCountThreshold,
                config.play_count_threshold_percent.to_string(),
            ),
            (Field::Scale, config.ui_scale.to_string()),
            (
                Field::VisualizationDbRange,
                config.visualization_db_range.to_string(),
            ),
            (
                Field::ReactiveSensitivity,
                config.reactive_sensitivity.to_string(),
            ),
            (
                Field::RadialRotationSpeed,
                config.radial_rotation_speed.to_string(),
            ),
            (Field::RadialBarWidth, config.radial_bar_width.to_string()),
            (
                Field::RadialRingOpacity,
                config.radial_ring_opacity.to_string(),
            ),
            (
                Field::ReactiveBloomIntensity,
                config.reactive_bloom_intensity.to_string(),
            ),
            (
                Field::RadialInnerDiameter,
                config.radial_inner_diameter.to_string(),
            ),
            (
                Field::ReactiveDbRange,
                config.reactive_db_range.unwrap().to_string(),
            ),
            (
                Field::SpectrumDbRange,
                config.spectrum_db_range.unwrap().to_string(),
            ),
            (Field::SpectrumGap, config.spectrum_gap.to_string()),
            (
                Field::SpectrumGravity,
                config.spectrum_peak_gravity.to_string(),
            ),
            (
                Field::SpectrumBarGravity,
                config.spectrum_bar_gravity.to_string(),
            ),
            (
                Field::SpectrumAutoWidth,
                match config.spectrum_bar_layout {
                    SpectrumBarLayout::Automatic { width } => width.to_string(),
                    SpectrumBarLayout::Fixed { .. } => {
                        panic!("precision fixture uses automatic width")
                    }
                },
            ),
            (
                Field::SpectrogramDbRange,
                config.spectrogram_db_range.unwrap().to_string(),
            ),
            (Field::WaveformGlow, config.waveform_glow.to_string()),
            (Field::WaveformRmsGain, config.waveform_rms_gain.to_string()),
            (
                Field::WaveformPeakGain,
                config.waveform_peak_gain.to_string(),
            ),
            (
                Field::WaveformPeakGamma,
                config.waveform_peak_gamma.to_string(),
            ),
            (
                Field::AlbumArtworkPeriodSeconds,
                config.album_artwork_period_seconds.to_string(),
            ),
        ]
    }

    fn edit_input(
        settings: &Entity<Settings>,
        window: WindowHandle<SettingsWindow>,
        field: Field,
        text: &str,
        cx: &mut TestAppContext,
    ) {
        cx.update_window(window.into(), |_, window, cx| {
            let input = settings.read(cx).inputs[&field].clone();
            input.update(cx, |input, cx| {
                let end = input.text().encode_utf16().count();
                input.replace_text_in_range(Some(0..end), text, window, cx);
            });
        })
        .unwrap();
    }

    #[gpui::test]
    fn rounded_float_displays_preserve_every_original_bit_across_apply_and_unrelated_edits(
        cx: &mut TestAppContext,
    ) {
        let (_directory, runtime, settings) = fixture(cx);
        let window = cx.add_window({
            let settings = settings.clone();
            move |_, _| SettingsWindow(settings)
        });
        let original = runtime.handle.config_snapshot();
        let precise = precise_config(&original);
        let (committed, result) = execute(&runtime, ConfigPatch::between(&original, &precise));
        assert!(result.is_ok());
        settings.update(cx, |this, cx| {
            this.reset(&committed, cx);
            assert_eq!(this.value(Field::Volume, cx), "12");
            assert_eq!(this.value(Field::PlayCountThreshold, cx), "23.5");
            assert_eq!(this.value(Field::Scale, cx), "1.23");
            assert_eq!(this.value(Field::SpectrumDbRange, cx), "83.5");
            assert_eq!(this.value(Field::SpectrumGravity, cx), "81.2");
            assert_eq!(
                numeric_bits(&this.parse(cx).unwrap()),
                numeric_bits(&precise)
            );
        });
        for unrelated_edit in [false, true] {
            if unrelated_edit {
                edit_input(&settings, window, Field::ScanMaxDepth, "7", cx);
            }
            let (patch, submission) = submit(&settings, cx);
            let (committed, result) = execute(&runtime, patch);
            assert!(result.is_ok());
            assert_eq!(numeric_bits(&committed), numeric_bits(&precise));
            settings.update(cx, |this, cx| {
                this.complete_settings_apply(submission, committed, result, cx);
                assert_eq!(
                    numeric_bits(&this.parse(cx).unwrap()),
                    numeric_bits(&precise)
                );
            });
        }
    }

    #[gpui::test]
    fn genuine_float_edits_accept_high_precision_without_formatting_while_typing(
        cx: &mut TestAppContext,
    ) {
        let (_directory, runtime, settings) = fixture(cx);
        let window = cx.add_window({
            let settings = settings.clone();
            move |_, _| SettingsWindow(settings)
        });
        let mut expected = precise_config(&runtime.handle.config_snapshot());
        settings.update(cx, |this, cx| {
            this.model.draft.spectrum_style = SpectrumStyle::Bars;
            this.sync_field_enabled(cx);
        });
        for (field, text) in numeric_texts(&expected) {
            edit_input(&settings, window, field, &text, cx);
            settings.update(cx, |this, cx| {
                assert_eq!(this.value(field, cx), text);
            });
        }
        // Volume's explicit percentage is converted exactly as entered, not
        // substituted with the original fractional config value.
        expected.volume = (expected.volume * 100.) / 100.;
        settings.update(cx, |this, cx| {
            assert_eq!(
                numeric_bits(&this.parse(cx).unwrap()),
                numeric_bits(&expected)
            );
        });
        let (patch, submission) = submit(&settings, cx);
        let (committed, result) = execute(&runtime, patch);
        assert!(result.is_ok());
        assert_eq!(numeric_bits(&committed), numeric_bits(&expected));
        settings.update(cx, |this, cx| {
            this.complete_settings_apply(submission, committed, result, cx);
            assert_eq!(
                numeric_bits(&this.parse(cx).unwrap()),
                numeric_bits(&expected)
            );
            assert_eq!(this.value(Field::Scale, cx), "1.23");
        });
    }

    #[gpui::test]
    fn retyping_rounded_values_is_explicit_and_optional_clear_stays_inherited(
        cx: &mut TestAppContext,
    ) {
        let (_directory, runtime, settings) = fixture(cx);
        let window = cx.add_window({
            let settings = settings.clone();
            move |_, _| SettingsWindow(settings)
        });
        let original = runtime.handle.config_snapshot();
        let precise = Arc::new(precise_config(&original));
        let (committed, result) = execute(&runtime, ConfigPatch::between(&original, &precise));
        assert!(result.is_ok());
        settings.update(cx, |this, cx| this.reset(&committed, cx));
        edit_input(&settings, window, Field::Scale, "1.23", cx);
        edit_input(&settings, window, Field::Volume, "12", cx);
        for field in [
            Field::ReactiveDbRange,
            Field::SpectrumDbRange,
            Field::SpectrogramDbRange,
        ] {
            edit_input(&settings, window, field, "", cx);
        }
        settings.update(cx, |this, cx| {
            let parsed = this.parse(cx).unwrap();
            assert_eq!(parsed.ui_scale.to_bits(), 1.23_f32.to_bits());
            assert_eq!(parsed.volume.to_bits(), 0.12_f32.to_bits());
            assert_eq!(parsed.reactive_db_range, None);
            assert_eq!(parsed.spectrum_db_range, None);
            assert_eq!(parsed.spectrogram_db_range, None);
        });
        let (patch, submission) = submit(&settings, cx);
        let (committed, result) = execute(&runtime, patch);
        assert!(result.is_ok());
        assert_eq!(committed.reactive_db_range, None);
        assert_eq!(committed.spectrum_db_range, None);
        assert_eq!(committed.spectrogram_db_range, None);
        settings.update(cx, |this, cx| {
            this.complete_settings_apply(submission, committed, result, cx);
            assert!(this.value(Field::SpectrumDbRange, cx).is_empty());
            assert_eq!(this.parse(cx).unwrap().spectrum_db_range, None);
            // Programmatic fills are not edits, even after earlier user input.
            this.reset(&precise, cx);
        });
        settings.update(cx, |this, cx| {
            assert_eq!(
                numeric_bits(&this.parse(cx).unwrap()),
                numeric_bits(&precise)
            );
        });
    }

    #[gpui::test]
    fn numeric_edits_during_apply_survive_completion_including_same_text_replacement(
        cx: &mut TestAppContext,
    ) {
        let (_directory, runtime, settings) = fixture(cx);
        let window = cx.add_window({
            let settings = settings.clone();
            move |_, _| SettingsWindow(settings)
        });
        let original = runtime.handle.config_snapshot();
        let precise = precise_config(&original);
        let (committed, result) = execute(&runtime, ConfigPatch::between(&original, &precise));
        assert!(result.is_ok());
        settings.update(cx, |this, cx| this.reset(&committed, cx));
        let (patch, submission) = submit(&settings, cx);
        let (committed, result) = execute(&runtime, patch);
        assert!(result.is_ok());
        edit_input(&settings, window, Field::Scale, "1.23", cx);
        edit_input(&settings, window, Field::WaveformPeakGamma, "1e", cx);
        edit_input(&settings, window, Field::SpectrumDbRange, "", cx);
        settings.update(cx, |this, cx| {
            this.complete_settings_apply(submission, committed, result, cx);
            assert_eq!(this.value(Field::Scale, cx), "1.23");
            assert_eq!(this.value(Field::WaveformPeakGamma, cx), "1e");
            assert!(this.value(Field::SpectrumDbRange, cx).is_empty());
            assert!(this.parse(cx).is_err());
        });
        edit_input(&settings, window, Field::WaveformPeakGamma, "1.876543", cx);
        let (patch, submission) = submit(&settings, cx);
        let (committed, result) = execute(&runtime, patch);
        assert!(result.is_ok());
        assert_eq!(committed.ui_scale.to_bits(), 1.23_f32.to_bits());
        assert_eq!(
            committed.waveform_peak_gamma.to_bits(),
            1.876543_f32.to_bits()
        );
        assert_eq!(committed.spectrum_db_range, None);
        settings.update(cx, |this, cx| {
            this.complete_settings_apply(submission, committed, result, cx);
            assert_eq!(this.parse(cx).unwrap(), *this.model.baseline());
        });
    }

    #[gpui::test]
    fn numeric_display_trims_zeroes_and_negative_zero_without_changing_the_draft(
        cx: &mut TestAppContext,
    ) {
        let (_directory, _runtime, settings) = fixture(cx);
        settings.update(cx, |this, cx| {
            this.model.draft.radial_rotation_speed = -0.0;
            this.model.draft.reactive_sensitivity = 1.5;
            this.model.draft.visualization_db_range = 80.0;
            this.fill(cx);
            assert_eq!(this.value(Field::RadialRotationSpeed, cx), "0");
            assert_eq!(this.value(Field::ReactiveSensitivity, cx), "1.5");
            assert_eq!(this.value(Field::VisualizationDbRange, cx), "80");
            assert_eq!(
                this.parse(cx).unwrap().radial_rotation_speed.to_bits(),
                (-0.0_f32).to_bits()
            );
        });
    }

    #[gpui::test]
    fn artwork_panel_apply_preserves_untouched_period_and_accepts_explicit_rounded_value(
        cx: &mut TestAppContext,
    ) {
        let (_directory, runtime, settings) = fixture(cx);
        let window = cx.add_window({
            let settings = settings.clone();
            move |_, _| SettingsWindow(settings)
        });
        let periods = Rc::new(RefCell::new(Vec::new()));
        let _subscription = cx.update(|cx| {
            let periods = periods.clone();
            cx.subscribe(&settings, move |_, event, _| {
                if let SettingsEvent::ApplyArtwork { options, .. } = event {
                    periods.borrow_mut().push(options.period_seconds.to_bits());
                }
            })
        });
        let mut options = AlbumArtworkOptions::from(runtime.handle.config_snapshot().as_ref());
        options.mode = AlbumArtworkMode::Rotating;
        options.period_seconds = 23.45678;
        cx.update_window(window.into(), |_, window, cx| {
            settings.update(cx, |this, cx| this.open_artwork(42, options, window, cx));
        })
        .unwrap();
        settings.update(cx, |this, cx| this.apply_settings(cx));
        assert_eq!(*periods.borrow(), vec![options.period_seconds.to_bits()]);
        settings.update(cx, |this, cx| {
            this.complete_panel_apply(this.model.session(), Ok(()), cx);
        });
        edit_input(
            &settings,
            window,
            Field::AlbumArtworkPeriodSeconds,
            "23.46",
            cx,
        );
        settings.update(cx, |this, cx| this.apply_settings(cx));
        assert_eq!(periods.borrow()[1], 23.46_f32.to_bits());
    }

    #[gpui::test]
    fn unchanged_volume_text_preserves_exact_config_precision(cx: &mut TestAppContext) {
        let (_directory, runtime, settings) = fixture(cx);
        settings.update(cx, |this, cx| {
            let mut config = runtime.handle.config_snapshot().as_ref().clone();
            for volume in [0.01_f32, 0.07, 0.1234567, 0.33333334, 0.9876543] {
                config.volume = volume;
                this.reset(&Arc::new(config.clone()), cx);
                assert_eq!(this.parse(cx).unwrap().volume.to_bits(), volume.to_bits());
            }
        });
    }

    #[gpui::test]
    fn successful_settings_apply_rebases_untouched_controls_and_keeps_later_edits(
        cx: &mut TestAppContext,
    ) {
        let (_directory, runtime, settings) = fixture(cx);
        let window = cx.add_window({
            let settings = settings.clone();
            move |_, _| SettingsWindow(settings)
        });
        settings.update(cx, |this, cx| {
            this.model.draft.ui_theme = Theme::TokyoNightDay;
            this.set_value(Field::ScanMaxDepth, "7", None, cx);
        });
        let (patch, submission) = submit(&settings, cx);
        assert!(
            futures::executor::block_on(runtime.handle.request_ack(Command::Volume { value: 0.2 }))
                .is_success()
        );
        let (committed, result) = execute(&runtime, patch);
        assert!(result.is_ok());
        // Hold the completion while a user leaves invalid text and changes a switch.
        cx.update_window(window.into(), |_, window, cx| {
            settings.update(cx, |this, cx| {
                this.set_value(Field::ScanMaxDepth, "1e", None, cx);
                let input = this.inputs[&Field::ScanMaxDepth].clone();
                input.update(cx, |input, cx| {
                    input.replace_text_in_range(Some(0..1), "1", window, cx);
                });
                this.model.draft.visualization_enabled = false;
                this.model.draft.ui_theme = Theme::TokyoNight;
                this.complete_settings_apply(submission, committed, result, cx);
                assert_eq!(this.model.baseline().scan_max_depth, 7);
                assert_eq!(this.model.baseline().volume, 0.2);
                assert_eq!(this.model.baseline().ui_theme, Theme::TokyoNightDay);
                assert_eq!(this.model.draft.ui_theme, Theme::TokyoNight);
                assert_eq!(this.value(Field::Volume, cx), "20");
                assert_eq!(this.value(Field::ScanMaxDepth, cx), "1e");
                input.update(cx, |input, cx| {
                    assert_eq!(
                        input.selected_text_range(false, window, cx).unwrap().range,
                        1..1
                    );
                });
                assert!(!this.model.draft.visualization_enabled);
                assert!(this.parse(cx).is_err());
                this.set_value(Field::ScanMaxDepth, "9", None, cx);
            });
        })
        .unwrap();
        let (patch, submission) = submit(&settings, cx);
        assert!(
            futures::executor::block_on(runtime.handle.request_ack(Command::Volume { value: 0.3 }))
                .is_success()
        );
        let (committed, result) = execute(&runtime, patch);
        assert!(result.is_ok());
        settings.update(cx, |this, cx| {
            this.complete_settings_apply(submission, committed, result, cx);
            let visible = this.parse(cx).unwrap();
            assert_eq!(visible.scan_max_depth, 9);
            assert_eq!(visible.volume, 0.3);
            assert!(!visible.visualization_enabled);
            assert_eq!(visible.ui_theme, Theme::TokyoNight);
            assert_eq!(visible, *this.model.baseline());
        });
    }

    #[gpui::test]
    fn root_paths_preserve_semicolons_and_trailing_spaces_across_apply(cx: &mut TestAppContext) {
        let (directory, runtime, settings) = fixture(cx);
        let roots = vec![
            directory.path().join("music;live"),
            directory.path().join("music "),
        ];
        for root in &roots {
            std::fs::create_dir(root).unwrap();
        }
        settings.update(cx, |this, cx| {
            this.model.draft.library_roots = roots.clone();
            assert_eq!(this.parse(cx).unwrap().library_roots, roots);
        });
        let (patch, submission) = submit(&settings, cx);
        let (committed, result) = execute(&runtime, patch);
        assert!(result.is_ok());
        assert_eq!(committed.library_roots, roots);
        settings.update(cx, |this, cx| {
            this.complete_settings_apply(submission, committed, result, cx);
            assert_eq!(this.parse(cx).unwrap().library_roots, roots);
        });
        assert_eq!(
            Config::load(&directory.path().join("config.toml"))
                .unwrap()
                .library_roots,
            roots,
        );
    }

    #[gpui::test]
    fn unedited_roots_rebase_external_changes_and_keep_pending_list_edits(cx: &mut TestAppContext) {
        let (directory, runtime, settings) = fixture(cx);
        let external = directory.path().join("external; ");
        let later = directory.path().join("later ");
        std::fs::create_dir(&external).unwrap();
        std::fs::create_dir(&later).unwrap();
        settings.update(cx, |this, cx| {
            this.set_value(Field::ScanMaxDepth, "7", None, cx);
        });
        let (patch, submission) = submit(&settings, cx);
        runtime
            .handle
            .update_gui_config(crate::core::GuiConfigChange::AddLibraryRoot(
                external.clone(),
            ))
            .unwrap();
        let (committed, result) = execute(&runtime, patch);
        assert!(result.is_ok());
        assert_eq!(committed.library_roots, vec![external.clone()]);
        settings.update(cx, |this, cx| {
            this.complete_settings_apply(submission, committed, result, cx);
            assert_eq!(
                this.parse(cx).unwrap().library_roots,
                vec![external.clone()]
            );
        });
        let (patch, submission) = submit(&settings, cx);
        let (committed, result) = execute(&runtime, patch);
        assert!(result.is_ok());
        settings.update(cx, |this, cx| {
            this.model.draft.library_roots.clear();
            this.model.draft.library_roots.push(later.clone());
            this.complete_settings_apply(submission, committed, result, cx);
            assert_eq!(this.model.baseline().library_roots, vec![external]);
            assert_eq!(this.parse(cx).unwrap().library_roots, vec![later]);
        });
    }

    #[gpui::test]
    fn failed_settings_apply_retains_edits_and_baseline_for_retry(cx: &mut TestAppContext) {
        let (directory, runtime, settings) = fixture(cx);
        let window = cx.add_window({
            let settings = settings.clone();
            move |_, _| SettingsWindow(settings)
        });
        let baseline_theme = runtime.handle.config_snapshot().ui_theme;
        let path = directory.path().join("config.toml");
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        settings.update(cx, |this, cx| {
            this.model.draft.ui_theme = Theme::TokyoNightDay;
            this.set_value(Field::ScanMaxDepth, "7", None, cx);
        });
        edit_input(&settings, window, Field::Scale, "1.345678", cx);
        let (patch, submission) = submit(&settings, cx);
        let (committed, result) = execute(&runtime, patch);
        assert!(result.is_err());
        edit_input(&settings, window, Field::Scale, "1.456789", cx);
        settings.update(cx, |this, cx| {
            this.set_value(Field::ScanMaxDepth, "9", None, cx);
            this.model.draft.visualization_enabled = false;
            this.complete_settings_apply(submission, committed, result, cx);
            assert_eq!(
                this.model.baseline().scan_max_depth,
                crate::config::DEFAULT_SCAN_MAX_DEPTH
            );
            assert_eq!(this.value(Field::ScanMaxDepth, cx), "9");
            assert!(!this.model.draft.visualization_enabled);
            assert_eq!(this.model.baseline().ui_theme, baseline_theme);
            assert_eq!(this.model.draft.ui_theme, Theme::TokyoNightDay);
            assert!(!this.model.applying());
            assert!(matches!(this.model.feedback(), Some(Err(_))));
            assert_eq!(
                this.parse(cx).unwrap().ui_scale.to_bits(),
                1.456789_f32.to_bits()
            );
        });
        std::fs::remove_dir(&path).unwrap();
        assert!(
            futures::executor::block_on(runtime.handle.request_ack(Command::Volume { value: 0.2 }))
                .is_success()
        );
        let (patch, submission) = submit(&settings, cx);
        let (committed, result) = execute(&runtime, patch);
        assert!(result.is_ok());
        settings.update(cx, |this, cx| {
            this.complete_settings_apply(submission, committed, result, cx);
            assert_eq!(this.model.baseline().scan_max_depth, 9);
            assert_eq!(
                this.model.baseline().ui_scale.to_bits(),
                1.456789_f32.to_bits()
            );
            assert_eq!(this.model.baseline().volume, 0.2);
            assert!(!this.model.baseline().visualization_enabled);
            assert_eq!(this.model.baseline().ui_theme, Theme::TokyoNightDay);
            assert_eq!(this.parse(cx).unwrap(), *this.model.baseline());
        });
    }

    #[gpui::test]
    fn pending_global_apply_blocks_cancel_until_completion(cx: &mut TestAppContext) {
        let (_directory, runtime, settings) = fixture(cx);
        settings.update(cx, |this, cx| {
            this.set_value(Field::ScanMaxDepth, "7", None, cx);
        });
        let (patch, submission) = submit(&settings, cx);
        settings.update(cx, |this, cx| {
            let session = this.model.session();
            this.set_value(Field::ScanMaxDepth, "9", None, cx);
            this.cancel(cx);
            assert!(this.active);
            assert!(this.model.applying());
            assert_eq!(this.model.session(), session);
            assert_eq!(this.value(Field::ScanMaxDepth, cx), "9");
        });
        let (committed, result) = execute(&runtime, patch);
        assert!(result.is_ok());
        settings.update(cx, |this, cx| {
            this.complete_settings_apply(submission, committed, result, cx);
            assert_eq!(this.value(Field::ScanMaxDepth, cx), "9");
            this.cancel(cx);
            assert!(!this.active);
            assert_eq!(this.value(Field::ScanMaxDepth, cx), "7");
            assert_eq!(this.handle.config_snapshot().scan_max_depth, 7);
        });
    }

    #[gpui::test]
    fn pending_panel_apply_blocks_cancel_until_success_or_failure(cx: &mut TestAppContext) {
        let (_directory, _runtime, settings) = fixture(cx);
        for result in [Ok(()), Err("save failed".to_owned())] {
            settings.update(cx, |this, cx| {
                let config = this.handle.config_snapshot();
                this.model
                    .reset_artwork(&config, 22, AlbumArtworkOptions::from(config.as_ref()));
                this.active = true;
                let Some(PanelApply::Artwork { session, .. }) =
                    this.model.begin_panel_apply(Some(17.0))
                else {
                    panic!("expected panel apply");
                };
                this.model.draft.album_artwork_period_seconds = 19.0;
                this.cancel(cx);
                assert!(this.active);
                assert!(this.model.applying());
                assert_eq!(this.model.session(), session);
                assert_eq!(this.model.target(), Target::Artwork(22));
                this.complete_panel_apply(session, result, cx);
                assert!(!this.model.applying());
                assert_eq!(this.model.draft.album_artwork_period_seconds, 19.0);
                this.cancel(cx);
                assert!(!this.active);
                assert_eq!(this.model.target(), Target::Global);
            });
        }
    }

    #[gpui::test]
    fn settings_completion_cannot_modify_a_reopened_edit_session(cx: &mut TestAppContext) {
        let (_directory, runtime, settings) = fixture(cx);
        settings.update(cx, |this, cx| {
            this.set_value(Field::ScanMaxDepth, "7", None, cx)
        });
        let (patch, old_submission) = submit(&settings, cx);
        let (old_committed, old_result) = execute(&runtime, patch);
        assert!(old_result.is_ok());
        settings.update(cx, |this, cx| {
            // Explicitly replace the session to exercise stale completion protection;
            // user cancellation cannot close a session with an in-flight apply.
            let reopened = this.handle.config_snapshot();
            this.reset(&reopened, cx);
            this.active = true;
            this.set_value(Field::ScanMaxDepth, "9", None, cx);
        });
        let (patch, submission) = submit(&settings, cx);
        settings.update(cx, |this, cx| {
            this.complete_settings_apply(old_submission, old_committed, old_result, cx);
            assert!(this.model.applying());
            assert!(this.model.feedback().is_none());
            assert_eq!(this.model.baseline().scan_max_depth, 7);
            assert_eq!(this.value(Field::ScanMaxDepth, cx), "9");
        });
        let (committed, result) = execute(&runtime, patch);
        assert!(result.is_ok());
        settings.update(cx, |this, cx| {
            this.complete_settings_apply(submission, committed, result, cx);
            assert!(!this.model.applying());
            assert_eq!(this.parse(cx).unwrap().scan_max_depth, 9);
        });
    }

    #[gpui::test]
    fn scan_min_duration_accepts_only_u32_seconds_and_applies(cx: &mut TestAppContext) {
        let (_directory, runtime, settings) = fixture(cx);
        settings.update(cx, |this, cx| {
            assert_eq!(this.value(Field::ScanMinDurationSeconds, cx), "0");
            for value in ["-1", "1.5", "4294967296", ""] {
                this.set_value(Field::ScanMinDurationSeconds, value, None, cx);
                assert!(this.parse(cx).is_err(), "accepted {value}");
            }
            this.set_value(
                Field::ScanMinDurationSeconds,
                u32::MAX.to_string(),
                None,
                cx,
            );
            assert_eq!(this.parse(cx).unwrap().scan_min_duration_seconds, u32::MAX);
        });
        for seconds in [30, 0] {
            settings.update(cx, |this, cx| {
                this.set_value(Field::ScanMinDurationSeconds, seconds.to_string(), None, cx);
            });
            let (patch, submission) = submit(&settings, cx);
            let (committed, result) = execute(&runtime, patch);
            assert!(result.is_ok());
            assert_eq!(committed.scan_min_duration_seconds, seconds);
            settings.update(cx, |this, cx| {
                this.complete_settings_apply(submission, committed, result, cx);
                assert_eq!(this.parse(cx).unwrap().scan_min_duration_seconds, seconds);
            });
        }
    }

    #[gpui::test]
    fn successful_feedback_clears_on_text_edits_and_later_apply_edits(cx: &mut TestAppContext) {
        let (_directory, runtime, settings) = fixture(cx);
        let window = cx.add_window({
            let settings = settings.clone();
            move |_, _| SettingsWindow(settings)
        });
        let (patch, submission) = submit(&settings, cx);
        let (committed, result) = execute(&runtime, patch);
        settings.update(cx, |this, cx| {
            this.complete_settings_apply(submission, committed, result, cx);
            assert_eq!(this.model.feedback(), &Some(Ok(())));
        });
        edit_input(&settings, window, Field::ScanMaxDepth, "7", cx);
        settings.read_with(cx, |this, _| assert!(this.model.feedback().is_none()));
        let (patch, submission) = submit(&settings, cx);
        let (committed, result) = execute(&runtime, patch);
        edit_input(&settings, window, Field::Scale, "1e", cx);
        settings.update(cx, |this, cx| {
            this.complete_settings_apply(submission, committed, result, cx);
            assert!(this.model.feedback().is_none());
            assert_eq!(this.value(Field::Scale, cx), "1e");
        });
    }

    #[gpui::test]
    fn invalid_disabled_drafts_route_to_owner_and_reset_only_that_field(cx: &mut TestAppContext) {
        let (_directory, _runtime, settings) = fixture(cx);
        settings.update(cx, |this, cx| {
            this.model.draft.spectrum_style = SpectrumStyle::Solid;
            this.sync_field_enabled(cx);
            this.set_value(Field::SpectrumAutoWidth, "invalid", None, cx);
            this.set_value(Field::Font, "custom-font", None, cx);
            this.page = SettingsPage::Playback;
            this.apply_settings(cx);
            assert!(this.page == SettingsPage::Visualizations);
            assert_eq!(this.visual_page, VisualPage::Spectrum);
            assert_eq!(this.value(Field::SpectrumAutoWidth, cx), "invalid");
            this.restore_field_text(Field::SpectrumAutoWidth, cx);
            assert_eq!(this.value(Field::Font, cx), "custom-font");
            assert!(this.parse(cx).is_ok());
            this.set_value(Field::AlbumArtworkPeriodSeconds, "0", None, cx);
            let error = this.parse(cx).unwrap_err();
            assert_eq!(
                error.downcast_ref::<FieldIssue>().unwrap().field,
                Field::AlbumArtworkPeriodSeconds
            );
            this.restore_field_text(Field::AlbumArtworkPeriodSeconds, cx);
            assert!(this.parse(cx).is_ok());
        });
    }

    #[gpui::test]
    fn spectrum_layout_parses_only_active_value_and_validates_boundaries(cx: &mut TestAppContext) {
        let (_directory, _runtime, settings) = fixture(cx);
        settings.update(cx, |this, cx| {
            this.set_value(
                Field::SpectrumFixedCount,
                "invalid inactive count",
                None,
                cx,
            );
            for text in ["1", "20", "3.1234567"] {
                this.set_value(Field::SpectrumAutoWidth, text, None, cx);
                assert_eq!(
                    this.parse(cx).unwrap().spectrum_bar_layout,
                    SpectrumBarLayout::Automatic {
                        width: text.parse().unwrap()
                    }
                );
            }
            for text in ["", "1e", "NaN", "inf", "0", "20.01"] {
                this.set_value(Field::SpectrumAutoWidth, text, None, cx);
                assert_eq!(
                    this.parse(cx)
                        .unwrap_err()
                        .downcast_ref::<FieldIssue>()
                        .unwrap()
                        .field,
                    Field::SpectrumAutoWidth
                );
            }
            this.restore_field_text(Field::SpectrumAutoWidth, cx);
            this.select_spectrum_layout(SpectrumLayoutMode::Fixed, cx);
            this.set_value(Field::SpectrumAutoWidth, "invalid inactive width", None, cx);
            for text in ["1", "64", "512"] {
                this.set_value(Field::SpectrumFixedCount, text, None, cx);
                assert_eq!(
                    this.parse(cx).unwrap().spectrum_bar_layout,
                    SpectrumBarLayout::Fixed {
                        count: text.parse().unwrap()
                    }
                );
            }
            for text in ["", "0", "513", "1.5", "-1"] {
                this.set_value(Field::SpectrumFixedCount, text, None, cx);
                assert_eq!(
                    this.parse(cx)
                        .unwrap_err()
                        .downcast_ref::<FieldIssue>()
                        .unwrap()
                        .field,
                    Field::SpectrumFixedCount
                );
            }
        });
    }

    #[gpui::test]
    fn spectrum_mode_switch_rejects_invalid_edits_and_same_mode_keeps_precision(
        cx: &mut TestAppContext,
    ) {
        let (_directory, _runtime, settings) = fixture(cx);
        settings.update(cx, |this, cx| {
            this.model.draft.spectrum_style = SpectrumStyle::Bars;
            this.sync_field_enabled(cx);
        });
        let window = cx.add_window({
            let settings = settings.clone();
            move |_, _| SettingsWindow(settings)
        });
        edit_input(&settings, window, Field::SpectrumAutoWidth, "1e", cx);
        settings.update(cx, |this, cx| {
            this.open_setting_dropdown(
                vec![ChoiceItem::new(
                    SettingChoice::SpectrumLayout(SpectrumLayoutMode::Fixed),
                    "Fixed count",
                )],
                0,
            );
            this.choose_setting(0, cx);
            assert_eq!(this.spectrum_layout_field(), Field::SpectrumAutoWidth);
            assert_eq!(this.value(Field::SpectrumAutoWidth, cx), "1e");
            assert!(this.layout_switch_error.is_some());
            this.select_spectrum_layout(SpectrumLayoutMode::Automatic, cx);
            assert_eq!(this.value(Field::SpectrumAutoWidth, cx), "1e");
        });
        edit_input(&settings, window, Field::SpectrumAutoWidth, "3.1234567", cx);
        settings.update(cx, |this, cx| {
            this.select_spectrum_layout(SpectrumLayoutMode::Automatic, cx);
            assert_eq!(this.value(Field::SpectrumAutoWidth, cx), "3.1234567");
            let SpectrumBarLayout::Automatic { width } =
                this.parse(cx).unwrap().spectrum_bar_layout
            else {
                panic!("automatic layout");
            };
            assert_eq!(width.to_bits(), 3.1234567_f32.to_bits());
            this.select_spectrum_layout(SpectrumLayoutMode::Fixed, cx);
            assert_eq!(
                this.parse(cx).unwrap().spectrum_bar_layout,
                SpectrumBarLayout::Fixed { count: 64 }
            );
            this.set_value(Field::SpectrumFixedCount, "0", None, cx);
            this.select_spectrum_layout(SpectrumLayoutMode::Automatic, cx);
            assert_eq!(this.spectrum_layout_field(), Field::SpectrumFixedCount);
            assert_eq!(this.value(Field::SpectrumFixedCount, cx), "0");
            this.restore_field_text(Field::SpectrumFixedCount, cx);
            assert_eq!(this.value(Field::SpectrumFixedCount, cx), "64");
            this.select_spectrum_layout(SpectrumLayoutMode::Automatic, cx);
            assert_eq!(
                this.parse(cx).unwrap().spectrum_bar_layout,
                SpectrumBarLayout::Automatic { width: 3.0 }
            );
        });
    }

    #[gpui::test]
    fn spectrum_mode_switch_during_apply_preserves_new_mode_and_later_partial_text(
        cx: &mut TestAppContext,
    ) {
        let (_directory, runtime, settings) = fixture(cx);
        settings.update(cx, |this, cx| {
            this.model.draft.spectrum_style = SpectrumStyle::Bars;
            this.sync_field_enabled(cx);
        });
        let window = cx.add_window({
            let settings = settings.clone();
            move |_, _| SettingsWindow(settings)
        });
        edit_input(&settings, window, Field::SpectrumAutoWidth, "4.567891", cx);
        let (patch, submission) = submit(&settings, cx);
        let (committed, result) = execute(&runtime, patch);
        settings.update(cx, |this, cx| {
            this.select_spectrum_layout(SpectrumLayoutMode::Fixed, cx);
        });
        edit_input(&settings, window, Field::SpectrumFixedCount, "127", cx);
        settings.update(cx, |this, cx| {
            this.complete_settings_apply(submission, committed, result, cx);
            assert_eq!(
                this.model.baseline().spectrum_bar_layout,
                SpectrumBarLayout::Automatic { width: 4.567891 }
            );
            assert_eq!(
                this.parse(cx).unwrap().spectrum_bar_layout,
                SpectrumBarLayout::Fixed { count: 127 }
            );
            assert!(this.model.feedback().is_none());
        });
        let (patch, submission) = submit(&settings, cx);
        let (committed, result) = execute(&runtime, patch);
        settings.update(cx, |this, cx| {
            this.select_spectrum_layout(SpectrumLayoutMode::Automatic, cx);
        });
        edit_input(&settings, window, Field::SpectrumAutoWidth, "1e", cx);
        settings.update(cx, |this, cx| {
            this.select_spectrum_layout(SpectrumLayoutMode::Fixed, cx);
            assert!(this.layout_switch_error.is_some());
            this.complete_settings_apply(submission, committed, result, cx);
            assert_eq!(
                this.model.baseline().spectrum_bar_layout,
                SpectrumBarLayout::Fixed { count: 127 }
            );
            assert_eq!(
                this.model.draft.spectrum_bar_layout,
                SpectrumBarLayout::Automatic { width: 3.0 }
            );
            assert_eq!(this.value(Field::SpectrumAutoWidth, cx), "1e");
            assert!(this.parse(cx).is_err());
            assert!(this.layout_switch_error.is_some());
            this.restore_field_text(Field::SpectrumAutoWidth, cx);
            assert!(this.layout_switch_error.is_none());
            assert_eq!(
                this.parse(cx).unwrap().spectrum_bar_layout,
                SpectrumBarLayout::Automatic { width: 3.0 }
            );
        });
        let (patch, submission) = submit(&settings, cx);
        let (committed, result) = execute(&runtime, patch);
        settings.update(cx, |this, cx| {
            this.complete_settings_apply(submission, committed, result, cx);
            assert_eq!(this.model.feedback(), &Some(Ok(())));
        });
    }

    #[gpui::test]
    fn spectrum_switch_back_fences_identical_text_and_discard_fences_completion(
        cx: &mut TestAppContext,
    ) {
        let (_directory, runtime, settings) = fixture(cx);
        let (patch, submission) = submit(&settings, cx);
        let (committed, result) = execute(&runtime, patch);
        settings.update(cx, |this, cx| {
            this.select_spectrum_layout(SpectrumLayoutMode::Fixed, cx);
            this.select_spectrum_layout(SpectrumLayoutMode::Automatic, cx);
            let mut external = committed.as_ref().clone();
            external.spectrum_bar_layout = SpectrumBarLayout::Fixed { count: 99 };
            this.complete_settings_apply(submission, Arc::new(external), result, cx);
            assert_eq!(
                this.model.baseline().spectrum_bar_layout,
                SpectrumBarLayout::Fixed { count: 99 }
            );
            assert_eq!(
                this.parse(cx).unwrap().spectrum_bar_layout,
                SpectrumBarLayout::Automatic { width: 3.0 }
            );
            assert_eq!(this.value(Field::SpectrumAutoWidth, cx), "3");
        });
        let (patch, submission) = submit(&settings, cx);
        let (committed, result) = execute(&runtime, patch);
        settings.update(cx, |this, cx| {
            this.reset(&Arc::clone(&committed), cx);
            this.select_spectrum_layout(SpectrumLayoutMode::Fixed, cx);
            this.complete_settings_apply(submission, committed, result, cx);
            assert_eq!(
                this.parse(cx).unwrap().spectrum_bar_layout,
                SpectrumBarLayout::Fixed { count: 64 }
            );
        });
    }

    #[gpui::test]
    fn empty_font_editor_selects_monospace(cx: &mut TestAppContext) {
        let (_directory, _runtime, settings) = fixture(cx);
        settings.update(cx, |this, cx| {
            this.set_value(Field::Font, "  ", None, cx);
            assert_eq!(this.parse(cx).unwrap().ui_font, "monospace");
        });
    }
}
