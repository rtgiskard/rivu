//! User-editable TOML settings, separate from the library database and GUI layout.
//!
//! The file has library, playback, interface, logging, and visualizations tables;
//! interface contains theme and album_artwork; visualizations contains the four audio-data views.
//! Missing fields use [`Config::default`]; unknown or misplaced fields and invalid
//! values are errors. `playback.repeat` accepts "off", "all", or "one". Omit
//! `playback.output_device` to use the system default. Paths are literal filesystem
//! paths, not shell input: no tilde or environment-variable expansion is performed.

use crate::model::RepeatMode;
use crate::theme::Theme;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

mod file;
mod persistence;

pub use persistence::default_path;

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlbumArtworkStyle {
    Plain,
    Vinyl,
    Glass,
    Halo,
    Metal,
    Card,
    Duotone,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlbumArtworkMode {
    Static,
    Rotating,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlbumArtworkDirection {
    Clockwise,
    Counterclockwise,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlbumArtworkOptions {
    pub style: AlbumArtworkStyle,
    pub mode: AlbumArtworkMode,
    pub direction: AlbumArtworkDirection,
    pub period_seconds: f32,
}

impl From<&Config> for AlbumArtworkOptions {
    fn from(config: &Config) -> Self {
        Self {
            style: config.album_artwork_style,
            mode: config.album_artwork_mode,
            direction: config.album_artwork_direction,
            period_seconds: config.album_artwork_period_seconds,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum VisualizationPalette {
    #[serde(rename = "tokyo_night")]
    TokyoNight,
    #[serde(rename = "deadbeef")]
    Deadbeef,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioVisualizerStyle {
    Bars,
    Rings,
    BarsRings,
    StereoDensity,
    SpatialSpectrum,
    FractalBeams,
    FractalFlow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioVisualizerOptions {
    pub style: AudioVisualizerStyle,
    pub reference_lines: bool,
    pub labels: bool,
}

impl From<&Config> for AudioVisualizerOptions {
    fn from(config: &Config) -> Self {
        Self {
            style: config.audio_visualizer_style,
            reference_lines: config.audio_visualizer_grid,
            labels: config.audio_visualizer_labels,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpectrumStyle {
    Bars,
    Outline,
    Led,
    Line,
    Solid,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Debug,
    Info,
    #[default]
    Warning,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisWindow {
    Hann,
    BlackmanHarris,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RgbColor(u32);

impl RgbColor {
    pub fn rgb(self) -> u32 {
        self.0
    }
}

impl std::str::FromStr for RgbColor {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let hex = value
            .strip_prefix('#')
            .filter(|hex| hex.len() == 6 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .context("Color must use #RRGGBB notation")?;
        Ok(Self(u32::from_str_radix(hex, 16)?))
    }
}

impl TryFrom<String> for RgbColor {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self> {
        value.parse()
    }
}

impl std::fmt::Display for RgbColor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "#{:06x}", self.0)
    }
}

impl From<RgbColor> for String {
    fn from(value: RgbColor) -> Self {
        value.to_string()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LibraryColumn {
    Artist,
    Album,
    Duration,
    Codec,
    Bitrate,
}

impl LibraryColumn {
    pub const ALL: [Self; 5] = [
        Self::Artist,
        Self::Album,
        Self::Duration,
        Self::Codec,
        Self::Bitrate,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Artist => "Artist",
            Self::Album => "Album",
            Self::Duration => "Duration",
            Self::Codec => "Codec",
            Self::Bitrate => "Bitrate",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Artist => "artist",
            Self::Album => "album",
            Self::Duration => "duration",
            Self::Codec => "codec",
            Self::Bitrate => "bitrate",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LibraryColumns {
    pub artist: bool,
    pub album: bool,
    pub duration: bool,
    pub codec: bool,
    pub bitrate: bool,
}

impl Default for LibraryColumns {
    fn default() -> Self {
        Self {
            artist: false,
            album: false,
            duration: true,
            codec: true,
            bitrate: false,
        }
    }
}

impl LibraryColumns {
    pub fn enabled(self, column: LibraryColumn) -> bool {
        match column {
            LibraryColumn::Artist => self.artist,
            LibraryColumn::Album => self.album,
            LibraryColumn::Duration => self.duration,
            LibraryColumn::Codec => self.codec,
            LibraryColumn::Bitrate => self.bitrate,
        }
    }

    pub(crate) fn toggle(&mut self, column: LibraryColumn) {
        let enabled = match column {
            LibraryColumn::Artist => &mut self.artist,
            LibraryColumn::Album => &mut self.album,
            LibraryColumn::Duration => &mut self.duration,
            LibraryColumn::Codec => &mut self.codec,
            LibraryColumn::Bitrate => &mut self.bitrate,
        };
        *enabled = !*enabled;
    }
}

pub(crate) const DEFAULT_PAGE_SIZE: u32 = 64;
pub(crate) const DEFAULT_SCAN_MAX_DEPTH: u32 = 64;

// Runtime fields are declared once, independently of TOML and IPC representations.
// Keep Settings requests field-based: omitted fields belong to the latest Core state.
// Nested Option fields distinguish leaving a value alone from explicitly clearing it.
macro_rules! config_fields {
    ($( $(#[$attr:meta])* pub $field:ident : $ty:ty, )*) => {
        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct Config {
            $( $(#[$attr])* pub $field: $ty, )*
        }

        #[derive(Clone, Debug, Default, PartialEq)]
        pub(crate) struct ConfigPatch {
            $( $field: Option<$ty>, )*
        }

        impl ConfigPatch {
            pub(crate) fn between(baseline: &Config, draft: &Config) -> Self {
                fn changed<T: Clone + PartialEq>(baseline: &T, draft: &T) -> Option<T> {
                    (baseline != draft).then(|| draft.clone())
                }
                Self {
                    $( $field: changed(&baseline.$field, &draft.$field), )*
                }
            }

            pub(crate) fn apply_to(self, config: &mut Config) {
                $( if let Some(value) = self.$field { config.$field = value; } )*
            }
        }
    };
}

config_fields! {
    pub library_roots: Vec<PathBuf>,
    /// Maximum directory depth traversed from each requested scan start.
    pub scan_max_depth: u32,
    /// Minimum duration in seconds for newly admitted tracks; 0 disables filtering.
    pub scan_min_duration_secs: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_device: Option<String>,
    pub volume: f32,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub play_count_threshold_percent: f64,
    /// Confirm GUI album and playlist actions that replace a non-empty queue.
    pub confirm_queue_replace: bool,
    pub tray_enabled: bool,
    pub mpris_enabled: bool,
    pub log_level: LogLevel,
    pub log_to_file: bool,
    pub log_retention_weeks: u32,
    /// Default number of rows fetched by GUI and TUI paginated views.
    pub page_size: u32,
    /// Retained decoded real-cover frames per cover, in MiB.
    pub artwork_decode_budget_mb: u32,
    /// Shared GPU material-buffer admission budget, applied at GUI startup.
    pub gpu_material_budget_mb: u32,
    pub library_columns: LibraryColumns,
    /// Startup presentation language: auto, en-US, or zh-CN.
    pub ui_language: String,
    /// Shared GUI and TUI color preset, independent of visualization colors.
    pub ui_theme: Theme,
    /// UI font family; defaults to sans-serif, including empty saved values.
    pub ui_font: String,
    pub ui_scale: f32,
    pub layout_snap_enabled: bool,
    /// Layout snap grid spacing in logical pixels before interface scaling.
    pub layout_snap_grid_px: u32,
    /// Stop audio visualizations and their analysis without changing playback.
    pub still_mode: bool,
    pub analysis_fps: u32,
    pub visual_background: RgbColor,
    pub visual_palette: VisualizationPalette,
    pub spectrum_style: SpectrumStyle,
    pub analysis_fft_size: u32,
    pub analysis_window: AnalysisWindow,
    pub spectrum_interpolate: bool,
    pub spectrum_bar_width: f32,
    pub spectrum_bars: u32,
    pub spectrum_gap: f32,
    pub spectrum_peaks: bool,
    pub spectrum_peak_hold_ms: u32,
    /// Falling acceleration in dB/s²; zero snaps to the current level after hold.
    pub spectrum_peak_gravity: f32,
    pub spectrum_bar_hold_ms: u32,
    /// Falling acceleration in dB/s²; zero snaps to the current level after hold.
    pub spectrum_bar_gravity: f32,
    pub spectrum_smoothing_ms: u32,
    pub spectrum_grid: bool,
    pub spectrum_labels: bool,
    pub audio_visualizer_style: AudioVisualizerStyle,
    pub audio_visualizer_grid: bool,
    pub audio_visualizer_labels: bool,
    pub audio_visualizer_sensitivity: f32,
    pub audio_visualizer_rotation_speed: f32,
    pub audio_visualizer_bar_width: f32,
    pub audio_visualizer_bar_glow_layers: u32,
    pub audio_visualizer_ring_opacity: f32,
    pub audio_visualizer_bloom_intensity: f32,
    pub audio_visualizer_inner_diameter: f32,
    pub audio_visualizer_fade_when_idle: bool,
    pub audio_visualizer_primary_color: RgbColor,
    pub audio_visualizer_secondary_color: RgbColor,

    pub spectrogram_labels: bool,
    pub spectrogram_interpolate: bool,
    /// Scale applied to the visible panel height when choosing source rows.
    pub spectrogram_sampling_points_scale: f32,
    /// Maximum logarithmic frequency rows generated for each cached column.
    pub spectrogram_interpolation_points: u32,
    pub waveform_labels: bool,
    pub waveform_seek_enabled: bool,
    /// Default display span inherited by visualizations without an override.
    pub visualization_db_range: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spectrum_db_range: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spectrogram_db_range: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_visualizer_db_range: Option<f32>,
    /// Maximum continuous audio history shown by Spectrogram.
    pub spectrogram_history_seconds: u32,
    pub waveform_cursor_color: RgbColor,
    pub waveform_glow: f32,
    pub waveform_rms_gain: f32,
    pub waveform_peak_gain: f32,
    pub waveform_peak_gamma: f32,
    pub album_artwork_style: AlbumArtworkStyle,
    pub album_artwork_mode: AlbumArtworkMode,
    /// Positive finite seconds per full rotation, used for new artwork panels.
    pub album_artwork_period_seconds: f32,
    pub album_artwork_direction: AlbumArtworkDirection,
    /// Animation request cap (0 follows display callbacks), at most 240 FPS.
    pub album_artwork_fps: u16,
    /// Enable reusable caches owned by visualization controls.
    pub visualization_cache: bool,
    pub media_read_buffer_mb: u32,
    pub ffmpeg_enabled: bool,
    pub pipewire_auto_mix: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            library_roots: Vec::new(),
            scan_max_depth: DEFAULT_SCAN_MAX_DEPTH,
            scan_min_duration_secs: 0,
            output_device: None,
            volume: 0.7,
            shuffle: false,
            repeat: RepeatMode::Off,
            play_count_threshold_percent: 20.0,
            confirm_queue_replace: true,
            tray_enabled: true,
            mpris_enabled: true,
            log_level: LogLevel::Warning,
            log_to_file: false,
            log_retention_weeks: 4,
            page_size: DEFAULT_PAGE_SIZE,
            artwork_decode_budget_mb: 64,
            gpu_material_budget_mb: 64,
            library_columns: LibraryColumns::default(),
            ui_language: "auto".to_owned(),
            ui_theme: Theme::default(),
            ui_font: "sans-serif".to_owned(),
            ui_scale: 1.0,
            layout_snap_enabled: true,
            layout_snap_grid_px: 16,
            still_mode: false,
            analysis_fps: 20,
            visual_background: RgbColor(0x08090c),
            visual_palette: VisualizationPalette::Deadbeef,
            spectrum_style: SpectrumStyle::Solid,
            analysis_fft_size: 8192,
            analysis_window: AnalysisWindow::BlackmanHarris,
            spectrum_interpolate: false,
            spectrum_bar_width: 3.0,
            spectrum_bars: 0,
            spectrum_gap: 1.0,
            spectrum_peaks: true,
            spectrum_peak_hold_ms: 400,
            spectrum_peak_gravity: 50.0,
            spectrum_bar_hold_ms: 0,
            spectrum_bar_gravity: 200.0,
            spectrum_smoothing_ms: 80,
            spectrum_grid: true,
            spectrum_labels: false,
            audio_visualizer_style: AudioVisualizerStyle::BarsRings,
            audio_visualizer_grid: false,
            audio_visualizer_labels: false,
            audio_visualizer_sensitivity: 1.5,
            audio_visualizer_rotation_speed: 0.5,
            audio_visualizer_bar_width: 0.6,
            audio_visualizer_bar_glow_layers: 2,
            audio_visualizer_ring_opacity: 0.8,
            audio_visualizer_bloom_intensity: 0.5,
            audio_visualizer_inner_diameter: 0.7,
            audio_visualizer_fade_when_idle: false,
            audio_visualizer_primary_color: RgbColor(0x7aa2f7),
            audio_visualizer_secondary_color: RgbColor(0xbb9af7),

            spectrogram_labels: true,
            spectrogram_interpolate: true,
            spectrogram_sampling_points_scale: 0.5,
            spectrogram_interpolation_points: 1024,
            waveform_labels: true,
            waveform_seek_enabled: true,
            visualization_db_range: 70.0,
            spectrum_db_range: None,
            spectrogram_db_range: None,
            audio_visualizer_db_range: None,
            spectrogram_history_seconds: 20,
            waveform_cursor_color: RgbColor(0x73daca),
            waveform_glow: 1.0,
            waveform_rms_gain: 0.94,
            waveform_peak_gain: 1.0,
            waveform_peak_gamma: 0.86,
            album_artwork_style: AlbumArtworkStyle::Halo,
            album_artwork_mode: AlbumArtworkMode::Rotating,
            album_artwork_period_seconds: 20.0,
            album_artwork_direction: AlbumArtworkDirection::Clockwise,
            album_artwork_fps: 0,
            visualization_cache: true,
            media_read_buffer_mb: 2,
            ffmpeg_enabled: false,
            pipewire_auto_mix: true,
        }
    }
}

impl Config {
    pub fn spectrum_db_range(&self) -> f32 {
        self.spectrum_db_range
            .unwrap_or(self.visualization_db_range)
    }

    pub fn spectrogram_db_range(&self) -> f32 {
        self.spectrogram_db_range
            .unwrap_or(self.visualization_db_range)
    }

    pub fn audio_visualizer_db_range(&self) -> f32 {
        self.audio_visualizer_db_range
            .unwrap_or(self.visualization_db_range)
    }

    pub(crate) fn gpu_material_budget_bytes(&self) -> u64 {
        u64::from(self.gpu_material_budget_mb) * 1024 * 1024
    }

    pub fn validate(&self) -> Result<()> {
        let mut roots = self
            .library_roots
            .iter()
            .map(|root| crate::library::logical_path(root))
            .collect::<Result<Vec<_>>>()?;
        roots.sort_unstable();
        for pair in roots.windows(2) {
            ensure!(
                !pair[1].starts_with(&pair[0]),
                "library.roots must not overlap: {} and {}",
                pair[0].display(),
                pair[1].display()
            );
        }
        ensure!(
            matches!(self.ui_language.as_str(), "auto" | "en-US" | "zh-CN"),
            "interface.language must be auto, en-US, or zh-CN; got {}",
            self.ui_language
        );
        ensure!(
            self.scan_max_depth >= 1,
            "library.scan_max_depth must be at least 1; got {}",
            self.scan_max_depth
        );
        ensure!(
            self.volume.is_finite() && (0.0..=1.0).contains(&self.volume),
            "playback.volume_percent must be finite and between 0 and 100 (inclusive); got {}",
            f64::from(self.volume) * 100.
        );
        ensure!(
            self.play_count_threshold_percent.is_finite()
                && (0.0..100.0).contains(&self.play_count_threshold_percent),
            "playback.play_count_threshold_percent must be finite and at least 0.0 but less than 100.0; got {}",
            self.play_count_threshold_percent
        );
        ensure!(
            self.ui_font.len() <= 128 && !self.ui_font.chars().any(char::is_control),
            "interface.font must be at most 128 characters without control characters"
        );
        ensure!(
            self.ui_scale.is_finite() && (0.75..=2.0).contains(&self.ui_scale),
            "interface.scale must be finite and between 0.75 and 2.0 (inclusive); got {}",
            self.ui_scale
        );
        ensure!(
            (1..=64).contains(&self.layout_snap_grid_px),
            "interface.layout_snap_grid_px must be between 1 and 64 (inclusive); got {}",
            self.layout_snap_grid_px
        );
        ensure!(
            matches!(self.media_read_buffer_mb, 1 | 2 | 4 | 8 | 16),
            "playback.media_read_buffer_mb must be one of 1, 2, 4, 8, or 16 MiB; got {}",
            self.media_read_buffer_mb
        );
        ensure!(
            (16..=1024).contains(&self.gpu_material_budget_mb),
            "interface.gpu_material_budget_mb must be between 16 and 1024 MiB"
        );
        ensure!(
            (1..=1024).contains(&self.artwork_decode_budget_mb),
            "interface.artwork_decode_budget_mb must be between 1 and 1024 MiB; got {}",
            self.artwork_decode_budget_mb
        );
        ensure!(
            (1..=520).contains(&self.log_retention_weeks),
            "logging.retention_weeks must be between 1 and 520; got {}",
            self.log_retention_weeks
        );
        ensure!(
            (20..=crate::model::MAX_QUERY_ROWS as u32).contains(&self.page_size),
            "interface.page_size must be between 20 and 256; got {}",
            self.page_size
        );
        ensure!(
            (5..=60).contains(&self.analysis_fps),
            "visualizations.analysis_fps must be between 5 and 60 (inclusive); got {}",
            self.analysis_fps
        );
        for (name, range) in [
            ("visualizations.db_range", Some(self.visualization_db_range)),
            ("visualizations.spectrum.db_range", self.spectrum_db_range),
            (
                "visualizations.spectrogram.db_range",
                self.spectrogram_db_range,
            ),
            (
                "visualizations.audio_visualizer.db_range",
                self.audio_visualizer_db_range,
            ),
        ] {
            if let Some(range) = range {
                ensure!(
                    range.is_finite() && (1.0..=160.0).contains(&range),
                    "{name} must be finite and between 1 and 160 dB; got {range}"
                );
            }
        }
        ensure!(
            matches!(
                self.analysis_fft_size,
                512 | 1024 | 2048 | 4096 | 8192 | 16384 | 32768
            ),
            "visualizations.analysis_fft_size must be a power of two from 512 to 32768"
        );
        ensure!(
            self.spectrum_bar_width.is_finite() && (1.0..=20.0).contains(&self.spectrum_bar_width),
            "visualizations.spectrum.bar_width must be between 1 and 20 pixels"
        );
        ensure!(
            self.spectrum_bars <= 512,
            "visualizations.spectrum.bars must be 0 (auto) or between 1 and 512"
        );
        ensure!(
            self.spectrum_gap.is_finite() && (0.0..=8.0).contains(&self.spectrum_gap),
            "visualizations.spectrum.gap must be between 0 and 8 pixels"
        );
        ensure!(
            self.audio_visualizer_bar_glow_layers <= 4,
            "visualizations.audio_visualizer.bar_glow_layers must be between 0 and 4"
        );
        for (name, value, range) in [
            (
                "visualizations.audio_visualizer.sensitivity",
                self.audio_visualizer_sensitivity,
                0.0..=5.0,
            ),
            (
                "visualizations.audio_visualizer.rotation_speed",
                self.audio_visualizer_rotation_speed,
                0.0..=10.0,
            ),
            (
                "visualizations.audio_visualizer.bar_width",
                self.audio_visualizer_bar_width,
                0.0..=2.0,
            ),
            (
                "visualizations.audio_visualizer.ring_opacity",
                self.audio_visualizer_ring_opacity,
                0.0..=1.0,
            ),
            (
                "visualizations.audio_visualizer.bloom_intensity",
                self.audio_visualizer_bloom_intensity,
                0.0..=2.0,
            ),
            (
                "visualizations.audio_visualizer.inner_diameter",
                self.audio_visualizer_inner_diameter,
                0.0..=2.0,
            ),
        ] {
            ensure!(
                value.is_finite() && range.contains(&value),
                "{name} is outside its supported range; got {value}"
            );
        }
        for (name, hold) in [
            (
                "visualizations.spectrum.peak_hold_ms",
                self.spectrum_peak_hold_ms,
            ),
            (
                "visualizations.spectrum.bar_hold_ms",
                self.spectrum_bar_hold_ms,
            ),
        ] {
            ensure!(hold <= 2000, "{name} must be between 0 and 2000 ms");
        }
        for (name, gravity) in [
            (
                "visualizations.spectrum.peak_gravity",
                self.spectrum_peak_gravity,
            ),
            (
                "visualizations.spectrum.bar_gravity",
                self.spectrum_bar_gravity,
            ),
        ] {
            ensure!(
                gravity.is_finite() && (0.0..=500.0).contains(&gravity),
                "{name} must be between 0 and 500 dB/s²"
            );
        }
        ensure!(
            self.spectrum_smoothing_ms <= 1000,
            "visualizations.spectrum.smoothing_ms must be between 0 and 1000 ms"
        );
        ensure!(
            self.spectrogram_sampling_points_scale.is_finite()
                && (0.4..=1.4).contains(&self.spectrogram_sampling_points_scale),
            "visualizations.spectrogram.sampling_points_scale must be between 0.4 and 1.4"
        );
        ensure!(
            (1..=60).contains(&self.spectrogram_history_seconds),
            "visualizations.spectrogram.history_seconds must be between 1 and 60 seconds"
        );
        ensure!(
            (64..=4096).contains(&self.spectrogram_interpolation_points),
            "visualizations.spectrogram.interpolation_points must be between 64 and 4096"
        );
        ensure!(
            self.waveform_glow.is_finite() && (0.0..=2.0).contains(&self.waveform_glow),
            "visualizations.waveform.glow must be between 0 and 2"
        );
        for (name, value, range) in [
            (
                "visualizations.waveform.rms_gain",
                self.waveform_rms_gain,
                0.0..=3.0,
            ),
            (
                "visualizations.waveform.peak_gain",
                self.waveform_peak_gain,
                0.0..=2.0,
            ),
            (
                "visualizations.waveform.peak_gamma",
                self.waveform_peak_gamma,
                0.25..=3.0,
            ),
        ] {
            ensure!(
                value.is_finite() && range.contains(&value),
                "{name} is outside its supported range; got {value}"
            );
        }
        ensure!(
            self.album_artwork_period_seconds.is_finite()
                && self.album_artwork_period_seconds > 0.0,
            "interface.album_artwork.period_seconds must be finite and greater than zero"
        );
        ensure!(
            self.album_artwork_fps <= 240,
            "interface.album_artwork.fps must be between 0 and 240"
        );
        Ok(())
    }
}
