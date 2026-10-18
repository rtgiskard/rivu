//! Configuration persistence: loading, saving, and destination resolution.
//!
//! TOML field mapping stays in `file`; runtime configuration and patches stay
//! in the parent module. This module owns filesystem effects and their failure
//! guarantees, including following symbolic links before atomic replacement.

use super::{Config, file};
use anyhow::{Context, Result, ensure};
use std::{
    borrow::Cow,
    fs,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

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

    /// Validate, serialize, and atomically replace the destination using a
    /// private same-directory temporary file. Symbolic links are followed to
    /// their destination, including missing targets; the links stay untouched.
    /// Failures before replacement leave the previous file untouched and clean
    /// up the temporary file.
    ///
    /// This deliberately does not fsync the file or directory: configuration
    /// writes must not block on storage flushes. Existing directory
    /// permissions are never changed.
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()
            .with_context(|| format!("cannot save invalid configuration {}", path.display()))?;
        let contents = toml::to_string_pretty(&file::FileConfigView::from(self))
            .with_context(|| format!("cannot serialize configuration {}", path.display()))?;
        let destination = configuration_destination(path)
            .with_context(|| format!("cannot resolve configuration {}", path.display()))?;
        let path = destination.as_ref();
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

fn configuration_destination(path: &Path) -> Result<Cow<'_, Path>> {
    let mut destination = Cow::Borrowed(path);
    let mut followed = 0;
    loop {
        match fs::symlink_metadata(destination.as_ref()) {
            Ok(metadata) if metadata.file_type().is_symlink() => {}
            Ok(_) => return Ok(destination),
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(destination),
            Err(error) => return Err(error.into()),
        }
        // Match Linux's bounded link traversal, including cycles and dangling
        // chains, without requiring the final file to already exist.
        ensure!(
            followed < 40,
            "configuration symbolic link traversal exceeds 40 links"
        );
        let target = fs::read_link(destination.as_ref())?;
        destination = Cow::Owned(if target.is_absolute() {
            target
        } else {
            destination
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(target)
        });
        followed += 1;
    }
}

/// The platform configuration directory, not Rivu's library/data directory.
/// If no home/configuration directory can be determined, use a local `.rivu`.
pub fn default_path() -> PathBuf {
    directories::ProjectDirs::from("", "", "rivu")
        .map(|dirs| dirs.config_dir().join("config.toml"))
        .unwrap_or_else(|| PathBuf::from(".rivu/config.toml"))
}
