use super::{
    BORDER, ButtonTooltip, DropdownItem, DropdownState, ERROR, POPOVER_MAX_HEIGHT, StatusKind,
    UI_INSET, active_control, button, caption, column,
    components::{DROPDOWN_ROW_REMS, disabled_control},
    copyable_message, dropdown_container, dropdown_row, dropdown_trigger, form_field, icon_button,
    input::{Input, InputEvent},
    row, section_group, status_message, toggle_button, two_column,
};
use crate::{
    artwork::{ArtworkCacheReport, ArtworkManager},
    config::{
        AlbumArtworkDirection, AlbumArtworkMode, AnalysisWindow, AudioVisualizerOptions,
        AudioVisualizerStyle, Config, LogLevel, RgbColor, SpectrumStyle, VisualizationPalette,
    },
    core::CoreHandle,
};
use anyhow::{Context as _, Result};
use gpui::{prelude::*, *};
use std::{
    cell::RefCell, collections::HashMap, path::PathBuf, rc::Rc, str::FromStr, sync::Arc,
    time::Duration,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingChoice {
    Device(usize),
    Font(usize),
    CustomFont,
    SpectrumStyle(SpectrumStyle),
    Fft(u32),
    AudioVisualizerStyle(AudioVisualizerStyle),
    Palette(VisualizationPalette),
    LogLevel(LogLevel),
    Window(AnalysisWindow),
    AlbumArtworkMode(AlbumArtworkMode),
    AlbumArtworkDirection(AlbumArtworkDirection),
}

impl SettingChoice {
    fn label(self) -> String {
        match self {
            Self::Device(_) | Self::Font(_) | Self::CustomFont => String::new(),
            Self::AlbumArtworkMode(AlbumArtworkMode::Static) => "Static".to_owned(),
            Self::AlbumArtworkMode(AlbumArtworkMode::Rotating) => "Rotating".to_owned(),
            Self::AlbumArtworkDirection(AlbumArtworkDirection::Clockwise) => "Clockwise".to_owned(),
            Self::AlbumArtworkDirection(AlbumArtworkDirection::Counterclockwise) => {
                "Counterclockwise".to_owned()
            }
            Self::SpectrumStyle(value) => spectrum_style_label(value).to_owned(),
            Self::AudioVisualizerStyle(value) => audio_visualizer_style_label(value).to_owned(),
            Self::Palette(value) => visualization_palette_label(value).to_owned(),
            Self::Fft(value) => value.to_string(),
            Self::LogLevel(value) => match value {
                LogLevel::Debug => "Debug".to_owned(),
                LogLevel::Info => "Info".to_owned(),
                LogLevel::Warning => "Warning".to_owned(),
                LogLevel::Error => "Error".to_owned(),
            },
            Self::Window(AnalysisWindow::Hann) => "Hann".to_owned(),
            Self::Window(AnalysisWindow::BlackmanHarris) => "Blackman–Harris".to_owned(),
            Self::Window(AnalysisWindow::None) => "None".to_owned(),
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
}
impl Field {
    fn label(self) -> &'static str {
        match self {
            Self::Roots => "Library folders",
            Self::ScanMaxDepth => "Maximum scan depth",
            Self::Volume => "Volume (%)",
            Self::PlayCountThreshold => "Play count threshold (%)",
            Self::LogRetentionWeeks => "Log retention (weeks)",
            Self::MediaReadBuffer => "Media read buffer (MiB)",
            Self::Font => "Font family",
            Self::Scale => "Interface scale",
            Self::LayoutSnapGrid => "Layout grid spacing (px)",
            Self::Fps => "Analysis rate (Hz)",
            Self::Background => "Background color",
            Self::VisualizationDb => "Default display range (dB)",
            Self::AudioVisualizerSensitivity => "Sensitivity",
            Self::AudioVisualizerRotationSpeed => "Rotation speed",
            Self::AudioVisualizerBarWidth => "Bar width",
            Self::AudioVisualizerBarGlowLayers => "Bar glow layers",
            Self::AudioVisualizerRingOpacity => "Ring opacity",
            Self::AudioVisualizerBloomIntensity => "Bloom intensity",
            Self::AudioVisualizerInnerDiameter => "Inner diameter",
            Self::AudioVisualizerPrimaryColor => "Primary color",
            Self::AudioVisualizerSecondaryColor => "Secondary color",
            Self::SpectrumDb | Self::AudioVisualizerDb => "Display range (dB)",
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
            Self::AlbumArtworkPeriod => "Seconds per circle",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::Roots => "Separate paths with semicolons.",
            Self::AlbumArtworkPeriod => "Finite and greater than zero; default 20 seconds.",
            Self::ScanMaxDepth => "Minimum 1; starting folder = depth 0.",
            Self::Volume => "0–100.",
            Self::PlayCountThreshold => "0 ≤ value < 100; based on time heard.",
            Self::LogRetentionWeeks => "1–520.",
            Self::MediaReadBuffer => "1, 2, 4, 8, or 16. Applies when media is next opened.",
            Self::Font => "",
            Self::Scale => "0.75–2.",
            Self::LayoutSnapGrid => {
                "1–64 logical pixels before interface scaling. Hold Alt while dragging to invert snapping temporarily."
            }
            Self::Fps => "5–60. Applies to Spectrum, Spectrogram, and Audio Visualizer.",
            Self::Background => "#RRGGBB. Applies to all views.",
            Self::AudioVisualizerPrimaryColor | Self::AudioVisualizerSecondaryColor => {
                "#RRGGBB. Shared by all instances, independent of the shared color scheme."
            }
            Self::CursorColor => "#RRGGBB. The waterline stays still while paused.",
            Self::VisualizationDb => {
                "1–160. Applies to Spectrum, Spectrogram, and Audio Visualizer. Fractal styles use it for energy response; playback volume is unchanged."
            }
            // These controls attach a live inherited-range tooltip in Settings::field.
            Self::SpectrumDb | Self::AudioVisualizerDb | Self::SpectrogramDb => "",
            Self::AudioVisualizerSensitivity => "0–5.",
            Self::AudioVisualizerRotationSpeed => "0–10.",
            Self::AudioVisualizerBarWidth
            | Self::AudioVisualizerBloomIntensity
            | Self::WaveformPeakGain => "0–2.",
            Self::AudioVisualizerBarGlowLayers => "0–4.",
            Self::AudioVisualizerRingOpacity => "0–1.",
            Self::AudioVisualizerInnerDiameter => "0–2; relative to panel size.",
            Self::SpectrumBarWidth => "1–20; used with automatic bar count.",
            Self::SpectrumBars => "1–512; 0 = automatic, using bar width and gap.",
            Self::SpectrumGap => "0–8.",
            Self::SpectrumHold | Self::SpectrumBarHold => "0–2000.",
            Self::SpectrumGravity => "0–500.",
            Self::SpectrumBarGravity => {
                "0–500; 0 snaps after hold, bypassing smoothing. Attacks stay immediate."
            }
            Self::SpectrumSmoothing => {
                "0–1000. Gravity 0 bypasses smoothing. Attacks stay immediate."
            }
            Self::SpectrogramHistory => "1–120; limited by panel width and analysis rate.",
            Self::SpectrogramSamplingPointsScale => {
                "0.4–1.4; relative to the tallest visible panel."
            }
            Self::SpectrogramInterpolationPoints => {
                "64–4096; applies with or without interpolation. More frequency rows do not add FFT detail."
            }
            Self::Glow => "0–2; 0 = off.",
            Self::WaveformRmsGain => "0–3. Display only; does not affect playback volume.",
            Self::WaveformPeakGamma => {
                "0.25–3; above 1 reduces low peaks, below 1 lifts them. Display only; does not affect playback volume."
            }
        }
    }

    fn form(self, control: impl IntoElement) -> Div {
        let hint = self.hint();
        let control = div()
            .id(("settings-field-help", self as usize))
            .w_full()
            .child(control)
            .when(!hint.is_empty(), |control| {
                control.tooltip(move |_, cx| cx.new(|_| ButtonTooltip { text: hint.into() }).into())
            });
        form_field(self.label(), control)
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
    id: &'static str,
    icon: &'static str,
    label: &'static str,
    enabled: bool,
    cx: &mut Context<Settings>,
    toggle: impl Fn(&mut Config) + 'static,
) -> Stateful<Div> {
    let hint = match id {
        "settings-tray" => "Changes require restarting the graphical host.",
        "settings-still-mode" => {
            "Stops visualization processing and discards results; playback and interface effects are unchanged."
        }
        "visual-cache" => {
            "Reduces CPU use at the cost of memory in Spectrum, Spectrogram, and Waveform."
        }
        "audio-visualizer-fade-idle" => "Controls the Bars, Rings and Bars + Rings styles.",
        "audio-visualizer-grid" => {
            "Bars and rings guides show normalized levels, not dB or frequency."
        }
        _ => "",
    };
    let label: SharedString = if hint.is_empty() {
        label.into()
    } else {
        format!("{label} ({hint})").into()
    };
    toggle_button(id, icon, label, enabled, cx, move |this, _| {
        toggle(&mut this.draft);
    })
}

fn settings_group(title: &'static str, content: impl IntoElement) -> Div {
    section_group(title, content)
}

fn settings_tooltip(
    text: impl Into<SharedString>,
) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView {
    let text = text.into();
    move |_, cx| cx.new(|_| ButtonTooltip { text: text.clone() }).into()
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
pub(super) fn audio_visualizer_style_label(style: AudioVisualizerStyle) -> &'static str {
    match style {
        AudioVisualizerStyle::Bars => "Bars",
        AudioVisualizerStyle::Rings => "Rings",
        AudioVisualizerStyle::BarsRings => "Bars + Rings",
        AudioVisualizerStyle::StereoDensity => "Stereo Density",
        AudioVisualizerStyle::SpatialSpectrum => "Spatial Spectrum",
        AudioVisualizerStyle::FractalBeams => "Fractal Beams",
        AudioVisualizerStyle::FractalFlow => "Fractal Flow",
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
    AlbumArtwork,
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
    Device,
    SettingsFont,
    SettingsStyle,
    SettingsFft,
    SettingsLogLevel,
    SettingsWindow,
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
}

impl EventEmitter<SettingsEvent> for Settings {}

/// Unsaved preferences stay independent from live playback snapshots.
pub(super) struct Settings {
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
    visualizer_panel: Option<u64>,
}

impl Focusable for Settings {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Settings {
    pub(super) fn new(
        handle: CoreHandle,
        artwork_manager: Arc<ArtworkManager>,
        devices: Arc<Vec<String>>,
        ffmpeg_status: String,
        mpris_status: String,
        ui_scale: f32,
        cx: &mut Context<Self>,
    ) -> Self {
        let config = handle.config_snapshot();
        let artwork_cache_age = cx.new(|cx| Input::new("30", "Age in days", cx));
        let artwork_cache_subscription = cx.subscribe(&artwork_cache_age, |this, _, event, cx| {
            if matches!(event, InputEvent::Changed { .. }) {
                this.artwork_cache_age_revision = this.artwork_cache_age_revision.wrapping_add(1);
                this.artwork_cache_report = None;
                this.artwork_cache_error = None;
                cx.notify();
            }
        });
        Self {
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
            page: SettingsPage::General,
            visual_page: VisualPage::Common,
            visualizer_panel: None,
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
        if devices_changed && self.dropdown.is_open() && self.dropdown_anchor == Measured::Device {
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

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.applying {
            return;
        }
        self.active = false;
        self.dropdown.close();
        self.reset(&self.handle.config_snapshot(), cx);
        cx.emit(SettingsEvent::Closed);
    }

    fn measurement(&self, key: Measured) -> AnyElement {
        let measured = Rc::clone(&self.measured);
        canvas(
            move |bounds, _, _| {
                measured.borrow_mut().insert(key, bounds);
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
        ]
        .into_iter()
        .map(|field| {
            let input = cx.new(|cx| {
                if field == Field::Font {
                    Input::new("", "sans-serif — search fonts", cx).with_trailing_space(px(28.))
                } else {
                    Input::new("", field.label(), cx)
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
                format!("Use \"{query}\""),
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
            return field.form(
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
                            text: format!("1–160 dB; empty = Common ({range} dB).").into(),
                        })
                        .into()
                    }),
            );
        }
        field.form(input)
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

    fn field_state(&self, field: Field, enabled: bool, reason: &'static str, cx: &App) -> Div {
        if enabled {
            return self.field(field).min_w(px(180.));
        }
        // Keep the draft value, but unmount the editor so disabled fields cannot take focus or edits.
        let control = div()
            .id(("inactive-setting", field as usize))
            .h(rems(36. / 16.))
            .p(rems(7. / 16.))
            .line_height(rems(20. / 16.))
            .text_size(rems(14. / 16.))
            .bg(rgb(super::PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .rounded(rems(5. / 16.))
            .cursor_default()
            .child(self.value(field, cx))
            .tooltip(move |_, cx| {
                cx.new(|_| ButtonTooltip {
                    text: reason.into(),
                })
                .into()
            });
        column()
            .min_w(px(180.))
            .child(field.form(control).opacity(0.45))
            .child(caption(reason).whitespace_normal())
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
        self.set_value(Field::Volume, (self.draft.volume * 100.).to_string(), cx);
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
            .context("Visualization background")?;
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
            .context("Audio Visualizer primary color")?;
        config.audio_visualizer_secondary_color = self
            .value(Field::AudioVisualizerSecondaryColor, cx)
            .trim()
            .parse::<RgbColor>()
            .context("Audio Visualizer secondary color")?;
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
            .context("Waveform waterline color")?;
        config.waveform_glow = self.number(Field::Glow, cx)?;
        config.waveform_rms_gain = self.number(Field::WaveformRmsGain, cx)?;
        config.waveform_peak_gain = self.number(Field::WaveformPeakGain, cx)?;
        config.waveform_peak_gamma = self.number(Field::WaveformPeakGamma, cx)?;
        config.album_artwork_period_seconds = self.number(Field::AlbumArtworkPeriod, cx)?;
        config.validate()?;
        Ok(config)
    }

    fn tab_button(
        &self,
        id: &'static str,
        label: &'static str,
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
                    .w(px(27.))
                    .h(px(34.))
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
            .child(self.measurement(Measured::SettingsFont));
        Field::Font.form(control)
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

    fn apply_settings(&mut self, cx: &mut Context<Self>) {
        if self.applying {
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
                let recovery = if self.draft.spectrum_style == SpectrumStyle::Solid
                    || !self.draft.spectrum_peaks
                {
                    " Retained inactive values are also validated. In Visualizations → Spectrum, choose a non-Solid Style to correct bar count, width or gap; enable Peak markers to correct marker hold or gravity."
                } else {
                    ""
                };
                self.feedback = Some(Err(format!("Invalid settings: {error:#}{recovery}")));
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
        self.font_value = value.clone();
        self.draft.ui_font = value.clone();
        self.inputs[&Field::Font].update(cx, |input, cx| input.set_text(value, cx));
    }

    fn choose_device(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some((_, item)) = self.dropdown.filtered_item(index) else {
            return;
        };
        match item.value {
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
                SettingChoice::Font(_) | SettingChoice::CustomFont => Measured::SettingsFont,
                SettingChoice::SpectrumStyle(_) => Measured::SettingsStyle,
                SettingChoice::Fft(_) => Measured::SettingsFft,
                SettingChoice::LogLevel(_) => Measured::SettingsLogLevel,
                SettingChoice::Window(_) => Measured::SettingsWindow,
                SettingChoice::AudioVisualizerStyle(_) => Measured::SettingsStyle,
                SettingChoice::Palette(_) => Measured::SettingsStyle,
                SettingChoice::Device(_) => Measured::Device,
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
            self.artwork_cache_error = Some(format!(
                "Enter a positive whole number of days (1–{}), then try again.",
                u64::MAX / 86_400
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
                            this.artwork_cache_error = Some(format!(
                                "Could not read the artwork cache: {error}. Check directory access and try again."
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
        let mut content = column()
            .gap_2()
            .child(caption(
                "Manual maintenance only. Changes take effect immediately; Apply is not required."
            ))
            .child(
                row()
                    .items_end()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        form_field(
                            "Expire files older than (days)",
                            div()
                                .id("settings-artwork-cache-age")
                                .child(self.artwork_cache_age.clone()),
                        )
                        .min_w(px(180.))
                        .flex_1(),
                    )
                    .child(
                        disabled_control(icon_button(
                            "settings-artwork-cache-calculate",
                            "󰃬",
                            "Calculate usage",
                            cx,
                            |this, _, cx| this.maintain_artwork_cache(false, cx),
                        ), busy),
                    )
                    .child(
                        disabled_control(icon_button(
                            "settings-artwork-cache-clean",
                            "󰆴",
                            "Clean expired",
                            cx,
                            |this, _, cx| this.maintain_artwork_cache(true, cx),
                        ), busy),
                    ),
            )
            .child(caption(
                "Uses file modification age. Artwork still held in memory is protected; missing artwork is regenerated when needed."
            ));
        if let Some((days, clean)) = self.artwork_cache_busy {
            content = content.child(status_message(
                format!(
                    "{} artwork cache (older than {days} days)…",
                    if clean { "Cleaning" } else { "Calculating" }
                ),
                StatusKind::Info,
            ));
        }
        if let Some((days, clean, report)) = &self.artwork_cache_report {
            content = content
                .child(status_message(
                    format!(
                        "{} complete · threshold: older than {days} days{}",
                        if *clean {
                            "Cleanup"
                        } else {
                            "Usage calculation"
                        },
                        if *clean {
                            " · totals measured before cleanup"
                        } else {
                            ""
                        },
                    ),
                    if report.failed_files == 0 {
                        StatusKind::Success
                    } else {
                        StatusKind::Error
                    },
                ))
                .child(caption(format!(
                    "Total: {} files · {} bytes",
                    report.files, report.bytes
                )))
                .child(caption(format!(
                    "Expired candidates (including protected): {} files · {} bytes",
                    report.expired_files, report.expired_bytes
                )))
                .child(caption(format!(
                    "Protected skips: {} files · {} bytes",
                    report.protected_files, report.protected_bytes
                )))
                .child(caption(format!(
                    "Removed: {} files · {} bytes",
                    report.removed_files, report.removed_bytes
                )))
                .child(status_message(
                    format!("File failures: {}", report.failed_files),
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
                content = content.child(caption(
                    "Check file permissions or directory access, then retry the action.",
                ));
            }
        }
        if let Some(error) = &self.artwork_cache_error {
            content = content.child(
                copyable_message("copy-artwork-cache-error", error.clone(), cx)
                    .text_color(rgb(ERROR)),
            );
        }
        settings_group("Artwork cache", content)
    }

    fn general_settings(&mut self, cx: &mut Context<Self>) -> Div {
        let chosen_device: SharedString = self
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
                                "System default",
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
                .child(self.measurement(Measured::Device)),
            );
        let draft = &self.draft;
        let (mpris, tray, ffmpeg, remix) = (
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
                    .position(|value| *value == this.draft.log_level)
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
                                .min_w(px(180.))
                                .gap_1()
                                .child(caption("Log level"))
                                .child(log_level_dropdown),
                        )
                        .child(
                            self.field(Field::LogRetentionWeeks)
                                .flex_1()
                                .min_w(px(180.)),
                        ),
                )
                .child(row().child(visual_switch(
                    "settings-log-file",
                    "",
                    "Write logs to disk",
                    draft.log_to_file,
                    cx,
                    |draft| draft.log_to_file = !draft.log_to_file,
                ))),
        );
        let library = settings_group(
            "Library",
            column()
                .gap_2()
                .child(
                    row()
                        .items_start()
                        .flex_wrap()
                        .gap_3()
                        .child(self.field(Field::Roots).flex_1().min_w(px(260.)))
                        .child(self.field(Field::ScanMaxDepth).flex_1().min_w(px(220.))),
                )
                .child(row().child(button(
                    "settings-same-files",
                    "Same files / Relocation candidates…",
                    cx,
                    |this, _, cx| {
                        this.dropdown.close();
                        cx.emit(SettingsEvent::ReviewDuplicates);
                    },
                ))),
        );
        let playback = settings_group(
            "Playback",
            column()
                .gap_2()
                .child(self.pair(Field::Volume, Field::PlayCountThreshold)),
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
                        .child(self.field(Field::MediaReadBuffer).flex_1().min_w(px(240.))),
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
                                    ffmpeg_hint(&self.ffmpeg_status)
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
                .child(two_column(
                    self.font_control(cx).min_w(px(180.)),
                    self.field(Field::Scale).min_w(px(180.)),
                ))
                .child(
                    Field::LayoutSnapGrid.form(
                        row()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(self.inputs[&Field::LayoutSnapGrid].clone()),
                            )
                            .child(visual_switch(
                                "settings-layout-snapping",
                                "󰐃",
                                "Layout snapping",
                                self.draft.layout_snap_enabled,
                                cx,
                                |draft| draft.layout_snap_enabled = !draft.layout_snap_enabled,
                            )),
                    ),
                )
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
                                    self.mpris_status
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
                            "settings-tray",
                            "",
                            "System tray",
                            tray,
                            cx,
                            |draft| draft.tray_enabled = !draft.tray_enabled,
                        )),
                )
                .id("settings-interface-help")
                .tooltip(settings_tooltip(
                    "System tray changes require restarting the graphical host.",
                )),
        );

        column()
            .gap_3()
            .child(library)
            .child(playback)
            .child(audio)
            .child(interface)
            .child(logging)
            .child(self.artwork_cache_settings(cx))
    }

    fn visual_settings_tabs(&self, cx: &mut Context<Self>) -> Div {
        let selected = self.visual_page;
        row()
            .flex_wrap()
            .flex_shrink_0()
            .gap_1()
            .px(gpui::px(UI_INSET))
            .py_2()
            .child(self.tab_button(
                "visual-common",
                "Common",
                selected == VisualPage::Common,
                cx,
                |settings| settings.visual_page = VisualPage::Common,
            ))
            .child(self.tab_button(
                "visual-spectrogram",
                "Spectrogram",
                selected == VisualPage::Spectrogram,
                cx,
                |settings| settings.visual_page = VisualPage::Spectrogram,
            ))
            .child(self.tab_button(
                "visual-spectrum",
                "Spectrum",
                selected == VisualPage::Spectrum,
                cx,
                |settings| settings.visual_page = VisualPage::Spectrum,
            ))
            .child(self.tab_button(
                "visual-waveform",
                "Waveform",
                selected == VisualPage::Waveform,
                cx,
                |settings| settings.visual_page = VisualPage::Waveform,
            ))
            .child(self.tab_button(
                "visual-audio-visualizer",
                "Audio Visualizer",
                selected == VisualPage::AudioVisualizer,
                cx,
                |settings| settings.visual_page = VisualPage::AudioVisualizer,
            ))
            .child(self.tab_button(
                "visual-album-artwork",
                "Album artwork",
                selected == VisualPage::AlbumArtwork,
                cx,
                |settings| settings.visual_page = VisualPage::AlbumArtwork,
            ))
    }

    fn visual_settings(&mut self, cx: &mut Context<Settings>) -> Div {
        let settings = &*self;
        let draft = &settings.draft;
        match settings.visual_page {
            VisualPage::AlbumArtwork => {
                let mode = dropdown_button(
                    "album-artwork-mode",
                    SettingChoice::AlbumArtworkMode(draft.album_artwork_mode).label(),
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
                                    DropdownItem::new(choice, choice.label())
                                })
                                .collect(),
                            selected,
                        );
                    },
                )
                .relative()
                .child(self.measurement(Measured::AlbumArtworkMode));
                let direction = dropdown_button(
                    "album-artwork-direction",
                    SettingChoice::AlbumArtworkDirection(draft.album_artwork_direction).label(),
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
                                    DropdownItem::new(choice, choice.label())
                                })
                                .collect(),
                            selected,
                        );
                    },
                )
                .relative()
                .child(self.measurement(Measured::AlbumArtworkDirection));
                column().child(settings_group(
                    "Playback motion",
                    column().gap_2()
                        .child(two_column(form_field("Mode", mode).min_w(px(180.)), form_field("Direction", direction).min_w(px(180.))))
                        .child(settings.single(Field::AlbumArtworkPeriod))
                        .id("settings-album-artwork-help")
                        .tooltip(settings_tooltip("Shared by all Album artwork panels. Rotation follows playback, not audio energy. Still mode freezes rotation without removing the cover.")),
                ))
            }
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
                            .position(|value| *value == this.draft.visual_palette)
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
                .child(self.measurement(Measured::SettingsFft));
                let window = dropdown_button(
                    "analysis-window",
                    SettingChoice::Window(draft.analysis_window).label(),
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
                        .id("settings-appearance-help")
                        .tooltip(settings_tooltip("Background: all views. Color scheme: Spectrum, Spectrogram, and Waveform. Display range: Spectrum, Spectrogram, and Audio Visualizer. Fractal styles use it for energy response; playback volume is unchanged."))))
                    .child(settings_group("Frequency analysis", column().gap_2()
                        .child(two_column(form_field("FFT size (samples)", fft).min_w(px(180.)), form_field("FFT window", window).min_w(px(180.))))
                        .child(settings.single(Field::Fps))
                        .id("settings-analysis-help")
                        .tooltip(settings_tooltip("Applies to Spectrum, Spectrogram, and Audio Visualizer. Changing FFT size or window clears visualization history."))))
                    .child(settings_group("Rendering", row().gap_2()
                        .child(visual_switch("settings-still-mode", "", "Still mode", draft.still_mode, cx, |draft| draft.still_mode = !draft.still_mode))
                        .id("settings-rendering-help")
                        .tooltip(settings_tooltip("Still mode stops visualization processing and discards results; playback and interface effects are unchanged. Rendering cache reduces CPU use at the cost of memory in Spectrum, Spectrogram, and Waveform."))
                        .child(visual_switch("visual-cache", "", "Rendering cache", draft.visualization_cache, cx, |draft| draft.visualization_cache = !draft.visualization_cache))
                    ))
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
                            .position(|value| *value == this.draft.spectrum_style)
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
                        .child(two_column(form_field("Style", style).min_w(px(180.)), settings.field_state(Field::SpectrumBars, !solid, "Retained with Solid. Choose another Style to edit.", cx)))
                        .child(two_column(
                            settings.field_state(Field::SpectrumBarWidth, !solid, "Retained with Solid. Choose another Style to edit.", cx),
                            settings.field_state(Field::SpectrumGap, !solid, "Retained with Solid. Choose another Style to edit.", cx),
                        ))
                        .child(settings.single(Field::SpectrumDb))
                        .child(row().flex_wrap().gap_2()
                            .child(visual_switch("spectrum-interpolate", "", "Frequency interpolation", draft.spectrum_interpolate, cx, |draft| draft.spectrum_interpolate = !draft.spectrum_interpolate))
                            .child(visual_switch("spectrum-grid", "", "Grid lines", draft.spectrum_grid, cx, |draft| draft.spectrum_grid = !draft.spectrum_grid))
                            .child(visual_switch("spectrum-labels", "", "Axis labels", draft.spectrum_labels, cx, |draft| draft.spectrum_labels = !draft.spectrum_labels)))
                        .id("settings-spectrum-appearance-help")
                        .tooltip(settings_tooltip("Automatic bar count uses bar width and gap."))))
                    .child(settings_group("Bar motion", column().gap_2()
                        .child(settings.pair(Field::SpectrumBarHold, Field::SpectrumBarGravity))
                        .child(settings.single(Field::SpectrumSmoothing))
                        .id("settings-bar-motion-help")
                        .tooltip(settings_tooltip("Gravity 0 snaps after hold, bypassing smoothing. Attacks stay immediate."))))
                    .child(settings_group("Peak markers", column().gap_2()
                        .child(row().child(visual_switch("spectrum-peaks", "", "Peak markers", draft.spectrum_peaks, cx, |draft| draft.spectrum_peaks = !draft.spectrum_peaks)))
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
                        .child(row().child(visual_switch(
                            "spectrogram-labels",
                            "",
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
                            "",
                            "Frequency interpolation",
                            draft.spectrogram_interpolate,
                            cx,
                            |draft| draft.spectrogram_interpolate = !draft.spectrogram_interpolate,
                        )))
                        .id("settings-frequency-sampling-help")
                        .tooltip(settings_tooltip(
                            "More frequency rows do not add FFT detail.",
                        )),
                )),
            VisualPage::AudioVisualizer => {
                let style = dropdown_button(
                    "audio-visualizer-style",
                    audio_visualizer_style_label(draft.audio_visualizer_style),
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
                                        audio_visualizer_style_label(value),
                                    )
                                })
                                .collect(),
                            selected,
                        );
                    },
                )
                .relative()
                .child(self.measurement(Measured::SettingsStyle));
                let hint = match draft.audio_visualizer_style {
                    AudioVisualizerStyle::StereoDensity => {
                        "Fixed Mid/Side axes. Color shows occupancy, not dBFS."
                    }
                    AudioVisualizerStyle::SpatialSpectrum => {
                        "Frequency versus left/right energy balance, not source direction. Color shows dBFS."
                    }
                    AudioVisualizerStyle::FractalBeams => {
                        "Bands deform branches; energy rises send outward pulses."
                    }
                    AudioVisualizerStyle::FractalFlow => {
                        "Bands steer light filaments; energy rises brighten their trails."
                    }
                    _ => "",
                };
                let instance = settings.visualizer_panel.is_some();
                let display = settings_group(
                    if instance { "This instance" } else { "New instance defaults" },
                    column()
                        .gap_2()
                        .child(form_field("Style", style.when(!hint.is_empty(), |control| {
                            control.tooltip(settings_tooltip(hint))
                        })))
                        .child(row().flex_wrap().gap_2()
                            .child(visual_switch("audio-visualizer-grid", "", "Reference lines",
                            draft.audio_visualizer_grid, cx,
                            |draft| draft.audio_visualizer_grid = !draft.audio_visualizer_grid,))
                            .child(visual_switch("audio-visualizer-labels", "", "Labels",
                            draft.audio_visualizer_labels, cx,
                            |draft| draft.audio_visualizer_labels = !draft.audio_visualizer_labels,)))
                        .id("settings-visualizer-display-help")
                        .tooltip(settings_tooltip(if instance {
                            "Only this visualizer changes. These controls are also in its right-click menu. Bars and rings guides show normalized levels, not dB or frequency."
                        } else {
                            "Existing instances keep their choices. Right-click a visualizer to edit it. Bars and rings guides show normalized levels, not dB or frequency."
                        })),
                );
                if instance {
                    return column().child(display);
                }
                let content = column().gap_3()
                    .child(display)
                    .child(settings_group("Response & motion", column().gap_2()
                        .child(settings.single(Field::AudioVisualizerDb))
                        .child(settings.pair(Field::AudioVisualizerSensitivity, Field::AudioVisualizerBloomIntensity))
                        .child(settings.pair(Field::AudioVisualizerInnerDiameter, Field::AudioVisualizerRotationSpeed))
                        .child(row().child(visual_switch("audio-visualizer-fade-idle", "", "Fade when idle", draft.audio_visualizer_fade_when_idle, cx, |draft| draft.audio_visualizer_fade_when_idle = !draft.audio_visualizer_fade_when_idle)))
                        .id("settings-visualizer-response-motion-help")
                        .tooltip(settings_tooltip("Effect parameters are shared by all instances, regardless of the default style. Inner diameter, rotation and Fade when idle control the Bars, Rings and Bars + Rings styles. Fractal styles: silence fades the light; pause freezes it. Pulses follow energy rises, not detected beats."))))
                    .child(settings_group("Bars & rings", column().gap_2()
                        .child(settings.pair(Field::AudioVisualizerBarWidth, Field::AudioVisualizerBarGlowLayers))
                        .child(settings.single(Field::AudioVisualizerRingOpacity))
                        .id("settings-bars-rings-help")
                        .tooltip(settings_tooltip("Effect parameters are shared by all instances. Bloom affects bars and rings; bar glow requires bloom above zero."))));
                content.child(settings_group(
                    "Colors",
                    column()
                        .gap_2()
                        .child(settings.pair(
                            Field::AudioVisualizerPrimaryColor,
                            Field::AudioVisualizerSecondaryColor,
                        ))
                        .id("settings-visualizer-colors-help")
                        .tooltip(settings_tooltip("Colors are shared by all instances, independent of the shared color scheme.")),
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
                                    "",
                                    "Time labels",
                                    draft.waveform_labels,
                                    cx,
                                    |draft| draft.waveform_labels = !draft.waveform_labels,
                                ))
                                .child(visual_switch(
                                    "waveform-seek",
                                    "",
                                    "Click and drag to seek",
                                    draft.waveform_seek_enabled,
                                    cx,
                                    |draft| {
                                        draft.waveform_seek_enabled = !draft.waveform_seek_enabled
                                    },
                                )),
                        )
                        .id("settings-waveform-help")
                        .tooltip(settings_tooltip("The waterline stays still while paused.")),
                ))
                .child(settings_group(
                    "Amplitude display",
                    column()
                        .gap_2()
                        .child(settings.pair(Field::WaveformRmsGain, Field::WaveformPeakGain))
                        .child(settings.single(Field::WaveformPeakGamma))
                        .id("settings-amplitude-help")
                        .tooltip(settings_tooltip(
                            "Display only; does not affect playback volume.",
                        )),
                )),
        }
    }

    fn panel(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let panel_id = u64::MAX;
        let category = row()
            .flex_wrap()
            .flex_shrink_0()
            .gap_1()
            .px(gpui::px(UI_INSET))
            .py(gpui::px(UI_INSET))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(self.tab_button(
                "settings-general",
                "General",
                self.page == SettingsPage::General,
                cx,
                |settings| settings.page = SettingsPage::General,
            ))
            .child(self.tab_button(
                "settings-visualizations",
                "Visualizations",
                self.page == SettingsPage::Visualizations,
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
            .when(self.visualizer_panel.is_none(), |panel| {
                panel.child(category)
            });
        let page_id = match (self.page, self.visual_page) {
            (SettingsPage::General, _) => "settings-general-scroll",
            (_, VisualPage::Common) => "settings-common-scroll",
            (_, VisualPage::Spectrum) => "settings-spectrum-scroll",
            (_, VisualPage::Spectrogram) => "settings-spectrogram-scroll",
            (_, VisualPage::Waveform) => "settings-waveform-scroll",
            (_, VisualPage::AudioVisualizer) => "settings-audio-visualizer-scroll",
            (_, VisualPage::AlbumArtwork) => "settings-album-artwork-scroll",
        };
        let content = match self.page {
            SettingsPage::General => self.general_settings(cx),
            SettingsPage::Visualizations => {
                if self.visualizer_panel.is_none() {
                    panel = panel.child(self.visual_settings_tabs(cx));
                }
                self.visual_settings(cx)
            }
        };
        let scroll = self.scrolls.entry(page_id).or_default().clone();
        self.active_scroll = Some(scroll.clone());
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
        let feedback = if self.applying {
            status_message("Applying settings…", StatusKind::Info).into_any_element()
        } else {
            match &self.feedback {
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
                .child(disabled_control(
                    icon_button(
                        "settings-apply",
                        "",
                        "Apply settings",
                        cx,
                        |this, _, cx| this.apply_settings(cx),
                    ),
                    self.applying,
                ))
                .child(disabled_control(
                    icon_button(
                        "settings-discard",
                        "",
                        "Discard changes and close",
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
                    .min(POPOVER_MAX_HEIGHT * self.ui_scale)
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
                    .max_w(px(620.))
                    .w(relative(0.9))
                    .h((window.viewport_size().height - px(32.))
                        .max(px(0.))
                        .min(px(640.)))
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
                            .child(div().flex_1().text_lg().child("Settings")),
                    )
                    .child(panel),
            )
    }
}
