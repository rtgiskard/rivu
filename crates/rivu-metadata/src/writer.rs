use crate::{configure_lofty, detect, options};
use anyhow::{Context, Result, bail, ensure};
use lofty::{
    config::WriteOptions,
    file::{AudioFile, FileType},
    tag::{Accessor, TagExt},
};
use serde::Serialize;
use std::{
    fs::{self, File, Metadata},
    io::{BufReader, Seek},
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

fn serialize_optional_path<S: serde::Serializer>(
    path: &Option<PathBuf>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    path.as_deref().map(JsonPath).serialize(serializer)
}

/// `None` leaves a field untouched; an empty string removes that field.
#[derive(Clone, Debug, Default)]
pub struct TagPatch {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum EditOutcome {
    Unchanged,
    Replaced {
        #[serde(serialize_with = "serialize_path")]
        backup_path: PathBuf,
    },
}

#[derive(Clone, Debug, Serialize)]
pub struct EditReport {
    #[serde(serialize_with = "serialize_path")]
    pub path: PathBuf,
    #[serde(flatten)]
    pub outcome: EditOutcome,
}

impl EditReport {
    pub fn changed(&self) -> bool {
        matches!(self.outcome, EditOutcome::Replaced { .. })
    }
    pub fn backup_path(&self) -> Option<&Path> {
        match &self.outcome {
            EditOutcome::Unchanged => None,
            EditOutcome::Replaced { backup_path } => Some(backup_path),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum EditFailureOutcome {
    BeforeReplace {
        #[serde(serialize_with = "serialize_optional_path")]
        backup_path: Option<PathBuf>,
    },
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
        write!(f, "{}: {:#}", self.path.display(), self.error)?;
        match &self.outcome {
            EditFailureOutcome::BeforeReplace { backup_path } => {
                write!(f, "; source was not replaced")?;
                if let Some(path) = backup_path {
                    write!(f, "; retained original backup: {}", path.display())?;
                }
            }
        }
        Ok(())
    }
}

impl std::error::Error for EditFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

enum NativeTag {
    Id3(lofty::id3::v2::Id3v2Tag),
    Ape(lofty::ape::ApeTag),
    Vorbis(lofty::ogg::tag::VorbisComments),
    Flac(lofty::flac::FlacFile),
    Mp4(lofty::mp4::Ilst),
    Riff(lofty::iff::wav::RiffInfoList),
    Aiff(lofty::iff::aiff::AiffTextChunks),
}

fn patch(tag: &mut impl Accessor, patch: &TagPatch) -> bool {
    let mut changed = false;
    macro_rules! field {
        ($field:ident, $get:ident, $set:ident, $remove:ident) => {
            if let Some(value) = &patch.$field {
                let current = tag.$get();
                if (value.is_empty() && current.is_some())
                    || (!value.is_empty() && current.as_deref() != Some(value.as_str()))
                {
                    drop(current);
                    if value.is_empty() {
                        tag.$remove();
                    } else {
                        tag.$set(value.clone());
                    }
                    changed = true;
                }
            }
        };
    }
    field!(title, title, set_title, remove_title);
    field!(artist, artist, set_artist, remove_artist);
    field!(album, album, set_album, remove_album);
    changed
}

impl NativeTag {
    fn read(file: &mut File, kind: FileType) -> Result<Self> {
        configure_lofty(true);
        let file = &mut BufReader::new(file);
        macro_rules! read {
            ($file:ty, $get:ident, $variant:ident) => {{
                let mut parsed = <$file>::read_from(file, options(true))?;
                Self::$variant(parsed.$get().map(std::mem::take).unwrap_or_default())
            }};
            ($file:ty, $get:ident, $variant:ident, required) => {{
                let mut parsed = <$file>::read_from(file, options(true))?;
                Self::$variant(std::mem::take(parsed.$get()))
            }};
        }
        Ok(match kind {
            FileType::Mpeg => read!(lofty::mpeg::MpegFile, id3v2_mut, Id3),
            FileType::Aac => read!(lofty::aac::AacFile, id3v2_mut, Id3),
            FileType::Wav => {
                let mut parsed = lofty::iff::wav::WavFile::read_from(file, options(true))?;
                if let Some(tag) = parsed.id3v2_mut() {
                    Self::Id3(std::mem::take(tag))
                } else {
                    Self::Riff(
                        parsed
                            .riff_info_mut()
                            .map(std::mem::take)
                            .unwrap_or_default(),
                    )
                }
            }
            FileType::Aiff => {
                let mut parsed = lofty::iff::aiff::AiffFile::read_from(file, options(true))?;
                if let Some(tag) = parsed.id3v2_mut() {
                    Self::Id3(std::mem::take(tag))
                } else {
                    Self::Aiff(
                        parsed
                            .text_chunks_mut()
                            .map(std::mem::take)
                            .unwrap_or_default(),
                    )
                }
            }
            FileType::Flac => {
                let mut parsed = lofty::flac::FlacFile::read_from(file, options(true))?;
                // The leading, read-only ID3 block remains untouched on disk.
                parsed.remove_id3v2();
                Self::Flac(parsed)
            }
            FileType::Vorbis => read!(
                lofty::ogg::VorbisFile,
                vorbis_comments_mut,
                Vorbis,
                required
            ),
            FileType::Opus => read!(lofty::ogg::OpusFile, vorbis_comments_mut, Vorbis, required),
            FileType::Speex => read!(lofty::ogg::SpeexFile, vorbis_comments_mut, Vorbis, required),
            FileType::Mp4 => read!(lofty::mp4::Mp4File, ilst_mut, Mp4),
            FileType::Ape => read!(lofty::ape::ApeFile, ape_mut, Ape),
            FileType::WavPack => read!(lofty::wavpack::WavPackFile, ape_mut, Ape),
            FileType::Mpc => read!(lofty::musepack::MpcFile, ape_mut, Ape),
            _ => bail!("Unsupported metadata write format"),
        })
    }
    fn patch(&mut self, value: &TagPatch) -> bool {
        match self {
            Self::Id3(t) => patch(t, value),
            Self::Ape(t) => patch(t, value),
            Self::Vorbis(t) => patch(t, value),
            Self::Mp4(t) => patch(t, value),
            Self::Riff(t) => patch(t, value),
            Self::Aiff(t) => patch(t, value),
            Self::Flac(file) => {
                if file.vorbis_comments().is_none() {
                    file.set_vorbis_comments(Default::default());
                }
                patch(
                    file.vorbis_comments_mut().expect("inserted comments"),
                    value,
                )
            }
        }
    }
    fn write(&self, file: &mut File) -> Result<()> {
        let mut options = WriteOptions::new()
            .remove_others(false)
            .lossy_text_encoding(false)
            .parse_options(options(true));
        match self {
            Self::Id3(t) => {
                use lofty::id3::v2::Id3v2Version;
                ensure!(
                    t.original_version() != Id3v2Version::V2,
                    "Writing ID3v2.2 would require an implicit version conversion"
                );
                options = options.use_id3v23(t.original_version() == Id3v2Version::V3);
                t.save_to(file, options)?;
            }
            Self::Ape(t) => t.save_to(file, options)?,
            Self::Vorbis(t) => t.save_to(file, options)?,
            Self::Mp4(t) => t.save_to(file, options)?,
            Self::Riff(t) => t.save_to(file, options)?,
            Self::Aiff(t) => t.save_to(file, options)?,
            Self::Flac(t) => t.save_to(file, options)?,
        }
        Ok(())
    }
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

/// Edit only title/artist/album in a concrete native tag. The source is never
/// opened for writing: a same-directory temporary copy is edited and atomically
/// replaced only after an independent backup has been kept.
pub fn edit_file(path: &Path, patch: &TagPatch) -> std::result::Result<EditReport, EditFailure> {
    let mut outcome = EditFailureOutcome::BeforeReplace { backup_path: None };
    edit_file_inner(path, patch, &mut outcome).map_err(|error| EditFailure {
        path: path.to_owned(),
        outcome,
        error,
    })
}

fn edit_file_inner(
    path: &Path,
    patch: &TagPatch,
    outcome: &mut EditFailureOutcome,
) -> Result<EditReport> {
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
    let before = crate::read_tags(&mut file)?.context("Unsupported metadata write format")?;
    let mut expected = before.clone();
    macro_rules! expected_field { ($($field:ident),*) => { $(if let Some(value) = &patch.$field {
        let value = value.trim_matches('\0').trim();
        expected.$field = (!value.is_empty()).then(|| value.to_owned());
    })* }; }
    expected_field!(title, artist, album);
    if expected == before {
        validate(&target, &file, &original)?;
        return Ok(EditReport {
            path: path.to_owned(),
            outcome: EditOutcome::Unchanged,
        });
    }
    file.rewind()?;
    let mut tag = NativeTag::read(&mut file, kind)?;
    ensure!(
        tag.patch(patch),
        "Edit would require changing a secondary tag that is not the native write target"
    );
    if let NativeTag::Id3(t) = &tag {
        ensure!(
            t.original_version() != lofty::id3::v2::Id3v2Version::V2,
            "Writing ID3v2.2 would require an implicit version conversion"
        );
    }
    validate(&target, &file, &original)?;
    let mut output = tempfile::Builder::new()
        .prefix(".rivu-metadata-")
        .tempfile_in(&parent)?;
    let output_metadata = output.as_file().metadata()?;
    ensure!(
        output_metadata.uid() == original.uid() && output_metadata.gid() == original.gid(),
        "Cannot preserve source ownership with atomic replacement"
    );
    file.rewind()?;
    std::io::copy(&mut file, output.as_file_mut())?;
    output.as_file_mut().rewind()?;
    tag.write(output.as_file_mut())
        .context("Writing native metadata to temporary copy")?;
    drop(tag);
    output.as_file_mut().rewind()?;
    let mut written = NativeTag::read(output.as_file_mut(), kind)?;
    ensure!(
        !written.patch(patch),
        "Native writer did not preserve the requested metadata edit"
    );
    drop(written);
    let after =
        crate::read_tags(output.as_file_mut())?.context("Written file lost metadata format")?;
    ensure!(
        after == expected,
        "Edit cannot preserve merged metadata (a secondary tag may expose a cleared value)"
    );
    output.as_file().set_permissions(original.permissions())?;
    crate::xattrs::copy(&file, output.as_file())?;
    validate(&target, &file, &original)?;
    let mut backup = tempfile::Builder::new()
        .prefix(".rivu-backup-")
        .tempfile_in(&parent)?;
    file.rewind()?;
    std::io::copy(&mut file, backup.as_file_mut())?;
    backup.as_file().set_permissions(original.permissions())?;
    crate::xattrs::copy(&file, backup.as_file())?;
    validate(&target, &file, &original)?;
    let (_, backup_path) = backup.keep().context("Keeping original metadata backup")?;
    *outcome = EditFailureOutcome::BeforeReplace {
        backup_path: Some(backup_path.clone()),
    };
    validate(&target, &file, &original)?;
    output.persist(&target).with_context(|| {
        format!(
            "Replacing metadata source; original backup: {}",
            backup_path.display()
        )
    })?;
    Ok(EditReport {
        path: path.to_owned(),
        outcome: EditOutcome::Replaced { backup_path },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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
