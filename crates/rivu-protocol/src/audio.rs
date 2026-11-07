//! Bounded finite native PCM rendering protocol.
use crate::{
    model::{CueSegment, Track},
    transport::{release_large_buffer, timeout},
};
use anyhow::{Context, Result, ensure};
use async_io::Async;
use futures_lite::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
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

pub const VERSION: u8 = 1;
pub const MAX_FRAME: usize = 256 * 1024;
/// Leaves room for event metadata and the postcard sequence prefix.
pub const MAX_PCM_SAMPLES: usize = (MAX_FRAME - 128) / size_of::<f32>();
pub const IO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Request {
    Hello {
        version: u8,
        instance: u32,
    },
    Read {
        expected: ExpectedSource,
        source: Option<u64>,
        signal: rivu_format::config::VisualizationSignal,
        range: Option<PlaybackRange>,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Response {
    Hello {
        version: u8,
        instance: u32,
    },
    Start {
        source: Option<u64>,
        identity: SourceIdentity,
        sample_rate: u32,
        layout: Vec<u32>,
        duration: Option<f64>,
    },
    Pcm {
        sequence: u64,
        media_start: f64,
        media_step: f64,
        samples: Vec<f32>,
    },
    Complete {
        frames: u64,
        duration: f64,
    },
    Error(String),
}
pub fn address(data_dir: &Path) -> Result<SocketAddr> {
    let base = crate::wire::address(data_dir)?;
    let mut name = base
        .as_abstract_name()
        .context("Expected abstract data directory address")?
        .to_vec();
    name.extend_from_slice(b".audio.v1");
    Ok(SocketAddr::from_abstract_name(name)?)
}
pub async fn connect(data_dir: &Path) -> Result<Async<UnixStream>> {
    let address = address(data_dir)?;
    let name = address
        .as_abstract_name()
        .context("Expected abstract audio address")?;
    let mut bytes = [0; 108];
    ensure!(name.len() + 1 < bytes.len(), "Audio address is too long");
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
                "Audio frame exceeds its byte limit",
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
        return Err(error).context("Encoding audio frame");
    }
    let length = (bytes.len() - 4) as u32;
    bytes[..4].copy_from_slice(&length.to_le_bytes());
    Ok(())
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    ensure!(
        bytes.len() <= MAX_FRAME,
        "Audio frame exceeds its byte limit"
    );
    let (value, remaining) = postcard::take_from_bytes(bytes)?;
    ensure!(remaining.is_empty(), "Audio frame has trailing data");
    Ok(value)
}

pub async fn read_frame<T: DeserializeOwned>(
    stream: &mut (impl AsyncRead + Unpin),
    bytes: &mut Vec<u8>,
) -> Result<T> {
    bytes.clear();
    release_large_buffer(bytes);
    let mut prefix = [0; 4];
    stream.read_exact(&mut prefix[..1]).await?;
    let result = timeout(IO_TIMEOUT, async {
        stream.read_exact(&mut prefix[1..]).await?;
        let length = u32::from_le_bytes(prefix) as usize;
        ensure!(length <= MAX_FRAME, "Audio frame exceeds its byte limit");
        bytes.clear();
        if length > bytes.capacity() {
            bytes.reserve_exact(length);
        }
        bytes.resize(length, 0);
        stream.read_exact(bytes).await?;
        decode(bytes)
    })
    .await
    .context("Incomplete audio frame timed out");
    bytes.clear();
    release_large_buffer(bytes);
    result?
}

/// Send a previously encoded frame; no producer lock is held during I/O.
pub async fn write_frame(
    stream: &mut (impl AsyncWrite + Unpin),
    bytes: &mut Vec<u8>,
) -> Result<()> {
    let result = timeout(IO_TIMEOUT, async {
        ensure!(
            bytes.len() >= 4 && bytes.len() <= 4 + MAX_FRAME,
            "Invalid encoded audio frame length"
        );
        ensure!(
            u32::from_le_bytes(bytes[..4].try_into().expect("checked prefix")) as usize
                == bytes.len() - 4,
            "Invalid encoded audio frame prefix"
        );
        stream.write_all(bytes).await?;
        stream.flush().await?;
        Ok(())
    })
    .await
    .context("Audio write timed out");
    bytes.clear();
    release_large_buffer(bytes);
    result?
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
        encode_frame_into(
            &Request::Hello {
                version: VERSION,
                instance: 0,
            },
            &mut bytes,
        )
        .unwrap();
        assert!(matches!(
            decode(&bytes[4..]).unwrap(),
            Request::Hello {
                version: VERSION,
                instance: 0
            }
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
}

#[cfg(test)]
mod pcm_tests {
    use super::*;

    #[test]
    fn finite_pcm_chunks_preserve_mapping_and_enforce_frame_budget() {
        let mut bytes = Vec::new();
        let response = Response::Pcm {
            sequence: u64::MAX,
            media_start: 1.25,
            media_step: 1.0 / 48000.0,
            samples: vec![0.25; MAX_PCM_SAMPLES],
        };
        encode_frame_into(&response, &mut bytes).unwrap();
        assert!(bytes.len() <= MAX_FRAME + 4);
        let Response::Pcm {
            sequence,
            media_start,
            media_step,
            samples,
        } = decode(&bytes[4..]).unwrap()
        else {
            panic!("expected PCM")
        };
        assert_eq!(sequence, u64::MAX);
        assert_eq!(media_start, 1.25);
        assert_eq!(media_step, 1.0 / 48000.0);
        assert_eq!(samples, vec![0.25; MAX_PCM_SAMPLES]);
        let oversized = Response::Pcm {
            sequence: 1,
            media_start: 0.0,
            media_step: 1.0,
            samples: vec![0.0; MAX_FRAME / 4],
        };
        assert!(encode_frame_into(&oversized, &mut bytes).is_err());
        assert!(bytes.is_empty());
    }

    #[test]
    fn read_request_retains_selected_identity_signal_and_range() {
        let expected = ExpectedSource {
            id: 7,
            path: PathBuf::from("selected.flac"),
            cue: None,
            fingerprint: Some("immutable".into()),
            decoder_hint: DecoderHint::Native,
        };
        let range = Some(PlaybackRange {
            start_seconds: 2.0,
            end_seconds: Some(4.0),
        });
        let request = Request::Read {
            expected: expected.clone(),
            source: None,
            signal: rivu_format::config::VisualizationSignal::Before,
            range,
        };
        let mut bytes = Vec::new();
        encode_frame_into(&request, &mut bytes).unwrap();
        let Request::Read {
            expected: actual,
            source,
            signal,
            range: actual_range,
        } = decode(&bytes[4..]).unwrap()
        else {
            panic!("expected read")
        };
        assert_eq!(actual, expected);
        assert_eq!(source, None);
        assert_eq!(signal, rivu_format::config::VisualizationSignal::Before);
        assert_eq!(actual_range, range);
    }

    #[test]
    fn captured_file_and_pcm_identities_round_trip_without_summary_metadata() {
        for identity in [
            SourceIdentity::File {
                device: 1,
                inode: 2,
                len: 3,
                modified: (4, 5),
                changed: (6, 7),
            },
            SourceIdentity::Pcm {
                storage: 8,
                start: 9,
                frames: 10,
            },
        ] {
            let response = Response::Start {
                source: Some(11),
                identity,
                sample_rate: 48000,
                layout: vec![0, 1],
                duration: Some(12.5),
            };
            let mut bytes = Vec::new();
            encode_frame_into(&response, &mut bytes).unwrap();
            let Response::Start {
                source,
                identity: actual,
                sample_rate,
                layout,
                duration,
            } = decode(&bytes[4..]).unwrap()
            else {
                panic!("expected start")
            };
            assert_eq!(actual, identity);
            assert_eq!(source, Some(11));
            assert_eq!(sample_rate, 48000);
            assert_eq!(layout, [0, 1]);
            assert_eq!(duration, Some(12.5));
        }
    }
}
