//! User-editable TOML settings, separate from the library database and GUI layout.
//!
//! Missing fields use [`Config::default`]; unknown fields and invalid values are
//! errors. `repeat` accepts `"off"`, `"all"`, or `"one"`. Omit `output_device` to
//! use the system default. Paths are literal filesystem paths, not shell input:
//! no tilde or environment-variable expansion is performed.

use crate::model::RepeatMode;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::LazyLock,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpectrumStyle {
    Bars,
    Outline,
    Led,
    Line,
    Solid,
}

fn is_nerd_font_family(family: &str) -> bool {
    let family = family.trim().to_ascii_lowercase();
    family.contains("nerd font")
        || family.contains("nerdfont")
        || family.ends_with(" nf")
        || family.ends_with(" nfm")
        || family.ends_with(" nfp")
}

pub(crate) fn nerd_fonts_available() -> bool {
    static AVAILABLE: LazyLock<bool> = LazyLock::new(|| {
        Command::new("fc-list")
            .args(["--format", "%{family}\\n"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .is_some_and(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .split(['\n', ','])
                    .any(is_nerd_font_family)
            })
    });
    *AVAILABLE
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpectrumWindow {
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub library_roots: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_device: Option<String>,
    pub volume: f32,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub play_count_threshold_percent: f64,
    pub mpris_enabled: bool,
    pub ui_scale: f32,
    pub analysis_fps: u32,
    pub visual_background: RgbColor,
    pub spectrum_style: SpectrumStyle,
    pub spectrum_min_hz: f32,
    pub spectrum_max_hz: f32,
    pub spectrum_fft_size: u32,
    pub spectrum_window: SpectrumWindow,
    pub spectrum_bands_per_octave: u32,
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
    pub spectrum_log_scale: bool,
    pub spectrum_grid: bool,
    pub spectrum_labels: bool,
    pub spectrogram_labels: bool,
    pub waveform_labels: bool,
    pub spectrogram_min_hz: f32,
    pub spectrogram_max_hz: f32,
    pub spectrum_db_range: f32,
    pub spectrogram_db_range: f32,
    pub spectrogram_log_scale: bool,
    pub spectrogram_history_seconds: u32,
    pub waveform_cursor_color: RgbColor,
    pub waveform_glow: f32,
    pub media_read_buffer_mb: u32,
    pub nerd_symbols: bool,
    pub ffmpeg_enabled: bool,
    pub pipewire_auto_mix: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            library_roots: Vec::new(),
            output_device: None,
            volume: 0.7,
            shuffle: false,
            repeat: RepeatMode::Off,
            play_count_threshold_percent: 20.0,
            mpris_enabled: true,
            ui_scale: 1.0,
            analysis_fps: 20,
            visual_background: RgbColor(0x08090c),
            spectrum_style: SpectrumStyle::Bars,
            spectrum_min_hz: 20.0,
            spectrum_max_hz: 20_000.0,
            spectrum_fft_size: 8192,
            spectrum_window: SpectrumWindow::BlackmanHarris,
            spectrum_bands_per_octave: 24,
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
            spectrum_log_scale: true,
            spectrum_grid: true,
            spectrum_labels: true,
            spectrogram_labels: true,
            waveform_labels: true,
            spectrogram_min_hz: 20.0,
            spectrogram_max_hz: 20_000.0,
            spectrum_db_range: 70.0,
            spectrogram_db_range: 70.0,
            spectrogram_log_scale: true,
            spectrogram_history_seconds: 10,
            waveform_cursor_color: RgbColor(0x73daca),
            waveform_glow: 1.0,
            media_read_buffer_mb: 2,
            nerd_symbols: nerd_fonts_available(),
            ffmpeg_enabled: false,
            pipewire_auto_mix: true,
        }
    }
}

impl Config {
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
        let mut document: toml::Table = toml::from_str(&contents)
            .with_context(|| format!("cannot parse TOML configuration {}", path.display()))?;
        // Removed visual controls are discarded so older configurations remain loadable.
        for key in [
            "visual_palette",
            "spectrogram_palette",
            "waveform_palette",
            "waveform_played_opacity",
        ] {
            document.remove(key);
        }
        // Constant release speed has no equivalent acceleration. Retire the old
        // key; absent gravity uses its default, while explicit gravity wins.
        document.remove("spectrum_peak_release_db_per_second");
        // The former shared range migrates once on load; saves use only the
        // independent ranges, and explicit new values take precedence.
        if let Some(range) = document.remove("analysis_db_range") {
            document
                .entry("spectrum_db_range")
                .or_insert_with(|| range.clone());
            document.entry("spectrogram_db_range").or_insert(range);
        }
        let config: Self = document
            .try_into()
            .with_context(|| format!("cannot parse TOML configuration {}", path.display()))?;
        config
            .validate()
            .with_context(|| format!("invalid configuration {}", path.display()))?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.volume.is_finite() && (0.0..=1.0).contains(&self.volume),
            "volume must be finite and between 0.0 and 1.0 (inclusive); got {}",
            self.volume
        );
        ensure!(
            self.play_count_threshold_percent.is_finite()
                && (0.0..100.0).contains(&self.play_count_threshold_percent),
            "play_count_threshold_percent must be finite and at least 0.0 but less than 100.0; got {}",
            self.play_count_threshold_percent
        );
        ensure!(
            self.ui_scale.is_finite() && (0.75..=2.0).contains(&self.ui_scale),
            "ui_scale must be finite and between 0.75 and 2.0 (inclusive); got {}",
            self.ui_scale
        );
        ensure!(
            matches!(self.media_read_buffer_mb, 1 | 2 | 4 | 8 | 16),
            "media_read_buffer_mb must be one of 1, 2, 4, 8, or 16 MiB; got {}",
            self.media_read_buffer_mb
        );
        ensure!(
            (5..=60).contains(&self.analysis_fps),
            "analysis_fps must be between 5 and 60 (inclusive); got {}",
            self.analysis_fps
        );
        for (name, min, max) in [
            ("spectrum", self.spectrum_min_hz, self.spectrum_max_hz),
            (
                "spectrogram",
                self.spectrogram_min_hz,
                self.spectrogram_max_hz,
            ),
        ] {
            ensure!(
                min.is_finite() && max.is_finite() && min > 0.0 && min < max && max <= 384_000.0,
                "{name} Hz range must be finite with 0 < min < max <= 384000; got {min}..{max}"
            );
        }
        for (name, range) in [
            ("spectrum_db_range", self.spectrum_db_range),
            ("spectrogram_db_range", self.spectrogram_db_range),
        ] {
            ensure!(
                range.is_finite() && (1.0..=160.0).contains(&range),
                "{name} must be between 1 and 160 dB; got {range}"
            );
        }
        ensure!(
            matches!(
                self.spectrum_fft_size,
                512 | 1024 | 2048 | 4096 | 8192 | 16384 | 32768
            ),
            "spectrum_fft_size must be a power of two from 512 to 32768"
        );
        ensure!(
            (1..=48).contains(&self.spectrum_bands_per_octave),
            "spectrum_bands_per_octave must be between 1 and 48"
        );
        ensure!(
            self.spectrum_bar_width.is_finite() && (1.0..=20.0).contains(&self.spectrum_bar_width),
            "spectrum_bar_width must be between 1 and 20 pixels"
        );
        ensure!(
            self.spectrum_bars <= 512,
            "spectrum_bars must be 0 (auto) or between 1 and 512"
        );
        ensure!(
            self.spectrum_gap.is_finite() && (0.0..=8.0).contains(&self.spectrum_gap),
            "spectrum_gap must be between 0 and 8 pixels"
        );
        for (name, hold) in [
            ("spectrum_peak_hold_ms", self.spectrum_peak_hold_ms),
            ("spectrum_bar_hold_ms", self.spectrum_bar_hold_ms),
        ] {
            ensure!(hold <= 2000, "{name} must be between 0 and 2000 ms");
        }
        for (name, gravity) in [
            ("spectrum_peak_gravity", self.spectrum_peak_gravity),
            ("spectrum_bar_gravity", self.spectrum_bar_gravity),
        ] {
            ensure!(
                gravity.is_finite() && (0.0..=500.0).contains(&gravity),
                "{name} must be between 0 and 500 dB/s²"
            );
        }
        ensure!(
            self.spectrum_smoothing_ms <= 1000,
            "spectrum_smoothing_ms must be between 0 and 1000 ms"
        );
        ensure!(
            (5..=120).contains(&self.spectrogram_history_seconds),
            "spectrogram_history_seconds must be between 5 and 120 seconds"
        );
        ensure!(
            self.waveform_glow.is_finite() && (0.0..=2.0).contains(&self.waveform_glow),
            "waveform_glow must be between 0 and 2"
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
        let contents = toml::to_string_pretty(self)
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
        assert_eq!(Config::load(&path).unwrap(), Config::default());
        let config = Config {
            library_roots: vec![
                PathBuf::from("Music/Live recordings"),
                PathBuf::from("音楽"),
            ],
            output_device: Some("USB DAC".to_owned()),
            volume: 0.25,
            shuffle: true,
            repeat: RepeatMode::All,
            play_count_threshold_percent: 35.5,
            mpris_enabled: false,
            ui_scale: 1.5,
            analysis_fps: 30,
            spectrogram_min_hz: 30.0,
            spectrogram_max_hz: 18_000.0,
            spectrum_db_range: 80.0,
            media_read_buffer_mb: 8,
            nerd_symbols: true,
            ffmpeg_enabled: true,
            pipewire_auto_mix: false,
            spectrum_style: SpectrumStyle::Solid,
            visual_background: "#101820".parse().unwrap(),
            spectrum_min_hz: 60.0,
            spectrum_max_hz: 16_000.0,
            spectrum_fft_size: 16384,
            spectrum_window: SpectrumWindow::Hann,
            spectrum_bands_per_octave: 36,
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
            spectrum_log_scale: false,
            spectrum_grid: false,
            spectrum_labels: false,
            spectrogram_labels: false,
            waveform_labels: false,
            spectrogram_db_range: 100.0,
            spectrogram_log_scale: false,
            spectrogram_history_seconds: 30,
            waveform_cursor_color: "#eeaa66".parse().unwrap(),
            waveform_glow: 0.0,
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
    fn pipewire_auto_mix_defaults_and_round_trips() {
        assert!(Config::default().pipewire_auto_mix);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "").unwrap();
        assert!(Config::load(&path).unwrap().pipewire_auto_mix);
        let config = Config {
            pipewire_auto_mix: false,
            ..Config::default()
        };
        config.save(&path).unwrap();
        assert!(!Config::load(&path).unwrap().pipewire_auto_mix);
    }

    #[test]
    fn omitted_fields_use_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "").unwrap();
        assert_eq!(Config::load(&path).unwrap(), Config::default());
        fs::write(&path, "shuffle = true\nrepeat = 'one'\n").unwrap();
        assert_eq!(
            Config::load(&path).unwrap(),
            Config {
                shuffle: true,
                repeat: RepeatMode::One,
                ..Config::default()
            }
        );
        directory.close().unwrap();
    }

    #[test]
    fn malformed_unknown_and_invalid_values_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        for contents in [
            "volume = [",
            "volum = 0.5",
            "volume = 'loud'",
            "volume = -0.01",
            "volume = 1.01",
            "volume = nan",
            "volume = inf",
            "volume = -inf",
            "play_count_threshold_percent = -0.01",
            "play_count_threshold_percent = 100.0",
            "play_count_threshold_percent = nan",
            "play_count_threshold_percent = inf",
            "ui_scale = 0.74",
            "ui_scale = 2.01",
            "ui_scale = nan",
            "ui_scale = inf",
            "analysis_fps = 4",
            "analysis_fps = 61",
            "analysis_fps = -1",
            "analysis_fps = 5.5",
            "spectrum_min_hz = 0",
            "spectrum_max_hz = 384001",
            "spectrum_min_hz = nan",
            "spectrum_min_hz = 300\nspectrum_max_hz = 200",
            "spectrogram_min_hz = 0",
            "spectrogram_min_hz = nan",
            "spectrogram_max_hz = 10\nspectrogram_min_hz = 20",
            "spectrum_db_range = 0",
            "spectrum_db_range = 161",
            "spectrogram_db_range = inf",
            "analysis_db_range = 0",
            "analysis_db_range = inf",
            "spectrum_fft_size = 256",
            "spectrum_fft_size = 65536",
            "spectrum_fft_size = 1000",
            "spectrum_window = 'hamming'",
            "spectrum_bands_per_octave = 0",
            "spectrum_bands_per_octave = 49",
            "spectrum_bar_width = 0.9",
            "spectrum_bar_width = 21",
            "spectrum_bar_width = nan",
            "spectrum_bars = 513",
            "spectrum_gap = -1",
            "spectrum_gap = 9",
            "spectrum_gap = nan",
            "spectrum_peak_hold_ms = 2001",
            "spectrum_peak_gravity = -1",
            "spectrum_peak_gravity = 501",
            "spectrum_peak_gravity = inf",
            "spectrum_bar_hold_ms = 2001",
            "spectrum_bar_gravity = -1",
            "spectrum_bar_gravity = 501",
            "spectrum_bar_gravity = nan",
            "spectrum_smoothing_ms = 1001",
            "spectrogram_history_seconds = 4",
            "spectrogram_history_seconds = 121",
            "waveform_glow = -1",
            "waveform_glow = 3",
            "visual_background = '#12345'",
            "visual_background = '#gg0000'",
            "visual_background = '#00000000'",
            "waveform_cursor_color = 123456",
            "spectrum_style = 'diagonal'",
            "repeat = 'forever'",
        ] {
            fs::write(&path, contents).unwrap();
            assert!(
                Config::load(&path).is_err(),
                "accepted invalid config: {contents}"
            );
        }
    }

    #[test]
    fn nerd_font_detection_and_saved_preferences() {
        for family in [
            "Symbols Nerd Font",
            "Symbols Nerd Font Mono",
            "FiraCode Nerd Font",
            "JetBrainsMono NerdFont",
            "Hack NF",
            "Iosevka NFM",
            "CaskaydiaCove NFP",
        ] {
            assert!(is_nerd_font_family(family), "missed {family}");
        }
        for family in ["DejaVu Sans", "monospace", "Noto Sans", "Nerdy Sans"] {
            assert!(!is_nerd_font_family(family), "misidentified {family}");
        }
        assert_eq!(Config::default().nerd_symbols, nerd_fonts_available());
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        for enabled in [false, true] {
            fs::write(&path, format!("nerd_symbols = {enabled}\n")).unwrap();
            let config = Config::load(&path).unwrap();
            assert_eq!(config.nerd_symbols, enabled);
            config.save(&path).unwrap();
            assert_eq!(Config::load(&path).unwrap().nerd_symbols, enabled);
        }
    }

    #[test]
    fn obsolete_release_is_removed_without_overriding_gravity() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        for gravity in [None, Some(75.0)] {
            let mut contents = "spectrum_peak_release_db_per_second = 42\n".to_owned();
            if let Some(gravity) = gravity {
                contents.push_str(&format!("spectrum_peak_gravity = {gravity}\n"));
            }
            fs::write(&path, contents).unwrap();
            let config = Config::load(&path).unwrap();
            assert_eq!(config.spectrum_peak_gravity, gravity.unwrap_or(50.0));
            assert_eq!(config.spectrum_peak_hold_ms, 400);
            config.save(&path).unwrap();
            let saved = fs::read_to_string(&path).unwrap();
            assert!(!saved.contains("spectrum_peak_release_db_per_second"));
            assert_eq!(Config::load(&path).unwrap(), config);
        }
    }

    #[test]
    fn spectrum_windows_and_fft_sizes_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        for spectrum_window in [
            SpectrumWindow::Hann,
            SpectrumWindow::BlackmanHarris,
            SpectrumWindow::None,
        ] {
            for spectrum_fft_size in [512, 1024, 2048, 4096, 8192, 16384, 32768] {
                let config = Config {
                    spectrum_window,
                    spectrum_fft_size,
                    ..Config::default()
                };
                config.save(&path).unwrap();
                assert_eq!(Config::load(&path).unwrap(), config);
            }
        }
    }

    #[test]
    fn legacy_shared_range_migrates_without_overriding_explicit_ranges() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            "analysis_db_range = 85\nspectrogram_db_range = 110\n",
        )
        .unwrap();
        let mut config = Config::load(&path).unwrap();
        assert_eq!(
            (config.spectrum_db_range, config.spectrogram_db_range),
            (85.0, 110.0)
        );
        config.spectrum_db_range = 60.0;
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
        let saved: toml::Table = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(!saved.contains_key("analysis_db_range"));
        fs::write(&path, "analysis_db_range = 85\nspectrum_db_range = 60\n").unwrap();
        config = Config::load(&path).unwrap();
        assert_eq!(
            (config.spectrum_db_range, config.spectrogram_db_range),
            (60.0, 85.0)
        );
        fs::write(&path, "analysis_db_range = 55\n").unwrap();
        config = Config::load(&path).unwrap();
        assert_eq!(
            (config.spectrum_db_range, config.spectrogram_db_range),
            (55.0, 55.0)
        );
    }

    #[test]
    fn removed_palette_fields_are_dropped_on_load_and_save() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            "shuffle = true\nspectrum_style = 'led'\nvisual_palette = 'legacy'\nspectrogram_palette = 'legacy'\nwaveform_palette = 'legacy'\nwaveform_played_opacity = 0.5\n",
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert!(config.shuffle);
        assert_eq!(config.spectrum_style, SpectrumStyle::Led);
        config.save(&path).unwrap();
        let saved: toml::Table = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            saved.get("shuffle").and_then(toml::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            saved.get("spectrum_style").and_then(toml::Value::as_str),
            Some("led")
        );
        for key in [
            "visual_palette",
            "spectrogram_palette",
            "waveform_palette",
            "waveform_played_opacity",
        ] {
            assert!(!saved.contains_key(key), "obsolete key was saved: {key}");
        }
        assert_eq!(Config::load(&path).unwrap(), config);
    }

    #[test]
    fn visual_parameter_boundaries_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        for high in [false, true] {
            let config = Config {
                spectrum_min_hz: 1.0,
                spectrum_max_hz: 384_000.0,
                spectrum_db_range: if high { 160.0 } else { 1.0 },
                spectrogram_db_range: if high { 1.0 } else { 160.0 },
                spectrum_gap: if high { 8.0 } else { 0.0 },
                spectrum_peak_hold_ms: if high { 2000 } else { 0 },
                spectrum_fft_size: if high { 32768 } else { 512 },
                spectrum_bands_per_octave: if high { 48 } else { 1 },
                spectrum_bar_width: if high { 20.0 } else { 1.0 },
                spectrum_bars: if high { 512 } else { 1 },
                spectrum_peak_gravity: if high { 500.0 } else { 0.0 },
                spectrum_bar_hold_ms: if high { 2000 } else { 0 },
                spectrum_bar_gravity: if high { 500.0 } else { 0.0 },
                spectrum_smoothing_ms: if high { 1000 } else { 0 },
                spectrogram_history_seconds: if high { 120 } else { 5 },
                waveform_glow: if high { 2.0 } else { 0.0 },
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
                spectrogram_min_hz: 31.0,
                spectrogram_max_hz: 19_000.0,
                spectrum_db_range: 55.0,
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
        let error = invalid.save(&path).unwrap_err();
        assert!(format!("{error:#}").contains("volume"));
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
        let error = Config::default().save(&path).unwrap_err();
        // Keep the error alive while checking cleanup of PersistError's file.
        assert_eq!(entries(directory.path()), vec![path]);
        assert_eq!(fs::read_to_string(sentinel).unwrap(), "unchanged");
        assert!(format!("{error:#}").contains("atomically replace"));
        directory.close().unwrap();
    }

    #[test]
    fn unreadable_configuration_is_not_treated_as_missing() {
        let directory = tempfile::tempdir().unwrap();
        let error = Config::load(directory.path()).unwrap_err();
        assert!(format!("{error:#}").contains("cannot read configuration"));
        directory.close().unwrap();
    }
}
