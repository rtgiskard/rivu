use std::path::Path;

pub(crate) use rivu_format::config::logical_path;

pub(crate) fn normalize_roots(
    roots: &[std::path::PathBuf],
) -> anyhow::Result<Vec<std::path::PathBuf>> {
    roots.iter().map(|root| logical_path(root)).collect()
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
