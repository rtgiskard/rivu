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
    let mut lowercase = [0; 6];
    let Some(lowercase) = lowercase.get_mut(..extension.len()) else {
        return false;
    };
    for (output, input) in lowercase.iter_mut().zip(extension.bytes()) {
        *output = input.to_ascii_lowercase();
    }
    matches!(
        &*lowercase,
        b"aac"
            | b"ac3"
            | b"aiff"
            | b"aif"
            | b"alac"
            | b"ape"
            | b"caf"
            | b"dff"
            | b"dsf"
            | b"dts"
            | b"dtshd"
            | b"eac3"
            | b"flac"
            | b"m4a"
            | b"mka"
            | b"mkv"
            | b"mlp"
            | b"mp3"
            | b"mp4"
            | b"oga"
            | b"ogg"
            | b"opus"
            | b"shn"
            | b"tak"
            | b"thd"
            | b"truehd"
            | b"tta"
            | b"wav"
            | b"wave"
            | b"webm"
            | b"wma"
    )
}

pub(super) fn is_cue_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("cue"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn audio_extensions_are_ascii_case_insensitive_and_exact() {
        for path in [
            "track.WaV",
            "track.TRuEhD",
            "track.DtShD",
            "track.aif",
            "track.wave",
        ] {
            assert!(is_audio_path(Path::new(path)), "{path}");
        }
        for path in [
            "track",
            "track.",
            ".wav",
            "track.wav.txt",
            "track.wavextra",
            "track.wáv",
            "track.CUE",
        ] {
            assert!(!is_audio_path(Path::new(path)), "{path}");
        }
        assert!(!is_audio_path(Path::new(OsStr::from_bytes(
            b"track.\xffwav"
        ))));
        assert!(is_cue_path(Path::new("sheet.CuE")));
        assert!(!is_cue_path(Path::new("sheet.cue.wav")));
    }
}
