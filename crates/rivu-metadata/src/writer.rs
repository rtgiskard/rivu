use crate::{configure_lofty, detect, options};
use anyhow::{Context, Result, ensure};
use lofty::{
    TextEncoding,
    config::WriteOptions,
    file::{FileType, TaggedFileExt},
    id3::v2::{Frame, Id3v2Tag},
    probe::Probe,
    tag::{Accessor, ItemKey, Tag, TagExt, TagType},
};
use serde::Serialize;
use std::{
    fs::{self, File, Metadata},
    io::{BufReader, Read, Seek},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
};

/// Lossless JSON representation for offline source-tag paths.
/// UTF-8 paths remain strings; other Unix paths expose their original bytes.
pub struct JsonPath<'a>(pub &'a Path);

impl Serialize for JsonPath<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        if let Some(path) = self.0.to_str() {
            serializer.serialize_str(path)
        } else {
            use serde::ser::SerializeStruct;
            let mut value = serializer.serialize_struct("UnixPath", 1)?;
            value.serialize_field("unix_bytes", self.0.as_os_str().as_bytes())?;
            value.end()
        }
    }
}

fn serialize_path<S: serde::Serializer>(
    path: &Path,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    JsonPath(path).serialize(serializer)
}

/// `None` leaves a field untouched; an empty string removes a text field.
/// For numbers, `Some(None)` clears and `Some(Some(n))` sets a positive value.
#[derive(Clone, Debug, Default)]
pub struct TagPatch {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub release_date: Option<String>,
    pub track_number: Option<Option<u32>>,
    pub disc_number: Option<Option<u32>>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum EditOutcome {
    Unchanged,
    Replaced,
}

#[derive(Clone, Debug)]
pub struct WrittenSource {
    pub fingerprint: [u8; 32],
    pub file_size: u64,
    pub modified_ns: i64,
}

impl WrittenSource {
    fn new(metadata: &Metadata, fingerprint: [u8; 32]) -> Result<Self> {
        let modified_ns = rivu_utils::regular_file::modified_unix_ns(metadata.modified()?)
            .context("Written metadata timestamp is outside signed Unix nanoseconds")?;
        Ok(Self {
            fingerprint,
            file_size: metadata.len(),
            modified_ns,
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct EditReport {
    #[serde(serialize_with = "serialize_path")]
    pub path: PathBuf,
    #[serde(flatten)]
    pub outcome: EditOutcome,
    pub tags: crate::Tags,
}

impl EditReport {
    pub fn changed(&self) -> bool {
        matches!(self.outcome, EditOutcome::Replaced)
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum EditFailureOutcome {
    BeforeReplace,
}

#[derive(Debug, Serialize)]
pub struct EditFailure {
    #[serde(serialize_with = "serialize_path")]
    pub path: PathBuf,
    #[serde(flatten)]
    pub outcome: EditFailureOutcome,
    #[serde(serialize_with = "serialize_edit_error")]
    error: anyhow::Error,
}

fn serialize_edit_error<S: serde::Serializer>(
    error: &anyhow::Error,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_str(&format!("{error:#}"))
}

impl std::fmt::Display for EditFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {:#}; source was not replaced",
            self.path.display(),
            self.error
        )
    }
}

impl std::error::Error for EditFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

fn text(value: &str) -> &str {
    value.trim_matches('\0').trim()
}

fn validate_patch(patch: &TagPatch) -> Result<()> {
    if let Some(value) = patch
        .release_date
        .as_deref()
        .map(text)
        .filter(|s| !s.is_empty())
    {
        ensure!(
            matches!(value.len(), 4 | 7 | 10),
            "Release date must be YYYY, YYYY-MM, or YYYY-MM-DD"
        );
        let date: lofty::tag::items::Timestamp = value.parse().context("Invalid release date")?;
        ensure!(
            date.to_string() == value,
            "Release date must be YYYY, YYYY-MM, or YYYY-MM-DD"
        );
    }
    for number in [patch.track_number, patch.disc_number]
        .into_iter()
        .flatten()
        .flatten()
    {
        ensure!(number > 0, "Track and disc numbers must be positive");
    }
    Ok(())
}

fn patch_tag(tag: &mut Tag, patch: &TagPatch) {
    for (value, key) in [
        (&patch.title, ItemKey::TrackTitle),
        (&patch.artist, ItemKey::TrackArtist),
        (&patch.album, ItemKey::AlbumTitle),
    ] {
        if let Some(value) = value {
            let value = text(value);
            if !value.is_empty() && crate::first(tag, key) == Some(value) {
                continue;
            }
            tag.remove_key(key);
            if !value.is_empty() {
                tag.insert_text(key, value.to_owned());
            }
        }
    }
    if let Some(value) = &patch.release_date {
        for key in [ItemKey::ReleaseDate, ItemKey::RecordingDate, ItemKey::Year] {
            tag.remove_key(key);
        }
        let value = text(value);
        if !value.is_empty() {
            tag.insert_text(ItemKey::RecordingDate, value.to_owned());
        }
    }
    if let Some(value) = patch.track_number {
        match value {
            Some(value) => tag.set_track(value),
            None => tag.remove_track(),
        }
    }
    if let Some(value) = patch.disc_number {
        match value {
            Some(value) => tag.set_disk(value),
            None => tag.remove_disk(),
        }
    }
}

fn read_target(file: &mut File, kind: FileType) -> Result<(Tag, Vec<TagType>)> {
    configure_lofty(true);
    file.rewind()?;
    let mut parsed = Probe::with_file_type(BufReader::new(file), kind)
        .options(options(true).implicit_conversions(true))
        .read()?;
    let target_type = match kind {
        FileType::Wav | FileType::Aiff => TagType::Id3v2,
        _ => kind.primary_tag_type(),
    };
    ensure!(
        kind.tag_support(target_type).is_writable(),
        "Unsupported metadata write format"
    );
    let mut target = parsed
        .remove(target_type)
        .unwrap_or_else(|| Tag::new(target_type));
    let mut secondary = Vec::new();
    for tag in parsed.tags() {
        if !secondary.contains(&tag.tag_type()) {
            secondary.push(tag.tag_type());
        }
        // Numeric conversion consumes a single value, unlike text multivalues.
        // Keep valid primary numbers/totals; secondary values fill missing numbers.
        for item in tag.items() {
            if matches!(
                item.key(),
                ItemKey::TrackNumber
                    | ItemKey::DiscNumber
                    | ItemKey::TrackTotal
                    | ItemKey::DiscTotal
            ) {
                if crate::positive_number(&target, item.key()).is_some()
                    || crate::positive_number(tag, item.key()).is_none()
                {
                    continue;
                }
                target.remove_key(item.key());
            }
            if !target.items().any(|existing| existing == item) {
                target.push(item.clone());
            }
        }
        for picture in tag.pictures() {
            if !target.pictures().contains(picture) {
                target.push_picture(picture.clone());
            }
        }
    }
    Ok((target, secondary))
}

fn write_target(tag: Tag, secondary: &[TagType], file: &mut File) -> Result<()> {
    let options = WriteOptions::new()
        .use_id3v23(false)
        .lossy_text_encoding(false)
        .parse_options(options(true).implicit_conversions(true));
    // Lofty's remove_others option currently has no writer implementation.
    // Explicit removals also cover read-only leading ID3 tags in FLAC/APE/MPC.
    for kind in secondary {
        file.rewind()?;
        kind.remove_from(file, options)?;
    }
    file.rewind()?;
    if tag.tag_type() == TagType::Id3v2 {
        let original: Id3v2Tag = tag.into();
        let mut frames: Vec<_> = original.into_iter().collect();
        frames.sort_by(|left, right| left.id_str().cmp(right.id_str()));
        let mut normalized = Id3v2Tag::new();
        for mut frame in frames {
            if frame.id().is_outdated() {
                continue;
            }
            match &mut frame {
                Frame::Text(t) => t.encoding = TextEncoding::UTF8,
                Frame::UserText(t) => t.encoding = TextEncoding::UTF8,
                Frame::Comment(t) => t.encoding = TextEncoding::UTF8,
                Frame::UnsynchronizedText(t) => t.encoding = TextEncoding::UTF8,
                Frame::UserUrl(t) => t.encoding = TextEncoding::UTF8,
                Frame::KeyValue(t) => t.encoding = TextEncoding::UTF8,
                _ => {}
            }
            normalized.insert(frame);
        }
        normalized.save_to(file, options)?;
    } else {
        match tag.tag_type() {
            TagType::Ape => lofty::ape::ApeTag::from(tag).save_to(file, options)?,
            TagType::VorbisComments => {
                // Lofty's vendor extraction reorders remaining generic items.
                // Remove it without swapping so multivalue precedence is stable.
                let vendor = tag.get_string(ItemKey::EncoderSoftware).map(str::to_owned);
                let mut tag = tag;
                tag.remove_key(ItemKey::EncoderSoftware);
                let mut native = lofty::ogg::tag::VorbisComments::from(tag);
                if let Some(vendor) = vendor {
                    native.set_vendor(vendor);
                }
                native.save_to(file, options)?
            }
            TagType::Mp4Ilst => lofty::mp4::Ilst::from(tag).save_to(file, options)?,
            _ => anyhow::bail!("Unsupported native metadata target"),
        }
    }
    Ok(())
}

fn validate_requested(after: &crate::Tags, patch: &TagPatch) -> Result<()> {
    for (requested, actual) in [
        (&patch.title, &after.title),
        (&patch.artist, &after.artist),
        (&patch.album, &after.album),
        (&patch.release_date, &after.release_date),
    ] {
        if let Some(value) = requested {
            let value = text(value);
            ensure!(
                actual.as_deref() == (!value.is_empty()).then_some(value),
                "Writer did not preserve a requested text value"
            );
        }
    }
    for (requested, actual) in [
        (patch.track_number, after.track_number),
        (patch.disc_number, after.disc_number),
    ] {
        if let Some(value) = requested {
            ensure!(
                actual == value,
                "Writer did not preserve a requested number"
            );
        }
    }
    Ok(())
}

fn identity(a: &Metadata, b: &Metadata) -> bool {
    a.is_file()
        && b.is_file()
        && a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
        && a.nlink() == 1
        && b.nlink() == 1
        && a.mode() == b.mode()
        && a.uid() == b.uid()
        && a.gid() == b.gid()
}

fn validate(path: &Path, file: &File, original: &Metadata) -> Result<()> {
    ensure!(
        identity(original, &file.metadata()?) && identity(original, &fs::symlink_metadata(path)?),
        "Metadata source identity changed during edit"
    );
    Ok(())
}

fn copy_source(source: &mut File, destination: &mut File) -> Result<()> {
    use std::os::fd::AsRawFd;

    // FICLONE preserves independent file contents through copy-on-write. Both
    // descriptors stay open, and the destination is our unpublished temporary.
    if unsafe { libc::ioctl(destination.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) } == 0 {
        return Ok(());
    }
    // A failed clone must not leave data or offsets in the ordinary-copy path.
    destination.set_len(0)?;
    destination.rewind()?;
    source.rewind()?;
    std::io::copy(source, destination)?;
    Ok(())
}

fn same_bytes(a: &mut File, b: &mut File, mut hasher: Option<&mut blake3::Hasher>) -> Result<bool> {
    let mut same = a.metadata()?.len() == b.metadata()?.len();
    if !same && hasher.is_none() {
        return Ok(false);
    }
    if same {
        a.rewind()?;
    }
    b.rewind()?;
    let mut left = [0; 64 * 1024];
    let mut right = [0; 64 * 1024];
    loop {
        let len = b.read(&mut right)?;
        if len == 0 {
            return Ok(same);
        }
        if let Some(hasher) = &mut hasher {
            hasher.update(&right[..len]);
        }
        if same {
            a.read_exact(&mut left[..len])?;
            same = left[..len] == right[..len];
            if !same && hasher.is_none() {
                return Ok(false);
            }
        }
    }
}

/// Normalize to one native tag, patch six descriptive fields, and atomically
/// replace a same-directory temporary copy without transcoding audio or backups.
/// Conversion can discard unsupported native-only fields; requested values are
/// read back and validated before replacement.
pub fn edit_file(path: &Path, patch: &TagPatch) -> std::result::Result<EditReport, EditFailure> {
    edit_file_inner(path, patch, false)
        .map(|(report, _)| report)
        .map_err(|error| EditFailure {
            path: path.to_owned(),
            outcome: EditFailureOutcome::BeforeReplace,
            error,
        })
}

/// Edit source tags and capture the final content fingerprint for library updates.
pub fn edit_file_with_source(
    path: &Path,
    patch: &TagPatch,
) -> std::result::Result<(EditReport, WrittenSource), EditFailure> {
    edit_file_inner(path, patch, true)
        .map(|(report, source)| (report, source.expect("source capture was requested")))
        .map_err(|error| EditFailure {
            path: path.to_owned(),
            outcome: EditFailureOutcome::BeforeReplace,
            error,
        })
}

fn edit_file_inner(
    path: &Path,
    patch: &TagPatch,
    capture_source: bool,
) -> Result<(EditReport, Option<WrittenSource>)> {
    validate_patch(patch)?;
    let original_path = fs::symlink_metadata(path).context("Inspecting metadata edit source")?;
    ensure!(
        original_path.is_file()
            && !original_path.file_type().is_symlink()
            && original_path.nlink() == 1,
        "Metadata editing requires an ordinary non-symlink file with one hard link"
    );
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()?;
    let target = parent.join(path.file_name().context("Missing metadata file name")?);
    let mut file = rivu_utils::regular_file::open_regular_file(&target)?;
    let original = file.metadata()?;
    ensure!(
        !original.permissions().readonly(),
        "Metadata source is read-only"
    );
    ensure!(
        identity(&original_path, &original),
        "Metadata source changed while opening"
    );
    file.try_lock().context("Locking metadata edit source")?;
    let kind = detect(&mut file)?.context("Unsupported metadata write format")?;
    let (mut tag, secondary) = read_target(&mut file, kind)?;
    let mut hasher = capture_source.then(blake3::Hasher::new);
    if tag.tag_type() != TagType::Id3v2 && secondary.is_empty() {
        let mut current = crate::Tags::default();
        crate::fill_missing(&mut current, &tag);
        if validate_requested(&current, patch).is_ok() {
            if let Some(hasher) = &mut hasher {
                file.rewind()?;
                hasher.update_reader(&mut file)?;
            }
            let source = hasher
                .map(|hasher| WrittenSource::new(&original, *hasher.finalize().as_bytes()))
                .transpose()?;
            validate(&target, &file, &original)?;
            return Ok((
                EditReport {
                    path: path.to_owned(),
                    outcome: EditOutcome::Unchanged,
                    tags: current,
                },
                source,
            ));
        }
    }
    patch_tag(&mut tag, patch);
    validate(&target, &file, &original)?;
    let mut output = tempfile::Builder::new()
        .prefix(".rivu-metadata-")
        .tempfile_in(&parent)?;
    let output_metadata = output.as_file().metadata()?;
    ensure!(
        output_metadata.uid() == original.uid() && output_metadata.gid() == original.gid(),
        "Cannot preserve source ownership with atomic replacement"
    );
    copy_source(&mut file, output.as_file_mut())?;
    write_target(tag, &secondary, output.as_file_mut())
        .context("Writing native metadata to temporary copy")?;
    let after =
        crate::read_tags(output.as_file_mut())?.context("Written file lost metadata format")?;
    validate_requested(&after, patch)?;
    validate(&target, &file, &original)?;
    let same = same_bytes(&mut file, output.as_file_mut(), hasher.as_mut())?;
    let source = hasher
        .map(|hasher| {
            let fingerprint = *hasher.finalize().as_bytes();
            if same {
                WrittenSource::new(&original, fingerprint)
            } else {
                WrittenSource::new(&output.as_file().metadata()?, fingerprint)
            }
        })
        .transpose()?;
    if same {
        validate(&target, &file, &original)?;
        return Ok((
            EditReport {
                path: path.to_owned(),
                outcome: EditOutcome::Unchanged,
                tags: after,
            },
            source,
        ));
    }
    output.as_file().set_permissions(original.permissions())?;
    crate::xattrs::copy(&file, output.as_file())?;
    validate(&target, &file, &original)?;
    output
        .persist(&target)
        .context("Replacing metadata source")?;
    Ok((
        EditReport {
            path: path.to_owned(),
            outcome: EditOutcome::Replaced,
            tags: after,
        },
        source,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_keeps_offsets_and_contents_independent_when_supported() {
        use std::{io::Write, os::fd::AsRawFd};

        let cache = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.cache");
        fs::create_dir_all(&cache).unwrap();
        let mut source = tempfile::tempfile_in(&cache).unwrap();
        source.write_all(b"original source bytes").unwrap();
        let probe = tempfile::tempfile_in(&cache).unwrap();
        if unsafe { libc::ioctl(probe.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) } != 0 {
            // Only the clone-specific assertions depend on filesystem support.
            return;
        }
        let mut destination = tempfile::tempfile_in(&cache).unwrap();
        source.seek(std::io::SeekFrom::Start(3)).unwrap();
        copy_source(&mut source, &mut destination).unwrap();
        // Ordinary copying rewinds and consumes the source. An unchanged offset
        // proves this successful call took the real FICLONE path.
        assert_eq!(source.stream_position().unwrap(), 3);
        assert_eq!(destination.stream_position().unwrap(), 0);
        let mut bytes = Vec::new();
        destination.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"original source bytes");
        destination.rewind().unwrap();
        destination.write_all(b"modified").unwrap();
        source.rewind().unwrap();
        bytes.clear();
        source.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"original source bytes");
    }

    #[test]
    fn failed_clone_rewinds_and_truncates_before_real_copy() {
        use std::{io::Write, os::fd::AsRawFd};

        // procfs files cannot be reflinked, but support ordinary read/seek I/O.
        // This exercises a genuine kernel clone failure on every test filesystem.
        let expected = fs::read("/proc/version").unwrap();
        let mut source = File::open("/proc/version").unwrap();
        let mut destination = tempfile::tempfile().unwrap();
        destination
            .write_all(&vec![b'x'; expected.len() + 4096])
            .unwrap();
        assert_ne!(
            unsafe { libc::ioctl(destination.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) },
            0
        );
        source.seek(std::io::SeekFrom::Start(3)).unwrap();
        copy_source(&mut source, &mut destination).unwrap();
        destination.rewind().unwrap();
        let mut bytes = Vec::new();
        destination.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, expected);
    }

    #[test]
    fn comparison_without_fingerprint_stops_at_first_difference() {
        use std::io::Write;

        let mut source = tempfile::tempfile().unwrap();
        let mut written = tempfile::tempfile().unwrap();
        source.write_all(&vec![7; 3 * 64 * 1024]).unwrap();
        written.write_all(&vec![8; 3 * 64 * 1024]).unwrap();
        assert!(!same_bytes(&mut source, &mut written, None).unwrap());
        assert_eq!(written.stream_position().unwrap(), 64 * 1024);
        written.set_len(4 * 64 * 1024).unwrap();
        written.rewind().unwrap();
        assert!(!same_bytes(&mut source, &mut written, None).unwrap());
        assert_eq!(written.stream_position().unwrap(), 0);
    }

    #[test]
    fn comparison_hashes_all_output_bytes_after_difference() {
        use std::io::Write;

        let original = vec![7; 3 * 64 * 1024];
        for output in [
            original.clone(),
            {
                let mut bytes = original.clone();
                bytes[0] = 8;
                bytes[2 * 64 * 1024] = 9;
                bytes
            },
            vec![8; original.len() + 1],
            vec![8; original.len() - 1],
        ] {
            let mut source = tempfile::tempfile().unwrap();
            let mut written = tempfile::tempfile().unwrap();
            source.write_all(&original).unwrap();
            written.write_all(&output).unwrap();
            let mut hasher = blake3::Hasher::new();
            let same = same_bytes(&mut source, &mut written, Some(&mut hasher)).unwrap();
            assert_eq!(same, original == output);
            assert_eq!(
                hasher.finalize().as_bytes(),
                blake3::hash(&output).as_bytes()
            );
            assert_eq!(
                source.stream_position().unwrap(),
                if original == output {
                    original.len() as u64
                } else if original.len() == output.len() {
                    64 * 1024
                } else {
                    original.len() as u64
                }
            );
        }
    }

    #[test]
    fn descriptor_and_path_identity_must_both_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source");
        fs::write(&path, b"original").unwrap();
        let file = File::open(&path).unwrap();
        let original = file.metadata().unwrap();
        fs::rename(&path, dir.path().join("moved")).unwrap();
        fs::write(&path, b"original").unwrap();
        assert!(validate(&path, &file, &original).is_err());
    }
}
