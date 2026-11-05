use super::*;
use std::{
    fs,
    io::{Seek, Write},
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::Path,
};

fn block(out: &mut Vec<u8>, kind: u8, last: bool, data: &[u8]) {
    out.push(kind | if last { 128 } else { 0 });
    out.extend_from_slice(&(data.len() as u32).to_be_bytes()[1..]);
    out.extend_from_slice(data);
}
fn flac(comments: &[&str], picture: Option<&[u8]>) -> Vec<u8> {
    let mut bytes = b"fLaC".to_vec();
    let mut stream_info = [0u8; 34];
    stream_info[0..4].copy_from_slice(&[0x10, 0, 0x10, 0]);
    stream_info[10..18].copy_from_slice(&((44100u64 << 44) | (15u64 << 36) | 1).to_be_bytes());
    block(&mut bytes, 0, false, &stream_info);
    block(&mut bytes, 2, false, b"TESTunknown application payload");
    let mut tag = Vec::new();
    tag.extend_from_slice(&6u32.to_le_bytes());
    tag.extend_from_slice(b"vendor");
    tag.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for comment in comments {
        tag.extend_from_slice(&(comment.len() as u32).to_le_bytes());
        tag.extend_from_slice(comment.as_bytes());
    }
    block(&mut bytes, 4, picture.is_none(), &tag);
    if let Some(picture) = picture {
        let mut data = 3u32.to_be_bytes().to_vec();
        data.extend_from_slice(&9u32.to_be_bytes());
        data.extend_from_slice(b"image/png");
        data.extend_from_slice(&0u32.to_be_bytes());
        for n in [1u32, 1, 24, 0, picture.len() as u32] {
            data.extend_from_slice(&n.to_be_bytes());
        }
        data.extend_from_slice(picture);
        block(&mut bytes, 6, true, &data);
    }
    bytes.extend_from_slice(b"\xff\xf8untouched encoded audio payload");
    bytes
}

#[test]
fn summary_preserves_precision_first_value_and_positive_numbers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("misleading.mp3");
    fs::write(
        &path,
        flac(
            &[
                "TITLE=Title",
                "ARTIST=First",
                "ARTIST=Second",
                "ALBUM=Album",
                "DATE=2024-03-09",
                "TRACKNUMBER=03/12",
                "DISCNUMBER=0/2",
            ],
            None,
        ),
    )
    .unwrap();
    let tags = read_tags(&mut File::open(path).unwrap()).unwrap().unwrap();
    assert_eq!(tags.title.as_deref(), Some("Title"));
    assert_eq!(tags.artist.as_deref(), Some("First"));
    assert_eq!(tags.release_date.as_deref(), Some("2024-03-09"));
    assert_eq!(tags.track_number, Some(3));
    assert_eq!(tags.disc_number, None);
}

#[test]
fn unsupported_content_has_no_tags_or_artwork() {
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(b"unsupported container").unwrap();
    assert_eq!(read_tags(&mut file).unwrap(), None);
    assert_eq!(
        read_artwork(file, 1024, |data| Some(data.to_vec())).unwrap(),
        None
    );
}

#[test]
fn supported_untagged_content_has_empty_tags() {
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(&flac(&[], None)).unwrap();
    assert_eq!(read_tags(&mut file).unwrap(), Some(Tags::default()));
    assert_eq!(
        read_artwork(file, 1024, |data| Some(data.to_vec())).unwrap(),
        None
    );
}

#[test]
fn artwork_is_content_detected_and_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("misleading.mp3");
    let image = include_bytes!("../../rivu-presentation/data/icons/rivu.png");
    fs::write(&path, flac(&[], Some(image))).unwrap();
    assert_eq!(
        read_artwork(File::open(&path).unwrap(), image.len(), |data| Some(
            data.to_vec()
        ))
        .unwrap(),
        Some(image.to_vec())
    );
    assert_eq!(
        read_artwork(File::open(&path).unwrap(), image.len() - 1, |data| Some(
            data.to_vec()
        ))
        .unwrap(),
        None
    );
    assert_eq!(
        read_artwork(File::open(path).unwrap(), 0, |data| Some(data.to_vec())).unwrap(),
        None
    );
}

#[test]
fn concrete_flac_edit_preserves_unknown_fields_pictures_multivalues_audio_and_backup() {
    use lofty::file::AudioFile;
    use lofty::ogg::OggPictureStorage;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("track.flac");
    let image = include_bytes!("../../rivu-presentation/data/icons/rivu.png");
    let original = flac(
        &[
            "TITLE=Old",
            "ARTIST=First",
            "ARTIST=Second",
            "X-CUSTOM=untouched",
            "DATE=2024-03-09",
        ],
        Some(image),
    );
    fs::write(&path, &original).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    let report = edit_file(
        &path,
        &TagPatch {
            title: Some("New".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let backup = report.backup_path().unwrap();
    assert!(report.changed());
    assert_eq!(fs::read(backup).unwrap(), original);
    assert_ne!(
        fs::metadata(&path).unwrap().ino(),
        fs::metadata(backup).unwrap().ino()
    );
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o640
    );
    let edited = fs::read(&path).unwrap();
    assert!(edited.ends_with(b"\xff\xf8untouched encoded audio payload"));
    assert!(
        edited
            .windows(31)
            .any(|w| w == b"TESTunknown application payload")
    );
    let parsed =
        lofty::flac::FlacFile::read_from(&mut File::open(&path).unwrap(), options(true)).unwrap();
    let tag = parsed.vorbis_comments().unwrap();
    assert_eq!(
        tag.get_all("ARTIST").collect::<Vec<_>>(),
        ["First", "Second"]
    );
    assert_eq!(tag.get("X-CUSTOM"), Some("untouched"));
    assert_eq!(tag.get("DATE"), Some("2024-03-09"));
    assert_eq!(parsed.pictures()[0].0.data(), image);
    // Mutating the replacement cannot affect the separately copied backup.
    fs::write(&path, b"changed afterwards").unwrap();
    assert_eq!(fs::read(backup).unwrap(), original);
}

#[test]
fn no_op_has_no_backup_or_inode_change() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("track.flac");
    fs::write(
        &path,
        flac(&["TITLE=Same", "ARTIST=First", "ARTIST=Second"], None),
    )
    .unwrap();
    let before = fs::metadata(&path).unwrap();
    let report = edit_file(
        &path,
        &TagPatch {
            title: Some("Same".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!report.changed());
    assert!(report.backup_path().is_none());
    let after = fs::metadata(&path).unwrap();
    assert_eq!(
        (before.ino(), before.mtime(), before.mtime_nsec()),
        (after.ino(), after.mtime(), after.mtime_nsec())
    );
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn directory_sync_failure_preserves_exact_replacement_and_backup_evidence() {
    for fail_at in [1, 2] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("track.flac");
        let original = flac(&["TITLE=Original"], None);
        fs::write(&path, &original).unwrap();
        let calls = std::cell::Cell::new(0);
        let failure = crate::writer::edit_file_with_sync(
            &path,
            &TagPatch {
                title: Some("Edited".into()),
                ..Default::default()
            },
            |directory| {
                calls.set(calls.get() + 1);
                if calls.get() == fail_at {
                    anyhow::bail!("injected directory sync failure");
                }
                directory.sync_all()?;
                Ok(())
            },
        )
        .unwrap_err();
        let backup = match &failure.outcome {
            EditFailureOutcome::BeforeReplace {
                backup_path: Some(backup),
            } if fail_at == 1 => backup,
            EditFailureOutcome::ReplacedDurabilityUnknown { backup_path } if fail_at == 2 => {
                backup_path
            }
            other => panic!("wrong replacement evidence: {other:?}"),
        };
        assert_eq!(fs::read(backup).unwrap(), original);
        let tags = read_tags(&mut File::open(&path).unwrap()).unwrap().unwrap();
        assert_eq!(
            tags.title.as_deref(),
            Some(if fail_at == 1 { "Original" } else { "Edited" })
        );
    }
}

#[test]
fn ambiguous_paths_and_unsupported_writes_do_not_mutate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source");
    let bytes = b"unsupported container";
    fs::write(&path, bytes).unwrap();
    let patch = TagPatch {
        title: Some("New".into()),
        ..Default::default()
    };
    assert!(edit_file(&path, &patch).is_err());
    assert_eq!(fs::read(&path).unwrap(), bytes);
    let link = dir.path().join("link");
    symlink(&path, &link).unwrap();
    assert!(edit_file(&link, &patch).is_err());
    let hard = dir.path().join("hard");
    fs::hard_link(&path, &hard).unwrap();
    assert!(edit_file(&path, &patch).is_err());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 3);
}

#[test]
fn sparse_audio_payload_has_no_whole_file_size_cap() {
    let cache = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.cache");
    fs::create_dir_all(&cache).unwrap();
    let mut file = tempfile::tempfile_in(cache).unwrap();
    file.write_all(&flac(&["TITLE=Sparse"], None)).unwrap();
    file.set_len(2 * 1024 * 1024 * 1024).unwrap();
    file.rewind().unwrap();
    assert_eq!(
        read_tags(&mut file).unwrap().unwrap().title.as_deref(),
        Some("Sparse")
    );
}

fn syncsafe(value: usize) -> [u8; 4] {
    [
        ((value >> 21) & 127) as u8,
        ((value >> 14) & 127) as u8,
        ((value >> 7) & 127) as u8,
        (value & 127) as u8,
    ]
}
fn frame(id: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut bytes = id.to_vec();
    bytes.extend_from_slice(&syncsafe(payload.len()));
    bytes.extend_from_slice(&[0, 0]);
    bytes.extend_from_slice(payload);
    bytes
}
fn tagged_wav(frames: &[u8], secondary: bool) -> Vec<u8> {
    fn chunk(bytes: &mut Vec<u8>, id: &[u8; 4], data: &[u8]) {
        bytes.extend_from_slice(id);
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(data);
        if !data.len().is_multiple_of(2) {
            bytes.push(0);
        }
    }
    let mut body = b"WAVE".to_vec();
    chunk(
        &mut body,
        b"fmt ",
        b"\x01\0\x01\0\x40\x1f\0\0\x80\x3e\0\0\x02\0\x10\0",
    );
    let mut id3 = b"ID3\x04\0\0".to_vec();
    id3.extend_from_slice(&syncsafe(frames.len()));
    id3.extend_from_slice(frames);
    chunk(&mut body, b"ID3 ", &id3);
    if secondary {
        let mut info = b"INFO".to_vec();
        chunk(&mut info, b"INAM", b"Secondary\0");
        chunk(&mut info, b"IART", b"Secondary artist\0");
        chunk(&mut info, b"XXXX", b"Unknown RIFF value\0");
        chunk(&mut body, b"LIST", &info);
    }
    chunk(&mut body, b"data", b"\0\x40");
    let mut bytes = b"RIFF".to_vec();
    bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&body);
    bytes
}

#[test]
fn mixed_tags_fill_missing_fields_and_refuse_resurrected_clear() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mixed.wav");
    fs::write(&path, tagged_wav(&frame(b"TIT2", b"\x03Primary"), true)).unwrap();
    let before = read_tags(&mut File::open(&path).unwrap()).unwrap().unwrap();
    assert_eq!(before.title.as_deref(), Some("Primary"));
    assert_eq!(before.artist.as_deref(), Some("Secondary artist"));
    let report = edit_file(
        &path,
        &TagPatch {
            title: Some("Changed".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(report.changed());
    let after = read_tags(&mut File::open(&path).unwrap()).unwrap().unwrap();
    assert_eq!(after.title.as_deref(), Some("Changed"));
    assert_eq!(after.artist, before.artist);
    let original = fs::read(&path).unwrap();
    assert!(
        original
            .windows(18)
            .any(|bytes| bytes == b"Unknown RIFF value")
    );
    assert!(
        edit_file(
            &path,
            &TagPatch {
                title: Some(String::new()),
                ..Default::default()
            }
        )
        .is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
}

#[test]
fn extended_attributes_and_acl_survive_replacement_and_backup() {
    use std::os::fd::AsRawFd;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("attributes.flac");
    fs::write(&path, flac(&["TITLE=Old"], None)).unwrap();
    let file = File::open(&path).unwrap();
    let value = b"user metadata attribute";
    let result = unsafe {
        libc::fsetxattr(
            file.as_raw_fd(),
            c"user.rivu-test".as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    if result != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOTSUP) {
        return;
    }
    assert_eq!(result, 0);
    let mut acl = 2u32.to_le_bytes().to_vec();
    for (tag, permission, id) in [
        (1u16, 6u16, u32::MAX),
        (2, 4, 12345),
        (4, 4, u32::MAX),
        (16, 4, u32::MAX),
        (32, 0, u32::MAX),
    ] {
        acl.extend_from_slice(&tag.to_le_bytes());
        acl.extend_from_slice(&permission.to_le_bytes());
        acl.extend_from_slice(&id.to_le_bytes());
    }
    let acl_result = unsafe {
        libc::fsetxattr(
            file.as_raw_fd(),
            c"system.posix_acl_access".as_ptr(),
            acl.as_ptr().cast(),
            acl.len(),
            0,
        )
    };
    if acl_result != 0 {
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ENOTSUP)
        );
    }
    let report = edit_file(
        &path,
        &TagPatch {
            title: Some("New".into()),
            ..Default::default()
        },
    )
    .unwrap();
    for path in [path.as_path(), report.backup_path().unwrap()] {
        let file = File::open(path).unwrap();
        let mut actual = [0; 128];
        let len = unsafe {
            libc::fgetxattr(
                file.as_raw_fd(),
                c"user.rivu-test".as_ptr(),
                actual.as_mut_ptr().cast(),
                actual.len(),
            )
        };
        assert_eq!(&actual[..usize::try_from(len).unwrap()], value);
        if acl_result == 0 {
            let len = unsafe {
                libc::fgetxattr(
                    file.as_raw_fd(),
                    c"system.posix_acl_access".as_ptr(),
                    actual.as_mut_ptr().cast(),
                    actual.len(),
                )
            };
            assert_eq!(&actual[..usize::try_from(len).unwrap()], acl);
        }
    }
}

#[test]
fn read_only_source_is_refused_without_backup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("read-only.flac");
    let original = flac(&["TITLE=Old"], None);
    fs::write(&path, &original).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
    assert!(
        edit_file(
            &path,
            &TagPatch {
                title: Some("New".into()),
                ..Default::default()
            }
        )
        .is_err()
    );
    assert_eq!(fs::read(path).unwrap(), original);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn id3v23_date_precision_and_version_survive_title_edit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dated.wav");
    let mut frames = Vec::new();
    for (id, value) in [
        (b"TIT2", b"\0Old".as_slice()),
        (b"TYER", b"\x002024"),
        (b"TDAT", b"\x000903"),
        (b"TIME", b"\x001234"),
    ] {
        frames.extend_from_slice(&frame(id, value));
    }
    let mut bytes = tagged_wav(&frames, false);
    let id3 = bytes.windows(4).position(|w| w == b"ID3 ").unwrap() + 8;
    bytes[id3 + 3] = 3;
    fs::write(&path, bytes).unwrap();
    let before = read_tags(&mut File::open(&path).unwrap()).unwrap().unwrap();
    assert_eq!(before.release_date.as_deref(), Some("2024-03-09T12:34"));
    edit_file(
        &path,
        &TagPatch {
            title: Some("New".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let after = read_tags(&mut File::open(&path).unwrap()).unwrap().unwrap();
    assert_eq!(after.release_date, before.release_date);
    let edited = fs::read(path).unwrap();
    let id3 = edited.windows(4).position(|w| w == b"ID3 ").unwrap() + 8;
    assert_eq!(edited[id3 + 3], 3);
}

#[test]
fn title_edit_preserves_a_legacy_embedded_cover() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy-picture.flac");
    let picture = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";
    let coverart = format!("COVERART={picture}");
    fs::write(
        &path,
        flac(&["TITLE=Old", &coverart, "COVERARTMIME=image/png"], None),
    )
    .unwrap();
    let original = read_artwork(File::open(&path).unwrap(), 1024, |bytes| {
        Some(bytes.to_vec())
    })
    .unwrap()
    .unwrap();
    edit_file(
        &path,
        &TagPatch {
            title: Some("New".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let edited = read_artwork(File::open(&path).unwrap(), 1024, |bytes| {
        Some(bytes.to_vec())
    })
    .unwrap()
    .unwrap();
    assert_eq!(edited, original);
}

#[test]
fn artwork_prefers_first_accepted_front_then_other_pictures() {
    fn picture(kind: u8, data: &[u8]) -> Vec<u8> {
        let mut payload = b"\x03image/png\0".to_vec();
        payload.extend_from_slice(&[kind, 0]);
        payload.extend_from_slice(data);
        frame(b"APIC", &payload)
    }
    let frames = [
        picture(4, b"other"),
        picture(3, b"rejected"),
        picture(3, b"accepted"),
        picture(3, b"later"),
    ]
    .concat();
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(&tagged_wav(&frames, false)).unwrap();
    let mut offered = Vec::new();
    let selected = read_artwork(file.try_clone().unwrap(), 1024, |data| {
        offered.push(data.to_vec());
        (data == b"accepted").then_some(data.len())
    })
    .unwrap();
    assert_eq!(selected, Some(8));
    assert_eq!(offered, [b"rejected".to_vec(), b"accepted".to_vec()]);

    offered.clear();
    let selected = read_artwork(file, 1024, |data| {
        offered.push(data.to_vec());
        (data == b"other").then_some(data.len())
    })
    .unwrap();
    assert_eq!(selected, Some(5));
    assert_eq!(
        offered,
        [
            b"rejected".to_vec(),
            b"accepted".to_vec(),
            b"later".to_vec(),
            b"other".to_vec(),
        ]
    );
}
