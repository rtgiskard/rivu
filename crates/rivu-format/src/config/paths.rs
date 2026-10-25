//! Shared application/offline-tool configuration and data path selection.
use std::path::PathBuf;

pub struct Paths {
    pub data_dir: PathBuf,
    pub config: PathBuf,
}

impl Paths {
    /// A config override does not choose the data directory. A data override
    /// supplies the config location only when no config override was given.
    pub fn resolve(data_dir: Option<PathBuf>, config: Option<PathBuf>) -> Self {
        let project = directories::ProjectDirs::from("", "", "rivu");
        let config = config.unwrap_or_else(|| {
            data_dir.as_ref().map_or_else(
                || {
                    project.as_ref().map_or_else(
                        || PathBuf::from(".rivu/config.toml"),
                        |dirs| dirs.config_dir().join("config.toml"),
                    )
                },
                |directory| directory.join("config.toml"),
            )
        });
        let data_dir = data_dir.unwrap_or_else(|| {
            project.as_ref().map_or_else(
                || PathBuf::from(".rivu"),
                |dirs| dirs.data_dir().to_path_buf(),
            )
        });
        Self { data_dir, config }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn selection_preserves_independent_config_and_data_overrides() {
        let defaults = Paths::resolve(None, None);
        let separate = Paths::resolve(None, Some("separate.toml".into()));
        assert_eq!(separate.data_dir, defaults.data_dir);
        assert_eq!(separate.config, Path::new("separate.toml"));
        let data_only = Paths::resolve(Some("chosen-data".into()), None);
        assert_eq!(data_only.data_dir, Path::new("chosen-data"));
        assert_eq!(data_only.config, Path::new("chosen-data/config.toml"));
        let both = Paths::resolve(Some("chosen-data".into()), Some("separate.toml".into()));
        assert_eq!(both.data_dir, data_only.data_dir);
        assert_eq!(both.config, separate.config);
    }
}
