//! Bounded finite native PCM rendering protocol.
use crate::{
    model::{CueSegment, Track},
    transport::{release_large_buffer, timeout},
};
use anyhow::{Context, Result, ensure};
use async_io::Async;
use futures_lite::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use rivu_format::decoder_hint::DecoderHint;
use serde::{
    Deserialize, Serialize,
    de::{DeserializeOwned, DeserializeSeed, EnumAccess, SeqAccess, VariantAccess, Visitor},
};
use std::{
    os::unix::net::{SocketAddr, UnixStream},
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

/// Decode storage owned by one physical PCM stream. Responses borrow this storage,
/// so another frame cannot overwrite samples until the consumer releases its borrow.
#[derive(Default)]
pub struct ResponseDecoder {
    response: Option<Response>,
    samples: Vec<f32>,
}

impl ResponseDecoder {
    pub fn decode(&mut self, bytes: &[u8]) -> Result<&Response> {
        ensure!(
            bytes.len() <= MAX_FRAME,
            "Audio frame exceeds its byte limit"
        );
        if let Some(Response::Pcm { samples, .. }) = self.response.take() {
            self.samples = samples;
        }
        let mut deserializer = postcard::Deserializer::from_bytes(bytes);
        let response = ResponseSeed(&mut self.samples).deserialize(&mut deserializer)?;
        let response = self.response.insert(response);
        ensure!(
            deserializer.finalize()?.is_empty(),
            "Audio frame has trailing data"
        );
        Ok(response)
    }

    pub async fn read_frame(
        &mut self,
        stream: &mut (impl AsyncRead + Unpin),
        bytes: &mut Vec<u8>,
    ) -> Result<&Response> {
        read_frame_with(stream, bytes, move |bytes| self.decode(bytes)).await
    }
}

// Serde's enum derive does not implement deserialize_in_place. Keep postcard in
// charge of all wire decoding, seeding only the PCM sample Vec into its standard
// in-place deserializer. Control frames leave that Vec available for later PCM.
struct ResponseSeed<'a>(&'a mut Vec<f32>);

impl<'de> DeserializeSeed<'de> for ResponseSeed<'_> {
    type Value = Response;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Response, D::Error> {
        deserializer.deserialize_enum(
            "Response",
            &["Hello", "Start", "Pcm", "Complete", "Error"],
            self,
        )
    }
}

impl<'de> Visitor<'de> for ResponseSeed<'_> {
    type Value = Response;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("an audio response")
    }

    fn visit_enum<A: EnumAccess<'de>>(self, access: A) -> Result<Response, A::Error> {
        #[derive(Deserialize)]
        #[serde(field_identifier)]
        enum Variant {
            Hello,
            Start,
            Pcm,
            Complete,
            Error,
        }
        let (variant, fields) = access.variant()?;
        Ok(match variant {
            Variant::Hello => {
                let (version, instance) = fields.tuple_variant(2, ResponseFields::new())?;
                Response::Hello { version, instance }
            }
            Variant::Start => {
                let (source, identity, sample_rate, layout, duration) =
                    fields.tuple_variant(5, ResponseFields::new())?;
                Response::Start {
                    source,
                    identity,
                    sample_rate,
                    layout,
                    duration,
                }
            }
            Variant::Pcm => return fields.tuple_variant(4, self),
            Variant::Complete => {
                let (frames, duration) = fields.tuple_variant(2, ResponseFields::new())?;
                Response::Complete { frames, duration }
            }
            Variant::Error => Response::Error(fields.newtype_variant()?),
        })
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut fields: A) -> Result<Response, A::Error> {
        let sequence = fields
            .next_element()?
            .ok_or_else(|| serde::de::Error::invalid_length(0, &self))?;
        let media_start = fields
            .next_element()?
            .ok_or_else(|| serde::de::Error::invalid_length(1, &self))?;
        let media_step = fields
            .next_element()?
            .ok_or_else(|| serde::de::Error::invalid_length(2, &self))?;
        fields
            .next_element_seed(SamplesSeed(&mut *self.0))?
            .ok_or_else(|| serde::de::Error::invalid_length(3, &"PCM samples"))?;
        Ok(Response::Pcm {
            sequence,
            media_start,
            media_step,
            samples: std::mem::take(self.0),
        })
    }
}

struct ResponseFields<T>(std::marker::PhantomData<T>);

impl<T> ResponseFields<T> {
    fn new() -> Self {
        Self(std::marker::PhantomData)
    }
}

impl<'de, T: Deserialize<'de>> Visitor<'de> for ResponseFields<T> {
    type Value = T;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("audio response fields")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, fields: A) -> Result<T, A::Error> {
        T::deserialize(serde::de::value::SeqAccessDeserializer::new(fields))
    }
}

struct SamplesSeed<'a>(&'a mut Vec<f32>);

impl<'de> DeserializeSeed<'de> for SamplesSeed<'_> {
    type Value = ();

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        Vec::<f32>::deserialize_in_place(deserializer, self.0)?;
        if self.0.len() > MAX_PCM_SAMPLES {
            return Err(serde::de::Error::custom(
                "PCM sample count exceeds its limit",
            ));
        }
        Ok(())
    }
}

pub fn address(data_dir: &Path) -> Result<SocketAddr> {
    crate::wire::service_address(data_dir, b".audio.v1")
}
pub async fn connect(data_dir: &Path) -> Result<Async<UnixStream>> {
    crate::wire::endpoint::connect_address(&address(data_dir)?).await
}

/// Encode directly into session storage, checking the limit before each append.
/// On failure no partial frame remains available to send.
pub fn encode_frame_into<T: Serialize>(value: &T, bytes: &mut Vec<u8>) -> Result<()> {
    crate::framing::encode(value, &[], MAX_FRAME, bytes).context("Encoding audio frame")
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
    read_frame_with(stream, bytes, decode).await
}

async fn read_frame_with<T>(
    stream: &mut (impl AsyncRead + Unpin),
    bytes: &mut Vec<u8>,
    decode: impl FnOnce(&[u8]) -> Result<T>,
) -> Result<T> {
    bytes.clear();
    release_large_buffer(bytes);
    let mut prefix = [0; 4];
    stream.read_exact(&mut prefix[..1]).await?;
    let result = timeout(IO_TIMEOUT, async {
        stream.read_exact(&mut prefix[1..]).await?;
        let length = crate::framing::decode_length(prefix, MAX_FRAME)?;
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
        crate::framing::validate_encoded(bytes, MAX_FRAME)?;
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
    use std::io::Write;

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
    fn reusable_decoder_preserves_wire_responses_across_chunk_sizes_and_control_frames() {
        let mut decoder = ResponseDecoder::default();
        let mut bytes = Vec::new();
        let mut actual = Vec::new();
        let responses = [
            Response::Hello {
                version: VERSION,
                instance: 7,
            },
            Response::Start {
                source: Some(9),
                identity: SourceIdentity::Pcm {
                    storage: 8,
                    start: 10,
                    frames: 100,
                },
                sample_rate: 48_000,
                layout: vec![0, 1],
                duration: Some(1.25),
            },
            Response::Pcm {
                sequence: 1,
                media_start: 0.25,
                media_step: 1.0 / 48_000.0,
                samples: vec![0.125; 64],
            },
            Response::Pcm {
                sequence: 2,
                media_start: 0.5,
                media_step: 1.0 / 48_000.0,
                samples: vec![-0.25; 2],
            },
            Response::Complete {
                frames: 33,
                duration: 1.25,
            },
            Response::Error("capture failed".into()),
            Response::Pcm {
                sequence: u64::MAX,
                media_start: 0.75,
                media_step: 1.0 / 48_000.0,
                samples: vec![0.5; MAX_PCM_SAMPLES],
            },
        ];
        for response in responses {
            encode_frame_into(&response, &mut bytes).unwrap();
            let decoded = decoder.decode(&bytes[4..]).unwrap();
            encode_frame_into(decoded, &mut actual).unwrap();
            assert_eq!(actual, bytes);
        }
    }

    #[test]
    fn reusable_decoder_rejects_malformed_frames_and_recovers() {
        let mut decoder = ResponseDecoder::default();
        let mut bytes = Vec::new();
        encode_frame_into(
            &Response::Pcm {
                sequence: 1,
                media_start: 0.0,
                media_step: 1.0,
                samples: vec![0.25; 8],
            },
            &mut bytes,
        )
        .unwrap();
        let body = &bytes[4..];
        decoder.decode(body).unwrap();
        for end in 0..body.len() {
            assert!(decoder.decode(&body[..end]).is_err());
        }
        let mut trailing = body.to_vec();
        trailing.push(0);
        assert!(decoder.decode(&trailing).is_err());
        assert!(decoder.decode(&[255; 10]).is_err());
        assert!(decoder.decode(&vec![0; MAX_FRAME + 1]).is_err());
        assert!(matches!(
            decoder.decode(body).unwrap(),
            Response::Pcm { samples, .. } if samples.as_slice() == [0.25; 8]
        ));
        encode_frame_into(
            &Response::Pcm {
                sequence: 2,
                media_start: 0.0,
                media_step: 1.0,
                samples: vec![0.0; MAX_PCM_SAMPLES + 1],
            },
            &mut bytes,
        )
        .unwrap();
        assert!(decoder.decode(&bytes[4..]).is_err());
    }

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
