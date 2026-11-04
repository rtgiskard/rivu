//! Waveform display snapshots and bounded control envelopes.
use crate::{model::Track, transport::timeout};
use anyhow::{Context, Result, ensure};
use async_io::Async;
use futures_lite::io::{AsyncReadExt, AsyncWriteExt};
use rivu_format::decoder_hint::DecoderHint;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
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
pub const VERSION: u8 = 2;
pub const MAX_FRAME: usize = 1024 * 1024;
pub const IO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaveKey {
    pub source: u64,
    pub revision: u64,
    pub suspension: u64,
    pub suspended: bool,
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
        track: Box<Track>,
        source: Option<u64>,
    },
}
#[derive(Serialize, Deserialize)]
pub enum Response<Frame> {
    Hello {
        version: u8,
        instance: u32,
    },
    Progressive {
        key: WaveKey,
        frame: Option<Box<Frame>>,
    },
    Full(Box<Frame>),
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
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let bytes = postcard::to_stdvec(value)?;
    ensure!(
        bytes.len() <= MAX_FRAME,
        "Waveform frame exceeds its byte limit"
    );
    Ok(bytes)
}
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let (value, remaining) = postcard::take_from_bytes(bytes)?;
    ensure!(remaining.is_empty(), "Waveform frame has trailing data");
    Ok(value)
}
pub async fn read_frame<T: DeserializeOwned>(stream: &mut Async<UnixStream>) -> Result<T> {
    let mut prefix = [0; 4];
    stream.read_exact(&mut prefix[..1]).await?;
    timeout(IO_TIMEOUT, async {
        stream.read_exact(&mut prefix[1..]).await?;
        let length = u32::from_le_bytes(prefix) as usize;
        ensure!(length <= MAX_FRAME, "Waveform frame exceeds its byte limit");
        let mut bytes = vec![0; length];
        stream.read_exact(&mut bytes).await?;
        decode(&bytes)
    })
    .await
    .context("Incomplete waveform frame timed out")?
}
pub async fn write_frame<T: Serialize>(stream: &mut Async<UnixStream>, value: &T) -> Result<()> {
    let bytes = encode(value)?;
    timeout(IO_TIMEOUT, async {
        stream
            .write_all(&(bytes.len() as u32).to_le_bytes())
            .await?;
        stream.write_all(&bytes).await?;
        stream.flush().await?;
        Ok(())
    })
    .await
    .context("Waveform write timed out")?
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
        let bytes = encode(&frame).unwrap();
        let legacy: LegacyWaveformFrame = decode(&bytes).unwrap();
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
        assert_eq!(encode(&legacy).unwrap(), bytes);
        let roundtrip: WaveformFrame = decode(&encode(&legacy).unwrap()).unwrap();
        assert_eq!(roundtrip.identity, frame.identity);
        assert_eq!(roundtrip.suspension_generation(), 10);
        assert!(roundtrip.is_suspended());
    }
}
