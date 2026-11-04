use super::*;
use std::{
    fs,
    io::{Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    process::{Child, Command},
    time::Duration,
};

const NATIVE_LAYOUT: [u32; 6] = [17, 0, 5, 3, 2, 1];

fn directory() -> tempfile::TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.cache");
    fs::create_dir_all(&root).unwrap();
    tempfile::Builder::new()
        .prefix("pcm-test-")
        .tempdir_in(root)
        .unwrap()
}

fn header(position: u64) -> PcmHeader {
    PcmHeader {
        sequence: position,
        epoch: 10,
        intent: 9,
        generation: 4,
        capture: 3,
        start_frame: position * PCM_BLOCK_FRAMES as u64,
        output_frame: 8192 + position * PCM_BLOCK_FRAMES as u64,
        sample_rate: 96000,
        gap: position == 77,
    }
}

fn sample(frame: usize, channel: usize) -> f32 {
    let value = (frame * 100 + channel) as f32 + 0.25;
    if channel.is_multiple_of(2) {
        value
    } else {
        -value
    }
}

fn samples(frames: usize, channels: usize) -> impl ExactSizeIterator<Item = f32> {
    (0..frames * channels).map(move |index| sample(index / channels, index % channels))
}

fn assert_native(
    block: &SharedPcmBlock<'_>,
    expected_header: PcmHeader,
    layout: &[u32],
    frames: usize,
) {
    assert_eq!(block.header(), expected_header);
    assert_eq!(block.layout(), layout);
    assert_eq!(block.len(), frames);
    for frame in 0..frames {
        for channel in 0..layout.len() {
            assert_eq!(
                block.sample(frame, channel).to_bits(),
                sample(frame, channel).to_bits()
            );
        }
    }
    assert!(block.still_current());
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn cross_process_child() {
    let Some(data_dir) = std::env::var_os("RIVU_PCM_TEST_CHILD") else {
        return;
    };
    let path = PathBuf::from(std::env::var_os("RIVU_PCM_TEST_PATH").unwrap());
    let window = SharedPcmWindow::open(&path).unwrap();
    assert_eq!(window.sequence(), 0);
    let mut signal = UnixStream::connect(Path::new(&data_dir).join("ready.sock")).unwrap();
    signal
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    signal.write_all(&[1]).unwrap();
    let mut byte = [0];
    signal.read_exact(&mut byte).unwrap();
    let mut expected = header(77);
    expected.sequence = 1;
    assert_eq!(window.sequence(), 1);
    let block = window.block(1).unwrap();
    assert_native(&block, expected, &NATIVE_LAYOUT, PCM_BLOCK_FRAMES);
    signal.write_all(&[2]).unwrap();
    signal.read_exact(&mut byte).unwrap();
    // The owner closed its advertised fd, but this mapping and its borrowed
    // accessor retain the shared memory without either process pinning a slot.
    assert!(SharedPcmWindow::open(&path).is_err());
    assert_native(&block, expected, &NATIVE_LAYOUT, PCM_BLOCK_FRAMES);
    signal.write_all(&[3]).unwrap();
}

#[test]
fn independent_process_native_pcm_survives_owner_drop() {
    let data = directory();
    let listener = UnixListener::bind(data.path().join("ready.sock")).unwrap();
    let window = SharedPcmWindow::new().unwrap();
    let path = window.share_path();
    let mut child = ChildGuard(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::cross_process_child", "--nocapture"])
            .env("RIVU_PCM_TEST_CHILD", data.path())
            .env("RIVU_PCM_TEST_PATH", &path)
            .spawn()
            .unwrap(),
    );
    let mut ready = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut ready, 1, 10_000) }, 1);
    let (mut signal, _) = listener.accept().unwrap();
    signal
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut byte = [0];
    signal.read_exact(&mut byte).unwrap();
    assert!(window.publish(
        header(77),
        &NATIVE_LAYOUT,
        samples(PCM_BLOCK_FRAMES, NATIVE_LAYOUT.len())
    ));
    signal.write_all(&[1]).unwrap();
    signal.read_exact(&mut byte).unwrap();
    drop(window);
    signal.write_all(&[2]).unwrap();
    signal.read_exact(&mut byte).unwrap();
    assert!(child.0.wait().unwrap().success());
}

#[test]
fn overflow_retains_exact_bounded_ordered_suffix_for_independent_readers() {
    let owner = SharedPcmWindow::new().unwrap();
    let reader = SharedPcmWindow::open(&owner.share_path()).unwrap();
    assert_eq!(owner.sequence(), 0);
    assert!(owner.block(0).is_none());
    assert!(reader.block(1).is_none());
    for sequence in 1..=(PCM_QUEUE_BLOCKS * 3) as u64 {
        assert!(owner.publish(
            header(sequence),
            &NATIVE_LAYOUT,
            samples(PCM_BLOCK_FRAMES, NATIVE_LAYOUT.len())
        ));
    }
    assert_eq!(reader.sequence(), (PCM_QUEUE_BLOCKS * 3) as u64);
    for sequence in 1..=(PCM_QUEUE_BLOCKS * 2) as u64 {
        assert!(reader.block(sequence).is_none());
    }
    for sequence in (PCM_QUEUE_BLOCKS * 2 + 1) as u64..=(PCM_QUEUE_BLOCKS * 3) as u64 {
        let block = reader.block(sequence).unwrap();
        assert_native(&block, header(sequence), &NATIVE_LAYOUT, PCM_BLOCK_FRAMES);
    }
    // A late reader has no shared cursor and can inspect the retained
    // publications in reverse order without consuming them for other readers.
    let late = SharedPcmWindow::open(&owner.share_path()).unwrap();
    for sequence in [
        (PCM_QUEUE_BLOCKS * 3) as u64,
        (PCM_QUEUE_BLOCKS * 2 + 1) as u64,
    ] {
        assert_native(
            &late.block(sequence).unwrap(),
            header(sequence),
            &NATIVE_LAYOUT,
            PCM_BLOCK_FRAMES,
        );
        assert!(reader.block(sequence).is_some());
    }
    assert!(late.block((PCM_QUEUE_BLOCKS * 3 + 1) as u64).is_none());
}

#[test]
fn borrowed_accessor_neither_copies_pcm_nor_prevents_overwrite() {
    let owner = SharedPcmWindow::new().unwrap();
    let reader = SharedPcmWindow::open(&owner.share_path()).unwrap();
    assert!(owner.publish(header(77), &[0, 1], [0.25, -0.5, 0.75, -1.0].into_iter()));
    let borrowed = reader.block(1).unwrap();
    let original_header = borrowed.header();
    assert_eq!(borrowed.sample(0, 0), 0.25);
    for sequence in 2..=PCM_QUEUE_BLOCKS as u64 {
        assert!(owner.publish(header(sequence), &[0], [0.0].into_iter()));
    }
    assert!(borrowed.still_current());
    assert!(owner.publish(header(88), &[17, 5, 0], [9.0, 8.0, 7.0].into_iter()));
    let replacement_sequence = PCM_QUEUE_BLOCKS as u64 + 1;
    assert!(reader.block(1).is_none());
    assert!(!borrowed.still_current());
    // Small metadata remains a stable snapshot; PCM is a direct atomic load from
    // the reused slot, even when its replacement has a different native layout.
    assert_eq!(borrowed.header(), original_header);
    assert_eq!(borrowed.layout(), [0, 1]);
    assert_eq!(borrowed.len(), 2);
    assert_eq!(borrowed.sample(0, 0), 9.0);
    assert_eq!(borrowed.sample(0, 1), 8.0);
    let replacement = reader.block(replacement_sequence).unwrap();
    assert_eq!(replacement.layout(), [17, 5, 0]);
    assert_eq!(replacement.len(), 1);
    assert_eq!(replacement.sample(0, 2), 7.0);
}

#[test]
fn full_native_layout_preserves_order_and_every_float_bit() {
    let owner = SharedPcmWindow::new().unwrap();
    let layout: [u32; MAX_PCM_CHANNELS] =
        std::array::from_fn(|index| (MAX_PCM_CHANNELS - 1 - index) as u32);
    let bits = [
        0x0000_0000,
        0x8000_0000,
        0x7fc0_1234,
        0xff80_0000,
        0x7f80_0000,
        0x0000_0001,
    ];
    assert!(
        owner.publish(
            header(77),
            &layout,
            (0..PCM_BLOCK_FRAMES * MAX_PCM_CHANNELS)
                .map(|index| f32::from_bits(bits[index % bits.len()])),
        )
    );
    let block = owner.block(1).unwrap();
    assert_eq!(block.layout(), layout);
    assert_eq!(block.len(), PCM_BLOCK_FRAMES);
    for frame in 0..PCM_BLOCK_FRAMES {
        for channel in 0..MAX_PCM_CHANNELS {
            assert_eq!(
                block.sample(frame, channel).to_bits(),
                bits[(frame * MAX_PCM_CHANNELS + channel) % bits.len()]
            );
        }
    }
}

#[test]
fn invalid_publications_leave_latest_completed_block_unchanged() {
    let owner = SharedPcmWindow::new().unwrap();
    assert!(owner.publish(header(1), &[0], [0.25].into_iter()));
    for layout in [
        &[][..],
        &[0, 0][..],
        &[18][..],
        &[0; MAX_PCM_CHANNELS + 1][..],
    ] {
        assert!(!owner.publish(header(2), layout, [0.5].into_iter()));
    }
    assert!(!owner.publish(header(2), &[0, 1], [0.5; 3].into_iter()));
    assert!(!owner.publish(header(2), &[0], std::iter::empty()));
    assert!(!owner.publish(
        header(2),
        &[0],
        std::iter::repeat_n(0.5, PCM_BLOCK_FRAMES + 1)
    ));
    let mut invalid = header(2);
    invalid.sample_rate = 0;
    assert!(!owner.publish(invalid, &[0], [0.5].into_iter()));
    invalid = header(2);
    invalid.epoch = 11;
    assert!(!owner.publish(invalid, &[0], [0.5].into_iter()));
    assert_eq!(owner.sequence(), 1);
    assert_eq!(owner.block(1).unwrap().sample(0, 0), 0.25);
    assert!(owner.block(2).is_none());
}

#[test]
fn capture_and_output_frame_ranges_must_both_remain_representable() {
    let owner = SharedPcmWindow::new().unwrap();
    let mut last = header(1);
    last.start_frame = u64::MAX - 1;
    last.output_frame = u64::MAX - 1;
    assert!(owner.publish(last, &[17], [0.25].into_iter()));
    assert_eq!(owner.block(1).unwrap().header(), last);
    assert!(!owner.publish(last, &[17], [0.25, 0.5].into_iter()));
    last.start_frame = u64::MAX;
    assert!(!owner.publish(last, &[17], [0.25].into_iter()));
    last.start_frame -= 1;
    last.output_frame = u64::MAX;
    assert!(!owner.publish(last, &[17], [0.25].into_iter()));
}

#[test]
fn reentrant_publisher_fails_immediately_without_affecting_outer_publication() {
    let owner = SharedPcmWindow::new().unwrap();
    assert!(owner.publish(
        header(7),
        &[0, 1],
        (0..4).map(|index| {
            assert!(!owner.publish(header(8), &[0], [99.0].into_iter()));
            index as f32
        })
    ));
    assert_eq!(owner.sequence(), 1);
    let block = owner.block(1).unwrap();
    assert_eq!(block.header().start_frame, header(7).start_frame);
    assert_eq!(block.sample(1, 1), 3.0);
    assert!(owner.publish(header(8), &[0], [99.0].into_iter()));
    assert_eq!(owner.sequence(), 2);
}

#[test]
fn iterator_panic_releases_publisher_without_exposing_partial_block() {
    let owner = SharedPcmWindow::new().unwrap();
    let result = std::panic::catch_unwind(|| {
        owner.publish(
            header(1),
            &[0],
            (0..4).map(|index| {
                assert!(index != 2, "interrupted sample iterator");
                index as f32
            }),
        );
    });
    assert!(result.is_err());
    assert_eq!(owner.sequence(), 0);
    assert!(owner.block(1).is_none());
    assert!(owner.publish(header(7), &[0], [0.75].into_iter()));
    let block = owner.block(1).unwrap();
    assert_eq!(block.header().start_frame, header(7).start_frame);
    assert_eq!(block.sample(0, 0), 0.75);
}

#[test]
fn reader_rejects_malformed_and_in_progress_metadata() {
    let owner = SharedPcmWindow::new().unwrap();
    let reader = SharedPcmWindow::open(&owner.share_path()).unwrap();
    assert!(owner.publish(header(1), &[0, 1], samples(2, 2)));
    let slot = &owner.storage().blocks[0];
    let revision = slot.revision.load(Ordering::Relaxed);
    slot.revision.store(revision + 1, Ordering::Release);
    assert!(reader.block(1).is_none());
    slot.revision.store(revision, Ordering::Release);
    for (field, bad) in [
        (&slot.frames, 0),
        (&slot.frames, PCM_BLOCK_FRAMES as u32 + 1),
        (&slot.channels, 0),
        (&slot.channels, MAX_PCM_CHANNELS as u32 + 1),
        (&slot.layout[0], 18),
        (&slot.layout[1], 0),
        (&slot.gap, 2),
        (&slot.sample_rate, 0),
    ] {
        let original = field.swap(bad, Ordering::Relaxed);
        assert!(reader.block(1).is_none());
        field.store(original, Ordering::Relaxed);
    }
    for (field, bad) in [
        (&slot.sequence, 2),
        (&slot.epoch, 11),
        (&slot.start_frame, u64::MAX),
        (&slot.output_frame, u64::MAX),
    ] {
        let original = field.swap(bad, Ordering::Relaxed);
        assert!(reader.block(1).is_none());
        field.store(original, Ordering::Relaxed);
    }
    assert_native(&reader.block(1).unwrap(), header(1), &[0, 1], 2);
}

fn memfd(bytes: usize) -> File {
    let fd = unsafe {
        libc::memfd_create(
            c"rivu-pcm-test".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    assert!(fd >= 0);
    let file = unsafe { File::from_raw_fd(fd) };
    file.set_len(bytes as u64).unwrap();
    file
}

fn file_path(file: &File) -> PathBuf {
    PathBuf::from(format!(
        "/proc/{}/fd/{}",
        std::process::id(),
        file.as_raw_fd()
    ))
}

#[test]
fn opening_checks_seals_exact_file_length_and_window_metadata() {
    let data = directory();
    let ordinary = data.path().join("ordinary");
    File::create(&ordinary)
        .unwrap()
        .set_len(size_of::<Storage>() as u64)
        .unwrap();
    assert!(SharedPcmWindow::open(&ordinary).is_err());
    let unsealed = memfd(size_of::<Storage>());
    assert!(SharedPcmWindow::open(&file_path(&unsealed)).is_err());
    for bytes in [size_of::<Storage>() - 1, size_of::<Storage>() + 1] {
        let wrong_size = memfd(bytes);
        assert_eq!(
            unsafe { libc::fcntl(wrong_size.as_raw_fd(), libc::F_ADD_SEALS, REQUIRED_SEALS) },
            0
        );
        assert!(SharedPcmWindow::open(&file_path(&wrong_size)).is_err());
    }
    let zeroed = memfd(size_of::<Storage>());
    assert_eq!(
        unsafe { libc::fcntl(zeroed.as_raw_fd(), libc::F_ADD_SEALS, REQUIRED_SEALS) },
        0
    );
    assert!(SharedPcmWindow::open(&file_path(&zeroed)).is_err());
    let owner = SharedPcmWindow::new().unwrap();
    let storage = owner.storage();
    for field in [&storage.magic, &storage.bytes] {
        let original = field.swap(0, Ordering::Relaxed);
        assert!(SharedPcmWindow::open(&owner.share_path()).is_err());
        field.store(original, Ordering::Relaxed);
    }
    for field in [
        &storage.version,
        &storage.slots,
        &storage.block_frames,
        &storage.max_channels,
    ] {
        let original = field.swap(0, Ordering::Relaxed);
        assert!(SharedPcmWindow::open(&owner.share_path()).is_err());
        field.store(original, Ordering::Relaxed);
    }
    assert!(SharedPcmWindow::open(&owner.share_path()).is_ok());
}

#[test]
fn sealed_owner_cannot_resize_and_readers_cannot_create_writable_mappings() {
    let owner = SharedPcmWindow::new().unwrap();
    assert!(owner.file.set_len(size_of::<Storage>() as u64 - 1).is_err());
    assert!(owner.file.set_len(size_of::<Storage>() as u64 + 1).is_err());
    let reader = SharedPcmWindow::open(&owner.share_path()).unwrap();
    assert!(!reader.publish(header(1), &[0], [0.5].into_iter()));
    let reopened = File::options()
        .read(true)
        .write(true)
        .open(owner.share_path())
        .unwrap();
    let address = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size_of::<Storage>(),
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            reopened.as_raw_fd(),
            0,
        )
    };
    if address != libc::MAP_FAILED {
        unsafe { libc::munmap(address, size_of::<Storage>()) };
        panic!("sealed PCM window accepted a new writable mapping");
    }
    assert!(owner.publish(header(1), &[0], [0.5].into_iter()));
}
