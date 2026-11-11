use super::fixtures::*;
use super::*;
use std::fs::File;

const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone)]
struct TestSource {
    id: i64,
    fingerprint: Option<String>,
    path: PathBuf,
}

impl TestSource {
    fn source(&self) -> Source<'_> {
        Source {
            id: self.id,
            fingerprint: self.fingerprint.as_deref(),
            path: &self.path,
        }
    }
}

fn source_fixture(path: PathBuf) -> TestSource {
    TestSource {
        id: 1,
        fingerprint: path
            .is_file()
            .then(|| fs::read(&path).unwrap())
            .map(|bytes| blake3::hash(&bytes).to_hex().to_string()),
        path,
    }
}

#[test]
fn cold_embedded_source_mismatch_does_not_poison_requested_identity() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("track.wav");
    let mut original_image = DEFAULT_IMAGE.to_vec();
    original_image.push(0);
    tagged_wav_image(&path, &original_image);
    let original_bytes = fs::read(&path).unwrap();
    let original = source_fixture(path.clone());
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    let manager = ArtworkManager::new(directory.path());
    let mut other_image = DEFAULT_IMAGE.to_vec();
    other_image.push(42);
    tagged_wav_image(&path, &other_image);
    File::open(&path).unwrap().set_modified(modified).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().len(),
        original_bytes.len() as u64
    );
    let replacement = source_fixture(path.clone());
    assert!(manager.load(original.source()).is_none());
    assert!(manager.cached_result(original.source()).is_none());
    let observed = manager.load(replacement.source()).unwrap();
    assert_eq!(fs::read(observed.path()).unwrap(), other_image);
    fs::write(&path, original_bytes).unwrap();
    let recovered = manager.load(original.source()).unwrap();
    assert_eq!(fs::read(recovered.path()).unwrap(), original_image);
    fs::remove_file(&path).unwrap();
    assert_eq!(manager.load(original.source()), Some(recovered));
    assert_eq!(manager.load(replacement.source()), Some(observed));
}

#[test]
fn manager_without_default_image_never_exports_a_fallback() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("data");
    let manager = ArtworkManager::new(&data);
    assert!(manager.cached_default().is_none());
    assert!(manager.export_default().is_none());
    assert!(manager.cached_default().is_none());
    assert!(!data.exists());
}

#[test]
fn originals_use_bucketed_128_bit_names() {
    let directory = tempfile::tempdir().unwrap();
    let manager = ArtworkManager::new(directory.path()).with_default_image(DEFAULT_IMAGE);
    let root = directory.path().join("artwork/cache");
    fs::create_dir_all(&root).unwrap();
    let hash = blake3::hash(DEFAULT_IMAGE).to_hex();
    let image = manager.image_uri(DEFAULT_IMAGE, MAX_IMAGE_BYTES).unwrap();
    let expected = root.join(&hash[..2]).join(format!("{}.png", &hash[2..32]));
    assert_eq!(image.path.as_ref(), expected.as_path());
    assert_eq!(
        manager.export_default().unwrap().path().as_ref(),
        expected.as_path()
    );
    assert_eq!(fs::read(&expected).unwrap(), DEFAULT_IMAGE);
    fs::remove_file(&expected).unwrap();
    fs::create_dir(&expected).unwrap();
    assert!(
        ArtworkManager::new(directory.path())
            .with_default_image(DEFAULT_IMAGE)
            .export_default()
            .is_none()
    );
}

#[test]
fn existing_originals_require_bounded_content_equality_without_overwriting() {
    let directory = tempfile::tempdir().unwrap();
    let manager = ArtworkManager::new(directory.path()).with_default_image(DEFAULT_IMAGE);
    let path = cache_paths::original_path(
        &directory.path().join("artwork/cache"),
        blake3::hash(DEFAULT_IMAGE),
        "png",
    );
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut collision = DEFAULT_IMAGE.to_vec();
    *collision.last_mut().unwrap() ^= 1;
    fs::write(&path, &collision).unwrap();
    assert_eq!(
        write_image(&path, DEFAULT_IMAGE, MAX_IMAGE_BYTES)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    assert!(manager.image_uri(DEFAULT_IMAGE, MAX_IMAGE_BYTES).is_none());
    assert!(manager.export_default().is_none());
    assert_eq!(fs::read(&path).unwrap(), collision);
    // A length mismatch is rejected before reading any existing content.
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(MAX_IMAGE_BYTES as u64 + 1)
        .unwrap();
    assert_eq!(
        write_image(&path, DEFAULT_IMAGE, MAX_IMAGE_BYTES)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(
        fs::metadata(&path).unwrap().len(),
        MAX_IMAGE_BYTES as u64 + 1
    );
    fs::write(&path, DEFAULT_IMAGE).unwrap();
    age_file(&path);
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    assert_eq!(
        write_image(&path, DEFAULT_IMAGE, MAX_IMAGE_BYTES)
            .unwrap()
            .path
            .as_ref(),
        path
    );
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
}

#[test]
fn racing_different_publishers_never_clobber_the_winner() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("collision.png");
    let barrier = std::sync::Barrier::new(2);
    let results = std::thread::scope(|scope| {
        let left = scope.spawn(|| {
            barrier.wait();
            write_image(&path, b"left", MAX_IMAGE_BYTES)
        });
        let right = scope.spawn(|| {
            barrier.wait();
            write_image(&path, b"rght", MAX_IMAGE_BYTES)
        });
        [left.join().unwrap(), right.join().unwrap()]
    });
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    let winner = if results[0].is_ok() { b"left" } else { b"rght" };
    assert_eq!(fs::read(&path).unwrap(), winner);
    assert_eq!(
        results.into_iter().find_map(Result::err).unwrap().kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn independent_managers_racing_identical_publications_share_a_complete_original() {
    let directory = tempfile::tempdir().unwrap();
    let barrier = std::sync::Barrier::new(4);
    let images = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..4)
            .map(|_| {
                scope.spawn(|| {
                    let manager =
                        ArtworkManager::new(directory.path()).with_default_image(DEFAULT_IMAGE);
                    barrier.wait();
                    manager.export_default().unwrap()
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>()
    });
    for image in &images {
        assert_eq!(image.uri(), images[0].uri());
        assert_eq!(fs::read(image.path()).unwrap(), DEFAULT_IMAGE);
    }
    assert_eq!(
        fs::read_dir(images[0].path().parent().unwrap())
            .unwrap()
            .count(),
        1
    );
}

#[cfg(unix)]
#[test]
fn original_publication_never_reuses_a_symlink() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("outside.png");
    fs::write(&target, DEFAULT_IMAGE).unwrap();
    let path = directory.path().join("cached.png");
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert_eq!(
        write_image(&path, DEFAULT_IMAGE, MAX_IMAGE_BYTES)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    assert!(
        fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(target).unwrap(), DEFAULT_IMAGE);
}

#[test]
fn startup_and_negative_lookups_never_export_default_artwork() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("data");
    let manager = ArtworkManager::new(&data).with_default_image(DEFAULT_IMAGE);
    let selected = source_fixture(directory.path().join("missing.wav"));
    assert!(manager.cached(selected.source()).is_none());
    assert!(manager.cached_default().is_none());
    assert!(!data.exists());
    assert!(manager.load(selected.source()).is_none());
    assert_eq!(manager.cached_result(selected.source()), Some(None));
    assert!(
        manager
            .load_with(selected.source(), || panic!("negative lookup reprobed"))
            .is_none()
    );
    assert!(!data.exists());
    let exported = manager.export_default().unwrap();
    assert_eq!(fs::read(file_path(exported.uri())).unwrap(), DEFAULT_IMAGE);
    assert!(manager.cached(selected.source()).is_none());
    assert!(
        manager
            .load_with(selected.source(), || panic!(
                "export changed negative lookup"
            ))
            .is_none()
    );
    assert!(!data.join("artwork/default.png").exists());
}

#[test]
fn default_export_is_singleflight_without_track_lookups() {
    let directory = tempfile::tempdir().unwrap();
    let manager = ArtworkManager::new(directory.path()).with_default_image(DEFAULT_IMAGE);
    let barrier = std::sync::Barrier::new(4);
    let held = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..4)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    manager.export_default().unwrap()
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert!(held.iter().all(|image| image == &held[0]));
    let path = file_path(held[0].uri());
    assert!(managed_filename(path.file_name().unwrap()));
    assert_eq!(manager.cached_default(), Some(held[0].clone()));
    assert_eq!(fs::read(path).unwrap(), DEFAULT_IMAGE);
}

#[test]
fn failed_default_export_is_remembered_after_filesystem_recovers() {
    let directory = tempfile::tempdir().unwrap();
    let blocked = directory.path().join("data");
    fs::write(&blocked, b"not a directory").unwrap();
    let manager = ArtworkManager::new(&blocked).with_default_image(DEFAULT_IMAGE);
    assert!(manager.cached_default().is_none());
    assert!(manager.export_default().is_none());
    fs::remove_file(&blocked).unwrap();
    fs::create_dir(&blocked).unwrap();
    assert!(manager.export_default().is_none());
    assert!(manager.cached_default().is_none());
    assert!(!blocked.join("artwork").exists());
}

#[test]
fn configured_encoded_limit_applies_to_embedded_and_directory_images() {
    let directory = tempfile::tempdir().unwrap();
    for (version, size) in [(2, 9), (3, 16), (4, 17)] {
        let source = directory.path().join(format!("large-{version}.wav"));
        let size = size * 1024 * 1024;
        pictures_wav(&source, version, &[(DEFAULT_IMAGE, size, 3)]);
        let selected = source_fixture(source);
        let default = ArtworkManager::new(directory.path()).with_default_image(DEFAULT_IMAGE);
        let result = default.load(selected.source());
        assert_eq!(result.is_some(), size <= DEFAULT_MAX_IMAGE_BYTES);
        let small = ArtworkManager::new(directory.path())
            .with_default_image(DEFAULT_IMAGE)
            .with_max_image_bytes(1024 * 1024);
        assert!(small.load(selected.source()).is_none());
        let large = ArtworkManager::new(directory.path())
            .with_default_image(DEFAULT_IMAGE)
            .with_max_image_bytes(64 * 1024 * 1024);
        let image = large.load(selected.source()).unwrap();
        assert_eq!(
            fs::metadata(file_path(image.uri())).unwrap().len(),
            size as u64
        );
    }
    let cover = directory.path().join("cover.png");
    fs::write(&cover, DEFAULT_IMAGE).unwrap();
    File::options()
        .write(true)
        .open(&cover)
        .unwrap()
        .set_len(DEFAULT_MAX_IMAGE_BYTES as u64)
        .unwrap();
    let selected = source_fixture(directory.path().join("missing.wav"));
    let default = ArtworkManager::new(directory.path()).with_default_image(DEFAULT_IMAGE);
    assert_eq!(
        fs::metadata(file_path(default.load(selected.source()).unwrap().uri()))
            .unwrap()
            .len(),
        DEFAULT_MAX_IMAGE_BYTES as u64
    );
    let small = ArtworkManager::new(directory.path())
        .with_default_image(DEFAULT_IMAGE)
        .with_max_image_bytes(1024 * 1024);
    assert!(small.load(selected.source()).is_none());
    File::options()
        .write(true)
        .open(&cover)
        .unwrap()
        .set_len(DEFAULT_MAX_IMAGE_BYTES as u64 + 1)
        .unwrap();
    assert!(
        ArtworkManager::new(directory.path())
            .with_default_image(DEFAULT_IMAGE)
            .load(selected.source())
            .is_none()
    );
    let large = ArtworkManager::new(directory.path())
        .with_default_image(DEFAULT_IMAGE)
        .with_max_image_bytes(64 * 1024 * 1024);
    assert!(large.load(selected.source()).is_some());
}

#[test]
fn unsynchronised_covers_preserve_original_bytes() {
    let directory = tempfile::tempdir().unwrap();
    for (version, tag_wide) in [(2, true), (3, true), (4, false), (4, true)] {
        let path = directory
            .path()
            .join(format!("unsync-{version}-{tag_wide}.wav"));
        budget_unsynchronised_wav(&path, version, tag_wide);
        let manager = ArtworkManager::new(directory.path()).with_default_image(DEFAULT_IMAGE);
        let artwork = manager.load(source_fixture(path).source()).unwrap();
        assert_eq!(fs::read(file_path(artwork.uri())).unwrap(), DEFAULT_IMAGE);
    }
}

#[test]
fn embedded_front_cover_wins_over_an_earlier_other_picture() {
    let directory = tempfile::tempdir().unwrap();
    let mut front = DEFAULT_IMAGE.to_vec();
    front.push(1);
    for version in [2, 3, 4] {
        let path = directory.path().join(format!("front-{version}.wav"));
        pictures_wav(
            &path,
            version,
            &[
                (DEFAULT_IMAGE, DEFAULT_IMAGE.len(), 0),
                (&front, front.len(), 3),
            ],
        );
        let manager = ArtworkManager::new(directory.path()).with_default_image(DEFAULT_IMAGE);
        let artwork = manager.load(source_fixture(path).source()).unwrap();
        assert_eq!(fs::read(file_path(artwork.uri())).unwrap(), front);
    }
}

#[test]
fn unusable_front_cover_does_not_hide_a_later_usable_picture() {
    let directory = tempfile::tempdir().unwrap();
    let rejected = b"not a supported image";
    for version in [2, 3, 4] {
        for usage in [0, 3] {
            let path = directory
                .path()
                .join(format!("fallback-{version}-{usage}.wav"));
            pictures_wav(
                &path,
                version,
                &[
                    (rejected, rejected.len(), 3),
                    (DEFAULT_IMAGE, DEFAULT_IMAGE.len(), usage),
                ],
            );
            let manager = ArtworkManager::new(directory.path()).with_default_image(DEFAULT_IMAGE);
            let artwork = manager.load(source_fixture(path).source()).unwrap();
            assert_eq!(fs::read(file_path(artwork.uri())).unwrap(), DEFAULT_IMAGE);
        }
    }
}

#[test]
fn corrupt_supported_images_do_not_hide_embedded_or_directory_fallbacks() {
    let mut corrupt_pixels = DEFAULT_IMAGE.to_vec();
    // Keep the complete PNG header but corrupt IDAT payload and its checksum.
    corrupt_pixels[48] ^= 0xff;
    let mut embedded = DEFAULT_IMAGE.to_vec();
    embedded.push(1);
    let mut directory_image = DEFAULT_IMAGE.to_vec();
    directory_image.push(2);
    for rejected in [&DEFAULT_IMAGE[..8], &DEFAULT_IMAGE[..33], &corrupt_pixels] {
        assert!(image::guess_format(rejected).is_ok());
        for version in [2, 3, 4] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join(format!("corrupt-{version}.wav"));
            pictures_wav(
                &path,
                version,
                &[
                    (rejected, rejected.len(), 3),
                    (&embedded, embedded.len(), 3),
                ],
            );
            let manager = ArtworkManager::new(directory.path());
            let artwork = manager.load(source_fixture(path.clone()).source()).unwrap();
            assert_eq!(fs::read(artwork.path()).unwrap(), embedded);

            // A corrupt embedded picture also permits the directory fallback.
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join(format!("corrupt-{version}.wav"));
            pictures_wav(&path, version, &[(rejected, rejected.len(), 3)]);
            fs::write(directory.path().join("cover.png"), rejected).unwrap();
            fs::write(directory.path().join("folder.jpg"), &directory_image).unwrap();
            let manager = ArtworkManager::new(directory.path());
            let artwork = manager.load(source_fixture(path).source()).unwrap();
            assert_eq!(fs::read(artwork.path()).unwrap(), directory_image);
        }
    }
}

#[test]
fn embedded_cover_size_boundary_applies_across_id3_versions() {
    let directory = tempfile::tempdir().unwrap();
    for version in [2, 3, 4] {
        for size in [DEFAULT_IMAGE.len(), MAX_IMAGE_BYTES] {
            let path = directory.path().join(format!("small-{version}-{size}.wav"));
            pictures_wav(&path, version, &[(DEFAULT_IMAGE, size, 3)]);
            let manager = ArtworkManager::new(directory.path())
                .with_default_image(DEFAULT_IMAGE)
                .with_max_image_bytes(MAX_IMAGE_BYTES);
            let artwork = manager.load(source_fixture(path).source()).unwrap();
            let retained = fs::read(file_path(artwork.uri())).unwrap();
            assert_eq!(retained.len(), size);
            assert!(retained.starts_with(DEFAULT_IMAGE));
        }
    }
}

#[test]
fn oversized_embedded_cover_does_not_hide_following_cover() {
    let directory = tempfile::tempdir().unwrap();
    for version in [2, 3, 4] {
        for size in [
            MAX_IMAGE_BYTES + 1,
            if version == 2 { 9 } else { 27 } * 1024 * 1024,
        ] {
            let path = directory
                .path()
                .join(format!("oversized-{version}-{size}.wav"));
            pictures_wav(
                &path,
                version,
                &[
                    (DEFAULT_IMAGE, size, 3),
                    (DEFAULT_IMAGE, DEFAULT_IMAGE.len(), 3),
                ],
            );
            let manager = ArtworkManager::new(directory.path())
                .with_default_image(DEFAULT_IMAGE)
                .with_max_image_bytes(MAX_IMAGE_BYTES);
            let artwork = manager.load(source_fixture(path).source()).unwrap();
            assert_eq!(fs::read(file_path(artwork.uri())).unwrap(), DEFAULT_IMAGE);
        }
    }
}

#[test]
fn oversized_embedded_cover_is_not_published() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("only-oversized.wav");
    pictures_wav(&path, 3, &[(DEFAULT_IMAGE, 27 * 1024 * 1024, 3)]);
    let manager = ArtworkManager::new(directory.path()).with_default_image(DEFAULT_IMAGE);
    let selected = source_fixture(path);
    assert!(manager.load(selected.source()).is_none());
    assert_eq!(manager.cached_result(selected.source()), Some(None));
}

fn file_path(uri: &str) -> PathBuf {
    url::Url::parse(uri).unwrap().to_file_path().unwrap()
}

fn result_uri(result: Option<Artwork>) -> Option<String> {
    result.map(|artwork| artwork.uri().to_owned())
}

fn age_file(path: &Path) {
    File::open(path)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(172_800))
        .unwrap();
}

#[test]
fn embedded_picture_is_deduplicated_and_relocated_content_does_not_reload() {
    let directory = tempfile::Builder::new()
        .prefix("rivu artwork #")
        .tempdir()
        .unwrap();
    let source = directory.path().join("track.wav");
    tagged_wav(&source);
    let manager = ArtworkManager::new(directory.path()).with_default_image(DEFAULT_IMAGE);
    let mut selected = source_fixture(source);
    assert!(manager.cached(selected.source()).is_none());
    let first = manager.load(selected.source()).unwrap();
    let path = file_path(first.uri());
    assert_eq!(fs::read(&path).unwrap(), DEFAULT_IMAGE);
    assert!(first.uri().contains("%23"));
    selected.id = 2;
    let second = manager.load(selected.source()).unwrap();
    assert_eq!(first.uri(), second.uri());
    assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    selected.path = directory.path().join("missing.wav");
    assert_eq!(manager.load(selected.source()), Some(second));
    selected.fingerprint = Some("changed".into());
    assert!(manager.load(selected.source()).is_none());
    assert!(manager.cached(selected.source()).is_none());
}

#[test]
fn failed_source_paths_retry_after_relocation_without_changing_content_identity() {
    let directory = tempfile::tempdir().unwrap();
    let cover_source = directory.path().join("available.wav");
    tagged_wav(&cover_source);
    for old_path in ["missing.wav", "unprobeable.wav"] {
        let manager = ArtworkManager::new(directory.path()).with_default_image(DEFAULT_IMAGE);
        let mut selected = source_fixture(directory.path().join(old_path));
        selected.fingerprint = Some(
            blake3::hash(&fs::read(&cover_source).unwrap())
                .to_hex()
                .to_string(),
        );
        if old_path == "unprobeable.wav" {
            fs::write(&selected.path, b"not an audio file").unwrap();
        }
        assert!(manager.load(selected.source()).is_none());
        assert_eq!(manager.cached_result(selected.source()), Some(None));
        assert_eq!(
            manager.load_with(selected.source(), || panic!("unchanged failure reprobed")),
            None
        );

        selected.path = cover_source.clone();
        assert!(manager.cached_result(selected.source()).is_none());
        assert!(manager.cached(selected.source()).is_none());
        let recovered = manager.load(selected.source()).unwrap();
        assert_eq!(fs::read(file_path(recovered.uri())).unwrap(), DEFAULT_IMAGE);
        selected.path = directory.path().join("moved-again.wav");
        assert_eq!(
            manager.load_with(selected.source(), || panic!("successful content reprobed")),
            Some(recovered)
        );
    }
}

#[test]
fn no_cover_result_retries_directory_after_relocation() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("no-cover.wav");
    tagged_wav_image(&source, b"unsupported image bytes");
    let manager = ArtworkManager::new(directory.path()).with_default_image(DEFAULT_IMAGE);
    let mut selected = source_fixture(source);
    assert!(manager.load(selected.source()).is_none());
    assert_eq!(manager.cached_result(selected.source()), Some(None));
    assert_eq!(
        manager.load_with(selected.source(), || panic!("unchanged negative reprobed")),
        None
    );
    let moved = directory.path().join("moved");
    fs::create_dir(&moved).unwrap();
    selected.path = moved.join("track.wav");
    write_test_wave(&mut File::create(&selected.path).unwrap());
    fs::write(moved.join("cover.png"), DEFAULT_IMAGE).unwrap();
    assert!(manager.cached_result(selected.source()).is_none());
    assert!(manager.cached(selected.source()).is_none());
    let recovered = manager.load(selected.source()).unwrap();
    assert_eq!(fs::read(file_path(recovered.uri())).unwrap(), DEFAULT_IMAGE);
}

#[cfg(unix)]
#[test]
fn directory_fifo_and_fifo_symlink_do_not_block_following_image() {
    let directory = tempfile::tempdir().unwrap();
    let fifo = directory.path().join("cover.png");
    let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: name is a live NUL-terminated path, and mkfifo retains no pointer.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    std::os::unix::fs::symlink(&fifo, directory.path().join("folder.png")).unwrap();
    fs::write(directory.path().join("front.png"), DEFAULT_IMAGE).unwrap();
    let selected = source_fixture(directory.path().join("track.wav"));
    let manager = ArtworkManager::new(directory.path()).with_default_image(DEFAULT_IMAGE);
    let (complete, completed) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let artwork = manager.load(selected.source()).unwrap();
        complete
            .send(fs::read(file_path(artwork.uri())).unwrap())
            .unwrap();
    });
    // A broken nonblocking-open contract must fail rather than hang the suite.
    let bytes = completed.recv_timeout(Duration::from_secs(2)).unwrap();
    worker.join().unwrap();
    assert_eq!(bytes, DEFAULT_IMAGE);
}

fn directory_image(path: &Path, marker: u8) -> Vec<u8> {
    let mut bytes = DEFAULT_IMAGE.to_vec();
    bytes.push(marker);
    fs::write(path, &bytes).unwrap();
    bytes
}

#[test]
fn directory_images_follow_embedded_covers_and_deterministic_filename_priority() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("track.wav");
    let selected = source_fixture(source.clone());
    let candidates = [
        "cover.jpg",
        "COVER.PNG",
        "cover.png",
        "Folder.PNG",
        "front.webp",
    ];
    let images: Vec<_> = candidates
        .iter()
        .enumerate()
        .map(|(index, name)| directory_image(&directory.path().join(name), index as u8))
        .collect();
    tagged_wav(&source);
    let embedded = ArtworkManager::new(directory.path())
        .with_default_image(DEFAULT_IMAGE)
        .load(selected.source())
        .unwrap();
    assert_eq!(fs::read(file_path(embedded.uri())).unwrap(), DEFAULT_IMAGE);
    write_test_wave(&mut File::create(&source).unwrap());
    for (name, expected) in candidates.iter().zip(&images) {
        // Each manager has a fresh session lookup; old immutable disk copies
        // must not become candidates or affect the chosen source image.
        let manager = ArtworkManager::new(directory.path()).with_default_image(DEFAULT_IMAGE);
        let image = manager.load(selected.source()).unwrap();
        assert_eq!(fs::read(file_path(image.uri())).unwrap(), *expected);
        assert_eq!(file_path(image.uri()).extension().unwrap(), "png");
        fs::remove_file(directory.path().join(name)).unwrap();
    }
    assert!(
        ArtworkManager::new(directory.path())
            .with_default_image(DEFAULT_IMAGE)
            .load(selected.source())
            .is_none()
    );
}

#[test]
fn directory_images_require_exact_stems_and_supported_extensions() {
    let directory = tempfile::tempdir().unwrap();
    let selected = source_fixture(directory.path().join("missing.wav"));
    for name in [
        "other.png",
        "cover.back.jpg",
        "cover-small.png",
        "cover.svg",
        "folder.avif",
        "front",
        "cover.png.bak",
    ] {
        directory_image(&directory.path().join(name), 1);
    }
    assert!(
        ArtworkManager::new(directory.path())
            .with_default_image(DEFAULT_IMAGE)
            .load(selected.source())
            .is_none()
    );
    for extension in [
        "PNG", "JpG", "JPEG", "GIF", "WebP", "BMP", "TIF", "TIFF", "ICO",
    ] {
        let path = directory.path().join(format!("FrOnT.{extension}"));
        let expected = directory_image(&path, 2);
        let image = ArtworkManager::new(directory.path())
            .with_default_image(DEFAULT_IMAGE)
            .load(selected.source())
            .unwrap();
        assert_eq!(fs::read(file_path(image.uri())).unwrap(), expected);
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn directory_fallback_skips_invalid_oversized_and_nested_images() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("track.wav");
    write_test_wave(&mut File::create(&source).unwrap());
    let cover = directory.path().join("cover.png");
    fs::write(&cover, DEFAULT_IMAGE).unwrap();
    File::options()
        .write(true)
        .open(&cover)
        .unwrap()
        .set_len(MAX_IMAGE_BYTES as u64 + 1)
        .unwrap();
    fs::write(directory.path().join("folder.png"), b"not an image").unwrap();
    let nested = directory.path().join("front.png");
    fs::create_dir(&nested).unwrap();
    fs::write(nested.join("cover.png"), DEFAULT_IMAGE).unwrap();
    let boundary = directory.path().join("front.TIFF");
    fs::write(&boundary, DEFAULT_IMAGE).unwrap();
    File::options()
        .write(true)
        .open(&boundary)
        .unwrap()
        .set_len(MAX_IMAGE_BYTES as u64)
        .unwrap();
    let selected = source_fixture(source);
    let manager = ArtworkManager::new(directory.path())
        .with_default_image(DEFAULT_IMAGE)
        .with_max_image_bytes(MAX_IMAGE_BYTES);
    let accepted = manager.load(selected.source()).unwrap();
    assert_eq!(
        fs::metadata(file_path(accepted.uri())).unwrap().len(),
        MAX_IMAGE_BYTES as u64
    );
    fs::remove_file(boundary).unwrap();
    assert!(
        ArtworkManager::new(directory.path())
            .with_default_image(DEFAULT_IMAGE)
            .with_max_image_bytes(MAX_IMAGE_BYTES)
            .load(selected.source())
            .is_none()
    );
}

#[test]
fn directory_images_are_path_bound_and_cached_as_immutable_content() {
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first");
    let second = directory.path().join("second");
    fs::create_dir(&first).unwrap();
    fs::create_dir(&second).unwrap();
    let old_bytes = directory_image(&first.join("cover.png"), 1);
    let new_bytes = directory_image(&second.join("cover.png"), 2);
    let mut selected = source_fixture(first.join("missing.wav"));
    let manager = ArtworkManager::new(directory.path()).with_default_image(DEFAULT_IMAGE);
    let original_identity = ArtworkIdentity::new(selected.source());
    // A missing or unprobeable audio file does not hide its directory cover.
    let old = manager.load(selected.source()).unwrap();
    fs::remove_file(first.join("cover.png")).unwrap();
    assert_eq!(fs::read(file_path(old.uri())).unwrap(), old_bytes);
    selected.path = second.join("unprobeable.wav");
    fs::write(&selected.path, b"not audio").unwrap();
    assert!(!original_identity.matches(selected.source()));
    assert!(manager.cached_result(selected.source()).is_none());
    assert!(manager.cached(selected.source()).is_none());
    let new = manager.load(selected.source()).unwrap();
    assert_ne!(new.uri(), old.uri());
    assert_eq!(fs::read(file_path(new.uri())).unwrap(), new_bytes);
    assert_eq!(fs::read(file_path(old.uri())).unwrap(), old_bytes);
    assert_eq!(
        manager.load_with(selected.source(), || panic!("unchanged content reprobed")),
        Some(new)
    );
}

#[test]
fn unfinished_directory_lookup_cannot_complete_a_relocated_request() {
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first");
    let second = directory.path().join("second");
    fs::create_dir(&first).unwrap();
    fs::create_dir(&second).unwrap();
    let old_bytes = directory_image(&first.join("cover.png"), 1);
    let new_bytes = directory_image(&second.join("cover.png"), 2);
    let original = source_fixture(first.join("track.wav"));
    let mut relocated = original.clone();
    relocated.path = second.join("track.wav");
    let manager = ArtworkManager::new(directory.path()).with_default_image(DEFAULT_IMAGE);
    let (started, loading) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let manager = &manager;
        let original = &original;
        let old = scope.spawn(move || {
            manager.load_with(original.source(), || {
                started.send(()).unwrap();
                released.recv().unwrap();
                manager.source_uri(original.source(), manager.max_image_bytes)
            })
        });
        loading.recv().unwrap();
        assert!(manager.cached_result(relocated.source()).is_none());
        assert!(manager.cached(relocated.source()).is_none());
        let new = scope.spawn(|| manager.load(relocated.source()).unwrap());
        release.send(()).unwrap();
        let old = old.join().unwrap().unwrap();
        let new = new.join().unwrap();
        assert_eq!(fs::read(file_path(old.uri())).unwrap(), old_bytes);
        assert_eq!(fs::read(file_path(new.uri())).unwrap(), new_bytes);
        assert_eq!(manager.cached(relocated.source()), Some(new));
    });
}

#[test]
fn concurrent_requests_probe_once_and_share_a_complete_image() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("track.wav");
    tagged_wav(&source);
    let manager = ArtworkManager::new(directory.path()).with_default_image(DEFAULT_IMAGE);
    let selected = source_fixture(source);
    let barrier = std::sync::Barrier::new(4);
    let probes = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    manager
                        .load_with(selected.source(), || {
                            probes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            manager.source_uri(selected.source(), manager.max_image_bytes)
                        })
                        .unwrap()
                })
            })
            .collect();
        for handle in handles {
            let artwork = handle.join().unwrap();
            assert_eq!(fs::read(file_path(artwork.uri())).unwrap(), DEFAULT_IMAGE);
            assert_eq!(manager.cached(selected.source()), Some(artwork));
        }
    });
    assert_eq!(probes.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[test]
fn cue_track_ids_share_positive_and_negative_physical_source_probes() {
    for positive in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let selected = source_fixture(directory.path().join("album.wav"));
        if positive {
            tagged_wav(&selected.path);
        }
        let manager = ArtworkManager::new(directory.path());
        let tracks: Vec<_> = (1..=4)
            .map(|id| TestSource {
                id,
                ..selected.clone()
            })
            .collect();
        let barrier = std::sync::Barrier::new(tracks.len());
        let probes = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            let manager = &manager;
            let barrier = &barrier;
            let probes = &probes;
            let handles: Vec<_> = tracks
                .iter()
                .map(|track| {
                    scope.spawn(move || {
                        barrier.wait();
                        manager.load_with(track.source(), || {
                            probes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            manager.source_uri(track.source(), manager.max_image_bytes)
                        })
                    })
                })
                .collect();
            for (track, handle) in tracks.iter().zip(handles) {
                let result = handle.join().unwrap();
                assert_eq!(result.is_some(), positive);
                assert_eq!(manager.cached_result(track.source()), Some(result));
            }
        });
        assert_eq!(probes.load(std::sync::atomic::Ordering::Relaxed), 1);
    }
}

#[test]
fn one_cue_lease_does_not_pin_idle_logical_aliases() {
    let directory = tempfile::tempdir().unwrap();
    let mut selected = source_fixture(directory.path().join("album.wav"));
    tagged_wav(&selected.path);
    let manager = ArtworkManager::new(directory.path());
    let held = manager.load(selected.source()).unwrap();
    for id in 2..=(MAX_ARTWORK_ENTRIES * 3) as i64 {
        selected.id = id;
        assert_eq!(
            manager.load_with(selected.source(), || panic!("same CUE source reprobed")),
            Some(held.clone())
        );
    }
    selected.id = 1;
    assert_eq!(manager.cached(selected.source()), Some(held));
    selected.id = 2;
    assert!(manager.cached_result(selected.source()).is_none());
    let cache = manager.entries.lock();
    assert_eq!(cache.insertion_order.len(), MAX_ARTWORK_ENTRIES);
    assert_eq!(cache.items.len(), MAX_ARTWORK_ENTRIES);
}

#[test]
fn shared_cue_misses_remain_bounded_and_path_and_version_sensitive() {
    let directory = tempfile::tempdir().unwrap();
    let manager = ArtworkManager::new(directory.path());
    let mut selected = source_fixture(directory.path().join("album.wav"));
    assert!(manager.load(selected.source()).is_none());
    for id in 2..=(MAX_ARTWORK_ENTRIES * 3) as i64 {
        selected.id = id;
        assert!(
            manager
                .load_with(selected.source(), || panic!("same CUE source reprobed"))
                .is_none()
        );
    }
    assert_eq!(
        manager.entries.lock().insertion_order.len(),
        MAX_ARTWORK_ENTRIES
    );
    assert_eq!(manager.entries.lock().items.len(), MAX_ARTWORK_ENTRIES);
    let moved = directory.path().join("moved");
    fs::create_dir(&moved).unwrap();
    let bytes = directory_image(&moved.join("cover.png"), 42);
    selected.path = moved.join("album.wav");
    let relocated = manager.load(selected.source()).unwrap();
    assert_eq!(fs::read(relocated.path()).unwrap(), bytes);
    // Another logical ID shares this directory result, not the original miss.
    selected.id += 1;
    assert_eq!(
        manager.load_with(selected.source(), || panic!("directory reprobed")),
        Some(relocated)
    );
    fs::remove_file(moved.join("cover.png")).unwrap();
    selected.fingerprint = Some("replacement".into());
    assert!(manager.load(selected.source()).is_none());
}

#[cfg(unix)]
#[test]
fn fifo_without_writer_falls_back_and_releases_worker_and_maintenance() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("track.wav");
    let name = std::ffi::CString::new(source.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: name is a live NUL-terminated path, and mkfifo retains no pointer.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let alias = directory.path().join("linked.wav");
    std::os::unix::fs::symlink(&source, &alias).unwrap();
    let manager = ArtworkManager::new(directory.path()).with_default_image(DEFAULT_IMAGE);
    let cache = directory.path().join("artwork/cache/11");
    fs::create_dir_all(&cache).unwrap();
    let expired = cache.join(format!("{}.png", "1".repeat(30)));
    fs::write(&expired, b"old").unwrap();
    let (complete, completed) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut selected = source_fixture(source);
        let direct = result_uri(manager.load(selected.source()));
        assert_eq!(manager.cached_result(selected.source()), Some(None));
        selected.id = 2;
        selected.path = alias;
        let linked = result_uri(manager.load(selected.source()));
        assert_eq!(manager.cached_result(selected.source()), Some(None));
        let report = manager.maintain(Duration::ZERO, true).unwrap();
        complete
            .send((direct, linked, report.removed_files))
            .unwrap();
    });
    // Do not join on failure: a regression must fail rather than hang the suite.
    let (direct, linked, removed) = completed.recv_timeout(Duration::from_secs(2)).unwrap();
    worker.join().unwrap();
    assert!(direct.is_none());
    assert!(linked.is_none());
    assert_eq!(removed, 1);
    assert!(!expired.exists());
}

#[test]
fn cache_only_reads_do_not_wait_for_an_inflight_probe() {
    let directory = tempfile::tempdir().unwrap();
    let manager = ArtworkManager::new(directory.path()).with_default_image(DEFAULT_IMAGE);
    let selected = source_fixture(directory.path().join("missing.wav"));
    let (started, loading) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let (read, reads) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let manager = &manager;
        let selected = &selected;
        let loader = scope.spawn(move || {
            manager.load_with(selected.source(), || {
                started.send(()).unwrap();
                released.recv().unwrap();
                ArtworkSource::Directory(None)
            })
        });
        loading.recv().unwrap();
        scope.spawn(move || read.send(manager.cached_result(selected.source())).unwrap());
        let observed = reads.recv_timeout(Duration::from_secs(2));
        release.send(()).unwrap();
        assert_eq!(observed.unwrap(), None);
        loader.join().unwrap();
    });
    assert_eq!(manager.cached_result(selected.source()), Some(None));
    assert!(manager.cached(selected.source()).is_none());
}

#[test]
fn held_old_version_survives_pressure_and_late_completion_does_not_replace_new() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("track.wav");
    tagged_wav(&source);
    let manager = ArtworkManager::new(directory.path()).with_default_image(DEFAULT_IMAGE);
    let old_track = source_fixture(source.clone());
    let mut current = old_track.clone();
    current.fingerprint = None;
    let (started, loading) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let (old, new) = std::thread::scope(|scope| {
        let manager = &manager;
        let old_track = &old_track;
        let loader = scope.spawn(move || {
            manager.load_with(old_track.source(), || {
                started.send(()).unwrap();
                released.recv().unwrap();
                manager.source_uri(old_track.source(), manager.max_image_bytes)
            })
        });
        loading.recv().unwrap();
        let new = manager.load(current.source()).unwrap();
        release.send(()).unwrap();
        (loader.join().unwrap().unwrap(), new)
    });
    assert_eq!(old.uri(), new.uri());
    assert_ne!(old, new);
    for id in 2..130 {
        let mut other = current.clone();
        other.id = id;
        other.path = directory.path().join("missing");
        manager.load(other.source());
    }
    assert_eq!(manager.cached(old_track.source()), Some(old));
    assert_eq!(manager.cached(current.source()), Some(new));
    assert!(file_path(manager.cached(current.source()).unwrap().uri()).is_file());
}

#[test]
fn held_results_can_exceed_target_and_idle_results_are_evicted() {
    let directory = tempfile::tempdir().unwrap();
    let manager = ArtworkManager::new(directory.path()).with_default_image(DEFAULT_IMAGE);
    let source = directory.path().join("track.wav");
    tagged_wav(&source);
    let mut selected = source_fixture(source);
    let mut held = Vec::new();
    for id in 1..=MAX_ARTWORK_ENTRIES + 2 {
        selected.id = id as i64;
        drop(manager.load(selected.source()).unwrap());
        held.push(manager.cached_result(selected.source()).unwrap().unwrap());
    }
    selected.id = 1;
    assert_eq!(manager.cached(selected.source()), Some(held[0].clone()));
    assert!(manager.cached_result(selected.source()).is_some());
    drop(held);
    selected.id = 1000;
    manager.load(selected.source());
    selected.id = 1;
    assert!(manager.cached_result(selected.source()).is_none());
    selected.id = (MAX_ARTWORK_ENTRIES + 2) as i64;
    assert!(manager.cached_result(selected.source()).is_some());
}

#[test]
fn manual_cleanup_respects_age_and_regenerates_after_manager_restart() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("track.wav");
    tagged_wav(&source);
    let manager = ArtworkManager::new(directory.path()).with_default_image(DEFAULT_IMAGE);
    let selected = source_fixture(source);
    let held = manager.load(selected.source()).unwrap();
    let original = file_path(held.uri());
    age_file(&original);
    let cache = original.parent().unwrap();
    let expired = cache.join(format!("{}.png", "1".repeat(30)));
    let fresh = cache.join(format!("{}.png", "2".repeat(30)));
    fs::write(&expired, b"old").unwrap();
    fs::write(&fresh, b"fresh").unwrap();
    age_file(&expired);
    let unmanaged = cache.join("notes.txt");
    fs::write(&unmanaged, b"keep").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&unmanaged, cache.join(format!("{}.png", "3".repeat(30)))).unwrap();
    let age = Duration::from_secs(86_400);
    let report = manager.maintain(age, false).unwrap();
    assert_eq!(report.files, 3);
    assert_eq!(report.bytes, DEFAULT_IMAGE.len() as u64 + 8);
    assert_eq!((report.expired_files, report.removed_files), (2, 0));
    let report = manager.maintain(age, true).unwrap();
    assert_eq!(
        (
            report.removed_files,
            report.removed_bytes,
            report.failed_files
        ),
        (2, DEFAULT_IMAGE.len() as u64 + 3, 0)
    );
    assert!(!original.exists());
    assert!(fresh.is_file());
    assert!(unmanaged.is_file());
    assert!(!expired.exists());
    assert_eq!(manager.cached(selected.source()), Some(held.clone()));
    drop(held);
    drop(manager);
    let restarted = ArtworkManager::new(directory.path());
    let reloaded = restarted.load(selected.source()).unwrap();
    assert_eq!(file_path(reloaded.uri()), original);
    assert_eq!(fs::read(original).unwrap(), DEFAULT_IMAGE);
}

#[test]
fn cleanup_removes_resident_originals_and_all_thumbnail_tiers() {
    let directory = tempfile::tempdir().unwrap();
    let manager = ArtworkManager::new(directory.path()).with_default_image(DEFAULT_IMAGE);
    let original = manager.export_default().unwrap();
    let source = original.path();
    let hash = blake3::hash(DEFAULT_IMAGE);
    let thumbnails = [200, 256, 400, 560, 512, 1024]
        .map(|tier| cache_paths::thumbnail_path(&source, hash, tier).unwrap());
    for thumbnail in &thumbnails {
        fs::write(thumbnail, b"thumbnail").unwrap();
    }
    age_file(&source);
    for thumbnail in &thumbnails {
        age_file(thumbnail);
    }
    let report = manager.maintain(Duration::ZERO, true).unwrap();
    assert_eq!(report.files, 7);
    assert_eq!(report.removed_files, 7);
    assert!(!source.exists());
    assert!(thumbnails.iter().all(|path| !path.exists()));
}

#[test]
fn maintenance_excludes_unmanaged_names_symlinks_and_nested_directories() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("artwork/cache");
    let bucket = root.join("ab");
    fs::create_dir_all(&bucket).unwrap();
    let short_hash = "1".repeat(30);
    let mut excluded = vec![
        root.join(format!("{short_hash}.png")),
        root.join(format!("{short_hash}-256.webp")),
        bucket.join(format!("{}.png", "2".repeat(29))),
        bucket.join(format!("{}.png", "2".repeat(31))),
        bucket.join(format!("{}{}.png", "A", "1".repeat(29))),
        bucket.join(format!("{short_hash}-128.webp")),
        bucket.join(format!("{short_hash}-2048.webp")),
        bucket.join(format!("{short_hash}-0256.webp")),
        bucket.join(format!("{short_hash}-256.png")),
        bucket.join(format!("{short_hash}-256-extra.webp")),
        bucket.join(format!("{short_hash}.PNG")),
        bucket.join("notes.txt"),
    ];
    for name in ["AB", "a", "abc", "g1", "unmanaged"] {
        let unmanaged = root.join(name);
        fs::create_dir_all(&unmanaged).unwrap();
        excluded.push(unmanaged.join(format!("{short_hash}.png")));
    }
    let nested = bucket.join("cd");
    fs::create_dir(&nested).unwrap();
    excluded.push(nested.join(format!("{short_hash}.png")));
    let named_directory = bucket.join(format!("{short_hash}.png"));
    fs::create_dir(&named_directory).unwrap();
    excluded.push(named_directory.join(format!("{short_hash}.png")));
    for path in &excluded {
        fs::write(path, b"keep").unwrap();
    }
    #[cfg(unix)]
    {
        let outside = directory.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let external = outside.join(format!("{short_hash}.png"));
        fs::write(&external, b"keep").unwrap();
        excluded.push(external.clone());
        std::os::unix::fs::symlink(&outside, root.join("ef")).unwrap();
        std::os::unix::fs::symlink(&external, bucket.join(format!("{short_hash}-512.webp")))
            .unwrap();
    }
    let managed = bucket.join(format!("{short_hash}-256.webp"));
    fs::write(&managed, b"remove").unwrap();
    let manager = ArtworkManager::new(directory.path()).with_default_image(DEFAULT_IMAGE);
    let inventory = manager.maintain(Duration::ZERO, false).unwrap();
    assert_eq!((inventory.files, inventory.bytes), (1, 6));
    assert!(managed.is_file());
    let cleaned = manager.maintain(Duration::ZERO, true).unwrap();
    assert_eq!(
        (cleaned.files, cleaned.removed_files, cleaned.failed_files),
        (1, 1, 0)
    );
    assert!(!managed.exists());
    for path in excluded {
        assert_eq!(fs::read(path).unwrap(), b"keep");
    }
}

#[test]
fn failures_are_cached_and_manual_directory_errors_are_reported() {
    let directory = tempfile::tempdir().unwrap();
    let manager = ArtworkManager::new(directory.path()).with_default_image(DEFAULT_IMAGE);
    let missing = source_fixture(directory.path().join("missing"));
    assert!(manager.load(missing.source()).is_none());
    assert!(
        manager
            .load_with(missing.source(), || panic!("cached failure reprobed"))
            .is_none()
    );
    assert_eq!(manager.cached_result(missing.source()), Some(None));
    assert_eq!(manager.maintain(Duration::ZERO, false).unwrap().files, 0);
    fs::create_dir_all(directory.path().join("artwork")).unwrap();
    fs::write(directory.path().join("artwork/cache"), b"blocked").unwrap();
    assert!(manager.maintain(Duration::ZERO, false).is_err());
    let blocked = directory.path().join("blocked");
    fs::write(&blocked, b"not a directory").unwrap();
    let unavailable = ArtworkManager::new(&blocked).with_default_image(DEFAULT_IMAGE);
    assert_eq!(unavailable.load(missing.source()), None);
    assert_eq!(unavailable.cached_result(missing.source()), Some(None));
}
