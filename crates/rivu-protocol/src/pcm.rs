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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PcmHeader {
    pub sequence: u64,
    pub epoch: u64,
    pub intent: u64,
    pub generation: u64,
    pub capture: u64,
    pub start_frame: u64,
    /// First frame in the current output instance, before hardware volume or routing.
    pub output_frame: u64,
    pub sample_rate: u32,
    pub gap: bool,
}
pub const PCM_BLOCK_FRAMES: usize = 1024;

use anyhow::{Context, Result, ensure};
use futures_lite::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use std::{
    os::{linux::net::SocketAddrExt, unix::net::SocketAddr},
    path::Path,
    time::Duration,
};
pub const DEADLINE: Duration = Duration::from_secs(5);
pub const MAGIC: &[u8; 8] = b"RIVUPCM4";
pub const REQUEST_SIZE: usize = 16;
pub const STATUS_SIZE: usize = 72;
pub fn address(data_dir: &Path) -> Result<SocketAddr> {
    let base = crate::wire::address(data_dir)?;
    let mut name = base
        .as_abstract_name()
        .context("PCM requires an abstract command socket")?
        .to_vec();
    name.extend_from_slice(b".pcm.v4");
    Ok(SocketAddr::from_abstract_name(name)?)
}
pub fn request(operation: u8, bits: u8, value: u64) -> [u8; REQUEST_SIZE] {
    let mut bytes = [0; REQUEST_SIZE];
    bytes[0] = operation;
    bytes[1] = bits;
    bytes[8..].copy_from_slice(&value.to_le_bytes());
    bytes
}
pub fn word(bytes: &[u8]) -> u64 {
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
    minimum_sequence: u64,
) -> Result<()> {
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
    bytes[48..56].copy_from_slice(&revision.to_le_bytes());
    bytes[56..64].copy_from_slice(&minimum_sequence.to_le_bytes());
    bytes[64..72].copy_from_slice(&state.heard_frame.to_le_bytes());
    crate::transport::timeout(DEADLINE, stream.write_all(&bytes)).await??;
    Ok(())
}
pub fn decode_status(bytes: &[u8; STATUS_SIZE]) -> Result<(PcmState, u64, u64)> {
    ensure!(
        bytes[44] <= 1 && bytes[45..48] == [0; 3],
        "Invalid PCM status"
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
        word(&bytes[56..64]),
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
            write_status(&mut bytes, state, 8, 9).await.unwrap();
            let bytes: [u8; STATUS_SIZE] = bytes.try_into().unwrap();
            assert_eq!(decode_status(&bytes).unwrap(), (state, 8, 9));
            assert_eq!(&bytes[45..48], &[0; 3]);
            assert_eq!(&bytes[64..72], &state.heard_frame.to_le_bytes());
            for index in [44, 45, 47] {
                let mut invalid = bytes;
                invalid[index] = 2;
                assert!(decode_status(&invalid).is_err(), "byte {index}");
            }
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
            let expected = request(1, 1, 123);
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
