use super::*;
use std::{
    fs,
    io::{Seek, Write},
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::Path,
};

fn assert_written_source(report: &EditReport, source: &WrittenSource, path: &Path) {
    let metadata = fs::metadata(path).unwrap();
    let bytes = fs::read(path).unwrap();
    assert_eq!(source.fingerprint, *blake3::hash(&bytes).as_bytes());
    assert_eq!(source.file_size, metadata.len());
    assert_eq!(
        source.modified_ns,
        metadata.mtime() * 1_000_000_000 + metadata.mtime_nsec()
    );
    assert!(
        serde_json::to_value(report)
            .unwrap()
            .get("source")
            .is_none()
    );
}

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
fn concrete_flac_edit_preserves_unknown_fields_pictures_multivalues_and_audio() {
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
    let (report, source) = edit_file_with_source(
        &path,
        &TagPatch {
            title: Some("New".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(report.changed());
    assert_written_source(&report, &source, &path);
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
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn normalized_same_value_edit_is_idempotent_without_inode_change() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("track.flac");
    fs::write(
        &path,
        flac(&["TITLE=Same", "ARTIST=First", "ARTIST=Second"], None),
    )
    .unwrap();
    edit_file(&path, &TagPatch::default()).unwrap();
    let modified = std::time::UNIX_EPOCH + std::time::Duration::new(1_000_000, 123_456_789);
    File::open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
    let before = fs::metadata(&path).unwrap();
    let (report, source) = edit_file_with_source(
        &path,
        &TagPatch {
            title: Some("Same".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!report.changed());
    assert_written_source(&report, &source, &path);
    assert_eq!(source.modified_ns, 1_000_000_123_456_789);
    let after = fs::metadata(&path).unwrap();
    assert_eq!(
        (before.ino(), before.mtime(), before.mtime_nsec()),
        (after.ino(), after.mtime(), after.mtime_nsec())
    );
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
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

fn tagged_mp3(primary_track: u8, secondary_track: u8) -> Vec<u8> {
    let frames = [
        frame(b"TIT2", b"\x03Primary"),
        frame(b"TRCK", &[3, b'0' + primary_track]),
    ]
    .concat();
    let mut bytes = b"ID3\x04\0\0".to_vec();
    bytes.extend_from_slice(&syncsafe(frames.len()));
    bytes.extend_from_slice(&frames);
    let mut audio = vec![0; 417];
    audio[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0]);
    for _ in 0..4 {
        bytes.extend_from_slice(&audio);
    }
    let mut id3v1 = [0; 128];
    id3v1[..3].copy_from_slice(b"TAG");
    id3v1[93..97].copy_from_slice(b"2024");
    id3v1[126] = secondary_track;
    id3v1[127] = 255;
    bytes.extend_from_slice(&id3v1);
    bytes
}

#[test]
fn title_only_mp3_edit_preserves_effective_track_over_id3v1() {
    use lofty::file::AudioFile;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mixed.mp3");
    for (primary, secondary, expected) in [(1, 2, 1), (0, 2, 2), (1, 0, 1)] {
        fs::write(&path, tagged_mp3(primary, secondary)).unwrap();
        let before = read_tags(&mut File::open(&path).unwrap()).unwrap().unwrap();
        assert_eq!(before.track_number, Some(expected));
        let report = edit_file(
            &path,
            &TagPatch {
                title: Some("Changed".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(report.tags.title.as_deref(), Some("Changed"));
        assert_eq!(report.tags.track_number, Some(expected));
        let parsed =
            lofty::mpeg::MpegFile::read_from(&mut File::open(&path).unwrap(), options(false))
                .unwrap();
        assert!(parsed.id3v1().is_none());
        let after = read_tags(&mut File::open(&path).unwrap()).unwrap().unwrap();
        assert_eq!(after.track_number, before.track_number);
    }
}

#[test]
fn effective_numeric_fields_and_totals_survive_secondary_conversion() {
    use lofty::{file::AudioFile, tag::Accessor};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mixed.flac");
    for (fields, expected) in [
        (
            [
                "TRACKNUMBER=1",
                "TRACKTOTAL=10",
                "DISCNUMBER=3",
                "DISCTOTAL=4",
            ],
            (Some(1), Some(10), Some(3), Some(4)),
        ),
        (
            [
                "TRACKNUMBER=0",
                "TRACKTOTAL=0",
                "DISCNUMBER=0",
                "DISCTOTAL=0",
            ],
            (Some(2), Some(20), Some(5), Some(6)),
        ),
    ] {
        let frames = [frame(b"TRCK", b"\x032/20"), frame(b"TPOS", b"\x035/6")].concat();
        let mut bytes = b"ID3\x04\0\0".to_vec();
        bytes.extend_from_slice(&syncsafe(frames.len()));
        bytes.extend_from_slice(&frames);
        bytes.extend_from_slice(&flac(&fields, None));
        fs::write(&path, bytes).unwrap();
        edit_file(
            &path,
            &TagPatch {
                title: Some("Changed".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let parsed =
            lofty::flac::FlacFile::read_from(&mut File::open(&path).unwrap(), options(false))
                .unwrap();
        assert!(parsed.id3v2().is_none());
        let tag = parsed.vorbis_comments().unwrap();
        assert_eq!(
            (tag.track(), tag.track_total(), tag.disk(), tag.disk_total()),
            expected
        );
    }
}

#[test]
fn explicit_encoding_preserves_primary_precedence_and_secondary_fill() {
    let frames = [
        frame(b"TIT2", b"\0Secondary"),
        frame(b"TPE1", b"\0\xc0\xf0\xf2\xe8\xf1\xf2"),
    ]
    .concat();
    let mut bytes = b"ID3\x04\0\0".to_vec();
    bytes.extend_from_slice(&syncsafe(frames.len()));
    bytes.extend_from_slice(&frames);
    bytes.extend_from_slice(&flac(
        &["TITLE=Primary Unicode 日本語", "TRACKNUMBER=1"],
        None,
    ));
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(&bytes).unwrap();
    let tags = read_tags_with_encoding(&mut file, TagEncoding::Windows1251)
        .unwrap()
        .unwrap();
    assert_eq!(tags.title.as_deref(), Some("Primary Unicode 日本語"));
    assert_eq!(tags.artist.as_deref(), Some("Артист"));
    assert_eq!(tags.track_number, Some(1));
}

#[test]
fn mixed_tags_merge_convertible_fields_remove_secondary_and_allow_clear() {
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
    use lofty::file::AudioFile;
    let parsed =
        lofty::iff::wav::WavFile::read_from(&mut File::open(&path).unwrap(), options(true))
            .unwrap();
    assert!(parsed.riff_info().is_none());
    assert!(parsed.id3v2().is_some());
    edit_file(
        &path,
        &TagPatch {
            title: Some(String::new()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        read_tags(&mut File::open(&path).unwrap())
            .unwrap()
            .unwrap()
            .title,
        None
    );
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn extended_attributes_and_acl_survive_replacement() {
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
    assert!(report.changed());
    {
        let path = path.as_path();
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
fn read_only_source_is_refused_without_artifacts() {
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
fn id3v23_date_precision_survives_v24_normalization() {
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
    let recovered =
        read_tags_with_encoding(&mut File::open(&path).unwrap(), TagEncoding::Windows1251)
            .unwrap()
            .unwrap();
    assert_eq!(recovered.release_date, before.release_date);
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
    assert_eq!(edited[id3 + 3], 4);
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

#[test]
fn six_field_patch_preserves_date_precision_and_clears_aliases() {
    let dir = tempfile::tempdir().unwrap();
    for (index, date) in ["2024", "2024-03", "2024-03-09"].into_iter().enumerate() {
        for wav in [false, true] {
            let path = dir.path().join(format!("precision-{index}-{wav}"));
            fs::write(
                &path,
                if wav {
                    tagged_wav(&frame(b"TIT2", b"\x03Old"), false)
                } else {
                    flac(&["TITLE=Old", "DATE=1999", "YEAR=1980"], None)
                },
            )
            .unwrap();
            let report = edit_file(
                &path,
                &TagPatch {
                    title: Some("Title".into()),
                    artist: Some("Artist".into()),
                    album: Some("Album".into()),
                    release_date: Some(date.into()),
                    track_number: Some(Some(12)),
                    disc_number: Some(Some(2)),
                },
            )
            .unwrap();
            assert_eq!(
                report.tags,
                Tags {
                    title: Some("Title".into()),
                    artist: Some("Artist".into()),
                    album: Some("Album".into()),
                    release_date: Some(date.into()),
                    track_number: Some(12),
                    disc_number: Some(2),
                }
            );
            let cleared = edit_file(
                &path,
                &TagPatch {
                    title: Some(String::new()),
                    artist: Some(String::new()),
                    album: Some(String::new()),
                    release_date: Some(String::new()),
                    track_number: Some(None),
                    disc_number: Some(None),
                },
            )
            .unwrap();
            assert_eq!(cleared.tags, Tags::default());
            assert_eq!(
                read_tags(&mut File::open(&path).unwrap()).unwrap(),
                Some(Tags::default())
            );
        }
    }
}

#[test]
fn encoding_override_recovers_only_latin1_id3_and_is_strict() {
    let cases = [
        (TagEncoding::Utf8, encoding_rs::UTF_8, "中文"),
        (TagEncoding::Gb18030, encoding_rs::GB18030, "简体中文"),
        (TagEncoding::Big5, encoding_rs::BIG5, "繁體中文"),
        (TagEncoding::ShiftJis, encoding_rs::SHIFT_JIS, "日本語"),
        (TagEncoding::EucKr, encoding_rs::EUC_KR, "한국어"),
        (TagEncoding::Windows1252, encoding_rs::WINDOWS_1252, "€café"),
        (
            TagEncoding::Windows1251,
            encoding_rs::WINDOWS_1251,
            "Русский",
        ),
    ];
    for (mode, codec, expected) in cases {
        let (encoded, _, errors) = codec.encode(expected);
        assert!(!errors);
        let mut payload = vec![0];
        payload.extend_from_slice(&encoded);
        let mut unicode_album = vec![1, 0xff, 0xfe];
        for unit in "专辑".encode_utf16() {
            unicode_album.extend_from_slice(&unit.to_le_bytes());
        }
        let frames = [
            frame(b"TIT2", &payload),
            frame(b"TPE1", "\u{3}正しいUnicode".as_bytes()),
            frame(b"TALB", &unicode_album),
        ]
        .concat();
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&tagged_wav(&frames, false)).unwrap();
        let tags = read_tags_with_encoding(&mut file, mode).unwrap().unwrap();
        assert_eq!(tags.title.as_deref(), Some(expected));
        assert_eq!(tags.artist.as_deref(), Some("正しいUnicode"));
        assert_eq!(tags.album.as_deref(), Some("专辑"));
        assert_ne!(read_tags(&mut file).unwrap().unwrap().title, tags.title);
    }
    let mut invalid = tempfile::tempfile().unwrap();
    invalid
        .write_all(&tagged_wav(&frame(b"TIT2", b"\0\xff"), false))
        .unwrap();
    assert!(read_tags_with_encoding(&mut invalid, TagEncoding::Utf8).is_err());
    assert!(read_tags(&mut invalid).is_ok());
    let mut non_id3 = tempfile::tempfile().unwrap();
    non_id3
        .write_all(&flac(&["TITLE=正确Unicode"], None))
        .unwrap();
    assert_eq!(
        read_tags_with_encoding(&mut non_id3, TagEncoding::Windows1251)
            .unwrap()
            .unwrap()
            .title
            .as_deref(),
        Some("正确Unicode")
    );
}

#[test]
fn same_value_id3_edit_normalizes_versions_utf8_and_then_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    for version in [2, 3, 4] {
        let path = dir.path().join(format!("v{version}.wav"));
        let frames = if version == 2 {
            // Known title plus an unmappable v2.2-only frame: conversion may
            // discard that frame, but must not reject the whole edit.
            b"TT2\0\0\x05\0SameXYZ\0\0\x02\x01\x02".to_vec()
        } else {
            frame(b"TIT2", b"\0Same")
        };
        let mut original = tagged_wav(&frames, false);
        let id3 = original.windows(4).position(|w| w == b"ID3 ").unwrap() + 8;
        original[id3 + 3] = version;
        fs::write(&path, &original).unwrap();
        let patch = TagPatch {
            title: Some("Same".into()),
            ..Default::default()
        };
        let report = edit_file(&path, &patch).unwrap();
        assert!(report.changed());
        assert_eq!(report.tags.title.as_deref(), Some("Same"));
        let saved = fs::read(&path).unwrap();
        let id3 = saved.windows(4).position(|w| w == b"ID3 ").unwrap() + 8;
        assert_eq!(saved[id3 + 3], 4);
        let title = saved.windows(4).position(|w| w == b"TIT2").unwrap();
        assert_eq!(saved[title + 10], 3, "ID3 descriptive text uses UTF-8");
        assert!(saved.windows(10).any(|w| w == b"data\x02\0\0\0\0\x40"));
        let inode = fs::metadata(&path).unwrap().ino();
        let (unchanged, source) = edit_file_with_source(&path, &patch).unwrap();
        assert!(!unchanged.changed());
        assert_written_source(&unchanged, &source, &path);
        assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
        assert_eq!(fs::read(&path).unwrap(), saved);
    }
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 3);
}

#[test]
fn leading_flac_id3_is_merged_and_removed_without_touching_audio() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("leading-id3.flac");
    let frames = frame(b"TPE1", b"\x03Secondary artist");
    let mut original = b"ID3\x04\0\0".to_vec();
    original.extend_from_slice(&syncsafe(frames.len()));
    original.extend_from_slice(&frames);
    original.extend_from_slice(&flac(&["TITLE=Same", "X-CUSTOM=preserve"], None));
    fs::write(&path, &original).unwrap();
    let report = edit_file(
        &path,
        &TagPatch {
            title: Some("Same".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(report.changed());
    assert_eq!(report.tags.artist.as_deref(), Some("Secondary artist"));
    let saved = fs::read(&path).unwrap();
    assert!(saved.starts_with(b"fLaC"));
    assert!(saved.ends_with(b"\xff\xf8untouched encoded audio payload"));
    assert!(saved.windows(17).any(|w| w == b"X-CUSTOM=preserve"));
    assert!(!edit_file(&path, &TagPatch::default()).unwrap().changed());
}

#[test]
fn aiff_text_is_transferred_to_id3v24_and_audio_is_unchanged() {
    fn chunk(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut bytes = id.to_vec();
        bytes.extend_from_slice(&(data.len() as u32).to_be_bytes());
        bytes.extend_from_slice(data);
        if !data.len().is_multiple_of(2) {
            bytes.push(0);
        }
        bytes
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("native.aiff");
    let audio = b"\0\0\0\0\0\0\0\0\x12\x34";
    let body = [
        b"AIFF".to_vec(),
        chunk(
            b"COMM",
            b"\0\x01\0\0\0\x01\0\x10\x40\x0e\xac\x44\0\0\0\0\0\0",
        ),
        chunk(b"NAME", b"Native title"),
        chunk(b"AUTH", b"Native artist"),
        chunk(b"ANNO", b"Unrelated comment"),
        chunk(b"SSND", audio),
    ]
    .concat();
    let mut original = b"FORM".to_vec();
    original.extend_from_slice(&(body.len() as u32).to_be_bytes());
    original.extend(body);
    fs::write(&path, original).unwrap();
    let report = edit_file(
        &path,
        &TagPatch {
            album: Some("Album".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(report.tags.title.as_deref(), Some("Native title"));
    assert_eq!(report.tags.artist.as_deref(), Some("Native artist"));
    use lofty::{file::AudioFile, tag::Accessor};
    let parsed =
        lofty::iff::aiff::AiffFile::read_from(&mut File::open(&path).unwrap(), options(true))
            .unwrap();
    assert!(parsed.text_chunks().is_none());
    let id3 = parsed.id3v2().unwrap();
    assert_eq!(id3.original_version(), lofty::id3::v2::Id3v2Version::V4);
    assert_eq!(id3.comment().as_deref(), Some("Unrelated comment"));
    assert!(
        fs::read(&path)
            .unwrap()
            .windows(audio.len())
            .any(|w| w == audio)
    );
    assert!(!edit_file(&path, &TagPatch::default()).unwrap().changed());
}
