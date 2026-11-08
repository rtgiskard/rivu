use super::*;
use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread::JoinHandle,
};

const CHILD_EVENT: &str = "RIVU_ARTWORK_EVENT:";

// Run through the real manager API in a separate process. Pipe acknowledgments
// make ownership changes deterministic, without timing sleeps or lease polling.
struct ForeignArtwork {
    child: Child,
    events: mpsc::Receiver<String>,
    output: Option<JoinHandle<()>>,
}

impl ForeignArtwork {
    fn spawn(directory: &Path, kind: &str) -> Self {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "artwork::tests::leases::foreign_artwork_process",
                "--nocapture",
            ])
            .env("RIVU_ARTWORK_CHILD_DIRECTORY", directory)
            .env("RIVU_ARTWORK_CHILD_KIND", kind)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (events, receiver) = mpsc::channel();
        let output = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Some((_, event)) = line.split_once(CHILD_EVENT) {
                    let _ = events.send(event.to_owned());
                }
            }
        });
        let foreign = Self {
            child,
            events: receiver,
            output: Some(output),
        };
        foreign.expect("ready");
        foreign
    }

    fn expect(&self, event: &str) {
        assert_eq!(
            self.events.recv_timeout(Duration::from_secs(10)).unwrap(),
            event
        );
    }

    fn command(&mut self, command: &str) {
        writeln!(self.child.stdin.as_mut().unwrap(), "{command}").unwrap();
        self.expect(command);
    }

    fn release(&mut self, crash: bool) {
        if crash {
            self.child.kill().unwrap();
            assert!(!self.child.wait().unwrap().success());
        } else {
            self.command("release");
            assert!(self.child.wait().unwrap().success());
        }
    }
}

impl Drop for ForeignArtwork {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(output) = self.output.take() {
            let _ = output.join();
        }
    }
}

fn child_event(event: &str) {
    println!("{CHILD_EVENT}{event}");
    io::stdout().flush().unwrap();
}

#[test]
fn foreign_artwork_process() {
    let Some(directory) = std::env::var_os("RIVU_ARTWORK_CHILD_DIRECTORY") else {
        return;
    };
    let directory = PathBuf::from(directory);
    let manager = ArtworkManager::new(&directory).with_default_image(DEFAULT_IMAGE);
    let selected = source_fixture(directory.join("track.wav"));
    let artwork = match std::env::var("RIVU_ARTWORK_CHILD_KIND").unwrap().as_str() {
        "fallback" => manager.export_default().unwrap(),
        "cover" => manager.load(selected.source()).unwrap(),
        _ => panic!("unknown foreign artwork kind"),
    };
    let mut manager = Some(manager);
    let mut artwork = Some(artwork);
    child_event("ready");
    for command in io::stdin().lock().lines() {
        let command = command.unwrap();
        match command.as_str() {
            "resident" => drop(artwork.take()),
            "leased" => drop(manager.take()),
            "release" => {
                drop(artwork.take());
                drop(manager.take());
                child_event("release");
                return;
            }
            _ => panic!("unknown foreign artwork command"),
        }
        child_event(&command);
    }
}

#[test]
fn foreign_process_originals_survive_cleanup_until_release_or_process_exit() {
    for kind in ["fallback", "cover"] {
        for owner in ["resident", "leased"] {
            for crash in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                tagged_wav(&directory.path().join("track.wav"));
                let mut foreign = ForeignArtwork::spawn(directory.path(), kind);
                foreign.command(owner);
                let path = cache_paths::original_path(
                    &directory.path().join("artwork/cache"),
                    blake3::hash(DEFAULT_IMAGE),
                    "png",
                );
                let thumbnail =
                    cache_paths::thumbnail_path(&path, blake3::hash(DEFAULT_IMAGE), 256).unwrap();
                fs::write(&thumbnail, b"thumbnail").unwrap();
                let cleaner = ArtworkManager::new(directory.path());
                let inventory = cleaner.maintain(Duration::ZERO, false).unwrap();
                assert_eq!(
                    (
                        inventory.files,
                        inventory.protected_files,
                        inventory.removed_files
                    ),
                    (2, 1, 0),
                    "{kind}, {owner}, crash={crash}"
                );
                let report = cleaner.maintain(Duration::ZERO, true).unwrap();
                assert_eq!(
                    (report.files, report.protected_files, report.removed_files),
                    (2, 1, 1),
                    "{kind}, {owner}, crash={crash}"
                );
                assert_eq!(fs::read(&path).unwrap(), DEFAULT_IMAGE);
                assert!(!thumbnail.exists());
                foreign.release(crash);
                let report = cleaner.maintain(Duration::ZERO, true).unwrap();
                assert_eq!(
                    (
                        report.protected_files,
                        report.removed_files,
                        report.failed_files
                    ),
                    (0, 1, 0),
                    "{kind}, {owner}, crash={crash}"
                );
                assert!(!path.exists());
            }
        }
    }
}

#[test]
fn independent_manager_residency_and_detached_leases_protect_both_original_sources() {
    for fallback in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("track.wav");
        tagged_wav(&source);
        let selected = source_fixture(source);
        let manager = ArtworkManager::new(directory.path()).with_default_image(DEFAULT_IMAGE);
        let cleaner = ArtworkManager::new(directory.path());
        let image = if fallback {
            manager.export_default().unwrap()
        } else {
            manager.load(selected.source()).unwrap()
        };
        let path = image.path();
        drop(image);
        assert_eq!(
            cleaner
                .maintain(Duration::ZERO, true)
                .unwrap()
                .protected_files,
            1
        );
        let held = if fallback {
            manager.cached_default().unwrap()
        } else {
            manager.cached(selected.source()).unwrap()
        };
        drop(manager);
        assert_eq!(
            cleaner
                .maintain(Duration::ZERO, true)
                .unwrap()
                .protected_files,
            1
        );
        assert_eq!(fs::read(&path).unwrap(), DEFAULT_IMAGE);
        drop(held);
        assert_eq!(
            cleaner
                .maintain(Duration::ZERO, true)
                .unwrap()
                .removed_files,
            1
        );
        assert!(!path.exists());
    }
}

#[test]
fn atomic_publication_is_protected_before_artwork_registration() {
    let directory = tempfile::tempdir().unwrap();
    let manager = ArtworkManager::new(directory.path()).with_default_image(DEFAULT_IMAGE);
    let cleaner = ArtworkManager::new(directory.path());
    let (sent, observed) = mpsc::channel();
    let _event = on_file_event(FileEvent::Published, move |path| {
        assert_eq!(fs::read(path).unwrap(), DEFAULT_IMAGE);
        sent.send(cleaner.maintain(Duration::ZERO, true).unwrap())
            .unwrap();
    });
    let image = manager.export_default().unwrap();
    let report = observed.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!((report.protected_files, report.removed_files), (1, 0));
    assert_eq!(fs::read(image.path()).unwrap(), DEFAULT_IMAGE);
}

#[test]
fn acquisition_republishes_if_cleanup_unlinks_the_opened_original_before_locking() {
    let directory = tempfile::tempdir().unwrap();
    let path = cache_paths::original_path(
        &directory.path().join("artwork/cache"),
        blake3::hash(DEFAULT_IMAGE),
        "png",
    );
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, DEFAULT_IMAGE).unwrap();
    let manager = ArtworkManager::new(directory.path()).with_default_image(DEFAULT_IMAGE);
    let cleaner = ArtworkManager::new(directory.path());
    let (sent, observed) = mpsc::channel();
    let _event = on_file_event(FileEvent::ReuseOpened, move |_| {
        sent.send(cleaner.maintain(Duration::ZERO, true).unwrap())
            .unwrap();
    });
    let image = manager.export_default().unwrap();
    let report = observed.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!((report.protected_files, report.removed_files), (0, 1));
    assert_eq!(image.path().as_ref(), path.as_path());
    assert_eq!(fs::read(image.path()).unwrap(), DEFAULT_IMAGE);
    assert_eq!(
        ArtworkManager::new(directory.path())
            .maintain(Duration::ZERO, true)
            .unwrap()
            .protected_files,
        1
    );
}

#[test]
fn cleanup_revalidates_the_locked_inode_before_unlinking_a_republished_name() {
    let directory = tempfile::tempdir().unwrap();
    let path = cache_paths::original_path(
        &directory.path().join("artwork/cache"),
        blake3::hash(DEFAULT_IMAGE),
        "png",
    );
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, DEFAULT_IMAGE).unwrap();
    let publisher = ArtworkManager::new(directory.path()).with_default_image(DEFAULT_IMAGE);
    let cleaner = ArtworkManager::new(directory.path());
    let (sent, observed) = mpsc::channel();
    let _event = on_file_event(FileEvent::CleanupLocked, move |path| {
        fs::remove_file(path).unwrap();
        sent.send(publisher.export_default().unwrap()).unwrap();
    });
    let report = cleaner.maintain(Duration::ZERO, true).unwrap();
    let image = observed.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!((report.removed_files, report.failed_files), (0, 0));
    assert_eq!(fs::read(image.path()).unwrap(), DEFAULT_IMAGE);
    assert_eq!(
        cleaner
            .maintain(Duration::ZERO, true)
            .unwrap()
            .protected_files,
        1
    );
}
