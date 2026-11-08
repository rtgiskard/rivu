//! Bounded native-channel PCM window, independent of display analysis.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PcmState {
    pub epoch: u64,
    pub intent: u64,
    pub applied_intent: u64,
    pub generation: u64,
    pub capture: u64,
    /// Audible frame cursor in the current output instance, preserved across capture restarts.
    pub heard_frame: u64,
    pub sample_rate: u32,
    pub active: bool,
}
impl PcmState {
    pub fn accepts(self, header: PcmHeader) -> bool {
        self.active
            && self.epoch.is_multiple_of(2)
            && self.intent == self.applied_intent
            && header.epoch == self.epoch
            && header.intent == self.intent
            && header.generation == self.generation
            && header.capture == self.capture
            && header.sample_rate == self.sample_rate
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PcmHeader {
    /// Logical block identity: a partial block can grow without a new sequence.
    /// Consumers retain a frame offset to admit only its newly published tail.
    pub sequence: u64,
    pub epoch: u64,
    pub intent: u64,
    pub generation: u64,
    pub capture: u64,
    pub start_frame: u64,
    /// First frame in the current output instance, before hardware volume or routing.
    pub output_frame: u64,
    /// Selected-source relative media seconds for the first PCM frame.
    pub media_start: f64,
    /// Media seconds advanced per PCM frame.
    pub media_step: f64,
    pub sample_rate: u32,
    pub gap: bool,
}
pub const PCM_BLOCK_FRAMES: usize = 1024;
pub const MAX_PCM_CHANNELS: usize = 18;

/// Canonical native speaker IDs, with each speaker present at most once.
pub fn valid_layout(layout: &[u32]) -> bool {
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
/// Maximum native output prefetch; shared retention derives its bound from this budget.
pub const PCM_OUTPUT_MAX_BYTES: usize = 16 * 1024 * 1024;

/// An observed prefix of a logical PCM block, including a growing partial tail.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct PcmPosition {
    pub sequence: u64,
    pub offset: u16,
}

use anyhow::{Context, Result, ensure};
use futures_lite::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use std::{
    os::{linux::net::SocketAddrExt, unix::net::SocketAddr},
    path::Path,
    time::Duration,
};
pub const DEADLINE: Duration = Duration::from_secs(5);
pub const MAGIC: &[u8; 8] = b"RIVUPCM6";
pub const REQUEST_SIZE: usize = 16;
pub const STATUS_SIZE: usize = 72;
pub const WELCOME_SIZE: usize = 16;
pub const MAX_SHARED_PATH: usize = 4096;
pub const MAX_CLIENTS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlRequest {
    Hello { instance: u32 },
    Subscribe { enabled: bool, revision: u64 },
}

impl ControlRequest {
    pub fn encode(self) -> [u8; REQUEST_SIZE] {
        let mut bytes = [0; REQUEST_SIZE];
        match self {
            Self::Hello { instance } => {
                bytes[8..].copy_from_slice(&u64::from(instance).to_le_bytes())
            }
            Self::Subscribe { enabled, revision } => {
                bytes[0] = 1;
                bytes[1] = u8::from(enabled);
                bytes[8..].copy_from_slice(&revision.to_le_bytes());
            }
        }
        bytes
    }

    pub fn decode(bytes: &[u8; REQUEST_SIZE]) -> Result<Self> {
        ensure!(bytes[2..8] == [0; 6], "Invalid PCM request padding");
        let value = word(&bytes[8..]);
        match bytes[0] {
            0 => {
                ensure!(bytes[1] == 0 && value != 0, "Invalid PCM Hello");
                Ok(Self::Hello {
                    instance: u32::try_from(value).context("Invalid PCM instance")?,
                })
            }
            1 => {
                ensure!(bytes[1] <= 1, "Invalid PCM subscription flags");
                Ok(Self::Subscribe {
                    enabled: bytes[1] != 0,
                    revision: value,
                })
            }
            _ => anyhow::bail!("Unknown PCM control request"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Welcome {
    pub instance: u32,
    pub path_length: usize,
}

impl Welcome {
    fn validate(self) -> Result<Self> {
        ensure!(self.instance != 0, "Invalid PCM welcome instance");
        ensure!(
            self.path_length != 0 && self.path_length <= MAX_SHARED_PATH,
            "Invalid shared PCM path length"
        );
        Ok(self)
    }

    pub fn encode(self) -> Result<[u8; WELCOME_SIZE]> {
        self.validate()?;
        let mut bytes = [0; WELCOME_SIZE];
        bytes[..8].copy_from_slice(MAGIC);
        bytes[8..12].copy_from_slice(&self.instance.to_le_bytes());
        bytes[12..].copy_from_slice(&(self.path_length as u32).to_le_bytes());
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8; WELCOME_SIZE]) -> Result<Self> {
        ensure!(&bytes[..8] == MAGIC, "Invalid PCM welcome magic");
        Self {
            instance: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            path_length: u32::from_le_bytes(bytes[12..].try_into().unwrap()) as usize,
        }
        .validate()
    }
}
pub fn address(data_dir: &Path) -> Result<SocketAddr> {
    let base = crate::wire::address(data_dir)?;
    let mut name = base
        .as_abstract_name()
        .context("PCM requires an abstract command socket")?
        .to_vec();
    name.extend_from_slice(b".pcm.v6");
    Ok(SocketAddr::from_abstract_name(name)?)
}
fn word(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes.try_into().unwrap())
}
pub struct ControlReader<const N: usize> {
    bytes: [u8; N],
    used: usize,
    deadline: Option<std::time::Instant>,
}
impl<const N: usize> Default for ControlReader<N> {
    fn default() -> Self {
        Self::new()
    }
}
impl<const N: usize> ControlReader<N> {
    pub fn new() -> Self {
        Self {
            bytes: [0; N],
            used: 0,
            deadline: None,
        }
    }
    pub async fn read(&mut self, stream: &mut (impl AsyncRead + Unpin)) -> Result<[u8; N]> {
        while self.used < N {
            let read = stream.read(&mut self.bytes[self.used..]);
            let count = if let Some(deadline) = self.deadline {
                crate::transport::timeout_at(deadline, read).await??
            } else {
                read.await?
            };
            ensure!(count != 0, "PCM control disconnected");
            self.used += count;
            if self.deadline.is_none() {
                self.deadline = Some(std::time::Instant::now() + DEADLINE);
            }
        }
        self.used = 0;
        self.deadline = None;
        Ok(self.bytes)
    }
}
pub async fn write_status(
    stream: &mut (impl AsyncWrite + Unpin),
    state: PcmState,
    revision: u64,
    minimum: PcmPosition,
) -> Result<()> {
    ensure!(
        usize::from(minimum.offset) <= PCM_BLOCK_FRAMES
            && (minimum.sequence != 0 || minimum.offset == 0),
        "Invalid PCM frontier"
    );
    let mut bytes = [0; STATUS_SIZE];
    for (slot, value) in bytes[..40].as_chunks_mut::<8>().0.iter_mut().zip([
        state.epoch,
        state.intent,
        state.applied_intent,
        state.generation,
        state.capture,
    ]) {
        slot.copy_from_slice(&value.to_le_bytes());
    }
    bytes[40..44].copy_from_slice(&state.sample_rate.to_le_bytes());
    bytes[44] = u8::from(state.active);
    bytes[45..47].copy_from_slice(&minimum.offset.to_le_bytes());
    bytes[48..56].copy_from_slice(&revision.to_le_bytes());
    bytes[56..64].copy_from_slice(&minimum.sequence.to_le_bytes());
    bytes[64..72].copy_from_slice(&state.heard_frame.to_le_bytes());
    crate::transport::timeout(DEADLINE, stream.write_all(&bytes)).await??;
    Ok(())
}
pub fn decode_status(bytes: &[u8; STATUS_SIZE]) -> Result<(PcmState, u64, PcmPosition)> {
    ensure!(bytes[44] <= 1 && bytes[47] == 0, "Invalid PCM status");
    let minimum = PcmPosition {
        sequence: word(&bytes[56..64]),
        offset: u16::from_le_bytes(bytes[45..47].try_into().unwrap()),
    };
    ensure!(
        usize::from(minimum.offset) <= PCM_BLOCK_FRAMES
            && (minimum.sequence != 0 || minimum.offset == 0),
        "Invalid PCM frontier"
    );
    Ok((
        PcmState {
            epoch: word(&bytes[..8]),
            intent: word(&bytes[8..16]),
            applied_intent: word(&bytes[16..24]),
            generation: word(&bytes[24..32]),
            capture: word(&bytes[32..40]),
            heard_frame: word(&bytes[64..72]),
            sample_rate: u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
            active: bytes[44] != 0,
        },
        word(&bytes[48..56]),
        minimum,
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        pin::Pin,
        task::{Context as TaskContext, Poll},
    };
    #[test]
    fn control_requests_require_canonical_flags_padding_and_instance_width() {
        for request in [
            ControlRequest::Hello { instance: 7 },
            ControlRequest::Subscribe {
                enabled: false,
                revision: 0,
            },
            ControlRequest::Subscribe {
                enabled: true,
                revision: u64::MAX,
            },
        ] {
            let encoded = request.encode();
            assert_eq!(ControlRequest::decode(&encoded).unwrap(), request);
            for index in 2..8 {
                let mut invalid = encoded;
                invalid[index] = 1;
                assert!(ControlRequest::decode(&invalid).is_err());
            }
            let mut invalid = encoded;
            invalid[1] = 2;
            assert!(ControlRequest::decode(&invalid).is_err());
            let mut invalid = encoded;
            invalid[0] = 2;
            assert!(ControlRequest::decode(&invalid).is_err());
        }
        for instance in [0, u64::from(u32::MAX) + 1, u64::MAX] {
            let mut invalid = ControlRequest::Hello { instance: 7 }.encode();
            invalid[8..].copy_from_slice(&instance.to_le_bytes());
            assert!(ControlRequest::decode(&invalid).is_err());
        }
        let mut invalid = ControlRequest::Hello { instance: 7 }.encode();
        invalid[1] = 1;
        assert!(ControlRequest::decode(&invalid).is_err());
    }

    #[test]
    fn welcome_bounds_and_native_layout_ids_are_shared_and_canonical() {
        for path_length in [1, MAX_SHARED_PATH] {
            let welcome = Welcome {
                instance: u32::MAX,
                path_length,
            };
            let encoded = welcome.encode().unwrap();
            assert_eq!(Welcome::decode(&encoded).unwrap(), welcome);
            let mut invalid = encoded;
            invalid[0] ^= 1;
            assert!(Welcome::decode(&invalid).is_err());
            let mut invalid = encoded;
            invalid[8..12].fill(0);
            assert!(Welcome::decode(&invalid).is_err());
            for length in [0, MAX_SHARED_PATH as u32 + 1, u32::MAX] {
                let mut invalid = encoded;
                invalid[12..].copy_from_slice(&length.to_le_bytes());
                assert!(Welcome::decode(&invalid).is_err());
            }
        }
        for (instance, path_length) in [(0, 1), (7, 0), (7, MAX_SHARED_PATH + 1)] {
            assert!(
                Welcome {
                    instance,
                    path_length
                }
                .encode()
                .is_err()
            );
        }
        assert!(valid_layout(&[0]));
        assert!(valid_layout(
            &(0..MAX_PCM_CHANNELS as u32).rev().collect::<Vec<_>>()
        ));
        for invalid in [
            &[][..],
            &[0, 0],
            &[MAX_PCM_CHANNELS as u32],
            &[0; MAX_PCM_CHANNELS + 1],
        ] {
            assert!(!valid_layout(invalid));
        }
    }

    #[test]
    fn status_round_trips_and_rejects_noncanonical_flags_and_padding() {
        futures::executor::block_on(async {
            let state = PcmState {
                epoch: 2,
                intent: 3,
                applied_intent: 3,
                generation: 4,
                capture: 5,
                heard_frame: 1234,
                sample_rate: 48_000,
                active: true,
            };
            let mut bytes = Vec::new();
            let minimum = PcmPosition {
                sequence: 9,
                offset: 512,
            };
            write_status(&mut bytes, state, 8, minimum).await.unwrap();
            let bytes: [u8; STATUS_SIZE] = bytes.try_into().unwrap();
            assert_eq!(decode_status(&bytes).unwrap(), (state, 8, minimum));
            assert_eq!(bytes[47], 0);
            assert_eq!(&bytes[64..72], &state.heard_frame.to_le_bytes());
            for index in [44, 47] {
                let mut invalid = bytes;
                invalid[index] = 2;
                assert!(decode_status(&invalid).is_err(), "byte {index}");
            }
            let mut invalid = bytes;
            invalid[45..47].copy_from_slice(&1025_u16.to_le_bytes());
            assert!(decode_status(&invalid).is_err());
            let mut invalid = bytes;
            invalid[56..64].fill(0);
            assert!(decode_status(&invalid).is_err());
            for offset in [0, PCM_BLOCK_FRAMES as u16] {
                let frontier = PcmPosition { offset, ..minimum };
                let mut encoded = Vec::new();
                write_status(&mut encoded, state, 8, frontier)
                    .await
                    .unwrap();
                assert_eq!(
                    decode_status(&encoded.try_into().unwrap()).unwrap(),
                    (state, 8, frontier)
                );
            }
            assert!(
                write_status(
                    &mut Vec::new(),
                    state,
                    8,
                    PcmPosition {
                        sequence: 0,
                        offset: 1
                    },
                )
                .await
                .is_err()
            );
        });
    }

    struct Fragmented {
        bytes: [u8; REQUEST_SIZE],
        offset: usize,
        paused: bool,
    }
    impl AsyncRead for Fragmented {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
            output: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.paused {
                return Poll::Pending;
            }
            let count = output.len().min(if self.offset == 0 {
                3
            } else {
                REQUEST_SIZE - self.offset
            });
            output[..count].copy_from_slice(&self.bytes[self.offset..self.offset + count]);
            self.offset += count;
            self.paused = self.offset == 3;
            Poll::Ready(Ok(count))
        }
    }
    #[test]
    fn notification_cancellation_preserves_partial_control_frame() {
        futures::executor::block_on(async {
            let expected = ControlRequest::Subscribe {
                enabled: true,
                revision: 123,
            }
            .encode();
            let mut stream = Fragmented {
                bytes: expected,
                offset: 0,
                paused: false,
            };
            let mut reader = ControlReader::<REQUEST_SIZE>::new();
            {
                let read = reader.read(&mut stream);
                futures::pin_mut!(read);
                assert!(futures::poll!(read).is_pending());
            }
            stream.paused = false;
            assert_eq!(reader.read(&mut stream).await.unwrap(), expected);
        });
    }
}
