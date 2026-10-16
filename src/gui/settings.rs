use super::{
    BORDER, ButtonTooltip, DropdownItem, DropdownState, ERROR, POPOVER_MAX_REMS, StatusKind,
    UI_INSET, active_control, button, caption, column,
    components::{DROPDOWN_ROW_REMS, disabled_control},
    copyable_message, dropdown_container, dropdown_row, dropdown_trigger, form_field, icon_button,
    input::{Input, InputEvent},
    row, section_group, status_message, toggle_button, two_column,
};
use crate::{
    artwork::{ArtworkCacheReport, ArtworkManager},
    config::{
        AlbumArtworkDirection, AlbumArtworkMode, AlbumArtworkOptions, AlbumArtworkStyle,
        AnalysisWindow, AudioVisualizerOptions, AudioVisualizerStyle, Config, LogLevel, RgbColor,
        SpectrumStyle, VisualizationPalette,
    },
    core::CoreHandle,
    i18n::Localizer,
};
use anyhow::{Context as _, Result};
use gpui::{prelude::*, *};
use std::{
    cell::RefCell, collections::HashMap, path::PathBuf, rc::Rc, str::FromStr, sync::Arc,
    time::Duration,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingChoice {
    Language(usize),
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
    Roots,
    ScanMaxDepth,
    Volume,
    PlayCountThreshold,
    LogRetentionWeeks,
    MediaReadBuffer,
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
            Self::Roots => "settings-library-folders",
            Self::ScanMaxDepth => "settings-maximum-scan-depth",
            Self::Volume => "settings-volume",
            Self::PlayCountThreshold => "settings-play-count-threshold",
            Self::LogRetentionWeeks => "settings-log-retention-weeks",
            Self::MediaReadBuffer => "settings-media-read-buffer-mib",
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
            Self::Roots => settings.tr("settings-library-roots-hint"),
            Self::AlbumArtworkPeriod => settings.tr("settings-artwork-period-hint"),
            Self::AlbumArtworkFps => settings.tr("settings-animation-fps-hint"),
            Self::ScanMaxDepth => settings.tr("settings-scan-depth-hint"),
            Self::Volume => "0–100.".into(),
            Self::PlayCountThreshold => settings.tr("settings-play-count-threshold-hint"),
            Self::LogRetentionWeeks => "1–520.".into(),
            Self::MediaReadBuffer => settings.tr("settings-media-buffer-hint"),
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

    fn form(self, settings: &Settings, control: impl IntoElement) -> Div {
        let hint = self.hint(settings);
        let control = div()
            .id(("settings-field-help", self as usize))
            .w_full()
            .child(control)
            .when(!hint.is_empty(), |control| {
                control
                    .tooltip(move |_, cx| cx.new(|_| ButtonTooltip { text: hint.clone() }).into())
            });
        form_field(settings.tr(self.label_id()), control)
    }
}

fn dropdown_button(
    id: &'static str,
    label: impl Into<SharedString>,
    cx: &mut Context<Settings>,
    action: impl Fn(&mut Settings, &mut Window, &mut Context<Settings>) + 'static,
) -> Stateful<Div> {
    dropdown_trigger(id, label).on_click(cx.listener(move |this, _, window, cx| {
        action(this, window, cx);
        this.focus.focus(window, cx);
        cx.notify();
        cx.stop_propagation();
    }))
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
    toggle_button(id, icon, label, enabled, cx, move |this, _| {
        toggle(&mut this.draft);
    })
}

fn settings_group(title: impl Into<SharedString>, content: impl IntoElement) -> Div {
    section_group(title, content)
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
pub(super) const ALBUM_ARTWORK_STYLES: [AlbumArtworkStyle; 7] = [
    AlbumArtworkStyle::Plain,
    AlbumArtworkStyle::Vinyl,
    AlbumArtworkStyle::Glass,
    AlbumArtworkStyle::Halo,
    AlbumArtworkStyle::Metal,
    AlbumArtworkStyle::Card,
    AlbumArtworkStyle::Duotone,
];

pub(super) fn album_artwork_style_id(style: AlbumArtworkStyle) -> &'static str {
    match style {
        AlbumArtworkStyle::Plain => "settings-plain",
        AlbumArtworkStyle::Vinyl => "settings-vinyl",
        AlbumArtworkStyle::Glass => "settings-glass",
        AlbumArtworkStyle::Halo => "settings-halo",
        AlbumArtworkStyle::Metal => "settings-metal",
        AlbumArtworkStyle::Card => "settings-card",
        AlbumArtworkStyle::Duotone => "settings-duotone",
    }
}

pub(super) fn album_artwork_style_label(i18n: &Localizer, style: AlbumArtworkStyle) -> &str {
    i18n.text(album_artwork_style_id(style))
}

pub(super) fn audio_visualizer_style_id(style: AudioVisualizerStyle) -> &'static str {
    match style {
        AudioVisualizerStyle::Bars => "settings-bars",
        AudioVisualizerStyle::Rings => "settings-rings",
        AudioVisualizerStyle::BarsRings => "settings-bars-rings",
        AudioVisualizerStyle::StereoDensity => "settings-stereo-density",
        AudioVisualizerStyle::SpatialSpectrum => "settings-spatial-spectrum",
        AudioVisualizerStyle::FractalBeams => "settings-fractal-beams",
        AudioVisualizerStyle::FractalFlow => "settings-fractal-flow",
    }
}

pub(super) fn audio_visualizer_style_label(i18n: &Localizer, style: AudioVisualizerStyle) -> &str {
    i18n.text(audio_visualizer_style_id(style))
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
enum InterfacePage {
    Basic,
    Layout,
    AlbumArtwork,
    Desktop,
    Logging,
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
    focus: FocusHandle,
    active: bool,
    measured: Rc<RefCell<HashMap<Measured, Bounds<Pixels>>>>,
    scrolls: HashMap<&'static str, ScrollHandle>,
    active_scroll: Option<ScrollHandle>,
    _subscriptions: Vec<Subscription>,
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
    interface_page: InterfacePage,
    visualizer_panel: Option<u64>,
    artwork_panel: Option<u64>,
}

impl Focusable for Settings {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
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
            focus: cx.focus_handle(),
            active: false,
            measured: Rc::new(RefCell::new(HashMap::new())),
            scrolls: HashMap::new(),
            active_scroll: None,
            _subscriptions: vec![artwork_cache_subscription],
            draft: config.as_ref().clone(),
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
            page: SettingsPage::Library,
            visual_page: VisualPage::Common,
            interface_page: InterfacePage::Basic,
            visualizer_panel: None,
            artwork_panel: None,
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

    pub(super) fn complete_visualizer_apply(
        &mut self,
        session: u64,
        result: std::result::Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        if self.session != session {
            return;
        }
        self.applying = false;
        self.feedback = Some(result);
        cx.notify();
    }

    pub(super) fn complete_artwork_apply(
        &mut self,
        session: u64,
        result: std::result::Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        self.complete_visualizer_apply(session, result, cx);
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.applying {
            return;
        }
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
            Field::Roots,
            Field::ScanMaxDepth,
            Field::Volume,
            Field::PlayCountThreshold,
            Field::LogRetentionWeeks,
            Field::MediaReadBuffer,
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
                    !generic.contains(&family.as_str()) && family != super::fonts::SYMBOLS_FAMILY
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

    fn field(&self, field: Field) -> Div {
        let input = self.inputs[&field].clone();
        if matches!(
            field,
            Field::SpectrumDb | Field::AudioVisualizerDb | Field::SpectrogramDb
        ) {
            let common = self.inputs[&Field::VisualizationDb].clone();
            let fallback = self.draft.visualization_db_range;
            let i18n = Arc::clone(&self.i18n);
            return field.form(
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
        field.form(self, input)
    }

    fn pair(&self, first: Field, second: Field) -> Div {
        two_column(
            self.field(first).min_w(rems(180. / 16.)),
            self.field(second).min_w(rems(180. / 16.)),
        )
    }

    fn single(&self, field: Field) -> Div {
        row().child(
            self.field(field)
                .w(relative(0.5))
                .min_w(rems(180. / 16.))
                .pr_2(),
        )
    }

    fn field_state(&self, field: Field, enabled: bool, cx: &App) -> Div {
        if enabled {
            return self.field(field).min_w(rems(180. / 16.));
        }
        // Keep the draft value, but unmount the editor so disabled fields cannot take focus or edits.
        let control = div()
            .id(("inactive-setting", field as usize))
            .line_height(rems(20. / 16.))
            .text_size(rems(14. / 16.))
            .bg(rgb(super::PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .rounded(rems(5. / 16.))
            .cursor_default()
            .child(
                div()
                    .h(rems(34. / 16.))
                    .w_full()
                    .p(rems(7. / 16.))
                    .overflow_hidden()
                    .child(self.value(field, cx)),
            );
        field
            .form(self, control)
            .opacity(0.45)
            .min_w(rems(180. / 16.))
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

    fn set_value(&mut self, field: Field, value: impl Into<SharedString>, cx: &mut App) {
        self.inputs[&field].update(cx, |input, cx| input.set_text(value.into(), cx));
    }

    fn reset(&mut self, config: &Config, cx: &mut App) {
        self.session = self.session.wrapping_add(1);
        self.visualizer_panel = None;
        self.artwork_panel = None;
        self.draft = config.clone();
        self.applying = false;
        self.feedback = None;
        self.artwork_cache_report = None;
        self.artwork_cache_error = None;
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
        self.set_value(
            Field::Volume,
            format!("{:.0}", self.draft.volume * 100.),
            cx,
        );
        self.set_value(
            Field::PlayCountThreshold,
            self.draft.play_count_threshold_percent.to_string(),
            cx,
        );
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
        self.set_value(
            Field::LayoutSnapGrid,
            self.draft.layout_snap_grid_px.to_string(),
            cx,
        );
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
            Field::AudioVisualizerSensitivity,
            self.draft.audio_visualizer_sensitivity.to_string(),
            cx,
        );
        self.set_value(
            Field::AudioVisualizerRotationSpeed,
            self.draft.audio_visualizer_rotation_speed.to_string(),
            cx,
        );
        self.set_value(
            Field::AudioVisualizerBarWidth,
            self.draft.audio_visualizer_bar_width.to_string(),
            cx,
        );
        self.set_value(
            Field::AudioVisualizerBarGlowLayers,
            self.draft.audio_visualizer_bar_glow_layers.to_string(),
            cx,
        );
        self.set_value(
            Field::AudioVisualizerRingOpacity,
            self.draft.audio_visualizer_ring_opacity.to_string(),
            cx,
        );
        self.set_value(
            Field::AudioVisualizerBloomIntensity,
            self.draft.audio_visualizer_bloom_intensity.to_string(),
            cx,
        );
        self.set_value(
            Field::AudioVisualizerInnerDiameter,
            self.draft.audio_visualizer_inner_diameter.to_string(),
            cx,
        );
        self.set_value(
            Field::AudioVisualizerPrimaryColor,
            self.draft.audio_visualizer_primary_color.to_string(),
            cx,
        );
        self.set_value(
            Field::AudioVisualizerSecondaryColor,
            self.draft.audio_visualizer_secondary_color.to_string(),
            cx,
        );
        self.set_value(
            Field::AudioVisualizerDb,
            self.draft
                .audio_visualizer_db_range
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
        self.set_value(
            Field::AlbumArtworkPeriod,
            self.draft.album_artwork_period_seconds.to_string(),
            cx,
        );
        self.set_value(
            Field::AlbumArtworkFps,
            self.draft.album_artwork_fps.to_string(),
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
        config.volume = f32::from(self.number::<u8>(Field::Volume, cx)?) / 100.;
        config.play_count_threshold_percent = self.number(Field::PlayCountThreshold, cx)?;
        config.log_retention_weeks = self.number(Field::LogRetentionWeeks, cx)?;
        config.media_read_buffer_mb = self.number(Field::MediaReadBuffer, cx)?;
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
                        this.choose_device(index, cx);
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
                let open =
                    this.dropdown.is_open() && this.dropdown_anchor == Measured::SettingsFont;
                if !open {
                    if matches!(key, "up" | "down") {
                        this.open_font_dropdown(false, cx);
                        cx.stop_propagation();
                        cx.notify();
                    }
                } else if key == "escape" {
                    this.dropdown.close();
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
                    .w(rems(27. / 16.))
                    .h(rems(34. / 16.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .font_family(super::fonts::SYMBOLS_FAMILY)
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
        Field::Font.form(self, control)
    }

    pub(super) fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let config = self.handle.config_snapshot();
        self.initialize_settings(cx);
        self.dropdown.close();
        self.reset(&config, cx);
        self.active = true;
        self.focus.focus(window, cx);
        cx.notify();
    }

    pub(super) fn open_visualizer(
        &mut self,
        panel_id: u64,
        options: AudioVisualizerOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open(window, cx);
        self.visualizer_panel = Some(panel_id);
        self.page = SettingsPage::Visualizations;
        self.visual_page = VisualPage::AudioVisualizer;
        self.draft.audio_visualizer_style = options.style;
        self.draft.audio_visualizer_grid = options.reference_lines;
        self.draft.audio_visualizer_labels = options.labels;
        cx.notify();
    }

    pub(super) fn open_artwork(
        &mut self,
        panel_id: u64,
        options: AlbumArtworkOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open(window, cx);
        self.artwork_panel = Some(panel_id);
        self.page = SettingsPage::Interface;
        self.interface_page = InterfacePage::AlbumArtwork;
        self.draft.album_artwork_style = options.style;
        self.draft.album_artwork_mode = options.mode;
        self.draft.album_artwork_direction = options.direction;
        self.draft.album_artwork_period_seconds = options.period_seconds;
        self.fill(cx);
        cx.notify();
    }

    fn apply_settings(&mut self, cx: &mut Context<Self>) {
        if self.applying {
            return;
        }
        if let Some(panel_id) = self.artwork_panel {
            let period = self.value(Field::AlbumArtworkPeriod, cx).parse::<f32>();
            let Ok(period_seconds) = period else {
                self.feedback = Some(Err(self
                    .i18n
                    .text("settings-artwork-period-hint")
                    .to_owned()));
                cx.notify();
                return;
            };
            if !period_seconds.is_finite() || period_seconds <= 0.0 {
                self.feedback = Some(Err(self
                    .i18n
                    .text("settings-artwork-period-hint")
                    .to_owned()));
                cx.notify();
                return;
            }
            self.applying = true;
            self.feedback = None;
            cx.emit(SettingsEvent::ApplyArtwork {
                panel_id,
                options: AlbumArtworkOptions {
                    style: self.draft.album_artwork_style,
                    mode: self.draft.album_artwork_mode,
                    direction: self.draft.album_artwork_direction,
                    period_seconds,
                },
                session: self.session,
            });
            cx.notify();
            return;
        }
        if let Some(panel_id) = self.visualizer_panel {
            self.applying = true;
            self.feedback = None;
            cx.emit(SettingsEvent::ApplyVisualizer {
                panel_id,
                options: AudioVisualizerOptions::from(&self.draft),
                session: self.session,
            });
            cx.notify();
            return;
        }
        let config = match self.parse(cx) {
            Ok(config) => config,
            Err(error) => {
                let mut recovery = String::new();
                for (inactive, key) in [
                    (
                        self.draft.spectrum_style == SpectrumStyle::Solid,
                        "settings-spectrum-style-recovery",
                    ),
                    (
                        !self.draft.spectrum_peaks,
                        "settings-spectrum-peaks-recovery",
                    ),
                    (
                        self.draft.album_artwork_mode == AlbumArtworkMode::Static,
                        "settings-artwork-recovery",
                    ),
                ] {
                    if inactive {
                        if !recovery.is_empty() {
                            recovery.push(' ');
                        }
                        recovery.push_str(self.i18n.text(key));
                    }
                }
                self.feedback = Some(Err(self.i18n.format(
                    "settings-invalid",
                    &[
                        ("error", format!("{error:#}").into()),
                        ("recovery", recovery.as_str().into()),
                    ],
                )));
                cx.notify();
                return;
            }
        };
        self.applying = true;
        self.feedback = None;
        let session = self.session;
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            let response = cx
                .background_executor()
                .spawn(async move { handle.apply_gui_settings(config) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.session != session {
                    return;
                }
                this.applying = false;
                this.feedback = Some(if response.ok {
                    Ok(())
                } else {
                    Err(response
                        .error
                        .unwrap_or_else(|| this.tr("settings-save-error").to_string()))
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
        self.font_value = value.clone();
        self.draft.ui_font = value.clone();
        self.inputs[&Field::Font].update(cx, |input, cx| input.set_text(value, cx));
    }

    fn choose_device(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some((_, item)) = self.dropdown.filtered_item(index) else {
            return;
        };
        match item.value {
            SettingChoice::Language(index) => {
                self.draft.ui_language = ["auto", "en-US", "zh-CN"][index].to_owned()
            }
            SettingChoice::Device(device_index) => {
                if device_index == 0 {
                    self.draft.output_device = None;
                } else if let Some(name) = self.devices.get(device_index - 1) {
                    self.draft.output_device = Some(name.clone());
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
            SettingChoice::SpectrumStyle(value) => self.draft.spectrum_style = value,
            SettingChoice::AudioVisualizerStyle(value) => self.draft.audio_visualizer_style = value,
            SettingChoice::Palette(value) => self.draft.visual_palette = value,
            SettingChoice::Fft(value) => self.draft.analysis_fft_size = value,
            SettingChoice::LogLevel(value) => self.draft.log_level = value,
            SettingChoice::Window(value) => self.draft.analysis_window = value,
            SettingChoice::AlbumArtworkStyle(value) => self.draft.album_artwork_style = value,
            SettingChoice::AlbumArtworkMode(value) => self.draft.album_artwork_mode = value,
            SettingChoice::AlbumArtworkDirection(value) => {
                self.draft.album_artwork_direction = value
            }
        }
        self.dropdown.select_index(index);
        self.dropdown.close();
        cx.notify();
    }

    fn open_setting_dropdown(&mut self, items: Vec<DropdownItem<SettingChoice>>, selected: usize) {
        if let Some(item) = items.first() {
            self.dropdown_anchor = match item.value {
                SettingChoice::Language(_) => Measured::Language,
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
        self.dropdown.open(items);
        self.dropdown.select_index(selected);
        if let Some(index) = self.dropdown.selected_index() {
            self.device_scroll
                .scroll_to_item(index, ScrollStrategy::Nearest);
        }
    }

    fn settings_dropdown_key(&mut self, key: &str, window: &Window, cx: &mut Context<Self>) {
        match key {
            "up" => self.dropdown.move_previous(),
            "down" => self.dropdown.move_next(),
            "pageup" | "pagedown" => {
                if let Some(index) = self.dropdown.selected_index() {
                    let viewport_height = self
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
                    self.dropdown.select_index(next);
                }
            }
            "enter" | "space" => {
                if let Some(index) = self.dropdown.selected_index() {
                    self.choose_device(index, cx);
                }
                cx.stop_propagation();
                return;
            }
            _ => return,
        }
        if let Some(index) = self.dropdown.selected_index() {
            self.device_scroll
                .scroll_to_item(index, ScrollStrategy::Nearest);
        }
        cx.stop_propagation();
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
        let session = self.session;
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
                    && this.session == session
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

    fn artwork_cache_settings(&self, cx: &mut Context<Self>) -> Div {
        let busy = self.artwork_cache_busy.is_some();
        let mut content = column().gap_2().child(
            row()
                .items_end()
                .gap_2()
                .child(
                    form_field(
                        self.tr("settings-expire-files-older-than-days"),
                        div()
                            .id("settings-artwork-cache-age")
                            .child(self.artwork_cache_age.clone()),
                    )
                    .min_w_0()
                    .flex_1(),
                )
                .child(disabled_control(
                    icon_button(
                        "settings-artwork-cache-calculate",
                        "󰃬",
                        self.tr("settings-calculate-usage"),
                        cx,
                        |this, _, cx| this.maintain_artwork_cache(false, cx),
                    ),
                    busy,
                ))
                .child(disabled_control(
                    icon_button(
                        "settings-artwork-cache-clean",
                        "󰆴",
                        self.tr("settings-clean-expired"),
                        cx,
                        |this, _, cx| this.maintain_artwork_cache(true, cx),
                    ),
                    busy,
                )),
        );
        if let Some((days, clean)) = self.artwork_cache_busy {
            content = content.child(status_message(
                self.i18n.format(
                    if clean {
                        "settings-cache-cleaning"
                    } else {
                        "settings-cache-calculating"
                    },
                    &[("days", days.to_string().into())],
                ),
                StatusKind::Info,
            ));
        }
        if let Some((days, clean, report)) = &self.artwork_cache_report {
            content = content
                .child(status_message(
                    self.i18n.format(
                        if *clean {
                            "settings-cache-cleaned"
                        } else {
                            "settings-cache-calculated"
                        },
                        &[("days", days.to_string().into())],
                    ),
                    if report.failed_files == 0 {
                        StatusKind::Success
                    } else {
                        StatusKind::Error
                    },
                ))
                .child(caption(self.i18n.format(
                    "settings-cache-total",
                    &[
                        ("files", report.files.to_string().into()),
                        ("bytes", report.bytes.to_string().into()),
                    ],
                )))
                .child(caption(self.i18n.format(
                    "settings-cache-expired",
                    &[
                        ("files", report.expired_files.to_string().into()),
                        ("bytes", report.expired_bytes.to_string().into()),
                    ],
                )))
                .child(caption(self.i18n.format(
                    "settings-cache-protected",
                    &[
                        ("files", report.protected_files.to_string().into()),
                        ("bytes", report.protected_bytes.to_string().into()),
                    ],
                )))
                .child(caption(self.i18n.format(
                    "settings-cache-removed",
                    &[
                        ("files", report.removed_files.to_string().into()),
                        ("bytes", report.removed_bytes.to_string().into()),
                    ],
                )))
                .child(status_message(
                    self.i18n.format(
                        "settings-cache-failures",
                        &[("count", report.failed_files.to_string().into())],
                    ),
                    if report.failed_files == 0 {
                        StatusKind::Info
                    } else {
                        StatusKind::Error
                    },
                ));
            if let Some(error) = &report.first_error {
                content = content.child(
                    copyable_message("copy-artwork-cache-file-error", error.clone(), cx)
                        .text_color(rgb(ERROR)),
                );
            }
            if report.failed_files > 0 {
                content = content.child(caption(self.tr("settings-cache-permissions-help")));
            }
        }
        if let Some(error) = &self.artwork_cache_error {
            content = content.child(
                copyable_message("copy-artwork-cache-error", error.clone(), cx)
                    .text_color(rgb(ERROR)),
            );
        }
        settings_group(self.tr("settings-artwork-cache"), content)
    }

    fn library_settings(&mut self, cx: &mut Context<Self>) -> Div {
        let library = settings_group(
            self.tr("settings-library"),
            column()
                .gap_2()
                .child(
                    row()
                        .items_start()
                        .flex_wrap()
                        .gap_3()
                        .child(self.field(Field::Roots).flex_1().min_w(rems(260. / 16.)))
                        .child(
                            self.field(Field::ScanMaxDepth)
                                .flex_1()
                                .min_w(rems(220. / 16.)),
                        ),
                )
                .child(two_column(
                    settings_group(
                        self.tr("settings-relocation"),
                        column()
                            .gap_2()
                            .child(caption(self.tr("settings-relocation-help")))
                            .child(button(
                                "settings-same-files",
                                self.tr("settings-review-candidates"),
                                cx,
                                |this, _, cx| {
                                    this.dropdown.close();
                                    cx.emit(SettingsEvent::ReviewDuplicates);
                                },
                            )),
                    ),
                    self.artwork_cache_settings(cx),
                )),
        );
        column().child(library)
    }

    fn playback_settings(&mut self, cx: &mut Context<Self>) -> Div {
        let chosen_device: SharedString = self
            .draft
            .output_device
            .clone()
            .unwrap_or_else(|| self.tr("settings-system-default").to_string())
            .into();
        let device = column()
            .flex_shrink_0()
            .gap_1()
            .child(caption(self.tr("settings-output-device")))
            .child(
                dropdown_button(
                    "output-device-selector",
                    chosen_device.clone(),
                    cx,
                    |this, _, _| {
                        let open = this.dropdown.is_open();
                        if open {
                            this.dropdown.close();
                        } else {
                            let selected = this
                                .draft
                                .output_device
                                .as_ref()
                                .and_then(|name| {
                                    this.devices.iter().position(|device| device == name)
                                })
                                .map_or(0, |index| index + 1);
                            let items = std::iter::once(DropdownItem::new(
                                SettingChoice::Device(0),
                                this.i18n.text("settings-system-default"),
                            ))
                            .chain(
                                this.devices.iter().enumerate().map(|(index, device)| {
                                    DropdownItem::new(
                                        SettingChoice::Device(index + 1),
                                        device.clone(),
                                    )
                                }),
                            );
                            this.dropdown_anchor = Measured::Device;
                            this.dropdown.open(items);
                            this.dropdown.select_index(selected);
                        }
                        if let Some(index) = this.dropdown.selected_index() {
                            this.device_scroll
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
                .child(self.measurement(Measured::Device, cx)),
            );
        let ffmpeg = self.draft.ffmpeg_enabled;
        let remix = self.draft.pipewire_auto_mix;
        let playback = settings_group(
            self.tr("settings-playback"),
            column()
                .gap_2()
                .child(self.pair(Field::Volume, Field::PlayCountThreshold)),
        );
        let audio = settings_group(
            self.tr("settings-audio"),
            column()
                .gap_2()
                .child(
                    row()
                        .items_start()
                        .flex_wrap()
                        .gap_3()
                        .child(device.flex_1().min_w(rems(260. / 16.)))
                        .child(
                            self.field(Field::MediaReadBuffer)
                                .flex_1()
                                .min_w(rems(240. / 16.)),
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
                                self.i18n.format(
                                    "settings-ffmpeg-status",
                                    &[
                                        (
                                            "state",
                                            self.i18n
                                                .text(if ffmpeg {
                                                    "settings-on"
                                                } else {
                                                    "settings-off"
                                                })
                                                .into(),
                                        ),
                                        (
                                            "detail",
                                            ffmpeg_hint(&self.i18n, &self.ffmpeg_status)
                                                .to_owned()
                                                .into(),
                                        ),
                                    ],
                                ),
                                cx,
                                |this, _, cx| {
                                    this.draft.ffmpeg_enabled = !this.draft.ffmpeg_enabled;
                                    cx.notify();
                                },
                            ),
                            ffmpeg,
                        ))
                        .child(visual_switch(
                            &self.i18n,
                            "settings-pipewire-auto-mix",
                            "󱀞",
                            self.tr("settings-pipewire-channel-remix"),
                            remix,
                            cx,
                            |draft| draft.pipewire_auto_mix = !draft.pipewire_auto_mix,
                        )),
                ),
        );
        column().gap_3().child(playback).child(audio)
    }

    fn basic_settings(&mut self, cx: &mut Context<Self>) -> Div {
        let draft = &self.draft;
        let language = dropdown_button(
            "settings-language",
            match draft.ui_language.as_str() {
                "en-US" => SharedString::from("English"),
                "zh-CN" => SharedString::from("简体中文"),
                _ => self.tr("settings-language-auto"),
            },
            cx,
            |this, _, _| {
                let values = ["auto", "en-US", "zh-CN"];
                let selected = values
                    .iter()
                    .position(|value| *value == this.draft.ui_language)
                    .unwrap_or(0);
                let labels = [
                    this.i18n.text("settings-language-auto"),
                    "English",
                    "简体中文",
                ];
                let items = labels
                    .into_iter()
                    .enumerate()
                    .map(|(index, label)| {
                        DropdownItem::new(SettingChoice::Language(index), label.to_owned())
                    })
                    .collect();
                this.open_setting_dropdown(items, selected);
            },
        )
        .relative()
        .child(self.measurement(Measured::Language, cx));
        let interface = settings_group(
            self.tr("settings-interface"),
            column()
                .gap_2()
                .child(form_field(self.tr("settings-language"), language))
                .child(caption(self.tr("settings-language-restart")))
                .child(two_column(
                    self.font_control(cx).min_w(rems(180. / 16.)),
                    self.field(Field::Scale).min_w(rems(180. / 16.)),
                )),
        );
        column().child(interface)
    }

    fn layout_settings(&mut self, cx: &mut Context<Self>) -> Div {
        column().child(two_column(
            Field::LayoutSnapGrid.form(
                self,
                row()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(self.inputs[&Field::LayoutSnapGrid].clone()),
                    )
                    .child(visual_switch(
                        &self.i18n,
                        "settings-layout-snapping",
                        "󰐃",
                        self.tr("settings-layout-snapping"),
                        self.draft.layout_snap_enabled,
                        cx,
                        |draft| draft.layout_snap_enabled = !draft.layout_snap_enabled,
                    )),
            ),
            div(),
        ))
    }

    fn desktop_settings(&mut self, cx: &mut Context<Self>) -> Div {
        let mpris = self.draft.mpris_enabled;
        let tray = self.draft.tray_enabled;
        column().child(
            row()
                .flex_wrap()
                .gap_2()
                .child(active_control(
                    icon_button(
                        "settings-mpris",
                        "󰐹",
                        self.i18n.format(
                            "settings-mpris-status",
                            &[
                                (
                                    "state",
                                    self.i18n
                                        .text(if mpris { "settings-on" } else { "settings-off" })
                                        .into(),
                                ),
                                ("detail", self.mpris_status.to_owned().into()),
                            ],
                        ),
                        cx,
                        |this, _, cx| {
                            this.draft.mpris_enabled = !this.draft.mpris_enabled;
                            cx.notify();
                        },
                    ),
                    mpris,
                ))
                .child(visual_switch(
                    &self.i18n,
                    "settings-tray",
                    "",
                    self.tr("settings-system-tray"),
                    tray,
                    cx,
                    |draft| draft.tray_enabled = !draft.tray_enabled,
                )),
        )
    }

    fn logging_settings(&mut self, cx: &mut Context<Self>) -> Div {
        let draft = &self.draft;
        let log_level_dropdown = dropdown_button(
            "settings-log-level",
            SettingChoice::LogLevel(draft.log_level).label(&self.i18n),
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
                    .position(|value| *value == this.draft.log_level)
                    .unwrap_or(0);
                this.open_setting_dropdown(
                    values
                        .into_iter()
                        .map(|value| {
                            DropdownItem::new(
                                SettingChoice::LogLevel(value),
                                SettingChoice::LogLevel(value).label(&this.i18n),
                            )
                        })
                        .collect(),
                    selected,
                );
            },
        )
        .relative()
        .child(self.measurement(Measured::SettingsLogLevel, cx));
        let logging = settings_group(
            self.tr("settings-logging"),
            column()
                .gap_2()
                .child(
                    row()
                        .items_start()
                        .flex_wrap()
                        .gap_3()
                        .child(
                            column()
                                .flex_1()
                                .min_w(rems(180. / 16.))
                                .gap_1()
                                .child(caption(self.tr("settings-log-level")))
                                .child(log_level_dropdown),
                        )
                        .child(
                            self.field(Field::LogRetentionWeeks)
                                .flex_1()
                                .min_w(rems(180. / 16.)),
                        ),
                )
                .child(row().child(visual_switch(
                    &self.i18n,
                    "settings-log-file",
                    "",
                    self.tr("settings-write-logs-to-disk"),
                    draft.log_to_file,
                    cx,
                    |draft| draft.log_to_file = !draft.log_to_file,
                ))),
        );
        column()
            .child(caption(self.tr("settings-logging-scope")))
            .child(logging)
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> Div {
        let mut sidebar = column()
            .gap_1()
            .w(rems(11.))
            .flex_shrink_0()
            .p(gpui::px(UI_INSET))
            .border_r_1()
            .border_color(rgb(BORDER));
        sidebar = sidebar.child(self.tab_button(
            "nav-library",
            self.tr("settings-library"),
            self.page == SettingsPage::Library,
            cx,
            |settings| settings.page = SettingsPage::Library,
        ));
        sidebar = sidebar.child(self.tab_button(
            "nav-playback-audio",
            self.tr("settings-playback-audio"),
            self.page == SettingsPage::Playback,
            cx,
            |settings| settings.page = SettingsPage::Playback,
        ));
        sidebar = sidebar.child(self.tab_button(
            "nav-interface",
            self.tr("settings-interface"),
            self.page == SettingsPage::Interface,
            cx,
            |settings| settings.page = SettingsPage::Interface,
        ));
        if self.page == SettingsPage::Interface {
            let mut children = column().pl_3().gap_1();
            children = children.child(self.tab_button(
                "nav-basic",
                self.tr("settings-basic"),
                self.interface_page == InterfacePage::Basic,
                cx,
                |settings| settings.interface_page = InterfacePage::Basic,
            ));
            children = children.child(self.tab_button(
                "nav-layout",
                self.tr("settings-layout"),
                self.interface_page == InterfacePage::Layout,
                cx,
                |settings| settings.interface_page = InterfacePage::Layout,
            ));
            children = children.child(self.tab_button(
                "nav-album-artwork",
                self.tr("settings-album-artwork"),
                self.interface_page == InterfacePage::AlbumArtwork,
                cx,
                |settings| settings.interface_page = InterfacePage::AlbumArtwork,
            ));
            children = children.child(self.tab_button(
                "nav-desktop-integration",
                self.tr("settings-desktop-integration"),
                self.interface_page == InterfacePage::Desktop,
                cx,
                |settings| settings.interface_page = InterfacePage::Desktop,
            ));
            children = children.child(self.tab_button(
                "nav-diagnostic-logging",
                self.tr("settings-diagnostic-logging"),
                self.interface_page == InterfacePage::Logging,
                cx,
                |settings| settings.interface_page = InterfacePage::Logging,
            ));
            sidebar = sidebar.child(children);
        }
        sidebar = sidebar.child(self.tab_button(
            "nav-visualizations",
            self.tr("settings-visualizations"),
            self.page == SettingsPage::Visualizations,
            cx,
            |settings| settings.page = SettingsPage::Visualizations,
        ));
        if self.page == SettingsPage::Visualizations {
            let mut children = column().pl_3().gap_1();
            children = children.child(self.tab_button(
                "nav-shared-settings",
                self.tr("settings-shared-settings"),
                self.visual_page == VisualPage::Common,
                cx,
                |settings| settings.visual_page = VisualPage::Common,
            ));
            children = children.child(self.tab_button(
                "nav-spectrum",
                self.tr("settings-spectrum"),
                self.visual_page == VisualPage::Spectrum,
                cx,
                |settings| settings.visual_page = VisualPage::Spectrum,
            ));
            children = children.child(self.tab_button(
                "nav-spectrogram",
                self.tr("settings-spectrogram"),
                self.visual_page == VisualPage::Spectrogram,
                cx,
                |settings| settings.visual_page = VisualPage::Spectrogram,
            ));
            children = children.child(self.tab_button(
                "nav-waveform",
                self.tr("settings-waveform"),
                self.visual_page == VisualPage::Waveform,
                cx,
                |settings| settings.visual_page = VisualPage::Waveform,
            ));
            children = children.child(self.tab_button(
                "nav-audio-visualizer",
                self.tr("settings-audio-visualizer"),
                self.visual_page == VisualPage::AudioVisualizer,
                cx,
                |settings| settings.visual_page = VisualPage::AudioVisualizer,
            ));
            sidebar = sidebar.child(children);
        }
        sidebar
    }

    fn artwork_settings(&mut self, cx: &mut Context<Self>) -> Div {
        let settings = &*self;
        let draft = &settings.draft;
        let style = dropdown_button(
            "album-artwork-style",
            SettingChoice::AlbumArtworkStyle(draft.album_artwork_style).label(&self.i18n),
            cx,
            |this, _, _| {
                let values = ALBUM_ARTWORK_STYLES;
                let selected = values
                    .iter()
                    .position(|value| *value == this.draft.album_artwork_style)
                    .unwrap_or(0);
                this.open_setting_dropdown(
                    values
                        .into_iter()
                        .map(|value| {
                            let choice = SettingChoice::AlbumArtworkStyle(value);
                            DropdownItem::new(choice, choice.label(&this.i18n))
                        })
                        .collect(),
                    selected,
                );
            },
        )
        .relative()
        .child(self.measurement(Measured::AlbumArtworkStyle, cx));
        let mode = dropdown_button(
            "album-artwork-mode",
            SettingChoice::AlbumArtworkMode(draft.album_artwork_mode).label(&self.i18n),
            cx,
            |this, _, _| {
                let values = [AlbumArtworkMode::Static, AlbumArtworkMode::Rotating];
                let selected = values
                    .iter()
                    .position(|value| *value == this.draft.album_artwork_mode)
                    .unwrap_or(0);
                this.open_setting_dropdown(
                    values
                        .into_iter()
                        .map(|value| {
                            let choice = SettingChoice::AlbumArtworkMode(value);
                            DropdownItem::new(choice, choice.label(&this.i18n))
                        })
                        .collect(),
                    selected,
                );
            },
        )
        .relative()
        .child(self.measurement(Measured::AlbumArtworkMode, cx));
        let direction = dropdown_button(
            "album-artwork-direction",
            SettingChoice::AlbumArtworkDirection(draft.album_artwork_direction).label(&self.i18n),
            cx,
            |this, _, _| {
                let values = [
                    AlbumArtworkDirection::Clockwise,
                    AlbumArtworkDirection::Counterclockwise,
                ];
                let selected = values
                    .iter()
                    .position(|value| *value == this.draft.album_artwork_direction)
                    .unwrap_or(0);
                this.open_setting_dropdown(
                    values
                        .into_iter()
                        .map(|value| {
                            let choice = SettingChoice::AlbumArtworkDirection(value);
                            DropdownItem::new(choice, choice.label(&this.i18n))
                        })
                        .collect(),
                    selected,
                );
            },
        )
        .relative()
        .child(self.measurement(Measured::AlbumArtworkDirection, cx));
        let rotating = draft.album_artwork_mode == AlbumArtworkMode::Rotating;
        column()
            .gap_3()
            .child(settings_group(
                settings.tr(if settings.artwork_panel.is_some() {
                    "settings-this-instance"
                } else {
                    "settings-new-instance-defaults"
                }),
                form_field(settings.tr("settings-style"), style),
            ))
            .child(settings_group(
                settings.tr("settings-animation"),
                column()
                    .gap_2()
                    .child(two_column(
                        form_field(settings.tr("settings-mode"), mode).min_w(rems(180. / 16.)),
                        div().min_w(rems(180. / 16.)).child(disabled_control(
                            form_field(settings.tr("settings-direction"), direction)
                                .id("settings-artwork-direction-field"),
                            !rotating,
                        )),
                    ))
                    .child(two_column(
                        settings.field_state(Field::AlbumArtworkPeriod, rotating, cx),
                        column().when(settings.artwork_panel.is_none(), |column| {
                            column
                                .min_w(rems(180. / 16.))
                                .gap_2()
                                .child(settings.field_state(Field::AlbumArtworkFps, true, cx))
                                .child(caption(settings.tr("settings-artwork-motion-help")))
                        }),
                    )),
            ))
    }

    fn visual_settings(&mut self, cx: &mut Context<Settings>) -> Div {
        let settings = &*self;
        let draft = &settings.draft;
        match settings.visual_page {
            VisualPage::Common => {
                let palette = dropdown_button(
                    "visual-palette",
                    visualization_palette_label(&self.i18n, draft.visual_palette),
                    cx,
                    |this, _, _| {
                        let values = [
                            VisualizationPalette::TokyoNight,
                            VisualizationPalette::Deadbeef,
                        ];
                        let selected = values
                            .iter()
                            .position(|value| *value == this.draft.visual_palette)
                            .unwrap_or(0);
                        this.open_setting_dropdown(
                            values
                                .into_iter()
                                .map(|value| {
                                    DropdownItem::new(
                                        SettingChoice::Palette(value),
                                        visualization_palette_label(&this.i18n, value),
                                    )
                                })
                                .collect(),
                            selected,
                        );
                    },
                )
                .relative()
                .child(self.measurement(Measured::SettingsStyle, cx));
                let fft = dropdown_button(
                    "analysis-fft",
                    draft.analysis_fft_size.to_string(),
                    cx,
                    |this, _, _| {
                        let values = [512, 1024, 2048, 4096, 8192, 16384, 32768];
                        let selected = values
                            .iter()
                            .position(|value| *value == this.draft.analysis_fft_size)
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
                .child(self.measurement(Measured::SettingsFft, cx));
                let window = dropdown_button(
                    "analysis-window",
                    SettingChoice::Window(draft.analysis_window).label(&self.i18n),
                    cx,
                    |this, _, _| {
                        let values = [
                            AnalysisWindow::Hann,
                            AnalysisWindow::BlackmanHarris,
                            AnalysisWindow::None,
                        ];
                        let selected = values
                            .iter()
                            .position(|value| *value == this.draft.analysis_window)
                            .unwrap_or(0);
                        this.open_setting_dropdown(
                            values
                                .into_iter()
                                .map(|value| {
                                    DropdownItem::new(
                                        SettingChoice::Window(value),
                                        SettingChoice::Window(value).label(&this.i18n),
                                    )
                                })
                                .collect(),
                            selected,
                        );
                    },
                )
                .relative()
                .child(self.measurement(Measured::SettingsWindow, cx));
                column()
                    .gap_3()
                    .child(settings_group(
                        settings.tr("settings-appearance"),
                        column()
                            .gap_2()
                            .child(two_column(
                                settings.field(Field::Background).min_w(rems(180. / 16.)),
                                form_field(settings.tr("settings-color-scheme"), palette)
                                    .min_w(rems(180. / 16.)),
                            ))
                            .id("settings-appearance-help")
                            .tooltip(settings_tooltip(settings.tr("settings-appearance-help"))),
                    ))
                    .child(settings_group(
                        settings.tr("settings-display-range"),
                        settings.single(Field::VisualizationDb),
                    ))
                    .child(settings_group(
                        settings.tr("settings-frequency-analysis"),
                        column()
                            .gap_2()
                            .child(two_column(
                                form_field(settings.tr("settings-fft-size-samples"), fft)
                                    .min_w(rems(180. / 16.)),
                                form_field(settings.tr("settings-fft-window"), window)
                                    .min_w(rems(180. / 16.)),
                            ))
                            .child(settings.single(Field::Fps))
                            .id("settings-analysis-help")
                            .tooltip(settings_tooltip(settings.tr("settings-analysis-help"))),
                    ))
                    .child(settings_group(
                        settings.tr("settings-processing-cache"),
                        row()
                            .gap_2()
                            .child(visual_switch(
                                &self.i18n,
                                "settings-still-mode",
                                "",
                                settings.tr("settings-still-mode"),
                                draft.still_mode,
                                cx,
                                |draft| draft.still_mode = !draft.still_mode,
                            ))
                            .child(visual_switch(
                                &self.i18n,
                                "visual-cache",
                                "",
                                settings.tr("settings-rendering-cache"),
                                draft.visualization_cache,
                                cx,
                                |draft| draft.visualization_cache = !draft.visualization_cache,
                            )),
                    ))
            }
            VisualPage::Spectrum => {
                let solid = draft.spectrum_style == SpectrumStyle::Solid;
                let style = dropdown_button(
                    "spectrum-style",
                    spectrum_style_label(&self.i18n, draft.spectrum_style),
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
                            .position(|value| *value == this.draft.spectrum_style)
                            .unwrap_or(0);
                        this.open_setting_dropdown(
                            values
                                .into_iter()
                                .map(|value| {
                                    DropdownItem::new(
                                        SettingChoice::SpectrumStyle(value),
                                        spectrum_style_label(&this.i18n, value),
                                    )
                                })
                                .collect(),
                            selected,
                        );
                    },
                )
                .relative()
                .child(self.measurement(Measured::SettingsStyle, cx));
                column()
                    .gap_3()
                    .child(settings_group(
                        settings.tr("settings-appearance"),
                        column()
                            .gap_2()
                            .child(two_column(
                                form_field(settings.tr("settings-style"), style)
                                    .min_w(rems(180. / 16.)),
                                settings.field_state(Field::SpectrumBars, !solid, cx),
                            ))
                            .child(two_column(
                                settings.field_state(Field::SpectrumBarWidth, !solid, cx),
                                settings.field_state(Field::SpectrumGap, !solid, cx),
                            ))
                            .child(settings.single(Field::SpectrumDb))
                            .child(
                                row()
                                    .flex_wrap()
                                    .gap_2()
                                    .child(visual_switch(
                                        &self.i18n,
                                        "spectrum-interpolate",
                                        "",
                                        settings.tr("settings-frequency-interpolation"),
                                        draft.spectrum_interpolate,
                                        cx,
                                        |draft| {
                                            draft.spectrum_interpolate = !draft.spectrum_interpolate
                                        },
                                    ))
                                    .child(visual_switch(
                                        &self.i18n,
                                        "spectrum-grid",
                                        "",
                                        settings.tr("settings-grid-lines"),
                                        draft.spectrum_grid,
                                        cx,
                                        |draft| draft.spectrum_grid = !draft.spectrum_grid,
                                    ))
                                    .child(visual_switch(
                                        &self.i18n,
                                        "spectrum-labels",
                                        "",
                                        settings.tr("settings-axis-labels"),
                                        draft.spectrum_labels,
                                        cx,
                                        |draft| draft.spectrum_labels = !draft.spectrum_labels,
                                    )),
                            ),
                    ))
                    .child(settings_group(
                        settings.tr("settings-bar-motion"),
                        column()
                            .gap_2()
                            .child(settings.pair(Field::SpectrumBarHold, Field::SpectrumBarGravity))
                            .child(settings.single(Field::SpectrumSmoothing)),
                    ))
                    .child(settings_group(
                        settings.tr("settings-peak-markers"),
                        column()
                            .gap_2()
                            .child(row().child(visual_switch(
                                &self.i18n,
                                "spectrum-peaks",
                                "",
                                settings.tr("settings-peak-markers"),
                                draft.spectrum_peaks,
                                cx,
                                |draft| draft.spectrum_peaks = !draft.spectrum_peaks,
                            )))
                            .child(two_column(
                                settings.field_state(Field::SpectrumHold, draft.spectrum_peaks, cx),
                                settings.field_state(
                                    Field::SpectrumGravity,
                                    draft.spectrum_peaks,
                                    cx,
                                ),
                            )),
                    ))
            }
            VisualPage::Spectrogram => column()
                .gap_3()
                .child(settings_group(
                    settings.tr("settings-display"),
                    column()
                        .gap_2()
                        .child(settings.pair(Field::SpectrogramDb, Field::SpectrogramHistory))
                        .child(row().child(visual_switch(
                            &self.i18n,
                            "spectrogram-labels",
                            "",
                            settings.tr("settings-axis-labels"),
                            draft.spectrogram_labels,
                            cx,
                            |draft| draft.spectrogram_labels = !draft.spectrogram_labels,
                        ))),
                ))
                .child(settings_group(
                    settings.tr("settings-frequency-sampling"),
                    column()
                        .gap_2()
                        .child(settings.pair(
                            Field::SpectrogramSamplingPointsScale,
                            Field::SpectrogramInterpolationPoints,
                        ))
                        .child(row().child(visual_switch(
                            &self.i18n,
                            "spectrogram-interpolate",
                            "",
                            settings.tr("settings-frequency-interpolation"),
                            draft.spectrogram_interpolate,
                            cx,
                            |draft| draft.spectrogram_interpolate = !draft.spectrogram_interpolate,
                        ))),
                )),
            VisualPage::AudioVisualizer => {
                let style = dropdown_button(
                    "audio-visualizer-style",
                    audio_visualizer_style_label(&self.i18n, draft.audio_visualizer_style),
                    cx,
                    |this, _, _| {
                        let values = [
                            AudioVisualizerStyle::Bars,
                            AudioVisualizerStyle::Rings,
                            AudioVisualizerStyle::BarsRings,
                            AudioVisualizerStyle::StereoDensity,
                            AudioVisualizerStyle::SpatialSpectrum,
                            AudioVisualizerStyle::FractalBeams,
                            AudioVisualizerStyle::FractalFlow,
                        ];
                        let selected = values
                            .iter()
                            .position(|value| *value == this.draft.audio_visualizer_style)
                            .unwrap_or(0);
                        this.open_setting_dropdown(
                            values
                                .into_iter()
                                .map(|value| {
                                    DropdownItem::new(
                                        SettingChoice::AudioVisualizerStyle(value),
                                        audio_visualizer_style_label(&this.i18n, value),
                                    )
                                })
                                .collect(),
                            selected,
                        );
                    },
                )
                .relative()
                .child(self.measurement(Measured::SettingsStyle, cx));
                let hint = match draft.audio_visualizer_style {
                    AudioVisualizerStyle::StereoDensity => {
                        settings.tr("settings-stereo-density-hint")
                    }
                    AudioVisualizerStyle::SpatialSpectrum => {
                        settings.tr("settings-spatial-spectrum-hint")
                    }
                    _ => SharedString::default(),
                };
                let instance = settings.visualizer_panel.is_some();
                let display = settings_group(
                    if instance {
                        settings.tr("settings-this-instance")
                    } else {
                        settings.tr("settings-new-instance-defaults")
                    },
                    column()
                        .gap_2()
                        .child(form_field(
                            settings.tr("settings-style"),
                            style.when(!hint.is_empty(), |control| {
                                control.tooltip(settings_tooltip(hint))
                            }),
                        ))
                        .child(
                            row()
                                .flex_wrap()
                                .gap_2()
                                .child(visual_switch(
                                    &self.i18n,
                                    "audio-visualizer-grid",
                                    "",
                                    settings.tr("settings-reference-lines"),
                                    draft.audio_visualizer_grid,
                                    cx,
                                    |draft| {
                                        draft.audio_visualizer_grid = !draft.audio_visualizer_grid
                                    },
                                ))
                                .child(visual_switch(
                                    &self.i18n,
                                    "audio-visualizer-labels",
                                    "",
                                    settings.tr("settings-labels"),
                                    draft.audio_visualizer_labels,
                                    cx,
                                    |draft| {
                                        draft.audio_visualizer_labels =
                                            !draft.audio_visualizer_labels
                                    },
                                )),
                        )
                        .id("settings-visualizer-display-help")
                        .tooltip(settings_tooltip(if instance {
                            settings.tr("settings-visualizer-instance-help")
                        } else {
                            settings.tr("settings-visualizer-defaults-help")
                        })),
                );
                if instance {
                    return column().child(display);
                }
                let content = column()
                    .gap_3()
                    .child(display)
                    .child(settings_group(
                        settings.tr("settings-response-motion"),
                        column()
                            .gap_2()
                            .child(settings.single(Field::AudioVisualizerDb))
                            .child(settings.pair(
                                Field::AudioVisualizerSensitivity,
                                Field::AudioVisualizerBloomIntensity,
                            ))
                            .child(settings.pair(
                                Field::AudioVisualizerInnerDiameter,
                                Field::AudioVisualizerRotationSpeed,
                            ))
                            .child(row().child(visual_switch(
                                &self.i18n,
                                "audio-visualizer-fade-idle",
                                "",
                                settings.tr("settings-fade-when-idle"),
                                draft.audio_visualizer_fade_when_idle,
                                cx,
                                |draft| {
                                    draft.audio_visualizer_fade_when_idle =
                                        !draft.audio_visualizer_fade_when_idle
                                },
                            )))
                            .id("settings-visualizer-response-motion-help")
                            .tooltip(settings_tooltip(
                                settings.tr("settings-visualizer-motion-help"),
                            )),
                    ))
                    .child(settings_group(
                        settings.tr("settings-bars-rings-section"),
                        column()
                            .gap_2()
                            .child(settings.pair(
                                Field::AudioVisualizerBarWidth,
                                Field::AudioVisualizerBarGlowLayers,
                            ))
                            .child(settings.single(Field::AudioVisualizerRingOpacity))
                            .id("settings-bars-rings-help")
                            .tooltip(settings_tooltip(
                                settings.tr("settings-visualizer-bars-rings-help"),
                            )),
                    ));
                content.child(settings_group(
                    settings.tr("settings-colors"),
                    column().gap_2().child(settings.pair(
                        Field::AudioVisualizerPrimaryColor,
                        Field::AudioVisualizerSecondaryColor,
                    )),
                ))
            }
            VisualPage::Waveform => column()
                .gap_3()
                .child(settings_group(
                    settings.tr("settings-appearance-interaction"),
                    column()
                        .gap_2()
                        .child(settings.pair(Field::CursorColor, Field::Glow))
                        .child(
                            row()
                                .flex_wrap()
                                .gap_2()
                                .child(visual_switch(
                                    &self.i18n,
                                    "waveform-labels",
                                    "",
                                    settings.tr("settings-time-labels"),
                                    draft.waveform_labels,
                                    cx,
                                    |draft| draft.waveform_labels = !draft.waveform_labels,
                                ))
                                .child(visual_switch(
                                    &self.i18n,
                                    "waveform-seek",
                                    "",
                                    settings.tr("settings-click-and-drag-to-seek"),
                                    draft.waveform_seek_enabled,
                                    cx,
                                    |draft| {
                                        draft.waveform_seek_enabled = !draft.waveform_seek_enabled
                                    },
                                )),
                        ),
                ))
                .child(settings_group(
                    settings.tr("settings-amplitude-display"),
                    column()
                        .gap_2()
                        .child(settings.pair(Field::WaveformRmsGain, Field::WaveformPeakGain))
                        .child(settings.single(Field::WaveformPeakGamma))
                        .id("settings-amplitude-help")
                        .tooltip(settings_tooltip(
                            settings.tr("settings-waveform-amplitude-help"),
                        )),
                )),
        }
    }

    fn panel(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
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
            SettingsPage::Interface => match self.interface_page {
                InterfacePage::Basic => ("settings-basic-scroll", self.basic_settings(cx)),
                InterfacePage::Layout => ("settings-layout-scroll", self.layout_settings(cx)),
                InterfacePage::AlbumArtwork => {
                    ("settings-artwork-scroll", self.artwork_settings(cx))
                }
                InterfacePage::Desktop => ("settings-desktop-scroll", self.desktop_settings(cx)),
                InterfacePage::Logging => ("settings-logging-scroll", self.logging_settings(cx)),
            },
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
        self.active_scroll = Some(scroll.clone());
        let mut body = row().flex_1().min_h_0().items_start();
        if self.visualizer_panel.is_none() && self.artwork_panel.is_none() {
            body = body.child(
                self.sidebar(cx)
                    .id("settings-sidebar")
                    .h_full()
                    .overflow_y_scroll(),
            );
        }
        panel = panel.child(
            body.child(
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
            ),
        );
        let feedback = if self.applying {
            status_message(self.tr("settings-applying-settings"), StatusKind::Info)
                .into_any_element()
        } else {
            match &self.feedback {
                Some(Err(error)) => copyable_message("copy-settings-error", error.clone(), cx)
                    .text_color(rgb(ERROR))
                    .into_any_element(),
                Some(Ok(())) => status_message(
                    self.tr("settings-last-apply-succeeded"),
                    StatusKind::Success,
                )
                .into_any_element(),
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
                .child(disabled_control(
                    icon_button(
                        "settings-apply",
                        "",
                        self.tr("settings-apply-settings"),
                        cx,
                        |this, _, cx| this.apply_settings(cx),
                    ),
                    self.applying,
                ))
                .child(disabled_control(
                    icon_button(
                        "settings-discard",
                        "",
                        self.tr("settings-discard-changes-and-close"),
                        cx,
                        |this, _, cx| {
                            if this.applying {
                                return;
                            }
                            this.cancel(cx);
                        },
                    ),
                    self.applying,
                )),
        );
        if self.dropdown.is_open() {
            let bounds = self.measured.borrow().get(&self.dropdown_anchor).copied();
            if let Some(bounds) = bounds {
                let selected = self.dropdown.selected_index().unwrap_or(0);
                let item_count = self.dropdown.filtered().count();
                let row_height = DROPDOWN_ROW_REMS * f32::from(window.rem_size());
                let height = (item_count as f32 * row_height)
                    .min(f32::from(window.rem_size()) * POPOVER_MAX_REMS)
                    .min(f32::from(window.viewport_size().height) * 0.45);
                let choices = uniform_list(
                    "settings-dropdown",
                    item_count,
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .filter_map(|index| {
                                let (item_index, item) = this.dropdown.filtered_item(index)?;
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
                .track_scroll(&self.device_scroll)
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
                                            if this.dropdown_anchor == Measured::SettingsFont
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
                                            this.dropdown.close();
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

impl Render for Settings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let panel = self.panel(window, cx);
        div()
            .id("settings-modal")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let key = event.keystroke.key.as_str();
                if this.dropdown.is_open() {
                    if this.settings_font_input_focused(window, cx) {
                        return;
                    }
                    if key == "escape" {
                        this.dropdown.close();
                        cx.notify();
                    } else {
                        this.settings_dropdown_key(key, window, cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                let modifiers = event.keystroke.modifiers;
                if matches!(key, "pageup" | "pagedown")
                    && !modifiers.control
                    && !modifiers.platform
                    && !modifiers.alt
                    && !modifiers.shift
                    && !modifiers.function
                    && this.focus.is_focused(window)
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
                } else if key == "escape" {
                    this.cancel(cx);
                    cx.stop_propagation();
                }
            }))
            .occlude()
            .absolute()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(0x000000).alpha(0.58))
            .child(
                column()
                    .max_w(rems(860. / 16.))
                    .w(relative(0.9))
                    .h((window.viewport_size().height - px(32.))
                        .max(px(0.))
                        .min(window.rem_size() * (640. / 16.)))
                    .gap_0()
                    .overflow_hidden()
                    .bg(rgb(super::PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .rounded_md()
                    .child(
                        row()
                            .h(rems(2.5))
                            .flex_shrink_0()
                            .px(gpui::px(UI_INSET))
                            .child(div().flex_1().text_lg().child(self.tr("settings-settings"))),
                    )
                    .child(panel),
            )
    }
}
