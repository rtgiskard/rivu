//! Core-owned, bounded native-channel PCM shared memory.
//!
//! The owner prefaults the configured native retention region before capture.
//! Readers map it read-only and borrow atomic samples directly: they never pin a
//! slot or make the publisher wait. Metadata is a validated snapshot, but samples may be
//! replaced while a reader uses them. This is deliberately best-effort observation,
//! not a lossless queue or an A/V synchronization mechanism.
use anyhow::{Context, Result, ensure};
use rivu_protocol::pcm::{PCM_BLOCK_FRAMES, PCM_OUTPUT_MAX_BYTES, PcmHeader, PcmPosition};
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
// Retain output prefetch, 200 ms observer headroom, and observation history.
// Native-channel strides avoid multiplying the maximum mono frame count by 18.
const MAX_PREFETCH_SAMPLES: usize = PCM_OUTPUT_MAX_BYTES / size_of::<f32>();
const MAX_QUEUE_BLOCKS: usize = MAX_PREFETCH_SAMPLES.div_ceil(PCM_BLOCK_FRAMES)
    + (MAX_PREFETCH_SAMPLES.div_ceil(5) * 4).div_ceil(PCM_BLOCK_FRAMES)
    + PCM_QUEUE_BLOCKS
    + 2;
const MAX_RETENTION_SAMPLES: usize = MAX_PREFETCH_SAMPLES
    + MAX_PREFETCH_SAMPLES.div_ceil(5) * 4
    + PCM_QUEUE_BLOCKS * PCM_BLOCK_FRAMES * MAX_PCM_CHANNELS
    + 4 * PCM_BLOCK_FRAMES * MAX_PCM_CHANNELS;
const POSITION_BITS: u32 = PCM_BLOCK_FRAMES.ilog2() + 1;
const POSITION_MASK: u64 = (1 << POSITION_BITS) - 1;
const MAGIC: u64 = u64::from_le_bytes(*b"RIVUPCM6");
const VERSION: u32 = 6;
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
    position: AtomicU64,
    configuration: AtomicU64,
    stride: AtomicU32,
    blocks: [Slot; MAX_QUEUE_BLOCKS],
    samples: [AtomicU32; MAX_RETENTION_SAMPLES],
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
    media_start: AtomicU64,
    media_step: AtomicU64,
    sample_rate: AtomicU32,
    gap: AtomicU32,
    frames: AtomicU32,
    channels: AtomicU32,
    layout: [AtomicU32; MAX_PCM_CHANNELS],
    sample_offset: AtomicU32,
}

/// A bounded shared PCM window, with exactly one writable owner.
///
/// `open` creates a read-only handle. Dropping the owner closes its advertised
/// fd; its `/proc/<pid>/fd/<fd>` path can then refer to a reused descriptor.
/// Mappings already opened by readers remain valid.
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
                c"rivu-pcm-v6".as_ptr(),
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
        // Fresh memfd pages are zero-initialized; zero is valid for these atomics.
        // Only the active native working region is materialized, not the arena.
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
        storage.stride.store(
            (PCM_BLOCK_FRAMES * MAX_PCM_CHANNELS) as u32,
            Ordering::Relaxed,
        );
        for slot in &storage.blocks[..PCM_QUEUE_BLOCKS] {
            slot.revision.store(0, Ordering::Relaxed);
        }
        window.prefault(PCM_QUEUE_BLOCKS * PCM_BLOCK_FRAMES * MAX_PCM_CHANNELS);
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
                && (PCM_QUEUE_BLOCKS..=MAX_QUEUE_BLOCKS)
                    .contains(&(storage.slots.load(Ordering::Relaxed) as usize))
                && storage.block_frames.load(Ordering::Relaxed) == PCM_BLOCK_FRAMES as u32
                && storage.max_channels.load(Ordering::Relaxed) == MAX_PCM_CHANNELS as u32,
            "Invalid PCM window metadata"
        );
        Ok(window)
    }

    fn map(file: File, writable: bool) -> Result<Self> {
        let protection = libc::PROT_READ | if writable { libc::PROT_WRITE } else { 0 };
        // The owner prefaults active sample pages. Readers must not materialize
        // the unused maximum arena while opening a read-only mapping.
        let flags = libc::MAP_SHARED;
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

    /// Latest logical block, or zero before the first publication.
    pub fn sequence(&self) -> u64 {
        self.frontier().sequence
    }

    /// Atomically observed prefix, including publications that only append a tail.
    pub fn frontier(&self) -> PcmPosition {
        let position = self.storage().position.load(Ordering::Acquire);
        PcmPosition {
            sequence: position >> POSITION_BITS,
            offset: (position & POSITION_MASK) as u16,
        }
    }

    /// Retain output prefetch plus a refill interval and observation history.
    /// Called while audio is fenced, before publishing the new native layout.
    pub fn retain_output(&self, output_frames: usize, rate: u32, channels: usize) -> Result<()> {
        ensure!(
            self.writable && rate != 0 && channels != 0 && channels <= MAX_PCM_CHANNELS,
            "Invalid native PCM retention layout"
        );
        ensure!(
            output_frames
                .checked_mul(channels)
                .is_some_and(|samples| samples <= MAX_PREFETCH_SAMPLES),
            "PCM prefetch exceeds output budget"
        );
        let frames = output_frames
            .checked_add((rate as usize).div_ceil(5))
            .and_then(|frames| frames.checked_add(PCM_QUEUE_BLOCKS * PCM_BLOCK_FRAMES))
            .context("PCM retention frame capacity overflow")?;
        let slots = (frames.div_ceil(PCM_BLOCK_FRAMES) + 2).max(PCM_QUEUE_BLOCKS);
        let stride = PCM_BLOCK_FRAMES * channels;
        let samples = slots
            .checked_mul(stride)
            .context("PCM retention sample capacity overflow")?;
        ensure!(
            slots <= MAX_QUEUE_BLOCKS && samples <= MAX_RETENTION_SAMPLES,
            "PCM retention exceeds bounded window"
        );
        ensure!(
            self.publishing
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok(),
            "PCM retention cannot change while publishing"
        );
        let _publishing = Publishing(&self.publishing);
        let storage = self.storage();
        storage.configuration.fetch_add(1, Ordering::AcqRel);
        // Preserve the metadata write fence when rebasing the shared sample arena.
        fence(Ordering::Release);
        for slot in &storage.blocks[..(storage.slots.load(Ordering::Relaxed) as usize).max(slots)] {
            slot.revision.store(0, Ordering::Relaxed);
        }
        self.prefault(samples);
        storage.stride.store(stride as u32, Ordering::Relaxed);
        storage.slots.store(slots as u32, Ordering::Relaxed);
        storage.configuration.fetch_add(1, Ordering::Release);
        Ok(())
    }

    fn prefault(&self, samples: usize) {
        for sample in self.storage().samples[..samples]
            .iter()
            .step_by(4096 / size_of::<u32>())
        {
            sample.fetch_or(0, Ordering::Relaxed);
        }
        if let Some(last) = self.storage().samples[..samples].last() {
            last.fetch_or(0, Ordering::Relaxed);
        }
    }

    pub fn retained_blocks(&self) -> usize {
        self.storage().slots.load(Ordering::Acquire) as usize
    }

    /// Publish native samples, filling the latest partial block when identity,
    /// layout and frame positions are continuous. Each fragment touches at most
    /// two slots and copies each sample once; it never waits or allocates.
    /// Readers retain an offset to consume each growing logical block tail once.
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
        let latest = self.sequence();
        let append = self.block(latest).filter(|previous| {
            let old = previous.header();
            !header.gap
                && old.epoch == header.epoch
                && old.intent == header.intent
                && old.generation == header.generation
                && old.capture == header.capture
                && old.sample_rate == header.sample_rate
                && old.start_frame + previous.len() as u64 == header.start_frame
                && old.output_frame + previous.len() as u64 == header.output_frame
                && old.media_step == header.media_step
                && {
                    // Division and repeated frame additions can differ by a few
                    // ulps. Do not spend a descriptor on that rounding noise:
                    // fragmented prefetch must retain its full audible span.
                    let end = old.media_start + previous.len() as f64 * old.media_step;
                    (end - header.media_start).abs()
                        <= (4.0 * f64::EPSILON * end.abs().max(header.media_start.abs()).max(1.0))
                            .min(old.media_step * 0.25)
                }
                && previous.layout() == layout
                && previous.len() < PCM_BLOCK_FRAMES
        });
        let incoming = header;
        let (sequence, previous_frames) = if let Some(previous) = append {
            header = previous.header();
            (latest, previous.len())
        } else {
            let Some(sequence) = latest.checked_add(1) else {
                return false;
            };
            (sequence, 0)
        };
        let first_frames = frames.min(PCM_BLOCK_FRAMES - previous_frames);
        let second_frames = frames - first_frames;
        let Some(final_sequence) = sequence.checked_add(u64::from(second_frames != 0)) else {
            return false;
        };
        if final_sequence > u64::MAX >> POSITION_BITS {
            return false;
        }
        header.sequence = sequence;
        let second = PcmHeader {
            sequence: final_sequence,
            start_frame: incoming.start_frame + first_frames as u64,
            output_frame: incoming.output_frame + first_frames as u64,
            media_start: incoming.media_start + first_frames as f64 * incoming.media_step,
            gap: false,
            ..incoming
        };
        let mut completed = [None, None];
        for (part, (header, previous_frames, frames)) in [
            (header, previous_frames, first_frames),
            (second, 0, second_frames),
        ]
        .into_iter()
        .enumerate()
        {
            if frames == 0 {
                continue;
            }
            let count = frames * layout.len();
            let index =
                ((header.sequence - 1) % u64::from(storage.slots.load(Ordering::Relaxed))) as usize;
            let slot = &storage.blocks[index];
            let stride = storage.stride.load(Ordering::Relaxed) as usize;
            let offset = index * stride;
            if (previous_frames + frames) * layout.len() > stride {
                return false;
            }
            let writing = slot.revision.load(Ordering::Relaxed).wrapping_add(1) | 1;
            slot.revision.store(writing, Ordering::Relaxed);
            // Any newly observed metadata or samples must also reveal invalidation
            // when a reader validates its snapshot after the acquire fence.
            fence(Ordering::Release);
            slot.sequence.store(header.sequence, Ordering::Relaxed);
            slot.epoch.store(header.epoch, Ordering::Relaxed);
            slot.intent.store(header.intent, Ordering::Relaxed);
            slot.generation.store(header.generation, Ordering::Relaxed);
            slot.capture.store(header.capture, Ordering::Relaxed);
            slot.start_frame
                .store(header.start_frame, Ordering::Relaxed);
            slot.output_frame
                .store(header.output_frame, Ordering::Relaxed);
            slot.media_start
                .store(header.media_start.to_bits(), Ordering::Relaxed);
            slot.media_step
                .store(header.media_step.to_bits(), Ordering::Relaxed);
            slot.sample_rate
                .store(header.sample_rate, Ordering::Relaxed);
            slot.gap.store(u32::from(header.gap), Ordering::Relaxed);
            slot.frames
                .store((previous_frames + frames) as u32, Ordering::Relaxed);
            slot.channels.store(layout.len() as u32, Ordering::Relaxed);
            slot.sample_offset.store(offset as u32, Ordering::Relaxed);
            for (target, channel) in slot.layout.iter().zip(layout) {
                target.store(*channel, Ordering::Relaxed);
            }
            let start = offset + previous_frames * layout.len();
            for target in &storage.samples[start..start + count] {
                let Some(value) = samples.next() else {
                    // A dishonest ExactSizeIterator leaves touched slots fenced.
                    return false;
                };
                target.store(value.to_bits(), Ordering::Relaxed);
            }
            completed[part] = Some((slot, writing));
        }
        if samples.next().is_some() {
            return false;
        }
        for (slot, writing) in completed.into_iter().flatten() {
            slot.revision
                .store(writing.wrapping_add(1), Ordering::Release);
        }
        let tail = if second_frames == 0 {
            previous_frames + first_frames
        } else {
            second_frames
        };
        storage.position.store(
            (final_sequence << POSITION_BITS) | tail as u64,
            Ordering::Release,
        );
        true
    }

    /// Borrow a retained publication. A slot being replaced or a malformed/torn
    /// metadata snapshot yields None immediately, with no retry or reader pin.
    pub fn block(&self, sequence: u64) -> Option<SharedPcmBlock<'_>> {
        if sequence == 0 || sequence > self.sequence() {
            return None;
        }
        let storage = self.storage();
        let configuration = storage.configuration.load(Ordering::Acquire);
        if !configuration.is_multiple_of(2) {
            return None;
        }
        let slots = storage.slots.load(Ordering::Relaxed) as usize;
        let slot = &storage.blocks[((sequence - 1) % slots as u64) as usize];
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
            media_start: f64::from_bits(slot.media_start.load(Ordering::Relaxed)),
            media_step: f64::from_bits(slot.media_step.load(Ordering::Relaxed)),
            sample_rate: slot.sample_rate.load(Ordering::Relaxed),
            gap: gap != 0,
        };
        let frames = slot.frames.load(Ordering::Relaxed) as usize;
        let channels = slot.channels.load(Ordering::Relaxed) as usize;
        let sample_offset = slot.sample_offset.load(Ordering::Relaxed) as usize;
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
            || storage.configuration.load(Ordering::Relaxed) != configuration
            || sample_offset
                .checked_add(frames.saturating_mul(channels))
                .is_none_or(|end| end > MAX_RETENTION_SAMPLES)
            || header.sequence != sequence
            || gap > 1
            || !valid_header(header, frames)
            || !valid_layout(&layout[..channels])
        {
            return None;
        }
        Some(SharedPcmBlock {
            slot,
            storage,
            configuration,
            sample_offset,
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
    storage: &'a Storage,
    configuration: u64,
    sample_offset: usize,
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
        f32::from_bits(
            self.storage.samples[self.sample_offset + frame * self.channels + channel]
                .load(Ordering::Relaxed),
        )
    }
    pub fn still_current(&self) -> bool {
        // Pair with the writer's release fence before payload replacement.
        // Prior relaxed sample loads must precede the revision/configuration
        // rechecks, including when they observed a not-yet-completed write.
        fence(Ordering::Acquire);
        self.slot.revision.load(Ordering::Acquire) == self.revision
            && self.storage.configuration.load(Ordering::Acquire) == self.configuration
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
        && header.media_start.is_finite()
        && header.media_step.is_finite()
        && header.media_step > 0.0
        && (header.media_start + frames as f64 * header.media_step).is_finite()
        && header.epoch.is_multiple_of(2)
        && header.start_frame.checked_add(frames as u64).is_some()
        && header.output_frame.checked_add(frames as u64).is_some()
}

#[cfg(test)]
mod tests;
