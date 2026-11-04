//! Core-owned, bounded native-channel PCM shared memory.
//!
//! The owner allocates and faults in the entire window before capture. Readers map
//! it read-only and borrow atomic samples directly: they never pin a slot or make
//! the publisher wait. Metadata is a validated snapshot, but samples may be
//! replaced while a reader uses them. This is deliberately best-effort observation,
//! not a lossless queue or an A/V synchronization mechanism.
use anyhow::{Context, Result, ensure};
use rivu_protocol::pcm::{PCM_BLOCK_FRAMES, PcmHeader};
use std::{
    fs::File,
    io,
    mem::size_of,
    os::fd::{AsRawFd, FromRawFd},
    path::{Path, PathBuf},
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering, fence},
};

pub const MAX_CLIENTS: usize = 16;
pub const PCM_QUEUE_BLOCKS: usize = 32;
pub const MAX_PCM_CHANNELS: usize = 18;
const MAGIC: u64 = u64::from_le_bytes(*b"RIVUPCM4");
const VERSION: u32 = 4;
const REQUIRED_SEALS: libc::c_int =
    libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_FUTURE_WRITE | libc::F_SEAL_SEAL;

#[repr(C, align(64))]
struct Storage {
    magic: AtomicU64,
    bytes: AtomicU64,
    version: AtomicU32,
    slots: AtomicU32,
    block_frames: AtomicU32,
    max_channels: AtomicU32,
    sequence: AtomicU64,
    blocks: [Slot; PCM_QUEUE_BLOCKS],
}

#[repr(C, align(64))]
struct Slot {
    revision: AtomicU64,
    sequence: AtomicU64,
    epoch: AtomicU64,
    intent: AtomicU64,
    generation: AtomicU64,
    capture: AtomicU64,
    start_frame: AtomicU64,
    output_frame: AtomicU64,
    sample_rate: AtomicU32,
    gap: AtomicU32,
    frames: AtomicU32,
    channels: AtomicU32,
    layout: [AtomicU32; MAX_PCM_CHANNELS],
    samples: [AtomicU32; PCM_BLOCK_FRAMES * MAX_PCM_CHANNELS],
}

/// A fixed shared PCM window, with exactly one writable owner.
///
/// `open` creates a read-only handle. Dropping the owner closes its advertised
/// `/proc/<pid>/fd/<fd>` path; mappings already opened by readers remain valid.
/// Neither publishing nor accessing a block allocates or waits for a reader.
pub struct SharedPcmWindow {
    storage: NonNull<Storage>,
    file: File,
    writable: bool,
    publishing: AtomicBool,
}

// SAFETY: the mapping stays alive until Drop and its size cannot change after
// sealing. All concurrently accessed mapped fields are atomics; the local flag
// admits only one publisher, and read-only handles never write mapped memory.
unsafe impl Send for SharedPcmWindow {}
unsafe impl Sync for SharedPcmWindow {}

impl SharedPcmWindow {
    pub fn new() -> Result<Self> {
        // SAFETY: the name is NUL-terminated and memfd_create retains no pointer.
        let fd = unsafe {
            libc::memfd_create(
                c"rivu-pcm-v4".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        ensure!(
            fd >= 0,
            "Creating PCM memfd: {}",
            io::Error::last_os_error()
        );
        // SAFETY: memfd_create returned a new owned file descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        file.set_len(size_of::<Storage>() as u64)
            .context("Sizing PCM window")?;
        let window = Self::map(file, true)?;
        // SAFETY: this is a fresh, exclusively owned mapping of the exact size.
        // Zero is a valid representation for every atomic integer in Storage.
        // Touch every byte now, allocating/faulting all pages before capture.
        unsafe {
            window
                .storage
                .as_ptr()
                .cast::<u8>()
                .write_bytes(0, size_of::<Storage>());
        }
        let storage = window.storage();
        storage
            .bytes
            .store(size_of::<Storage>() as u64, Ordering::Relaxed);
        storage.version.store(VERSION, Ordering::Relaxed);
        storage
            .slots
            .store(PCM_QUEUE_BLOCKS as u32, Ordering::Relaxed);
        storage
            .block_frames
            .store(PCM_BLOCK_FRAMES as u32, Ordering::Relaxed);
        storage
            .max_channels
            .store(MAX_PCM_CHANNELS as u32, Ordering::Relaxed);
        storage.magic.store(MAGIC, Ordering::Release);
        // FUTURE_WRITE permits this already-established owner mapping, but
        // prevents writable mappings or writes through subsequently opened fds.
        // SHRINK/GROW protect every mapped access against file-size changes.
        let sealed =
            unsafe { libc::fcntl(window.file.as_raw_fd(), libc::F_ADD_SEALS, REQUIRED_SEALS) };
        ensure!(
            sealed == 0,
            "Sealing PCM window: {}",
            io::Error::last_os_error()
        );
        Ok(window)
    }

    /// Open the authenticated owner-provided memfd path without write access.
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).context("Opening PCM window")?;
        let metadata = file.metadata().context("Inspecting PCM window")?;
        ensure!(metadata.is_file(), "PCM window is not a regular file");
        // Validate seals before length/mmap: a sealed file cannot shrink between
        // these checks and the first mapped load (which could otherwise SIGBUS).
        let seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
        ensure!(
            seals >= 0 && seals & REQUIRED_SEALS == REQUIRED_SEALS,
            "PCM window is not sealed"
        );
        ensure!(
            file.metadata()?.len() == size_of::<Storage>() as u64,
            "Invalid PCM window file length"
        );
        let window = Self::map(file, false)?;
        let storage = window.storage();
        ensure!(
            storage.magic.load(Ordering::Acquire) == MAGIC
                && storage.bytes.load(Ordering::Relaxed) == size_of::<Storage>() as u64
                && storage.version.load(Ordering::Relaxed) == VERSION
                && storage.slots.load(Ordering::Relaxed) == PCM_QUEUE_BLOCKS as u32
                && storage.block_frames.load(Ordering::Relaxed) == PCM_BLOCK_FRAMES as u32
                && storage.max_channels.load(Ordering::Relaxed) == MAX_PCM_CHANNELS as u32,
            "Invalid PCM window metadata"
        );
        Ok(window)
    }

    fn map(file: File, writable: bool) -> Result<Self> {
        let protection = libc::PROT_READ | if writable { libc::PROT_WRITE } else { 0 };
        // Read-side page faults also belong to setup, not the following loop.
        let flags = libc::MAP_SHARED | if writable { 0 } else { libc::MAP_POPULATE };
        // SAFETY: fd is live and its length was established/validated by caller.
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size_of::<Storage>(),
                protection,
                flags,
                file.as_raw_fd(),
                0,
            )
        };
        ensure!(
            address != libc::MAP_FAILED,
            "Mapping PCM window: {}",
            io::Error::last_os_error()
        );
        let Some(storage) = NonNull::new(address.cast::<Storage>()) else {
            // A successful address-zero mapping cannot back a Rust reference.
            unsafe { libc::munmap(address, size_of::<Storage>()) };
            anyhow::bail!("PCM window mapped at address zero");
        };
        Ok(Self {
            storage,
            file,
            writable,
            publishing: AtomicBool::new(false),
        })
    }

    fn storage(&self) -> &Storage {
        // SAFETY: mmap is aligned, initialized, live, and cannot be resized.
        unsafe { self.storage.as_ref() }
    }

    pub fn share_path(&self) -> PathBuf {
        PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            self.file.as_raw_fd()
        ))
    }

    /// Latest completed publication, or zero before the first publication.
    pub fn sequence(&self) -> u64 {
        self.storage().sequence.load(Ordering::Acquire)
    }

    /// Publish native interleaved samples, assigning a monotonically increasing
    /// sequence regardless of `header.sequence`. Returns false for read-only or
    /// concurrently publishing handles, invalid metadata/layout/length, or an
    /// exhausted sequence counter. Never waits or allocates.
    pub fn publish(
        &self,
        mut header: PcmHeader,
        layout: &[u32],
        mut samples: impl ExactSizeIterator<Item = f32>,
    ) -> bool {
        if !self.writable || !valid_layout(layout) {
            return false;
        }
        let count = samples.len();
        let frames = count / layout.len();
        if !count.is_multiple_of(layout.len()) || !valid_header(header, frames) {
            return false;
        }
        if self
            .publishing
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        let _publishing = Publishing(&self.publishing);
        let storage = self.storage();
        let Some(sequence) = storage.sequence.load(Ordering::Relaxed).checked_add(1) else {
            return false;
        };
        header.sequence = sequence;
        let slot = &storage.blocks[((sequence - 1) % PCM_QUEUE_BLOCKS as u64) as usize];
        let writing = slot.revision.load(Ordering::Relaxed).wrapping_add(1) | 1;
        slot.revision.store(writing, Ordering::Relaxed);
        // A reader acquiring any new atomic metadata must also observe this
        // invalidation when it checks revision after taking its snapshot.
        fence(Ordering::Release);
        slot.sequence.store(sequence, Ordering::Relaxed);
        slot.epoch.store(header.epoch, Ordering::Relaxed);
        slot.intent.store(header.intent, Ordering::Relaxed);
        slot.generation.store(header.generation, Ordering::Relaxed);
        slot.capture.store(header.capture, Ordering::Relaxed);
        slot.start_frame
            .store(header.start_frame, Ordering::Relaxed);
        slot.output_frame
            .store(header.output_frame, Ordering::Relaxed);
        slot.sample_rate
            .store(header.sample_rate, Ordering::Relaxed);
        slot.gap.store(u32::from(header.gap), Ordering::Relaxed);
        slot.frames.store(frames as u32, Ordering::Relaxed);
        slot.channels.store(layout.len() as u32, Ordering::Relaxed);
        for (target, channel) in slot.layout.iter().zip(layout) {
            target.store(*channel, Ordering::Relaxed);
        }
        for target in &slot.samples[..count] {
            let Some(value) = samples.next() else {
                // A broken ExactSizeIterator leaves the slot unpublished. A
                // subsequent publisher can replace it without waiting.
                return false;
            };
            target.store(value.to_bits(), Ordering::Relaxed);
        }
        if samples.next().is_some() {
            return false;
        }
        slot.revision
            .store(writing.wrapping_add(1), Ordering::Release);
        storage.sequence.store(sequence, Ordering::Release);
        true
    }

    /// Borrow a retained publication. A slot being replaced or a malformed/torn
    /// metadata snapshot yields None immediately, with no retry or reader pin.
    pub fn block(&self, sequence: u64) -> Option<SharedPcmBlock<'_>> {
        if sequence == 0 || sequence > self.sequence() {
            return None;
        }
        let slot = &self.storage().blocks[((sequence - 1) % PCM_QUEUE_BLOCKS as u64) as usize];
        let revision = slot.revision.load(Ordering::Acquire);
        if revision == 0 || revision & 1 != 0 {
            return None;
        }
        let gap = slot.gap.load(Ordering::Relaxed);
        let header = PcmHeader {
            sequence: slot.sequence.load(Ordering::Relaxed),
            epoch: slot.epoch.load(Ordering::Relaxed),
            intent: slot.intent.load(Ordering::Relaxed),
            generation: slot.generation.load(Ordering::Relaxed),
            capture: slot.capture.load(Ordering::Relaxed),
            start_frame: slot.start_frame.load(Ordering::Relaxed),
            output_frame: slot.output_frame.load(Ordering::Relaxed),
            sample_rate: slot.sample_rate.load(Ordering::Relaxed),
            gap: gap != 0,
        };
        let frames = slot.frames.load(Ordering::Relaxed) as usize;
        let channels = slot.channels.load(Ordering::Relaxed) as usize;
        if channels == 0 || channels > MAX_PCM_CHANNELS {
            return None;
        }
        let mut layout = [0; MAX_PCM_CHANNELS];
        for (target, source) in layout.iter_mut().zip(&slot.layout[..channels]) {
            *target = source.load(Ordering::Relaxed);
        }
        // Pairs with the writer's release fence even when atomic metadata loads
        // observed an in-progress replacement rather than its final revision.
        fence(Ordering::Acquire);
        if slot.revision.load(Ordering::Relaxed) != revision
            || header.sequence != sequence
            || gap > 1
            || !valid_header(header, frames)
            || !valid_layout(&layout[..channels])
        {
            return None;
        }
        Some(SharedPcmBlock {
            slot,
            revision,
            header,
            frames,
            channels,
            layout,
        })
    }
}

impl Drop for SharedPcmWindow {
    fn drop(&mut self) {
        // SAFETY: this handle owns precisely this mapping; block borrows cannot
        // outlive the handle. Other processes/handles own independent mappings.
        unsafe { libc::munmap(self.storage.as_ptr().cast(), size_of::<Storage>()) };
    }
}

struct Publishing<'a>(&'a AtomicBool);
impl Drop for Publishing<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Stable, small metadata snapshot plus direct access to unpinned PCM storage.
/// Samples may change concurrently, including if `still_current()` just passed.
/// No PCM payload is copied into this accessor.
pub struct SharedPcmBlock<'a> {
    slot: &'a Slot,
    revision: u64,
    header: PcmHeader,
    frames: usize,
    channels: usize,
    layout: [u32; MAX_PCM_CHANNELS],
}
impl SharedPcmBlock<'_> {
    pub fn header(&self) -> PcmHeader {
        self.header
    }
    pub fn layout(&self) -> &[u32] {
        &self.layout[..self.channels]
    }
    /// Frame count, not interleaved sample count.
    pub fn len(&self) -> usize {
        self.frames
    }
    pub fn is_empty(&self) -> bool {
        self.frames == 0
    }
    /// Direct atomic load. Panics for a frame/channel outside the snapshot.
    pub fn sample(&self, frame: usize, channel: usize) -> f32 {
        assert!(
            frame < self.frames && channel < self.channels,
            "PCM sample index out of bounds"
        );
        f32::from_bits(self.slot.samples[frame * self.channels + channel].load(Ordering::Relaxed))
    }
    pub fn still_current(&self) -> bool {
        self.slot.revision.load(Ordering::Acquire) == self.revision
    }
}

fn valid_layout(layout: &[u32]) -> bool {
    if layout.is_empty() || layout.len() > MAX_PCM_CHANNELS {
        return false;
    }
    let mut seen = 0_u32;
    for &channel in layout {
        if channel >= MAX_PCM_CHANNELS as u32 || seen & (1 << channel) != 0 {
            return false;
        }
        seen |= 1 << channel;
    }
    true
}

fn valid_header(header: PcmHeader, frames: usize) -> bool {
    frames != 0
        && frames <= PCM_BLOCK_FRAMES
        && header.sample_rate != 0
        && header.epoch.is_multiple_of(2)
        && header.start_frame.checked_add(frames as u64).is_some()
        && header.output_frame.checked_add(frames as u64).is_some()
}

#[cfg(test)]
mod tests;
