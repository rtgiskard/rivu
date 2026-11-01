//! Shared metadata readers and explicit, backed-up file edits.

#[cfg(feature = "artwork")]
pub mod artwork;

mod writer;
mod xattrs;

#[cfg(test)]
mod tests;

pub use writer::{EditReport, TagPatch, edit_file};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub release_date: Option<String>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
}

use anyhow::{Result, ensure};
use lofty::{
    config::{GlobalOptions, ParseOptions, ParsingMode, apply_global_options},
    file::{FileType, TaggedFileExt},
    probe::Probe,
    tag::{ItemKey, Tag},
};
use std::{
    fs::File,
    io::{BufReader, Seek},
};

pub(crate) fn configure_lofty(preserve_format_items: bool) {
    apply_global_options(
        GlobalOptions::new()
            .allocation_limit(128 * 1024 * 1024)
            .preserve_format_specific_items(preserve_format_items),
    );
}

pub(crate) fn options(pictures: bool) -> ParseOptions {
    ParseOptions::new()
        .read_properties(false)
        .read_cover_art(pictures)
        .parsing_mode(ParsingMode::Strict)
        .implicit_conversions(false)
}

pub(crate) fn detect(file: &mut File) -> Result<Option<FileType>> {
    file.rewind()?;
    let probe = Probe::new(file).guess_file_type()?;
    Ok(probe.file_type())
}

fn first(tag: &Tag, key: ItemKey) -> Option<String> {
    tag.get_strings(key)
        .flat_map(|s| s.split('\0'))
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(str::to_owned)
}

fn summarize(tag: &Tag) -> Tags {
    let numeric = |key| {
        first(tag, key)
            .and_then(|s| s.split('/').next()?.trim().parse().ok())
            .filter(|n| *n > 0)
    };
    Tags {
        title: first(tag, ItemKey::TrackTitle),
        artist: first(tag, ItemKey::TrackArtist),
        album: first(tag, ItemKey::AlbumTitle),
        release_date: first(tag, ItemKey::ReleaseDate)
            .or_else(|| first(tag, ItemKey::RecordingDate))
            .or_else(|| first(tag, ItemKey::Year)),
        track_number: numeric(ItemKey::TrackNumber),
        disc_number: numeric(ItemKey::DiscNumber),
    }
}

/// Content-detected descriptive tags; `None` means Lofty does not support this
/// container, not that tags are absent. Audio properties and pictures are skipped.
pub fn read_tags(file: &mut File) -> Result<Option<Tags>> {
    configure_lofty(false);
    ensure!(
        file.metadata()?.is_file(),
        "Metadata input is not an ordinary file"
    );
    let Some(kind) = detect(file)? else {
        return Ok(None);
    };
    file.rewind()?;
    // Read-only semantic normalization joins legacy TYER/TDAT/TIME dates;
    // concrete preservation reads and writes keep conversions disabled.
    let parsed = Probe::with_file_type(BufReader::new(file), kind)
        .options(options(false).implicit_conversions(true))
        .read()?;
    let mut result = Tags::default();
    // Primary format wins; other native tags fill only missing fields.
    for tag in parsed.primary_tag().into_iter().chain(
        parsed
            .tags()
            .iter()
            .filter(|t| t.tag_type() != parsed.primary_tag_type()),
    ) {
        let tags = summarize(tag);
        macro_rules! fill { ($($field:ident),*) => { $(if result.$field.is_none() { result.$field = tags.$field; })* }; }
        fill!(
            title,
            artist,
            album,
            release_date,
            track_number,
            disc_number
        );
    }
    Ok(Some(result))
}

/// Offer front covers before other embedded pictures to `select`, returning the
/// first accepted result. Empty pictures and pictures exceeding `limit` after
/// parsing are skipped. Unsupported containers return no artwork.
pub fn read_artwork<T>(
    mut file: File,
    limit: usize,
    mut select: impl FnMut(&[u8]) -> Option<T>,
) -> Result<Option<T>> {
    configure_lofty(false);
    ensure!(
        file.metadata()?.is_file(),
        "Artwork input is not an ordinary file"
    );
    let Some(kind) = detect(&mut file)? else {
        return Ok(None);
    };
    file.rewind()?;
    let parsed = Probe::with_file_type(BufReader::new(file), kind)
        .options(options(true).implicit_conversions(true))
        .read()?;
    for front in [true, false] {
        for picture in parsed.tags().iter().flat_map(|tag| tag.pictures()) {
            if (picture.pic_type() == lofty::picture::PictureType::CoverFront) != front
                || picture.data().is_empty()
                || picture.data().len() > limit
            {
                continue;
            }
            if let Some(selected) = select(picture.data()) {
                return Ok(Some(selected));
            }
        }
    }
    Ok(None)
}
