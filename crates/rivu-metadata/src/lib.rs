//! Shared metadata readers and explicit, atomic file edits.

#[cfg(feature = "artwork")]
pub mod artwork;

mod writer;
mod xattrs;

#[cfg(any(test, feature = "test-support"))]
mod fixtures;

#[cfg(test)]
mod tests;

pub use writer::{
    EditFailure, EditFailureOutcome, EditOutcome, EditReport, JsonPath, TagPatch, WrittenSource,
    edit_file, edit_file_with_source,
};

use serde::{Deserialize, Serialize};

pub use rivu_format::tags::{TagEncoding, TagValues};

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

fn first(tag: &Tag, key: ItemKey) -> Option<&str> {
    tag.get_strings(key)
        .flat_map(|s| s.split('\0'))
        .map(str::trim)
        .find(|s| !s.is_empty())
}

fn positive_number(tag: &Tag, key: ItemKey) -> Option<u32> {
    first(tag, key)
        .and_then(|s| s.split('/').next()?.trim().parse().ok())
        .filter(|n| *n > 0)
}

fn fill_missing(result: &mut Tags, tag: &Tag) {
    if result.title.is_none() {
        result.title = first(tag, ItemKey::TrackTitle).map(str::to_owned);
    }
    if result.artist.is_none() {
        result.artist = first(tag, ItemKey::TrackArtist).map(str::to_owned);
    }
    if result.album.is_none() {
        result.album = first(tag, ItemKey::AlbumTitle).map(str::to_owned);
    }
    if result.release_date.is_none() {
        result.release_date = first(tag, ItemKey::ReleaseDate)
            .or_else(|| first(tag, ItemKey::RecordingDate))
            .or_else(|| first(tag, ItemKey::Year))
            .map(str::to_owned);
    }
    if result.track_number.is_none() {
        result.track_number = positive_number(tag, ItemKey::TrackNumber);
    }
    if result.disc_number.is_none() {
        result.disc_number = positive_number(tag, ItemKey::DiscNumber);
    }
}

/// Content-detected descriptive tags; `None` means Lofty does not support this
/// container, not that tags are absent. Audio properties and pictures are skipped.
pub fn read_tags(file: &mut File) -> Result<Option<Tags>> {
    read_tags_with_encoding(file, TagEncoding::Declared)
}

/// Override only ID3 text explicitly marked Latin-1. Declared Unicode and
/// non-ID3 tags are never reinterpreted; invalid byte sequences are errors.
pub fn read_tags_with_encoding(file: &mut File, encoding: TagEncoding) -> Result<Option<Tags>> {
    configure_lofty(false);
    ensure!(
        file.metadata()?.is_file(),
        "Metadata input is not an ordinary file"
    );
    let Some(kind) = detect(file)? else {
        return Ok(None);
    };
    let parsed = read_with_encoding(file, kind, encoding)?;
    let mut result = Tags::default();
    // Primary format wins; other native tags fill only missing fields.
    for tag in parsed.primary_tag().into_iter().chain(
        parsed
            .tags()
            .iter()
            .filter(|t| t.tag_type() != parsed.primary_tag_type()),
    ) {
        fill_missing(&mut result, tag);
    }
    Ok(Some(result))
}

fn read_with_encoding(
    file: &mut File,
    kind: FileType,
    encoding: TagEncoding,
) -> Result<lofty::file::TaggedFile> {
    use lofty::file::AudioFile;
    file.rewind()?;
    let reader = &mut BufReader::new(file);
    // Join legacy TYER/TDAT/TIME dates before summarizing on both paths.
    let options = options(false).implicit_conversions(true);
    if encoding == TagEncoding::Declared {
        return Ok(Probe::with_file_type(reader, kind)
            .options(options)
            .read()?);
    }
    macro_rules! read {
        ($ty:ty) => {{
            let mut parsed = <$ty>::read_from(reader, options)?;
            if let Some(id3) = parsed.id3v2_mut() {
                recover_id3_text(id3, encoding)?;
            }
            parsed.into()
        }};
    }
    Ok(match kind {
        FileType::Mpeg => read!(lofty::mpeg::MpegFile),
        FileType::Aac => read!(lofty::aac::AacFile),
        FileType::Wav => read!(lofty::iff::wav::WavFile),
        FileType::Aiff => read!(lofty::iff::aiff::AiffFile),
        FileType::Flac => read!(lofty::flac::FlacFile),
        FileType::Ape => read!(lofty::ape::ApeFile),
        FileType::Mpc => read!(lofty::musepack::MpcFile),
        _ => Probe::with_file_type(reader, kind)
            .options(options)
            .read()?,
    })
}

fn recover_id3_text(tag: &mut lofty::id3::v2::Id3v2Tag, encoding: TagEncoding) -> Result<()> {
    use lofty::{TextEncoding, id3::v2::Frame};
    let codec = match encoding {
        TagEncoding::Declared => return Ok(()),
        TagEncoding::Utf8 => encoding_rs::UTF_8,
        TagEncoding::Gb18030 => encoding_rs::GB18030,
        TagEncoding::Big5 => encoding_rs::BIG5,
        TagEncoding::ShiftJis => encoding_rs::SHIFT_JIS,
        TagEncoding::EucKr => encoding_rs::EUC_KR,
        TagEncoding::Windows1252 => encoding_rs::WINDOWS_1252,
        TagEncoding::Windows1251 => encoding_rs::WINDOWS_1251,
    };
    let recover = |value: &mut std::borrow::Cow<'_, str>| -> Result<()> {
        if value.is_ascii() {
            return Ok(());
        }
        // Lofty's Latin-1 decoder maps each byte directly to its scalar.
        let bytes = value
            .chars()
            .map(|c| u8::try_from(u32::from(c)).map_err(anyhow::Error::from))
            .collect::<Result<Vec<_>>>()?;
        let decoded = codec
            .decode_without_bom_handling_and_without_replacement(&bytes)
            .ok_or_else(|| anyhow::anyhow!("Invalid {} bytes in Latin-1 ID3 text", codec.name()))?;
        *value = decoded.into_owned().into();
        Ok(())
    };
    let original = std::mem::take(tag);
    for mut frame in original {
        match &mut frame {
            Frame::Text(text) if text.encoding == TextEncoding::Latin1 => {
                recover(&mut text.value)?;
                text.encoding = TextEncoding::UTF8;
            }
            Frame::UserText(text) if text.encoding == TextEncoding::Latin1 => {
                recover(&mut text.description)?;
                recover(&mut text.content)?;
                text.encoding = TextEncoding::UTF8;
            }
            _ => {}
        }
        tag.insert(frame);
    }
    Ok(())
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
