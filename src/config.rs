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
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub library_roots: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_device: Option<String>,
    pub volume: f32,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub mpris_enabled: bool,
    pub ui_scale: f32,
    pub analysis_fps: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            library_roots: Vec::new(),
            output_device: None,
            volume: 0.7,
            shuffle: false,
            repeat: RepeatMode::Off,
            mpris_enabled: true,
            ui_scale: 1.0,
            analysis_fps: 20,
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
        let config: Self = toml::from_str(&contents)
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
            self.ui_scale.is_finite() && (0.75..=2.0).contains(&self.ui_scale),
            "ui_scale must be finite and between 0.75 and 2.0 (inclusive); got {}",
            self.ui_scale
        );
        ensure!(
            (5..=60).contains(&self.analysis_fps),
            "analysis_fps must be between 5 and 60 (inclusive); got {}",
            self.analysis_fps
        );
        Ok(())
    }

    /// Validate, serialize, and sync a private same-directory temporary file,
    /// then atomically replace the destination. Failures before replacement
    /// leave the previous file untouched and clean up the temporary file.
    ///
    /// On Unix, newly created directories are private and the containing
    /// directory is synced after replacement for crash durability. An error
    /// from that final sync explicitly reports that replacement already happened.
    /// Existing directory permissions are never changed.
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
        #[cfg(unix)]
        let directory = fs::File::open(parent)
            .with_context(|| format!("cannot open configuration directory {}", parent.display()))?;
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
        temporary.as_file().sync_all().with_context(|| {
            format!("cannot sync temporary configuration for {}", path.display())
        })?;
        // Discard PersistError's owned file here so cleanup is immediate, rather
        // than deferred until the caller drops the returned error.
        temporary
            .persist(path)
            .map_err(|error| error.error)
            .with_context(|| {
                format!("cannot atomically replace configuration {}", path.display())
            })?;
        #[cfg(unix)]
        directory.sync_all().with_context(|| {
            format!(
                "configuration {} was replaced, but syncing directory {} failed",
                path.display(),
                parent.display()
            )
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
            mpris_enabled: false,
            ui_scale: 1.5,
            analysis_fps: 30,
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
    fn malformed_unknown_and_invalid_values_report_path_and_field() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let cases = [
            ("volume = [", "TOML"),
            ("volum = 0.5", "volum"),
            ("volume = 'loud'", "TOML"),
            ("volume = -0.01", "volume"),
            ("volume = 1.01", "volume"),
            ("volume = nan", "volume"),
            ("volume = inf", "volume"),
            ("volume = -inf", "volume"),
            ("ui_scale = 0.74", "ui_scale"),
            ("ui_scale = 2.01", "ui_scale"),
            ("ui_scale = nan", "ui_scale"),
            ("ui_scale = inf", "ui_scale"),
            ("analysis_fps = 4", "analysis_fps"),
            ("analysis_fps = 61", "analysis_fps"),
            ("analysis_fps = -1", "TOML"),
            ("analysis_fps = 5.5", "TOML"),
            ("repeat = 'forever'", "forever"),
        ];
        for (contents, expected) in cases {
            fs::write(&path, contents).unwrap();
            let error = format!("{:#}", Config::load(&path).unwrap_err());
            assert!(error.contains(expected), "{contents}: {error}");
            assert!(error.contains(&path.display().to_string()), "{error}");
        }
        directory.close().unwrap();
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
