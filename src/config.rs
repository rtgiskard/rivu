//! User-editable TOML settings, separate from the library database and GUI layout.
//!
//! The file has library, playback, interface, logging, and visualizations tables;
//! interface contains album_artwork; visualizations contains the four audio-data views.
//! Missing fields use [`Config::default`]; unknown or misplaced fields and invalid
//! values are errors. `playback.repeat` accepts "off", "all", or "one". Omit
//! `playback.output_device` to use the system default. Paths are literal filesystem
//! paths, not shell input: no tilde or environment-variable expansion is performed.

use crate::model::RepeatMode;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

mod file;

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
            codec: false,
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub library_roots: Vec<PathBuf>,
    /// Maximum directory depth traversed from each requested scan start.
    pub scan_max_depth: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_device: Option<String>,
    pub volume: f32,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub play_count_threshold_percent: f64,
    pub tray_enabled: bool,
    pub mpris_enabled: bool,
    pub log_level: LogLevel,
    pub log_to_file: bool,
    pub log_retention_weeks: u32,
    /// Default number of rows fetched by GUI and TUI paginated views.
    pub page_size: u32,
    pub library_columns: LibraryColumns,
    /// Startup presentation language: auto, en-US, or zh-CN.
    pub ui_language: String,
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
            output_device: None,
            volume: 0.7,
            shuffle: false,
            repeat: RepeatMode::Off,
            play_count_threshold_percent: 20.0,
            tray_enabled: true,
            mpris_enabled: true,
            log_level: LogLevel::Warning,
            log_to_file: false,
            log_retention_weeks: 4,
            page_size: DEFAULT_PAGE_SIZE,
            library_columns: LibraryColumns::default(),
            ui_language: "auto".to_owned(),
            ui_font: "sans-serif".to_owned(),
            ui_scale: 1.0,
            layout_snap_enabled: true,
            layout_snap_grid_px: 8,
            still_mode: false,
            analysis_fps: 20,
            visual_background: RgbColor(0x08090c),
            visual_palette: VisualizationPalette::Deadbeef,
            spectrum_style: SpectrumStyle::Bars,
            analysis_fft_size: 8192,
            analysis_window: AnalysisWindow::BlackmanHarris,
            spectrum_interpolate: true,
            spectrum_bar_width: 3.0,
            spectrum_bars: 0,
            spectrum_gap: 1.0,
            spectrum_peaks: true,
            spectrum_peak_hold_ms: 400,
            spectrum_peak_gravity: 50.0,
            spectrum_bar_hold_ms: 0,
            spectrum_bar_gravity: 50.0,
            spectrum_smoothing_ms: 80,
            spectrum_grid: true,
            spectrum_labels: true,
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
            waveform_rms_gain: 1.6,
            waveform_peak_gain: 0.75,
            waveform_peak_gamma: 1.1,
            album_artwork_style: AlbumArtworkStyle::Plain,
            album_artwork_mode: AlbumArtworkMode::Static,
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

    /// Read settings without creating files or directories. Only a missing file
    /// selects defaults; unreadable, malformed, and invalid files are errors.
    pub fn load(path: &Path) -> Result<Self> {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot read configuration {}", path.display()));
            }
        };
        let file: file::FileConfig = toml::from_str(&contents)
            .with_context(|| format!("cannot parse TOML configuration {}", path.display()))?;
        let mut config = file.into_config();
        config.ui_language = crate::i18n::normalize_language(&config.ui_language).to_owned();
        if config.ui_font.is_empty() {
            config.ui_font = "sans-serif".to_owned();
        }
        config
            .validate()
            .with_context(|| format!("invalid configuration {}", path.display()))?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
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
            (1..=120).contains(&self.spectrogram_history_seconds),
            "visualizations.spectrogram.history_seconds must be between 1 and 120 seconds"
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

    /// Validate, serialize, and atomically replace the destination using a
    /// private same-directory temporary file. Failures before replacement
    /// leave the previous file untouched and clean up the temporary file.
    ///
    /// This deliberately does not fsync the file or directory: configuration
    /// writes must not block on storage flushes. Existing directory
    /// permissions are never changed.
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()
            .with_context(|| format!("cannot save invalid configuration {}", path.display()))?;
        let contents = toml::to_string_pretty(&file::FileConfigView::from(self))
            .with_context(|| format!("cannot serialize configuration {}", path.display()))?;
        ensure!(
            path.file_name().is_some(),
            "configuration path must name a file: {}",
            path.display()
        );
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut directories = fs::DirBuilder::new();
        directories.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directories.mode(0o700);
        }
        directories.create(parent).with_context(|| {
            format!("cannot create configuration directory {}", parent.display())
        })?;
        // tempfile creates the file with owner-only permissions (0600 on Unix).
        let mut temporary = tempfile::Builder::new()
            .prefix(".rivu-config-")
            .suffix(".tmp")
            .tempfile_in(parent)
            .with_context(|| {
                format!(
                    "cannot create temporary configuration in {}",
                    parent.display()
                )
            })?;
        temporary.write_all(contents.as_bytes()).with_context(|| {
            format!(
                "cannot write temporary configuration for {}",
                path.display()
            )
        })?;
        // Discard PersistError's owned file here so cleanup is immediate, rather
        // than deferred until the caller drops the returned error.
        temporary
            .persist(path)
            .map_err(|error| error.error)
            .with_context(|| {
                format!("cannot atomically replace configuration {}", path.display())
            })?;
        Ok(())
    }
}

/// The platform configuration directory, not Rivu's library/data directory.
/// If no home/configuration directory can be determined, use a local `.rivu`.
pub fn default_path() -> PathBuf {
    directories::ProjectDirs::from("", "", "rivu")
        .map(|dirs| dirs.config_dir().join("config.toml"))
        .unwrap_or_else(|| PathBuf::from(".rivu/config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_language_normalizes_and_round_trips_without_resolving_auto() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        assert_eq!(Config::load(&path).unwrap().ui_language, "auto");
        for (input, expected) in [("AUTO", "auto"), ("en_us", "en-US"), ("ZH_cn", "zh-CN")] {
            fs::write(&path, format!("[interface]\nlanguage = '{input}'\n")).unwrap();
            let config = Config::load(&path).unwrap();
            assert_eq!(config.ui_language, expected);
            config.save(&path).unwrap();
            assert_eq!(Config::load(&path).unwrap(), config);
        }
        for value in ["", "fr-FR", "zh-TW", "zh-Hant"] {
            fs::write(&path, format!("[interface]\nlanguage = '{value}'\n")).unwrap();
            let error = Config::load(&path).unwrap_err();
            assert!(format!("{error:#}").contains("interface.language"));
        }
    }

    #[test]
    fn library_column_selections_preserve_partial_defaults_and_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        for (contents, expected) in [
            ("", LibraryColumns::default()),
            (
                "[interface.library_columns]\nartist = true\n",
                LibraryColumns {
                    artist: true,
                    ..LibraryColumns::default()
                },
            ),
            (
                "[interface.library_columns]\nduration = false\n",
                LibraryColumns {
                    duration: false,
                    ..LibraryColumns::default()
                },
            ),
            (
                "[interface.library_columns]\nartist = false\nalbum = false\nduration = false\ncodec = false\nbitrate = false\n",
                LibraryColumns {
                    artist: false,
                    album: false,
                    duration: false,
                    codec: false,
                    bitrate: false,
                },
            ),
            (
                "[interface.library_columns]\nartist = true\nalbum = true\nduration = false\ncodec = true\nbitrate = true\n",
                LibraryColumns {
                    artist: true,
                    album: true,
                    duration: false,
                    codec: true,
                    bitrate: true,
                },
            ),
        ] {
            fs::write(&path, contents).unwrap();
            let config = Config::load(&path).unwrap();
            assert_eq!(config.library_columns, expected);
            config.save(&path).unwrap();
            assert_eq!(Config::load(&path).unwrap(), config);
        }
    }

    #[test]
    fn album_artwork_settings_round_trip_and_reject_invalid_motion_values() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        for (name, style) in [
            ("plain", AlbumArtworkStyle::Plain),
            ("vinyl", AlbumArtworkStyle::Vinyl),
            ("glass", AlbumArtworkStyle::Glass),
            ("halo", AlbumArtworkStyle::Halo),
            ("metal", AlbumArtworkStyle::Metal),
            ("card", AlbumArtworkStyle::Card),
            ("duotone", AlbumArtworkStyle::Duotone),
        ] {
            fs::write(&path, format!("interface.album_artwork.style = '{name}'\n")).unwrap();
            let config = Config::load(&path).unwrap();
            assert_eq!(config.album_artwork_style, style);
            config.save(&path).unwrap();
            let saved = fs::read_to_string(&path).unwrap();
            let saved: toml::Value = toml::from_str(&saved).unwrap();
            assert_eq!(
                saved["interface"]["album_artwork"]["style"].as_str(),
                Some(name)
            );
            assert!(saved["visualizations"].get("album_artwork").is_none());
            assert_eq!(Config::load(&path).unwrap(), config);
        }
        for mode in [AlbumArtworkMode::Static, AlbumArtworkMode::Rotating] {
            for direction in [
                AlbumArtworkDirection::Clockwise,
                AlbumArtworkDirection::Counterclockwise,
            ] {
                for period in [f32::MIN_POSITIVE, 0.125, 20.0, f32::MAX] {
                    let config = Config {
                        album_artwork_mode: mode,
                        album_artwork_period_seconds: period,
                        album_artwork_direction: direction,
                        ..Config::default()
                    };
                    config.save(&path).unwrap();
                    assert_eq!(Config::load(&path).unwrap(), config);
                }
            }
        }
        for period in ["0", "-1", "nan", "inf", "-inf"] {
            fs::write(
                &path,
                format!("interface.album_artwork.period_seconds = {period}\n"),
            )
            .unwrap();
            let error = Config::load(&path).unwrap_err();
            assert!(format!("{error:#}").contains("interface.album_artwork.period_seconds"));
        }
        Config::default().save(&path).unwrap();
        let original = fs::read(&path).unwrap();
        for period in [0.0, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let config = Config {
                album_artwork_period_seconds: period,
                ..Config::default()
            };
            assert!(config.save(&path).is_err());
            assert_eq!(fs::read(&path).unwrap(), original);
        }
        for fps in [0, 1, 60, 240] {
            fs::write(&path, format!("interface.album_artwork.fps = {fps}\n")).unwrap();
            let config = Config::load(&path).unwrap();
            assert_eq!(config.album_artwork_fps, fps);
            config.save(&path).unwrap();
            assert_eq!(Config::load(&path).unwrap(), config);
        }
        Config::default().save(&path).unwrap();
        let original = fs::read(&path).unwrap();
        for fps in [241, u16::MAX] {
            let config = Config {
                album_artwork_fps: fps,
                ..Config::default()
            };
            assert!(config.validate().is_err());
            assert!(config.save(&path).is_err());
            assert_eq!(fs::read(&path).unwrap(), original);
        }
        for invalid in [
            "style = 'unknown'",
            "mode = 'spinning'",
            "direction = 'left'",
            "unknown = true",
            "fps = -1",
            "fps = 241",
            "fps = 65536",
            "fps = 1.5",
            "fps = '60'",
        ] {
            fs::write(&path, format!("[interface.album_artwork]\n{invalid}\n")).unwrap();
            assert!(Config::load(&path).is_err());
        }
        fs::write(&path, "interface.album_artwork.mode = 'rotating'\n").unwrap();
        let partial = Config::load(&path).unwrap();
        assert_eq!(partial.album_artwork_mode, AlbumArtworkMode::Rotating);
        assert_eq!(partial.album_artwork_style, AlbumArtworkStyle::Plain);
        assert_eq!(partial.album_artwork_fps, 0);
        assert_eq!(
            partial.album_artwork_period_seconds,
            Config::default().album_artwork_period_seconds
        );
        assert_eq!(
            partial.album_artwork_direction,
            Config::default().album_artwork_direction
        );
    }

    #[test]
    fn album_artwork_rejects_obsolete_visualizations_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        for contents in [
            "[visualizations.album_artwork]\n",
            "visualizations.album_artwork.mode = 'rotating'\n",
            "[interface.album_artwork]\nmode = 'rotating'\n[visualizations.album_artwork]\nstyle = 'vinyl'\n",
        ] {
            fs::write(&path, contents).unwrap();
            assert!(Config::load(&path).is_err(), "accepted config: {contents}");
        }
    }

    #[test]
    fn grouped_file_defaults_partial_tables_and_font_normalization() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        for contents in [
            "",
            "[library]\n[playback]\n[interface]\n[logging]\n[visualizations]\n",
            "[visualizations.spectrum]\n[visualizations.spectrogram]\n[visualizations.audio_visualizer]\n[visualizations.waveform]\n",
        ] {
            fs::write(&path, contents).unwrap();
            assert_eq!(Config::load(&path).unwrap(), Config::default());
        }
        fs::write(
            &path,
            "[playback]\nshuffle = true\nrepeat = 'one'\n[interface]\nfont = ''\n[visualizations]\ndb_range = 95\n[visualizations.spectrum]\nbars = 42\n[visualizations.waveform]\nseek_enabled = false\n",
        ).unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(
            config,
            Config {
                shuffle: true,
                repeat: RepeatMode::One,
                visualization_db_range: 95.0,
                spectrum_bars: 42,
                waveform_seek_enabled: false,
                ..Config::default()
            }
        );
        assert_eq!(config.spectrum_db_range(), 95.0);
        assert_eq!(config.spectrogram_db_range(), 95.0);
        assert_eq!(config.audio_visualizer_db_range(), 95.0);
    }

    #[test]
    fn legacy_volume_migration_preserves_gain_across_save_and_reload() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        for percent in [0, 1, 50, 100] {
            fs::write(&path, format!("[playback]\nvolume = {percent}\n")).unwrap();
            let config = Config::load(&path).unwrap();
            let expected_gain = percent as f32 / 100.0;
            assert!((crate::volume::gain(config.volume) - expected_gain).abs() < 1e-6);
            config.save(&path).unwrap();
            let saved: toml::Value = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            let playback = saved["playback"].as_table().unwrap();
            assert!(!playback.contains_key("volume"));
            assert!(playback.contains_key("volume_percent"));
            let reloaded = Config::load(&path).unwrap();
            assert_eq!(reloaded.volume, config.volume);
            assert!((crate::volume::gain(reloaded.volume) - expected_gain).abs() < 1e-6);
        }
    }

    #[test]
    fn canonical_volume_accepts_numeric_percent_without_gain_migration() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        for (percent, level) in [
            ("0", 0.0),
            ("0.0", 0.0),
            ("1", 0.01),
            ("12.3456789", 0.123456789_f64 as f32),
            ("50", 0.5),
            ("100", 1.0),
            ("100.0", 1.0),
        ] {
            fs::write(&path, format!("[playback]\nvolume_percent = {percent}\n")).unwrap();
            let config = Config::load(&path).unwrap();
            assert_eq!(config.volume, level);
            config.save(&path).unwrap();
            assert_eq!(Config::load(&path).unwrap().volume, level);
        }
    }

    #[test]
    fn volume_percent_round_trip_preserves_continuous_level_precision() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        for volume in [
            0.0,
            f32::from_bits(1),
            f32::MIN_POSITIVE,
            0.12345679,
            f32::from_bits(0.5_f32.to_bits() + 1),
            f32::from_bits(1.0_f32.to_bits() - 1),
            1.0,
        ] {
            let config = Config {
                volume,
                ..Config::default()
            };
            config.save(&path).unwrap();
            assert_eq!(Config::load(&path).unwrap().volume, volume);
        }
    }

    #[test]
    fn volume_file_rejects_conflicting_keys_and_invalid_values() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        for keys in [
            "volume = 50\nvolume_percent = 50",
            "volume_percent = 0\nvolume = 0",
        ] {
            fs::write(&path, format!("[playback]\n{keys}\n")).unwrap();
            let error = Config::load(&path).unwrap_err();
            assert!(format!("{error:#}").contains("cannot coexist"));
        }
        for (key, invalid_values) in [
            (
                "volume",
                &[
                    "-1", "101", "256", "0.7", "70.0", "nan", "inf", "-inf", "'50'", "true", "[]",
                ][..],
            ),
            (
                "volume_percent",
                &[
                    "-1",
                    "-0.000001",
                    "100.000001",
                    "101",
                    "nan",
                    "inf",
                    "-inf",
                    "'50'",
                    "true",
                    "[]",
                ][..],
            ),
        ] {
            for value in invalid_values {
                fs::write(&path, format!("[playback]\n{key} = {value}\n")).unwrap();
                assert!(Config::load(&path).is_err(), "accepted {key} = {value}");
            }
        }
    }

    #[test]
    fn unknown_misplaced_legacy_and_mixed_file_keys_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        for contents in [
            "volume = 0.5",
            "library_roots = []",
            "volume = 0.5\n[playback]\nshuffle = true",
            "[audio]\nvolume = 0.5",
            "[integration]\ntray_enabled = true",
            "[analysis]\nfps = 30",
            "[miscellaneous]",
            "[library]\nvolume = 0.5",
            "[playback]\nui_scale = 1.0",
            "[interface]\nfont = 'sans-serif'\nui_font = 'serif'",
            "[logging]\nlog_level = 'debug'",
            "[visualizations]\nspectrum_bars = 42",
            "[visualizations.spectrum]\nunknown = true",
            "[visualizations.spectrogram]\nunknown = true",
            "[visualizations.audio_visualizer]\nunknown = true",
            "[visualizations.waveform]\nunknown = true",
            "[visualizations.unknown]",
            "[visualizations.spectrum]\nvolume = 0.5",
        ] {
            fs::write(&path, contents).unwrap();
            assert!(Config::load(&path).is_err(), "accepted config: {contents}");
        }
    }

    #[test]
    fn saved_file_has_only_the_stable_grouped_tables() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let config = Config::default();
        config.save(&path).unwrap();
        let saved: toml::Table = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.len(), 5);
        for group in [
            "library",
            "playback",
            "interface",
            "logging",
            "visualizations",
        ] {
            assert!(saved[group].is_table());
        }
        let visualizations = saved["visualizations"].as_table().unwrap();
        let nested: Vec<_> = visualizations
            .iter()
            .filter(|(_, value)| value.is_table())
            .map(|(key, _)| key.as_str())
            .collect();
        assert_eq!(
            nested,
            ["audio_visualizer", "spectrogram", "spectrum", "waveform"]
        );
    }

    fn entries(path: &Path) -> Vec<PathBuf> {
        let mut paths: Vec<_> = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        paths.sort();
        paths
    }

    #[test]
    fn missing_file_returns_defaults_without_creating_directories() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing/config.toml");
        assert_eq!(Config::load(&path).unwrap(), Config::default());
        assert!(entries(directory.path()).is_empty());
        directory.close().unwrap();
    }

    #[test]
    fn save_reload_replaces_file_and_leaves_no_temporary_files() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().join("settings");
        let path = parent.join("config.toml");
        Config::default().save(&path).unwrap();
        let config = Config {
            library_roots: vec![
                PathBuf::from("Music/Live recordings"),
                PathBuf::from("音楽"),
            ],
            scan_max_depth: 32,
            output_device: Some("USB DAC".to_owned()),
            volume: 0.25,
            shuffle: true,
            repeat: RepeatMode::All,
            play_count_threshold_percent: 35.5,
            tray_enabled: false,
            mpris_enabled: false,
            log_level: LogLevel::Debug,
            log_to_file: true,
            log_retention_weeks: 8,
            library_columns: LibraryColumns {
                artist: true,
                album: true,
                duration: false,
                codec: true,
                bitrate: true,
            },
            ui_font: "Noto Sans".to_owned(),
            ui_language: "zh-CN".to_owned(),
            ui_scale: 1.5,
            layout_snap_enabled: false,
            layout_snap_grid_px: 12,
            still_mode: true,
            analysis_fps: 30,
            visualization_db_range: 90.0,
            spectrum_db_range: Some(80.0),
            audio_visualizer_db_range: Some(60.0),
            media_read_buffer_mb: 8,
            ffmpeg_enabled: true,
            pipewire_auto_mix: false,
            spectrum_style: SpectrumStyle::Solid,
            visual_palette: VisualizationPalette::TokyoNight,
            visual_background: "#101820".parse().unwrap(),
            analysis_fft_size: 16384,
            analysis_window: AnalysisWindow::Hann,
            spectrum_interpolate: false,
            spectrum_bar_width: 4.0,
            spectrum_bars: 48,
            spectrum_gap: 0.5,
            spectrum_peaks: false,
            spectrum_peak_hold_ms: 500,
            spectrum_peak_gravity: 30.0,
            spectrum_bar_hold_ms: 100,
            spectrum_bar_gravity: 25.0,
            spectrum_smoothing_ms: 120,
            spectrum_grid: false,
            spectrum_labels: false,
            audio_visualizer_style: AudioVisualizerStyle::Rings,
            audio_visualizer_grid: true,
            audio_visualizer_labels: true,
            audio_visualizer_sensitivity: 2.0,
            audio_visualizer_rotation_speed: 1.0,
            audio_visualizer_bar_width: 0.8,
            audio_visualizer_bar_glow_layers: 3,
            audio_visualizer_ring_opacity: 0.7,
            audio_visualizer_bloom_intensity: 1.0,
            audio_visualizer_inner_diameter: 0.8,
            audio_visualizer_fade_when_idle: true,
            audio_visualizer_primary_color: "#123456".parse().unwrap(),
            audio_visualizer_secondary_color: "#654321".parse().unwrap(),
            spectrogram_labels: false,
            spectrogram_interpolate: false,
            spectrogram_interpolation_points: 2048,
            spectrogram_sampling_points_scale: 1.0,
            spectrogram_db_range: Some(100.0),
            spectrogram_history_seconds: 30,
            waveform_labels: false,
            waveform_seek_enabled: false,
            waveform_cursor_color: "#eeaa66".parse().unwrap(),
            waveform_glow: 0.0,
            waveform_rms_gain: 2.0,
            waveform_peak_gain: 0.5,
            waveform_peak_gamma: 1.25,
            album_artwork_style: AlbumArtworkStyle::Glass,
            album_artwork_mode: AlbumArtworkMode::Rotating,
            album_artwork_period_seconds: 12.5,
            album_artwork_direction: AlbumArtworkDirection::Counterclockwise,
            album_artwork_fps: 120,
            visualization_cache: false,
            page_size: 32,
        };
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
        assert_eq!(entries(&parent), vec![path.clone()]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
            assert_eq!(
                fs::metadata(&parent).unwrap().permissions().mode() & 0o077,
                0
            );
        }
        directory.close().unwrap();
    }

    #[test]
    fn scan_max_depth_round_trips_and_rejects_zero() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");

        let config = Config {
            scan_max_depth: 123,
            ..Config::default()
        };
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap().scan_max_depth, 123);

        fs::write(&path, "library.scan_max_depth = 0\n").unwrap();
        assert!(Config::load(&path).is_err());
        directory.close().unwrap();
    }

    #[test]
    fn layout_snapping_round_trips_limits_and_rejects_invalid_values_without_overwriting() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        for (enabled, spacing) in [(false, 1), (true, 64)] {
            let config = Config {
                layout_snap_enabled: enabled,
                layout_snap_grid_px: spacing,
                ..Config::default()
            };
            config.save(&path).unwrap();
            assert_eq!(Config::load(&path).unwrap(), config);
            let saved = fs::read_to_string(&path).unwrap();
            for invalid_spacing in [0, 65, u32::MAX] {
                let invalid = Config {
                    layout_snap_grid_px: invalid_spacing,
                    ..config.clone()
                };
                let error = invalid.save(&path).unwrap_err();
                assert!(format!("{error:#}").contains("interface.layout_snap_grid_px"));
                assert_eq!(fs::read_to_string(&path).unwrap(), saved);
            }
        }

        for value in ["0", "65", "-1", "1.5", "'8'"] {
            fs::write(&path, format!("interface.layout_snap_grid_px = {value}\n")).unwrap();
            assert!(Config::load(&path).is_err(), "accepted spacing {value}");
        }
        fs::write(&path, "interface.layout_snap_enabled = 'true'\n").unwrap();
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn malformed_unknown_and_invalid_values_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        for contents in [
            "playback.volume_percent = [",
            "playback.volume_percent = 'loud'",
            "playback.volume_percent = -0.01",
            "playback.volume_percent = 100.01",
            "playback.volume_percent = nan",
            "playback.volume_percent = inf",
            "playback.volume_percent = -inf",
            "playback.play_count_threshold_percent = -0.01",
            "playback.play_count_threshold_percent = 100.0",
            "playback.play_count_threshold_percent = nan",
            "playback.play_count_threshold_percent = inf",
            "logging.retention_weeks = 0",
            "interface.scale = 0.74",
            "interface.scale = 2.01",
            "interface.scale = nan",
            "interface.scale = inf",
            "visualizations.analysis_fps = 4",
            "visualizations.analysis_fps = 61",
            "visualizations.analysis_fps = -1",
            "visualizations.analysis_fps = 5.5",
            "visualizations.spectrum.db_range = 0",
            "visualizations.spectrum.db_range = 161",
            "visualizations.spectrogram.db_range = inf",
            "visualizations.analysis_fft_size = 256",
            "visualizations.analysis_fft_size = 65536",
            "visualizations.analysis_fft_size = 1000",
            "visualizations.analysis_window = 'hamming'",
            "visualizations.spectrum.bar_width = 0.9",
            "visualizations.spectrum.bar_width = 21",
            "visualizations.spectrum.bar_width = nan",
            "visualizations.spectrum.bars = 513",
            "visualizations.spectrum.gap = -1",
            "visualizations.spectrum.gap = 9",
            "visualizations.spectrum.gap = nan",
            "visualizations.audio_visualizer.sensitivity = -1",
            "visualizations.audio_visualizer.sensitivity = 6",
            "visualizations.audio_visualizer.bar_glow_layers = 5",
            "visualizations.audio_visualizer.ring_opacity = 2",
            "visualizations.spectrum.peak_hold_ms = 2001",
            "visualizations.spectrum.peak_gravity = -1",
            "visualizations.spectrum.peak_gravity = 501",
            "visualizations.spectrum.peak_gravity = inf",
            "visualizations.spectrum.bar_hold_ms = 2001",
            "visualizations.spectrum.bar_gravity = -1",
            "visualizations.spectrum.bar_gravity = 501",
            "visualizations.spectrum.bar_gravity = nan",
            "visualizations.spectrum.smoothing_ms = 1001",
            "visualizations.spectrogram.history_seconds = 0",
            "visualizations.waveform.glow = -1",
            "visualizations.waveform.glow = 3",
            "visualizations.waveform.rms_gain = -1",
            "visualizations.waveform.rms_gain = 4",
            "visualizations.waveform.peak_gain = -1",
            "visualizations.waveform.peak_gain = 3",
            "visualizations.waveform.peak_gamma = 0.1",
            "visualizations.waveform.peak_gamma = 3.1",
            "visualizations.background = '#12345'",
            "visualizations.background = '#gg0000'",
            "visualizations.background = '#00000000'",
            "visualizations.waveform.cursor_color = 123456",
            "visualizations.spectrum.style = 'diagonal'",
            "playback.repeat = 'forever'",
            "interface.library_columns = false",
            "interface.library_columns.artist = 'true'",
            "interface.library_columns.album = 1",
            "interface.library_columns.duration = []",
            "interface.library_columns.codec = 'flac'",
            "interface.library_columns.bitrate = 320",
            "interface.library_columns.title = true",
            "library.library_columns.duration = false",
        ] {
            fs::write(&path, contents).unwrap();
            assert!(
                Config::load(&path).is_err(),
                "accepted invalid config: {contents}"
            );
        }
    }

    #[test]
    fn analysis_windows_and_fft_sizes_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        for analysis_window in [
            AnalysisWindow::Hann,
            AnalysisWindow::BlackmanHarris,
            AnalysisWindow::None,
        ] {
            for analysis_fft_size in [512, 1024, 2048, 4096, 8192, 16384, 32768] {
                let config = Config {
                    analysis_window,
                    analysis_fft_size,
                    ..Config::default()
                };
                config.save(&path).unwrap();
                assert_eq!(Config::load(&path).unwrap(), config);
            }
        }
    }

    #[test]
    fn visualization_ranges_inherit_and_preserve_explicit_overrides() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mut config = Config {
            visualization_db_range: 85.0,
            ..Config::default()
        };
        assert_eq!(config.spectrum_db_range(), 85.0);
        assert_eq!(config.spectrogram_db_range(), 85.0);
        assert_eq!(config.audio_visualizer_db_range(), 85.0);
        config.save(&path).unwrap();
        let saved: toml::Table = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        for view in ["spectrum", "spectrogram", "audio_visualizer"] {
            assert!(
                !saved["visualizations"][view]
                    .as_table()
                    .unwrap()
                    .contains_key("db_range")
            );
        }
        assert!(
            !saved["playback"]
                .as_table()
                .unwrap()
                .contains_key("output_device")
        );
        assert_eq!(Config::load(&path).unwrap(), config);

        fs::write(
            &path,
            "visualizations.db_range = 85\nvisualizations.spectrum.db_range = 70\nvisualizations.spectrogram.db_range = 110\nvisualizations.audio_visualizer.db_range = 85\n",
        )
        .unwrap();
        config = Config::load(&path).unwrap();
        assert_eq!(config.spectrum_db_range, Some(70.0));
        assert_eq!(config.spectrogram_db_range, Some(110.0));
        assert_eq!(config.audio_visualizer_db_range, Some(85.0));
        config.visualization_db_range = 95.0;
        assert_eq!(config.spectrum_db_range(), 70.0);
        assert_eq!(config.spectrogram_db_range(), 110.0);
        assert_eq!(config.audio_visualizer_db_range(), 85.0);
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
        config.spectrum_db_range = None;
        assert_eq!(config.spectrum_db_range(), 95.0);
    }

    #[test]
    fn visualization_ranges_reject_nonfinite_and_out_of_range_values() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        for key in [
            "visualizations.db_range",
            "visualizations.spectrum.db_range",
            "visualizations.spectrogram.db_range",
            "visualizations.audio_visualizer.db_range",
        ] {
            for value in ["nan", "inf", "-inf", "0", "0.99", "160.01", "161"] {
                fs::write(&path, format!("{key} = {value}\n")).unwrap();
                assert!(Config::load(&path).is_err(), "accepted {key} = {value}");
            }
        }
    }

    #[test]
    fn visual_parameter_boundaries_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        for high in [false, true] {
            let config = Config {
                visualization_db_range: if high { 160.0 } else { 1.0 },
                spectrum_db_range: Some(if high { 160.0 } else { 1.0 }),
                spectrogram_db_range: Some(if high { 1.0 } else { 160.0 }),
                audio_visualizer_db_range: Some(if high { 160.0 } else { 1.0 }),
                spectrum_gap: if high { 8.0 } else { 0.0 },
                spectrum_peak_hold_ms: if high { 2000 } else { 0 },
                analysis_fft_size: if high { 32768 } else { 512 },
                spectrum_bar_width: if high { 20.0 } else { 1.0 },
                spectrum_bars: if high { 512 } else { 1 },
                spectrum_peak_gravity: if high { 500.0 } else { 0.0 },
                spectrum_bar_hold_ms: if high { 2000 } else { 0 },
                spectrum_bar_gravity: if high { 500.0 } else { 0.0 },
                spectrum_smoothing_ms: if high { 1000 } else { 0 },
                spectrogram_history_seconds: if high { 120 } else { 1 },
                spectrogram_interpolation_points: if high { 4096 } else { 64 },
                waveform_glow: if high { 2.0 } else { 0.0 },
                waveform_rms_gain: if high { 3.0 } else { 0.0 },
                waveform_peak_gain: if high { 2.0 } else { 0.0 },
                waveform_peak_gamma: if high { 3.0 } else { 0.25 },
                visual_background: if high {
                    RgbColor(0xffffff)
                } else {
                    RgbColor(0)
                },
                ..Config::default()
            };
            config.save(&path).unwrap();
            assert_eq!(Config::load(&path).unwrap(), config);
        }
    }

    #[test]
    fn boundary_values_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        for volume in [0.0, 1.0] {
            for ui_scale in [0.75, 2.0] {
                for analysis_fps in [5, 60] {
                    let config = Config {
                        volume,
                        ui_scale,
                        analysis_fps,
                        ..Config::default()
                    };
                    config.save(&path).unwrap();
                    assert_eq!(Config::load(&path).unwrap(), config);
                }
            }
        }
        assert_eq!(entries(directory.path()), vec![path]);
        directory.close().unwrap();
    }

    #[test]
    fn play_count_threshold_boundaries_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        for play_count_threshold_percent in [0.0, f64::from_bits(100.0_f64.to_bits() - 1)] {
            let config = Config {
                play_count_threshold_percent,
                ..Config::default()
            };
            config.save(&path).unwrap();
            assert_eq!(Config::load(&path).unwrap(), config);
        }
        directory.close().unwrap();
    }

    #[test]
    fn custom_spectrum_range_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        for spectrum_style in [
            SpectrumStyle::Bars,
            SpectrumStyle::Outline,
            SpectrumStyle::Led,
            SpectrumStyle::Line,
            SpectrumStyle::Solid,
        ] {
            let config = Config {
                spectrum_style,
                spectrum_db_range: Some(55.0),
                ..Config::default()
            };
            config.save(&path).unwrap();
            assert_eq!(Config::load(&path).unwrap(), config);
        }
    }

    #[test]
    fn invalid_save_preserves_previous_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        Config::default().save(&path).unwrap();
        let original = fs::read(&path).unwrap();
        let invalid = Config {
            volume: f32::NAN,
            ..Config::default()
        };
        assert!(invalid.save(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(entries(directory.path()), vec![path]);
        directory.close().unwrap();
    }

    #[test]
    fn failed_replacement_cleans_temporary_file_immediately() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::create_dir(&path).unwrap();
        let sentinel = path.join("keep");
        fs::write(&sentinel, "unchanged").unwrap();
        let _error = Config::default().save(&path).unwrap_err();
        // Keep the error alive while checking cleanup of PersistError's file.
        assert_eq!(entries(directory.path()), vec![path]);
        assert_eq!(fs::read_to_string(sentinel).unwrap(), "unchanged");
        directory.close().unwrap();
    }

    #[test]
    fn unreadable_configuration_is_not_treated_as_missing() {
        let directory = tempfile::tempdir().unwrap();
        assert!(Config::load(directory.path()).is_err());
        directory.close().unwrap();
    }

    #[test]
    fn log_retention_weeks_must_be_bounded() {
        for log_retention_weeks in [0, 521] {
            let config = Config {
                log_retention_weeks,
                ..Config::default()
            };
            assert!(config.validate().is_err());
        }
    }

    #[test]
    fn page_size_round_trips_and_rejects_out_of_range_values() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        for page_size in [20, 64, 256] {
            fs::write(&path, format!("interface.page_size = {page_size}\n")).unwrap();
            let config = Config::load(&path).unwrap();
            assert_eq!(config.page_size, page_size);
            config.save(&path).unwrap();
            assert_eq!(Config::load(&path).unwrap().page_size, page_size);
        }
        for page_size in [0, 19, 257, u32::MAX] {
            fs::write(&path, format!("interface.page_size = {page_size}\n")).unwrap();
            assert!(Config::load(&path).is_err());
        }
    }
}
