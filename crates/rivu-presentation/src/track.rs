use std::{borrow::Cow, path::Path};

/// Display missing titles without changing valid metadata or search semantics.
pub fn track_title<'a>(title: &'a str, path: &'a Path, cue_number: Option<u32>) -> Cow<'a, str> {
    if title.trim().is_empty() {
        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
        match cue_number {
            Some(number) => Cow::Owned(format!("{stem} · #{number:02}")),
            None => stem,
        }
    } else {
        Cow::Borrowed(title)
    }
}
