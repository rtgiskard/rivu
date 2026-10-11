use anyhow::{Context, Result};
use std::path::{Component, Path, PathBuf};

/// Normalize an absolute logical path without resolving symlinks or requiring it to exist.
pub(crate) fn logical_path(path: &Path) -> Result<PathBuf> {
    let mut normalized = if path.is_absolute() {
        PathBuf::with_capacity(path.as_os_str().len())
    } else {
        let mut directory = std::env::current_dir().context("resolve current directory")?;
        directory.reserve(path.as_os_str().len());
        directory
    };
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    Ok(normalized)
}

pub(super) fn is_audio_path(path: &Path) -> bool {
    let Some(extension) = path.extension().and_then(|value| value.to_str()) else {
        return false;
    };
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "aac"
            | "ac3"
            | "aiff"
            | "aif"
            | "alac"
            | "ape"
            | "caf"
            | "dff"
            | "dsf"
            | "dts"
            | "dtshd"
            | "eac3"
            | "flac"
            | "m4a"
            | "mka"
            | "mkv"
            | "mlp"
            | "mp3"
            | "mp4"
            | "oga"
            | "ogg"
            | "opus"
            | "shn"
            | "tak"
            | "thd"
            | "truehd"
            | "tta"
            | "wav"
            | "wave"
            | "webm"
            | "wma"
    )
}

pub(super) fn is_cue_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("cue"))
}
