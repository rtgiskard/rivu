mod model;
mod pages;

use model::{Completion, PanelApply, SettingsModel, Submission, Target};

use super::{
    UI_INSET, album_artwork::album_artwork_style_label, visualization::audio_visualizer_style_label,
};
use crate::{
    artwork::{ArtworkCacheReport, ArtworkManager},
    config::{
        AlbumArtworkDirection, AlbumArtworkMode, AlbumArtworkOptions, AlbumArtworkStyle,
        AnalysisWindow, AudioVisualizerOptions, AudioVisualizerStyle, Config, ConfigPatch,
        LogLevel, RgbColor, SpectrumStyle, VisualizationPalette,
    },
    core::CoreHandle,
    i18n::Localizer,
    theme::Theme,
};
use anyhow::{Context as _, Result};
use gpui::{prelude::*, *};
use rivu_ui::{
    ButtonTooltip, ChoiceAction, ChoiceMode, ChoicePopup, ChoiceState, Dialog, DropdownItem,
    FocusScope, StatusKind, activate_control, active_control, button, column, copyable_message,
    disabled_control, dropdown_trigger, form_field, icon_button as common_icon_button,
    input::{Input, InputEvent},
    input_aligned_control, row, section_group, status_message,
    theme::{Palette, palette},
    toggle_button, two_column,
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
    Fft(u32),
    AudioVisualizerStyle(AudioVisualizerStyle),
    Palette(VisualizationPalette),
    LogLevel(LogLevel),
    Window(AnalysisWindow),
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
            Self::AudioVisualizerStyle(value) => {
                audio_visualizer_style_label(i18n, value).to_owned()
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
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Field {
    ScanMaxDepth,
    ScanMinDuration,
    Volume,
    PlayCountThreshold,
    LogRetentionWeeks,
    MediaReadBuffer,
    ArtworkDecodeBudget,
    Font,
    Scale,
    LayoutSnapGrid,
    Fps,
    Background,
    VisualizationDb,
    AudioVisualizerSensitivity,
    AudioVisualizerRotationSpeed,
    AudioVisualizerBarWidth,
    AudioVisualizerBarGlowLayers,
    AudioVisualizerRingOpacity,
    AudioVisualizerBloomIntensity,
    AudioVisualizerInnerDiameter,
    AudioVisualizerPrimaryColor,
    AudioVisualizerSecondaryColor,
    AudioVisualizerDb,
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
    AlbumArtworkPeriod,
    AlbumArtworkFps,
}
impl Field {
    fn label_id(self) -> &'static str {
        match self {
            Self::ScanMaxDepth => "settings-maximum-scan-depth",
            Self::ScanMinDuration => "settings-minimum-scan-duration-secs",
            Self::Volume => "settings-volume",
            Self::PlayCountThreshold => "settings-play-count-threshold",
            Self::LogRetentionWeeks => "settings-log-retention-weeks",
            Self::MediaReadBuffer => "settings-media-read-buffer-mib",
            Self::ArtworkDecodeBudget => "settings-artwork-decode-budget-mib",
            Self::Font => "settings-font-family",
            Self::Scale => "settings-interface-scale",
            Self::LayoutSnapGrid => "settings-layout-grid-spacing-px",
            Self::Fps => "settings-analysis-rate-hz",
            Self::Background => "settings-background-color",
            Self::VisualizationDb => "settings-default-display-range-db",
            Self::AudioVisualizerSensitivity => "settings-sensitivity",
            Self::AudioVisualizerRotationSpeed => "settings-rotation-speed",
            Self::AudioVisualizerBarWidth => "settings-bar-width",
            Self::AudioVisualizerBarGlowLayers => "settings-bar-glow-layers",
            Self::AudioVisualizerRingOpacity => "settings-ring-opacity",
            Self::AudioVisualizerBloomIntensity => "settings-bloom-intensity",
            Self::AudioVisualizerInnerDiameter => "settings-inner-diameter",
            Self::AudioVisualizerPrimaryColor => "settings-primary-color",
            Self::AudioVisualizerSecondaryColor => "settings-secondary-color",
            Self::SpectrumDb | Self::AudioVisualizerDb => "settings-display-range-db",
            Self::SpectrogramDb => "settings-color-range-db",
            Self::SpectrumBarWidth => "settings-automatic-bar-width-px",
            Self::SpectrumBars => "settings-bar-count",
            Self::SpectrumGap => "settings-bar-gap-px",
            Self::SpectrumHold => "settings-peak-hold-ms",
            Self::SpectrumGravity => "settings-peak-gravity-db-s",
            Self::SpectrumBarHold => "settings-bar-hold-ms",
            Self::SpectrumBarGravity => "settings-bar-gravity-db-s",
            Self::SpectrumSmoothing => "settings-release-smoothing-ms",
            Self::SpectrogramHistory => "settings-history-limit-s",
            Self::SpectrogramSamplingPointsScale => "settings-source-row-scale",
            Self::SpectrogramInterpolationPoints => "settings-maximum-frequency-rows",
            Self::CursorColor => "settings-waterline-color",
            Self::Glow => "settings-glow-strength",
            Self::WaveformRmsGain => "settings-rms-gain",
            Self::WaveformPeakGain => "settings-peak-gain",
            Self::WaveformPeakGamma => "settings-peak-gamma",
            Self::AlbumArtworkPeriod => "settings-seconds-per-circle",
            Self::AlbumArtworkFps => "settings-animation-rate-fps",
        }
    }

    fn label(self, i18n: &Localizer) -> &str {
        i18n.text(self.label_id())
    }

    fn hint(self, settings: &Settings) -> SharedString {
        match self {
            Self::AlbumArtworkPeriod
                if settings.model.draft.album_artwork_mode == AlbumArtworkMode::Static =>
            {
                settings.tr("settings-artwork-recovery")
            }
            Self::SpectrumBars | Self::SpectrumBarWidth | Self::SpectrumGap
                if settings.model.draft.spectrum_style == SpectrumStyle::Solid =>
            {
                settings.tr("settings-spectrum-style-recovery")
            }
            Self::SpectrumHold | Self::SpectrumGravity if !settings.model.draft.spectrum_peaks => {
                settings.tr("settings-spectrum-peaks-recovery")
            }
            Self::AlbumArtworkPeriod => settings.tr("settings-artwork-period-hint"),
            Self::AlbumArtworkFps => format!(
                "{} {}",
                settings.tr("settings-animation-fps-hint"),
                settings.tr("settings-artwork-motion-help")
            )
            .into(),
            Self::ScanMaxDepth => settings.tr("settings-scan-depth-hint"),
            Self::ScanMinDuration => settings.tr("settings-scan-duration-hint"),
            Self::Volume => "0–100.".into(),
            Self::PlayCountThreshold => settings.tr("settings-play-count-threshold-hint"),
            Self::LogRetentionWeeks => "1–520.".into(),
            Self::MediaReadBuffer => settings.tr("settings-media-buffer-hint"),
            Self::ArtworkDecodeBudget => settings.tr("settings-artwork-decode-budget-hint"),
            Self::Font => "".into(),
            Self::Scale => "0.75–2.".into(),
            Self::LayoutSnapGrid => settings.tr("settings-layout-grid-hint"),
            Self::Fps => settings.tr("settings-analysis-rate-hint"),
            Self::Background => settings.tr("settings-background-hint"),
            Self::AudioVisualizerPrimaryColor | Self::AudioVisualizerSecondaryColor => {
                settings.tr("settings-colors-hint")
            }
            Self::CursorColor => settings.tr("settings-waterline-hint"),
            Self::VisualizationDb => settings.tr("settings-visualizer-db-hint"),
            // These controls attach a live inherited-range tooltip in Settings::field.
            Self::SpectrumDb | Self::AudioVisualizerDb | Self::SpectrogramDb => "".into(),
            Self::AudioVisualizerSensitivity => "0–5.".into(),
            Self::AudioVisualizerRotationSpeed => "0–10.".into(),
            Self::AudioVisualizerBarWidth
            | Self::AudioVisualizerBloomIntensity
            | Self::WaveformPeakGain => "0–2.".into(),
            Self::AudioVisualizerBarGlowLayers => "0–4.".into(),
            Self::AudioVisualizerRingOpacity => "0–1.".into(),
            Self::AudioVisualizerInnerDiameter => settings.tr("settings-visualizer-diameter-hint"),
            Self::SpectrumBarWidth => settings.tr("settings-spectrum-bar-width-hint"),
            Self::SpectrumBars => settings.tr("settings-spectrum-bar-count-hint"),
            Self::SpectrumGap => "0–8.".into(),
            Self::SpectrumHold | Self::SpectrumBarHold => "0–2000.".into(),
            Self::SpectrumGravity => "0–500.".into(),
            Self::SpectrumBarGravity => settings.tr("settings-spectrum-bar-gravity-hint"),
            Self::SpectrumSmoothing => settings.tr("settings-spectrum-smoothing-hint"),
            Self::SpectrogramHistory => settings.tr("settings-spectrogram-history-hint"),
            Self::SpectrogramSamplingPointsScale => {
                settings.tr("settings-spectrogram-source-scale-hint")
            }
            Self::SpectrogramInterpolationPoints => {
                settings.tr("settings-spectrogram-frequency-rows-hint")
            }
            Self::Glow => settings.tr("settings-waveform-glow-hint"),
            Self::WaveformRmsGain => settings.tr("settings-waveform-rms-hint"),
            Self::WaveformPeakGamma => settings.tr("settings-waveform-gamma-hint"),
        }
    }

    fn form(self, theme: &'static Palette, settings: &Settings, control: impl IntoElement) -> Div {
        let hint = self.hint(settings);
        let control = div()
            .id(("settings-field-help", self as usize))
            .w_full()
            .child(control)
            .when(!hint.is_empty(), |control| {
                control
                    .tooltip(move |_, cx| cx.new(|_| ButtonTooltip { text: hint.clone() }).into())
            });
        form_field(theme, settings.tr(self.label_id()), control)
    }
}

fn icon_button(
    id: impl Into<ElementId>,
    icon: impl Into<SharedString>,
    hint: impl Into<SharedString>,
    cx: &mut Context<Settings>,
    action: impl Fn(&mut Settings, &mut Window, &mut Context<Settings>) + 'static,
) -> Stateful<Div> {
    input_aligned_control(common_icon_button(id, icon, hint, cx, action))
}

fn dropdown_button(
    id: &'static str,
    label: impl Into<SharedString>,
    cx: &mut Context<Settings>,
    action: impl Fn(&mut Settings, &mut Window, &mut Context<Settings>) + 'static,
) -> Stateful<Div> {
    input_aligned_control(activate_control(
        dropdown_trigger(palette(cx), id, label, cx),
        cx,
        move |this, window, cx| {
            action(this, window, cx);
            cx.notify();
        },
    ))
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
    let label: SharedString = label.into();
    let hint = match id {
        "settings-tray" => i18n.text("settings-restart-hint").to_owned(),
        "settings-still-mode" => i18n.text("settings-still-mode-hint").to_owned(),
        "visual-cache" => i18n.text("settings-rendering-cache-hint").to_owned(),
        "audio-visualizer-fade-idle" => i18n.text("settings-visualizer-fade-hint").to_owned(),
        "audio-visualizer-grid" => i18n.text("settings-visualizer-guides-hint").to_owned(),
        _ => String::new(),
    };
    let label: SharedString = if hint.is_empty() {
        label
    } else {
        format!("{label} ({hint})").into()
    };
    input_aligned_control(toggle_button(
        id,
        icon,
        format!(
            "{label}: {}",
            i18n.text(if enabled {
                "settings-on"
            } else {
                "settings-off"
            })
        ),
        enabled,
        cx,
        move |this, cx| {
            toggle(&mut this.model.draft);
            this.sync_field_enabled(cx);
        },
    ))
}

fn settings_group(
    theme: &'static Palette,
    title: impl Into<SharedString>,
    content: impl IntoElement,
) -> Div {
    section_group(theme, title, content)
}

fn settings_card(
    theme: &'static Palette,
    title: impl Into<SharedString>,
    content: impl IntoElement,
) -> Div {
    settings_group(theme, title, content)
        .border_1()
        .border_color(rgb(theme.structure_border))
        .rounded(rems(5. / 16.))
}

fn settings_tooltip(
    text: impl Into<SharedString>,
) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView {
    let text = text.into();
    move |_, cx| cx.new(|_| ButtonTooltip { text: text.clone() }).into()
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
    SettingsFft,
    SettingsLogLevel,
    SettingsWindow,
    AlbumArtworkStyle,
    AlbumArtworkMode,
    AlbumArtworkDirection,
}

pub(super) enum SettingsEvent {
    Closed,
    ReviewDuplicates,
    ApplyVisualizer {
        panel_id: u64,
        options: AudioVisualizerOptions,
        session: u64,
    },
    ApplyArtwork {
        panel_id: u64,
        options: AlbumArtworkOptions,
        session: u64,
    },
}

impl EventEmitter<SettingsEvent> for Settings {}

struct SettingsSubmission {
    model: Submission,
    inputs: HashMap<Field, String>,
}

/// Unsaved preferences stay independent from live playback snapshots.
pub(super) struct Settings {
    i18n: Arc<Localizer>,
    text: HashMap<String, SharedString>,
    handle: CoreHandle,
    artwork_manager: Arc<ArtworkManager>,
    artwork_cache_age: Entity<Input>,
    artwork_cache_age_revision: u64,
    artwork_cache_busy: Option<(u64, bool)>,
    artwork_cache_report: Option<(u64, bool, ArtworkCacheReport)>,
    artwork_cache_error: Option<String>,
    devices: Arc<Vec<String>>,
    ffmpeg_status: String,
    mpris_status: String,
    ui_scale: f32,
    scope: FocusScope,
    active: bool,
    measured: Rc<RefCell<HashMap<Measured, Bounds<Pixels>>>>,
    scrolls: HashMap<&'static str, Rc<ScrollHandle>>,
    active_scroll: Option<ScrollHandle>,
    _subscriptions: Vec<Subscription>,
    model: SettingsModel,
    inputs: HashMap<Field, Entity<Input>>,
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
        let artwork_cache_age =
            cx.new(|cx| Input::new("30", i18n.text("settings-age-in-days").to_owned(), cx));
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
            artwork_cache_age,
            artwork_cache_age_revision: 0,
            artwork_cache_busy: None,
            artwork_cache_report: None,
            artwork_cache_error: None,
            devices,
            ffmpeg_status,
            mpris_status,
            ui_scale,
            scope: FocusScope::new(cx),
            active: false,
            measured: Rc::new(RefCell::new(HashMap::new())),
            scrolls: HashMap::new(),
            active_scroll: None,
            _subscriptions: vec![artwork_cache_subscription],
            model: SettingsModel::new(&config),
            inputs: HashMap::new(),
            initialized: false,
            dropdown: ChoiceState::default(),
            font_options: Vec::new(),
            font_value: config.ui_font.clone(),
            dropdown_anchor: Measured::Device,
            page: SettingsPage::Library,
            visual_page: VisualPage::Common,
        }
    }

    pub(super) fn sync_runtime(
        &mut self,
        devices: &Arc<Vec<String>>,
        ffmpeg_status: &str,
        mpris_status: &str,
        ui_scale: f32,
        cx: &mut Context<Self>,
    ) {
        let devices_changed = !Arc::ptr_eq(&self.devices, devices) && self.devices != *devices;
        let scale_changed = self.ui_scale != ui_scale;
        let changed = devices_changed
            || self.ffmpeg_status != ffmpeg_status
            || self.mpris_status != mpris_status
            || self.ui_scale != ui_scale;
        if !changed {
            return;
        }
        self.devices = Arc::clone(devices);
        self.ffmpeg_status.clear();
        self.ffmpeg_status.push_str(ffmpeg_status);
        self.mpris_status.clear();
        self.mpris_status.push_str(mpris_status);
        self.ui_scale = ui_scale;
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
        self.active = false;
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
            Field::ScanMinDuration,
            Field::Volume,
            Field::PlayCountThreshold,
            Field::LogRetentionWeeks,
            Field::MediaReadBuffer,
            Field::ArtworkDecodeBudget,
            Field::Font,
            Field::Scale,
            Field::LayoutSnapGrid,
            Field::Fps,
            Field::Background,
            Field::VisualizationDb,
            Field::AudioVisualizerSensitivity,
            Field::AudioVisualizerRotationSpeed,
            Field::AudioVisualizerBarWidth,
            Field::AudioVisualizerBarGlowLayers,
            Field::AudioVisualizerRingOpacity,
            Field::AudioVisualizerBloomIntensity,
            Field::AudioVisualizerInnerDiameter,
            Field::AudioVisualizerPrimaryColor,
            Field::AudioVisualizerSecondaryColor,
            Field::AudioVisualizerDb,
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
            Field::AlbumArtworkPeriod,
            Field::AlbumArtworkFps,
        ]
        .into_iter()
        .map(|field| {
            let input = cx.new(|cx| {
                if field == Field::Font {
                    Input::new("", self.tr("settings-font-search-placeholder"), cx)
                        .with_trailing_space(px(28.))
                } else {
                    Input::new("", self.tr(field.label_id()), cx)
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

    fn font_items(&self, query: &str) -> Vec<DropdownItem<SettingChoice>> {
        let query = query.trim();
        let search = query.to_lowercase();
        let mut items: Vec<_> = self
            .font_options
            .iter()
            .enumerate()
            .filter(|(_, option)| option.search.contains(&search))
            .map(|(index, option)| {
                DropdownItem::new(SettingChoice::Font(index), option.family.clone())
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
            Field::SpectrumDb | Field::AudioVisualizerDb | Field::SpectrogramDb
        ) {
            let common = self.inputs[&Field::VisualizationDb].clone();
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

    fn field_state(&self, field: Field, cx: &App) -> Div {
        self.field(field, cx).min_w(rems(180. / 16.))
    }

    fn sync_field_enabled(&self, cx: &mut App) {
        let solid = self.model.draft.spectrum_style == SpectrumStyle::Solid;
        for (field, enabled) in [
            (
                Field::AlbumArtworkPeriod,
                self.model.draft.album_artwork_mode == AlbumArtworkMode::Rotating,
            ),
            (Field::SpectrumBars, !solid),
            (Field::SpectrumBarWidth, !solid),
            (Field::SpectrumGap, !solid),
            (Field::SpectrumHold, self.model.draft.spectrum_peaks),
            (Field::SpectrumGravity, self.model.draft.spectrum_peaks),
        ] {
            if let Some(input) = self.inputs.get(&field) {
                input.update(cx, |input, cx| input.set_enabled(enabled, cx));
            }
        }
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
            .map_err(|error| anyhow::anyhow!("{}: {error}", field.label(&self.i18n)))
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
                .map_err(|error| anyhow::anyhow!("{}: {error}", field.label(&self.i18n)))
        }
    }

    fn set_value(
        &mut self,
        field: Field,
        value: impl Into<SharedString>,
        unchanged: Option<&HashMap<Field, String>>,
        cx: &mut App,
    ) {
        if unchanged.is_some_and(|inputs| {
            inputs
                .get(&field)
                .is_none_or(|submitted| self.inputs[&field].read(cx).text() != submitted)
        }) {
            return;
        }
        self.inputs[&field].update(cx, |input, cx| input.set_text(value.into(), cx));
    }

    fn reset(&mut self, config: &Arc<Config>, cx: &mut App) {
        self.model.reset(config);
        self.artwork_cache_report = None;
        self.artwork_cache_error = None;
        self.fill(cx);
    }

    fn fill(&mut self, cx: &mut App) {
        self.fill_unchanged(None, cx);
    }

    fn fill_unchanged(&mut self, unchanged: Option<&HashMap<Field, String>>, cx: &mut App) {
        self.set_value(
            Field::ScanMaxDepth,
            self.model.draft.scan_max_depth.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::ScanMinDuration,
            self.model.draft.scan_min_duration_secs.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::Volume,
            (self.model.draft.volume * 100.).to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::PlayCountThreshold,
            self.model.draft.play_count_threshold_percent.to_string(),
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
            Field::MediaReadBuffer,
            self.model.draft.media_read_buffer_mb.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::ArtworkDecodeBudget,
            self.model.draft.artwork_decode_budget_mb.to_string(),
            unchanged,
            cx,
        );
        self.font_value = self.model.draft.ui_font.clone();
        self.set_value(Field::Font, self.model.draft.ui_font.clone(), unchanged, cx);
        self.set_value(
            Field::Scale,
            self.model.draft.ui_scale.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::LayoutSnapGrid,
            self.model.draft.layout_snap_grid_px.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::Fps,
            self.model.draft.analysis_fps.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::Background,
            self.model.draft.visual_background.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::VisualizationDb,
            self.model.draft.visualization_db_range.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::AudioVisualizerSensitivity,
            self.model.draft.audio_visualizer_sensitivity.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::AudioVisualizerRotationSpeed,
            self.model.draft.audio_visualizer_rotation_speed.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::AudioVisualizerBarWidth,
            self.model.draft.audio_visualizer_bar_width.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::AudioVisualizerBarGlowLayers,
            self.model
                .draft
                .audio_visualizer_bar_glow_layers
                .to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::AudioVisualizerRingOpacity,
            self.model.draft.audio_visualizer_ring_opacity.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::AudioVisualizerBloomIntensity,
            self.model
                .draft
                .audio_visualizer_bloom_intensity
                .to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::AudioVisualizerInnerDiameter,
            self.model.draft.audio_visualizer_inner_diameter.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::AudioVisualizerPrimaryColor,
            self.model.draft.audio_visualizer_primary_color.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::AudioVisualizerSecondaryColor,
            self.model
                .draft
                .audio_visualizer_secondary_color
                .to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::AudioVisualizerDb,
            self.model
                .draft
                .audio_visualizer_db_range
                .map(|range| range.to_string())
                .unwrap_or_default(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::SpectrumDb,
            self.model
                .draft
                .spectrum_db_range
                .map(|range| range.to_string())
                .unwrap_or_default(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::SpectrumGap,
            self.model.draft.spectrum_gap.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::SpectrumHold,
            self.model.draft.spectrum_peak_hold_ms.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::SpectrumGravity,
            self.model.draft.spectrum_peak_gravity.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::SpectrumBarHold,
            self.model.draft.spectrum_bar_hold_ms.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::SpectrumBarGravity,
            self.model.draft.spectrum_bar_gravity.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::SpectrumSmoothing,
            self.model.draft.spectrum_smoothing_ms.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::SpectrumBarWidth,
            self.model.draft.spectrum_bar_width.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::SpectrumBars,
            self.model.draft.spectrum_bars.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::SpectrogramDb,
            self.model
                .draft
                .spectrogram_db_range
                .map(|range| range.to_string())
                .unwrap_or_default(),
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
            Field::SpectrogramSamplingPointsScale,
            self.model
                .draft
                .spectrogram_sampling_points_scale
                .to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::SpectrogramInterpolationPoints,
            self.model
                .draft
                .spectrogram_interpolation_points
                .to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::CursorColor,
            self.model.draft.waveform_cursor_color.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::Glow,
            self.model.draft.waveform_glow.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::WaveformRmsGain,
            self.model.draft.waveform_rms_gain.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::WaveformPeakGain,
            self.model.draft.waveform_peak_gain.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::WaveformPeakGamma,
            self.model.draft.waveform_peak_gamma.to_string(),
            unchanged,
            cx,
        );
        self.set_value(
            Field::AlbumArtworkPeriod,
            self.model.draft.album_artwork_period_seconds.to_string(),
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
        config.scan_min_duration_secs = self.number(Field::ScanMinDuration, cx)?;
        let volume_percent = self.number::<f32>(Field::Volume, cx)?;
        // Preserve the exact level when the displayed percentage is unchanged:
        // multiplying and dividing by 100 can otherwise lose f32 precision.
        if volume_percent != self.model.draft.volume * 100. {
            config.volume = volume_percent / 100.;
        }
        config.play_count_threshold_percent = self.number(Field::PlayCountThreshold, cx)?;
        config.log_retention_weeks = self.number(Field::LogRetentionWeeks, cx)?;
        config.media_read_buffer_mb = self.number(Field::MediaReadBuffer, cx)?;
        config.artwork_decode_budget_mb = self.number(Field::ArtworkDecodeBudget, cx)?;
        config.ui_font = self.value(Field::Font, cx).trim().to_owned();
        if config.ui_font.is_empty() {
            config.ui_font = "sans-serif".to_owned();
        }
        config.ui_scale = self.number(Field::Scale, cx)?;
        config.layout_snap_grid_px = self.number(Field::LayoutSnapGrid, cx)?;
        config.analysis_fps = self.number(Field::Fps, cx)?;
        config.visualization_db_range = self.number(Field::VisualizationDb, cx)?;
        config.visual_background = self
            .value(Field::Background, cx)
            .trim()
            .parse::<RgbColor>()
            .context(self.tr("settings-visualization-background"))?;
        config.audio_visualizer_sensitivity = self.number(Field::AudioVisualizerSensitivity, cx)?;
        config.audio_visualizer_rotation_speed =
            self.number(Field::AudioVisualizerRotationSpeed, cx)?;
        config.audio_visualizer_bar_width = self.number(Field::AudioVisualizerBarWidth, cx)?;
        config.audio_visualizer_bar_glow_layers =
            self.number(Field::AudioVisualizerBarGlowLayers, cx)?;
        config.audio_visualizer_ring_opacity =
            self.number(Field::AudioVisualizerRingOpacity, cx)?;
        config.audio_visualizer_bloom_intensity =
            self.number(Field::AudioVisualizerBloomIntensity, cx)?;
        config.audio_visualizer_inner_diameter =
            self.number(Field::AudioVisualizerInnerDiameter, cx)?;
        config.audio_visualizer_primary_color = self
            .value(Field::AudioVisualizerPrimaryColor, cx)
            .trim()
            .parse::<RgbColor>()
            .context(self.tr("settings-audio-visualizer-primary-color"))?;
        config.audio_visualizer_secondary_color = self
            .value(Field::AudioVisualizerSecondaryColor, cx)
            .trim()
            .parse::<RgbColor>()
            .context(self.tr("settings-audio-visualizer-secondary-color"))?;
        config.audio_visualizer_db_range = self.optional_number(Field::AudioVisualizerDb, cx)?;
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
            .context(self.tr("settings-waveform-waterline-color"))?;
        config.waveform_glow = self.number(Field::Glow, cx)?;
        config.waveform_rms_gain = self.number(Field::WaveformRmsGain, cx)?;
        config.waveform_peak_gain = self.number(Field::WaveformPeakGain, cx)?;
        config.waveform_peak_gamma = self.number(Field::WaveformPeakGamma, cx)?;
        config.album_artwork_period_seconds = self.number(Field::AlbumArtworkPeriod, cx)?;
        config.album_artwork_fps = self.number(Field::AlbumArtworkFps, cx)?;
        config.validate()?;
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
        active_control(
            palette(cx),
            button(id, label, cx, move |this, _, cx| {
                this.dropdown.close();
                select(this);
                cx.notify();
            }),
            active,
        )
        .flex_shrink_0()
    }
}

impl Settings {
    fn initialize_settings(&mut self, cx: &mut Context<Self>) {
        if self.initialized {
            return;
        }
        self.initialize(cx);
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
            .child(input)
            .child(
                div()
                    .id("settings-font-arrow")
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
                    .child("󰅀")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            if this.dropdown.is_open()
                                && this.dropdown_anchor == Measured::SettingsFont
                            {
                                this.dropdown.close();
                            } else {
                                this.open_font_dropdown(false, cx);
                            }
                            this.inputs[&Field::Font].focus_handle(cx).focus(window, cx);
                            cx.stop_propagation();
                            cx.notify();
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
        self.scope.focus(window, cx);
        cx.notify();
    }

    pub(super) fn open_visualizer(
        &mut self,
        panel_id: u64,
        options: AudioVisualizerOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.model
            .reset_visualizer(&self.handle.config_snapshot(), panel_id, options);
        self.page = SettingsPage::Visualizations;
        self.visual_page = VisualPage::AudioVisualizer;
        self.show_session(window, cx);
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

    fn apply_settings(&mut self, cx: &mut Context<Self>) {
        if self.model.applying() {
            return;
        }
        if self.model.target() != Target::Global {
            let period = if matches!(self.model.target(), Target::Artwork(_)) {
                self.value(Field::AlbumArtworkPeriod, cx)
                    .parse::<f32>()
                    .ok()
            } else {
                None
            };
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
                Some(PanelApply::Visualizer {
                    panel_id,
                    options,
                    session,
                }) => {
                    cx.emit(SettingsEvent::ApplyVisualizer {
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
            let (response, committed) = cx
                .background_executor()
                .spawn(async move {
                    let response = handle.apply_gui_settings(patch);
                    (response, handle.config_snapshot())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let result = if response.ok {
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
            inputs: self
                .inputs
                .iter()
                .map(|(&field, input)| (field, input.read(cx).text().to_owned()))
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
        match self
            .model
            .complete_apply(submission.model, committed, result)
        {
            Completion::Stale => return,
            Completion::Failed => {}
            Completion::Committed => {
                // Preserve invalid/partial input and its caret, not just parsed edits.
                self.fill_unchanged(Some(&submission.inputs), cx);
                self.font_value = self.value(Field::Font, cx);
            }
        }
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
        let Some((_, item)) = self.dropdown.filtered_item(index) else {
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
            SettingChoice::AudioVisualizerStyle(value) => {
                self.model.draft.audio_visualizer_style = value
            }
            SettingChoice::Palette(value) => self.model.draft.visual_palette = value,
            SettingChoice::Fft(value) => self.model.draft.analysis_fft_size = value,
            SettingChoice::LogLevel(value) => self.model.draft.log_level = value,
            SettingChoice::Window(value) => self.model.draft.analysis_window = value,
            SettingChoice::AlbumArtworkStyle(value) => self.model.draft.album_artwork_style = value,
            SettingChoice::AlbumArtworkMode(value) => self.model.draft.album_artwork_mode = value,
            SettingChoice::AlbumArtworkDirection(value) => {
                self.model.draft.album_artwork_direction = value
            }
        }
        self.dropdown.select_index(index);
        self.dropdown.close();
        self.sync_field_enabled(cx);
        cx.notify();
    }

    fn open_setting_dropdown(&mut self, items: Vec<DropdownItem<SettingChoice>>, selected: usize) {
        if let Some(item) = items.first() {
            self.dropdown_anchor = match item.value {
                SettingChoice::Language(_) => Measured::Language,
                SettingChoice::Theme(_) => Measured::Theme,
                SettingChoice::Font(_) | SettingChoice::CustomFont => Measured::SettingsFont,
                SettingChoice::SpectrumStyle(_) => Measured::SettingsStyle,
                SettingChoice::Fft(_) => Measured::SettingsFft,
                SettingChoice::LogLevel(_) => Measured::SettingsLogLevel,
                SettingChoice::Window(_) => Measured::SettingsWindow,
                SettingChoice::AudioVisualizerStyle(_) => Measured::SettingsStyle,
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
                let id = match self.visual_page {
                    VisualPage::Common => "settings-common-scroll",
                    VisualPage::Spectrum => "settings-spectrum-scroll",
                    VisualPage::Spectrogram => "settings-spectrogram-scroll",
                    VisualPage::Waveform => "settings-waveform-scroll",
                    VisualPage::AudioVisualizer => "settings-audio-visualizer-scroll",
                };
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
                        self.sidebar(cx)
                            .id("settings-sidebar")
                            .w_full()
                            .h_full()
                            .overflow_y_scroll()
                            .track_scroll(&sidebar_scroll),
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
                    .overflow_y_scroll()
                    .track_scroll(&scroll)
                    .child(content.flex_shrink_0().p(gpui::px(UI_INSET))),
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
                Some(Err(error)) => copyable_message("copy-settings-error", error.clone(), cx)
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
                .gap_3()
                .flex_shrink_0()
                .px(gpui::px(UI_INSET))
                .py(gpui::px(UI_INSET))
                .min_h(rems(3.))
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
                .child(disabled_control(
                    colors,
                    icon_button(
                        "settings-apply",
                        "",
                        self.tr("settings-apply-settings"),
                        cx,
                        |this, _, cx| this.apply_settings(cx),
                    ),
                    self.model.applying(),
                ))
                .child(icon_button(
                    "settings-discard",
                    "",
                    self.tr("settings-discard-changes-and-close"),
                    cx,
                    |this, _, cx| this.cancel(cx),
                )),
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
                            row()
                                .h(rems(2.5))
                                .flex_shrink_0()
                                .px(gpui::px(UI_INSET))
                                .child(
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
    use crate::{core::Runtime, model::Command};
    // gpui::* also imports its test macro; expansion needs Rust's builtin attribute.
    use core::prelude::v1::test;

    fn fixture(cx: &mut TestAppContext) -> (tempfile::TempDir, Runtime, Entity<Settings>) {
        cx.update(|cx| {
            rivu_ui::theme::set_palette(Theme::default().palette(), cx);
            rivu_ui::theme::set_typography(
                gpui::font("sans-serif"),
                crate::gui::appearance::fonts::SYMBOLS_FAMILY,
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
        let runtime = Runtime::start(directory.path(), &path).unwrap();
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
        runtime: &Runtime,
        patch: ConfigPatch,
    ) -> (Arc<Config>, std::result::Result<(), String>) {
        let response = runtime.handle.apply_gui_settings(patch);
        let result = if response.ok {
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
                            DropdownItem::new(choice, choice.label(&this.i18n))
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
                this.set_value(Field::AlbumArtworkPeriod, "1e", None, cx);
                let input = this.inputs[&Field::AlbumArtworkPeriod].clone();
                this.open_setting_dropdown(
                    vec![DropdownItem::new(
                        SettingChoice::AlbumArtworkMode(AlbumArtworkMode::Static),
                        "Static",
                    )],
                    0,
                );
                this.choose_setting(0, cx);
                input.update(cx, |input, cx| {
                    input.replace_text_in_range(Some(0..2), "99", window, cx);
                });
                assert_eq!(this.value(Field::AlbumArtworkPeriod, cx), "1e");
                assert_eq!(this.inputs[&Field::AlbumArtworkPeriod], input);
                this.open_setting_dropdown(
                    vec![DropdownItem::new(
                        SettingChoice::AlbumArtworkMode(AlbumArtworkMode::Rotating),
                        "Rotating",
                    )],
                    0,
                );
                this.choose_setting(0, cx);
                assert_eq!(this.value(Field::AlbumArtworkPeriod, cx), "1e");
                input.update(cx, |input, cx| {
                    input.replace_text_in_range(Some(0..2), "12", window, cx);
                });
                assert_eq!(this.value(Field::AlbumArtworkPeriod, cx), "12");
            });
        })
        .unwrap();
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
            runtime
                .handle
                .request_ack(Command::Volume { value: 0.2 })
                .ok
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
                this.model.draft.still_mode = true;
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
                assert!(this.model.draft.still_mode);
                assert!(this.parse(cx).is_err());
                this.set_value(Field::ScanMaxDepth, "9", None, cx);
            });
        })
        .unwrap();
        let (patch, submission) = submit(&settings, cx);
        assert!(
            runtime
                .handle
                .request_ack(Command::Volume { value: 0.3 })
                .ok
        );
        let (committed, result) = execute(&runtime, patch);
        assert!(result.is_ok());
        settings.update(cx, |this, cx| {
            this.complete_settings_apply(submission, committed, result, cx);
            let visible = this.parse(cx).unwrap();
            assert_eq!(visible.scan_max_depth, 9);
            assert_eq!(visible.volume, 0.3);
            assert!(visible.still_mode);
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
        let baseline_theme = runtime.handle.config_snapshot().ui_theme;
        let path = directory.path().join("config.toml");
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        settings.update(cx, |this, cx| {
            this.model.draft.ui_theme = Theme::TokyoNightDay;
            this.set_value(Field::ScanMaxDepth, "7", None, cx);
        });
        let (patch, submission) = submit(&settings, cx);
        let (committed, result) = execute(&runtime, patch);
        assert!(result.is_err());
        settings.update(cx, |this, cx| {
            this.set_value(Field::ScanMaxDepth, "9", None, cx);
            this.model.draft.still_mode = true;
            this.complete_settings_apply(submission, committed, result, cx);
            assert_eq!(
                this.model.baseline().scan_max_depth,
                crate::config::DEFAULT_SCAN_MAX_DEPTH
            );
            assert_eq!(this.value(Field::ScanMaxDepth, cx), "9");
            assert!(this.model.draft.still_mode);
            assert_eq!(this.model.baseline().ui_theme, baseline_theme);
            assert_eq!(this.model.draft.ui_theme, Theme::TokyoNightDay);
            assert!(!this.model.applying());
            assert!(matches!(this.model.feedback(), Some(Err(_))));
        });
        std::fs::remove_dir(&path).unwrap();
        assert!(
            runtime
                .handle
                .request_ack(Command::Volume { value: 0.2 })
                .ok
        );
        let (patch, submission) = submit(&settings, cx);
        let (committed, result) = execute(&runtime, patch);
        assert!(result.is_ok());
        settings.update(cx, |this, cx| {
            this.complete_settings_apply(submission, committed, result, cx);
            assert_eq!(this.model.baseline().scan_max_depth, 9);
            assert_eq!(this.model.baseline().volume, 0.2);
            assert!(this.model.baseline().still_mode);
            assert_eq!(this.model.baseline().ui_theme, Theme::TokyoNightDay);
            assert_eq!(this.parse(cx).unwrap(), *this.model.baseline());
        });
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
            this.cancel(cx);
            assert!(!this.active);
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
            assert_eq!(this.value(Field::ScanMinDuration, cx), "0");
            for value in ["-1", "1.5", "4294967296", ""] {
                this.set_value(Field::ScanMinDuration, value, None, cx);
                assert!(this.parse(cx).is_err(), "accepted {value}");
            }
            this.set_value(Field::ScanMinDuration, u32::MAX.to_string(), None, cx);
            assert_eq!(this.parse(cx).unwrap().scan_min_duration_secs, u32::MAX);
        });
        for seconds in [30, 0] {
            settings.update(cx, |this, cx| {
                this.set_value(Field::ScanMinDuration, seconds.to_string(), None, cx);
            });
            let (patch, submission) = submit(&settings, cx);
            let (committed, result) = execute(&runtime, patch);
            assert!(result.is_ok());
            assert_eq!(committed.scan_min_duration_secs, seconds);
            settings.update(cx, |this, cx| {
                this.complete_settings_apply(submission, committed, result, cx);
                assert_eq!(this.parse(cx).unwrap().scan_min_duration_secs, seconds);
            });
        }
    }
}
