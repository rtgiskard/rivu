//! Configuration persistence: loading, saving, and destination resolution.
//!
//! The format crate owns TOML mapping and validation. This module owns filesystem
//! effects and atomic replacement, including symbolic-link destinations.

use super::Config;
use anyhow::{Context, Result, ensure};
use std::{
    borrow::Cow,
    fs,
    io::{ErrorKind, Write},
    path::Path,
};

pub trait ConfigPersistence: Sized {
    fn load(path: &Path) -> Result<Self>;
    fn save(&self, path: &Path) -> Result<()>;
    fn save_with_baseline(&self, path: &Path, baseline: Option<&Self>) -> Result<()>;
}

impl ConfigPersistence for Config {
    /// Read settings without creating files or directories. Only a missing file
    /// selects defaults; unreadable, malformed, and invalid files are errors.
    fn load(path: &Path) -> Result<Self> {
        let contents = match rivu_format::config::read_toml(path) {
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

    /// Validate, patch, and atomically replace the destination using a
    /// private same-directory temporary file. Symbolic links are followed to
    /// their destination, including missing targets; the links stay untouched.
    /// Failures before replacement leave the previous file untouched and clean
    /// up the temporary file.
    ///
    /// This deliberately does not fsync the file or directory: configuration
    /// writes must not block on storage flushes. Existing directory
    /// permissions are never changed.
    fn save(&self, path: &Path) -> Result<()> {
        self.save_with_baseline(path, None)
    }

    fn save_with_baseline(&self, path: &Path, baseline: Option<&Self>) -> Result<()> {
        let destination = configuration_destination(path)
            .with_context(|| format!("cannot resolve configuration {}", path.display()))?;
        let path = destination.as_ref();
        ensure!(
            path.file_name().is_some(),
            "configuration path must name a file: {}",
            path.display()
        );
        let original = match rivu_format::config::read_toml(path) {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot read configuration {}", path.display()));
            }
        };
        let contents = rivu_format::config::update_toml(
            original.as_deref().unwrap_or_default(),
            baseline.filter(|_| original.is_some()),
            self,
        )
        .with_context(|| format!("cannot update configuration {}", path.display()))?;
        if original.as_deref() == Some(contents.as_str()) {
            return Ok(());
        }
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

/// Follow only the final path component's symbolic links, leaving intermediate
/// directory resolution to the filesystem. The final target and parent may be
/// missing; writers create parents separately and preserve the links themselves.
fn configuration_destination(path: &Path) -> Result<Cow<'_, Path>> {
    let mut destination = Cow::Borrowed(path);
    let mut followed = 0;
    loop {
        match fs::symlink_metadata(destination.as_ref()) {
            Ok(metadata) if metadata.file_type().is_symlink() => {}
            Ok(_) => return Ok(destination),
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(destination),
            Err(error) => return Err(error).context("Resolving configuration target"),
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn filesystem_reads_share_the_format_byte_budget_and_preserve_input() {
        use rivu_format::config::MAX_CONFIG_BYTES;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        for size in [0, MAX_CONFIG_BYTES, MAX_CONFIG_BYTES + 1] {
            let text = " ".repeat(size);
            fs::write(&path, &text).unwrap();
            let result = Config::load(&path);
            if size <= MAX_CONFIG_BYTES {
                assert_eq!(result.unwrap(), Config::default());
            } else {
                assert_eq!(
                    result
                        .unwrap_err()
                        .downcast_ref::<std::io::Error>()
                        .unwrap()
                        .kind(),
                    ErrorKind::InvalidData,
                );
                let config = Config {
                    volume: 0.25,
                    ..Config::default()
                };
                assert!(config.save(&path).is_err());
                assert!(
                    config
                        .save_with_baseline(&path, Some(&Config::default()))
                        .is_err()
                );
            }
            assert_eq!(fs::read_to_string(&path).unwrap(), text);
        }
    }

    #[test]
    fn oversized_save_preserves_previous_file_and_missing_destinations() {
        use rivu_format::config::MAX_CONFIG_BYTES;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let previous = "# preserve original bytes\n[playback]\nvolume_percent = 25\n";
        fs::write(&path, previous).unwrap();
        let config = Config {
            output_device: Some("\0".repeat(MAX_CONFIG_BYTES / 6 + 1)),
            ..Config::default()
        };
        config.validate().unwrap();
        let error = config.save(&path).unwrap_err();
        assert!(format!("{error:#}").contains("exceeds"));
        assert_eq!(fs::read_to_string(&path).unwrap(), previous);
        assert_eq!(Config::load(&path).unwrap().volume, 0.25);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);

        let missing = directory.path().join("missing/config.toml");
        assert!(config.save(&missing).is_err());
        assert!(!missing.parent().unwrap().exists());
    }

    #[test]
    fn retained_comments_count_toward_the_output_budget() {
        use rivu_format::config::MAX_CONFIG_BYTES;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let original = format!("#{}\n", "x".repeat(MAX_CONFIG_BYTES - 2));
        fs::write(&path, &original).unwrap();
        assert_eq!(Config::load(&path).unwrap(), Config::default());
        let config = Config {
            volume: 0.25,
            ..Config::default()
        };
        assert!(config.save(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn non_utf8_input_is_not_overwritten_with_or_without_a_saved_baseline() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let original = b"# invalid UTF-8: \xff\n";
        fs::write(&path, original).unwrap();
        let config = Config {
            volume: 0.25,
            ..Config::default()
        };
        assert!(config.save(&path).is_err());
        assert!(
            config
                .save_with_baseline(&path, Some(&Config::default()))
                .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn destination_preserves_directory_aliases_and_missing_relative_targets() {
        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("real");
        fs::create_dir(&real).unwrap();
        let alias = directory.path().join("alias");
        symlink("real", &alias).unwrap();
        symlink("missing/target.toml", real.join("config.toml")).unwrap();
        assert_eq!(
            configuration_destination(&alias.join("config.toml")).unwrap(),
            alias.join("missing/target.toml")
        );
        assert!(!real.join("missing").exists());
    }
}
