//! Waveform display snapshots and bounded control envelopes.
use crate::{
    model::{CueSegment, Track},
    transport::{release_large_buffer, timeout},
};
use anyhow::{Context, Result, ensure};
use async_io::Async;
use futures_lite::io::{AsyncReadExt, AsyncWriteExt};
use rivu_format::decoder_hint::DecoderHint;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    io::{self, Write},
    os::{
        linux::net::SocketAddrExt,
        unix::{
            ffi::OsStrExt,
            net::{SocketAddr, UnixStream},
        },
    },
    path::{Path, PathBuf},
    time::Duration,
};

/// Content version of an opened file or an immutable retained PCM span.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SourceIdentity {
    File {
        device: u64,
        inode: u64,
        len: u64,
        modified: (i64, i64),
        changed: (i64, i64),
    },
    Pcm {
        storage: u64,
        start: u64,
        frames: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlaybackRange {
    pub start_seconds: f64,
    pub end_seconds: Option<f64>,
}

/// Captured when a decoder is loaded, never refreshed from a path or library
/// snapshot. Keep the initial duration separate from the EOF-corrected timeline.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum WaveformIdentity {
    Progressive {
        content: SourceIdentity,
        ffmpeg: bool,
        duration: Option<f64>,
        sample_rate: u32,
        channels: u16,
    },
    FullScan {
        duration: Option<f64>,
        sample_rate: u32,
    },
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct WaveformFrame {
    pub revision: u64,
    pub complete: bool,
    pub path: Option<PathBuf>,
    pub range: Option<PlaybackRange>,
    pub decoder_hint: DecoderHint,
    pub duration: Option<f64>,
    pub span_seconds: f64,
    // Sample width is bin_frames / bin_count, independent of EOF array truncation.
    // Widen the numerator so coarsening cannot overflow the u64 sample domain.
    pub bin_frames: u128,
    pub bin_count: u64,
    pub max_peak: f32,
    /// `None` is not yet decoded; `Some(0.0)` is decoded silence.
    pub peaks: Vec<Option<f32>>,
    /// RMS of the current contiguous decoded interval in each rolling bin.
    pub rms: Vec<Option<f32>>,
    pub rms_counts: Vec<u64>,
    pub left_peaks: Vec<Option<f32>>,
    pub right_peaks: Vec<Option<f32>>,
    pub left_rms: Vec<Option<f32>>,
    pub right_rms: Vec<Option<f32>>,
    pub left_rms_counts: Vec<u64>,
    pub right_rms_counts: Vec<u64>,
    pub rms_spans: Vec<Option<std::ops::Range<u64>>>,
    /// Last PCM revision per bin, used to fence asynchronous full scans.
    pub bin_revisions: Vec<u64>,
    pub bin_epochs: Vec<u64>,
    traversal: u64,
    suspended: bool,
    suspension_generation: u64,
    pub identity: Option<WaveformIdentity>,
    pub scan_source_identity: Option<SourceIdentity>,
}

impl Default for WaveformFrame {
    fn default() -> Self {
        Self {
            revision: 0,
            path: None,
            complete: false,
            range: None,
            decoder_hint: DecoderHint::Auto,
            duration: None,
            span_seconds: 0.0,
            bin_frames: 0,
            bin_count: 0,
            max_peak: 1.0,
            peaks: Vec::new(),
            rms: Vec::new(),
            rms_counts: Vec::new(),
            left_peaks: Vec::new(),
            right_peaks: Vec::new(),
            left_rms: Vec::new(),
            right_rms: Vec::new(),
            left_rms_counts: Vec::new(),
            right_rms_counts: Vec::new(),
            rms_spans: Vec::new(),
            bin_revisions: Vec::new(),
            bin_epochs: Vec::new(),
            traversal: 1,
            suspended: false,
            suspension_generation: 0,
            identity: None,
            scan_source_identity: None,
        }
    }
}

impl WaveformFrame {
    pub fn matches(
        &self,
        path: &Path,
        range: Option<PlaybackRange>,
        decoder_hint: DecoderHint,
    ) -> bool {
        self.path.as_deref() == Some(path)
            && self.range == range
            && self.decoder_hint == decoder_hint
    }

    pub fn suspension_generation(&self) -> u64 {
        self.suspension_generation
    }

    pub fn is_suspended(&self) -> bool {
        self.suspended
    }

    pub fn traversal_epoch(&self) -> u64 {
        self.traversal
    }

    /// Heap storage retained by the bounded display snapshot.
    pub fn retained_capacity_bytes(&self) -> usize {
        fn bytes<T>(values: &Vec<T>) -> usize {
            values.capacity() * std::mem::size_of::<T>()
        }
        bytes(&self.peaks)
            + bytes(&self.rms)
            + bytes(&self.rms_counts)
            + bytes(&self.left_peaks)
            + bytes(&self.right_peaks)
            + bytes(&self.left_rms)
            + bytes(&self.right_rms)
            + bytes(&self.left_rms_counts)
            + bytes(&self.right_rms_counts)
            + bytes(&self.rms_spans)
            + bytes(&self.bin_revisions)
            + bytes(&self.bin_epochs)
    }

    /// Record producer-owned state as snapshot metadata, without managing its lifecycle.
    pub fn set_producer_metadata(
        &mut self,
        traversal: u64,
        suspended: bool,
        suspension_generation: u64,
    ) {
        self.traversal = traversal;
        self.suspended = suspended;
        self.suspension_generation = suspension_generation;
    }
}
pub const VERSION: u8 = 3;
pub const MAX_FRAME: usize = 1024 * 1024;
pub const IO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaveKey {
    pub source: u64,
    pub revision: u64,
    pub suspension: u64,
    pub suspended: bool,
}

/// Client expectation only; Core still reloads and compares the authoritative track.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExpectedSource {
    pub id: i64,
    pub path: PathBuf,
    pub cue: Option<CueSegment>,
    pub fingerprint: Option<String>,
    pub decoder_hint: DecoderHint,
}

impl From<&Track> for ExpectedSource {
    fn from(track: &Track) -> Self {
        Self {
            id: track.id,
            path: track.path.clone(),
            cue: track.cue.clone(),
            fingerprint: track.fingerprint.clone(),
            decoder_hint: track.decoder_hint,
        }
    }
}

impl ExpectedSource {
    pub fn matches(&self, track: &Track) -> bool {
        self.id == track.id
            && self.path == track.path
            && self.cue == track.cue
            && self.fingerprint == track.fingerprint
            && self.decoder_hint == track.decoder_hint
    }
}

#[derive(Serialize, Deserialize)]
pub enum Request {
    Hello {
        version: u8,
        instance: u32,
    },
    Progressive {
        known: Option<WaveKey>,
    },
    Full {
        expected: ExpectedSource,
        source: Option<u64>,
    },
}
#[derive(Serialize, Deserialize)]
pub enum Response<Frame> {
    Hello { version: u8, instance: u32 },
    Progressive { key: WaveKey, frame: Option<Frame> },
    Full(Frame),
    Error(String),
}
pub fn address(data_dir: &Path) -> Result<SocketAddr> {
    let base = crate::wire::address(data_dir)?;
    let mut name = base
        .as_abstract_name()
        .context("Expected abstract data directory address")?
        .to_vec();
    name.extend_from_slice(b".waveform");
    Ok(SocketAddr::from_abstract_name(name)?)
}
pub async fn connect(data_dir: &Path) -> Result<Async<UnixStream>> {
    let address = address(data_dir)?;
    let name = address
        .as_abstract_name()
        .context("Expected abstract waveform address")?;
    let mut bytes = [0; 108];
    ensure!(name.len() + 1 < bytes.len(), "Waveform address is too long");
    bytes[1..=name.len()].copy_from_slice(name);
    let path = Path::new(std::ffi::OsStr::from_bytes(&bytes[..name.len() + 1]));
    let stream = Async::<UnixStream>::connect(path).await?;
    crate::wire::verify_peer(&stream)?;
    Ok(stream)
}
struct FrameWriter<'a>(&'a mut Vec<u8>);

impl Write for FrameWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let required = self.0.len().saturating_add(bytes.len());
        if required > 4 + MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Waveform frame exceeds its byte limit",
            ));
        }
        if required > self.0.capacity() {
            let target = required
                .max(self.0.capacity().saturating_mul(2))
                .min(4 + MAX_FRAME);
            self.0.reserve_exact(target - self.0.len());
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Encode directly into session storage, checking the limit before each append.
/// On failure no partial frame remains available to send.
pub fn encode_frame_into<T: Serialize>(value: &T, bytes: &mut Vec<u8>) -> Result<()> {
    bytes.clear();
    bytes.resize(4, 0);
    if let Err(error) = postcard::to_io(value, FrameWriter(bytes)) {
        bytes.clear();
        release_large_buffer(bytes);
        return Err(error).context("Encoding waveform frame");
    }
    let length = (bytes.len() - 4) as u32;
    bytes[..4].copy_from_slice(&length.to_le_bytes());
    Ok(())
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    ensure!(
        bytes.len() <= MAX_FRAME,
        "Waveform frame exceeds its byte limit"
    );
    let (value, remaining) = postcard::take_from_bytes(bytes)?;
    ensure!(remaining.is_empty(), "Waveform frame has trailing data");
    Ok(value)
}

pub async fn read_frame<T: DeserializeOwned>(
    stream: &mut Async<UnixStream>,
    bytes: &mut Vec<u8>,
) -> Result<T> {
    bytes.clear();
    release_large_buffer(bytes);
    let mut prefix = [0; 4];
    stream.read_exact(&mut prefix[..1]).await?;
    let result = timeout(IO_TIMEOUT, async {
        stream.read_exact(&mut prefix[1..]).await?;
        let length = u32::from_le_bytes(prefix) as usize;
        ensure!(length <= MAX_FRAME, "Waveform frame exceeds its byte limit");
        bytes.clear();
        if length > bytes.capacity() {
            bytes.reserve_exact(length);
        }
        bytes.resize(length, 0);
        stream.read_exact(bytes).await?;
        decode(bytes)
    })
    .await
    .context("Incomplete waveform frame timed out");
    bytes.clear();
    release_large_buffer(bytes);
    result?
}

/// Send a previously encoded frame; no producer lock is held during I/O.
pub async fn write_frame(stream: &mut Async<UnixStream>, bytes: &mut Vec<u8>) -> Result<()> {
    let result = timeout(IO_TIMEOUT, async {
        ensure!(
            bytes.len() >= 4 && bytes.len() <= 4 + MAX_FRAME,
            "Invalid encoded waveform frame length"
        );
        ensure!(
            u32::from_le_bytes(bytes[..4].try_into().expect("checked prefix")) as usize
                == bytes.len() - 4,
            "Invalid encoded waveform frame prefix"
        );
        stream.write_all(bytes).await?;
        stream.flush().await?;
        Ok(())
    })
    .await
    .context("Waveform write timed out");
    bytes.clear();
    release_large_buffer(bytes);
    result?
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    // Preserve the pre-extraction positional layout, including its skipped fields.
    #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
    struct LegacyWaveformFrame {
        revision: u64,
        complete: bool,
        path: Option<PathBuf>,
        range: Option<PlaybackRange>,
        decoder_hint: DecoderHint,
        duration: Option<f64>,
        span_seconds: f64,
        // Sample width is bin_frames / bin_count, independent of EOF array truncation.
        // Widen the numerator so coarsening cannot overflow the u64 sample domain.
        bin_frames: u128,
        bin_count: u64,
        max_peak: f32,
        /// `None` is not yet decoded; `Some(0.0)` is decoded silence.
        peaks: Vec<Option<f32>>,
        /// RMS of the current contiguous decoded interval in each rolling bin.
        rms: Vec<Option<f32>>,
        rms_counts: Vec<u64>,
        left_peaks: Vec<Option<f32>>,
        right_peaks: Vec<Option<f32>>,
        left_rms: Vec<Option<f32>>,
        right_rms: Vec<Option<f32>>,
        left_rms_counts: Vec<u64>,
        right_rms_counts: Vec<u64>,
        #[serde(skip)]
        channel_weights: Vec<[f32; 2]>,
        rms_spans: Vec<Option<std::ops::Range<u64>>>,
        #[serde(skip)]
        rms_prefixes: Vec<Option<()>>,
        /// Last PCM revision per bin, used to fence asynchronous full scans.
        bin_revisions: Vec<u64>,
        bin_epochs: Vec<u64>,
        traversal: u64,
        suspended: bool,
        suspension_generation: u64,
        identity: Option<WaveformIdentity>,
        scan_source_identity: Option<SourceIdentity>,
    }

    #[test]
    fn snapshot_preserves_legacy_positional_layout_and_metadata() {
        let mut frame = WaveformFrame {
            revision: 17,
            complete: true,
            path: Some(PathBuf::from("track.wav")),
            range: Some(PlaybackRange {
                start_seconds: 1.5,
                end_seconds: Some(3.0),
            }),
            decoder_hint: DecoderHint::Native,
            duration: Some(1.5),
            span_seconds: 2.0,
            bin_frames: 96000,
            bin_count: 2,
            max_peak: 1.25,
            peaks: vec![Some(1.25), None],
            rms: vec![Some(0.5), None],
            rms_counts: vec![11, 0],
            left_peaks: vec![Some(0.75), None],
            right_peaks: vec![Some(1.0), None],
            left_rms: vec![Some(0.25), None],
            right_rms: vec![Some(0.375), None],
            left_rms_counts: vec![12, 0],
            right_rms_counts: vec![13, 0],
            rms_spans: vec![Some(3..14), None],
            bin_revisions: vec![15, 16],
            bin_epochs: vec![7, 8],
            ..Default::default()
        };
        frame.set_producer_metadata(9, true, 10);
        let content = SourceIdentity::File {
            device: 1,
            inode: 2,
            len: 3,
            modified: (4, 5),
            changed: (6, 7),
        };
        frame.identity = Some(WaveformIdentity::Progressive {
            content,
            ffmpeg: true,
            duration: Some(1.5),
            sample_rate: 48000,
            channels: 2,
        });
        frame.scan_source_identity = Some(content);
        let mut bytes = Vec::new();
        encode_frame_into(&frame, &mut bytes).unwrap();
        let legacy: LegacyWaveformFrame = decode(&bytes[4..]).unwrap();
        assert_eq!(legacy.revision, frame.revision);
        assert_eq!(legacy.path, frame.path);
        assert_eq!(legacy.range, frame.range);
        assert_eq!(legacy.rms_counts, frame.rms_counts);
        assert_eq!(legacy.left_rms_counts, frame.left_rms_counts);
        assert_eq!(legacy.right_rms_counts, frame.right_rms_counts);
        assert_eq!(legacy.rms_spans, frame.rms_spans);
        assert_eq!(legacy.bin_epochs, frame.bin_epochs);
        assert_eq!(legacy.traversal, 9);
        assert!(legacy.suspended);
        assert_eq!(legacy.suspension_generation, 10);
        assert_eq!(legacy.identity, frame.identity);
        assert_eq!(legacy.scan_source_identity, frame.scan_source_identity);
        assert!(legacy.channel_weights.is_empty());
        assert!(legacy.rms_prefixes.is_empty());
        let mut legacy_bytes = Vec::new();
        encode_frame_into(&legacy, &mut legacy_bytes).unwrap();
        assert_eq!(legacy_bytes, bytes);
        let roundtrip: WaveformFrame = decode(&legacy_bytes[4..]).unwrap();
        assert_eq!(roundtrip.identity, frame.identity);
        assert_eq!(roundtrip.suspension_generation(), 10);
        assert!(roundtrip.is_suspended());
    }
}

#[cfg(test)]
mod framing_tests {
    use super::*;
    use crate::transport::RETAINED_BUFFER_LIMIT;

    #[test]
    fn encoder_bounds_each_append_and_recovers_after_oversized_values() {
        let mut bytes = Vec::new();
        // This sequence length has a three-byte varint prefix.
        let exact = vec![0u8; MAX_FRAME - 3];
        encode_frame_into(&exact, &mut bytes).unwrap();
        assert_eq!(bytes.len(), 4 + MAX_FRAME);
        assert_eq!(
            u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize,
            MAX_FRAME
        );
        assert_eq!(decode::<Vec<u8>>(&bytes[4..]).unwrap(), exact);

        assert!(encode_frame_into(&vec![0u8; MAX_FRAME], &mut bytes).is_err());
        assert!(bytes.is_empty());
        assert!(bytes.capacity() <= RETAINED_BUFFER_LIMIT);
        encode_frame_into(&Request::Progressive { known: None }, &mut bytes).unwrap();
        assert!(matches!(
            decode(&bytes[4..]).unwrap(),
            Request::Progressive { known: None }
        ));
    }

    #[test]
    fn decoder_rejects_oversized_trailing_and_truncated_input() {
        assert!(decode::<Request>(&vec![0; MAX_FRAME + 1]).is_err());
        let mut bytes = Vec::new();
        encode_frame_into(
            &Request::Hello {
                version: VERSION,
                instance: 7,
            },
            &mut bytes,
        )
        .unwrap();
        let body = bytes[4..].to_vec();
        assert!(decode::<Request>(&body[..body.len() - 1]).is_err());
        let mut trailing = body;
        trailing.push(0);
        assert!(decode::<Request>(&trailing).is_err());
        assert!(decode::<Vec<u8>>(&[0xff; 10]).is_err());
    }

    #[test]
    fn reader_rejects_untrusted_prefixes_without_retaining_bulk_storage() {
        for (length, body) in [
            (MAX_FRAME + 1, vec![]),
            (0, vec![]),
            (3, vec![0]),
            (2, vec![255, 255]),
        ] {
            let (receiver, mut sender) = UnixStream::pair().unwrap();
            sender.write_all(&(length as u32).to_le_bytes()).unwrap();
            sender.write_all(&body).unwrap();
            drop(sender);
            let mut receiver = Async::new(receiver).unwrap();
            let mut bytes = Vec::with_capacity(MAX_FRAME);
            assert!(async_io::block_on(read_frame::<Request>(&mut receiver, &mut bytes)).is_err());
            assert!(bytes.is_empty());
            assert!(bytes.capacity() <= RETAINED_BUFFER_LIMIT);
        }
    }

    #[test]
    fn socket_frames_reuse_small_storage_and_retire_bulk_transfers() {
        async_io::block_on(async {
            let (sender, receiver) = UnixStream::pair().unwrap();
            let mut sender = Async::new(sender).unwrap();
            let mut receiver = Async::new(receiver).unwrap();
            let send = async {
                let mut bytes = Vec::new();
                for length in [8, 8, RETAINED_BUFFER_LIMIT + 1, 8] {
                    encode_frame_into(&vec![42u8; length], &mut bytes).unwrap();
                    write_frame(&mut sender, &mut bytes).await.unwrap();
                    assert!(bytes.is_empty());
                    assert!(bytes.capacity() <= RETAINED_BUFFER_LIMIT);
                }
            };
            let receive = async {
                let mut bytes = Vec::new();
                for length in [8, 8, RETAINED_BUFFER_LIMIT + 1, 8] {
                    let frame: Vec<u8> = read_frame(&mut receiver, &mut bytes).await.unwrap();
                    assert_eq!(frame, vec![42u8; length]);
                    assert!(bytes.is_empty());
                    assert!(bytes.capacity() <= RETAINED_BUFFER_LIMIT);
                }
            };
            futures::join!(send, receive);
        });
    }
}
