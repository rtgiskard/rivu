//! Configuration persistence: loading, saving, and destination resolution.
//!
//! The format crate owns TOML mapping and validation. This module owns filesystem
//! effects and atomic replacement, including symbolic-link destinations.

use super::Config;
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

pub trait ConfigPersistence: Sized {
    fn load(path: &Path) -> Result<Self>;
    fn save(&self, path: &Path) -> Result<()>;
}

impl ConfigPersistence for Config {
    /// Read settings without creating files or directories. Only a missing file
    /// selects defaults; unreadable, malformed, and invalid files are errors.
    fn load(path: &Path) -> Result<Self> {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot read configuration {}", path.display()));
            }
        };
        rivu_format::config::from_toml(&contents)
            .with_context(|| format!("invalid configuration {}", path.display()))
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
    fn save(&self, path: &Path) -> Result<()> {
        let contents = rivu_format::config::to_toml(self)
            .with_context(|| format!("cannot serialize configuration {}", path.display()))?;
        let destination = rivu_format::ownership::configuration_destination(path)
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

/// The platform configuration directory, not Rivu's library/data directory.
/// If no home/configuration directory can be determined, use a local `.rivu`.
pub fn default_path() -> PathBuf {
    directories::ProjectDirs::from("", "", "rivu")
        .map(|dirs| dirs.config_dir().join("config.toml"))
        .unwrap_or_else(|| PathBuf::from(".rivu/config.toml"))
}
