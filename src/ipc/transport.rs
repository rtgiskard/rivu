use super::protocol::{
    HelloResponse, MAX_REQUEST, MAX_RESPONSE, PROTOCOL_VERSION, RequestType, ResponseType,
    UNKNOWN_INSTANCE, WireRequest, WireResponse,
};
use anyhow::{Context, Result, bail, ensure};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

const BRIDGE_ALLOWANCE: Duration = Duration::from_secs(1);
const FRAME_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const TRANSPORT_ALLOWANCE: Duration = Duration::from_secs(1);

pub(super) fn bridge_timeout(operation_timeout: Duration) -> Duration {
    operation_timeout + BRIDGE_ALLOWANCE
}

pub(super) fn response_timeout(operation_timeout: Duration) -> Duration {
    bridge_timeout(operation_timeout) + FRAME_WRITE_TIMEOUT + TRANSPORT_ALLOWANCE
}

fn decode_message_header(bytes: &[u8], limit: usize) -> Result<(u8, &[u8])> {
    ensure!(bytes.len() >= 2, "IPC message header is truncated");
    let version = bytes[0];
    ensure!(
        version == PROTOCOL_VERSION,
        "Unsupported IPC protocol version {version}"
    );
    ensure!(bytes.len() <= limit, "IPC frame exceeds maximum size");
    Ok((bytes[1], &bytes[2..]))
}

fn decode_body<T: for<'de> Deserialize<'de>>(bytes: &[u8], context: &'static str) -> Result<T> {
    // Postcard's slice decoder checks string lengths before allocating and
    // suppresses sequence size hints larger than the remaining input. These
    // nonrecursive schemas have no maps or zero-byte collection elements, so
    // allocation and iteration scale with actual frame bytes, not raw lengths.
    // Decoded heap usage can exceed the wire-size limit. Reassess this bound
    // before adding recursive types, zero-byte elements, or custom deserializers.
    let (value, remaining) = postcard::take_from_bytes(bytes).context(context)?;
    ensure!(remaining.is_empty(), "Trailing bytes in IPC message body");
    Ok(value)
}

pub(super) fn decode_request_frame(bytes: &[u8]) -> Result<WireRequest> {
    let (message_type, body) = decode_message_header(bytes, MAX_REQUEST)?;
    let message_type = RequestType::try_from(message_type)?;
    let request = match message_type {
        RequestType::Hello => WireRequest::Hello(decode_body(body, "Decoding IPC Hello request")?),
        RequestType::State => WireRequest::State(decode_body(body, "Decoding IPC state request")?),
        RequestType::Query => WireRequest::Query(decode_body(body, "Decoding IPC query request")?),
        RequestType::Command => {
            WireRequest::Command(decode_body(body, "Decoding IPC command request")?)
        }
        RequestType::Watch => WireRequest::Watch(decode_body(body, "Decoding IPC watch request")?),
    };
    Ok(request)
}

pub(super) fn decode_response_frame(bytes: &[u8]) -> Result<WireResponse> {
    let (message_type, body) = decode_message_header(bytes, MAX_RESPONSE)?;
    let message_type = ResponseType::try_from(message_type)?;
    let response = match message_type {
        ResponseType::Hello => {
            WireResponse::Hello(decode_body(body, "Decoding IPC Hello response")?)
        }
        ResponseType::State => {
            WireResponse::State(decode_body(body, "Decoding IPC state response")?)
        }
        ResponseType::Query => {
            WireResponse::Query(decode_body(body, "Decoding IPC query response")?)
        }
        ResponseType::Ack => WireResponse::Ack(decode_body(body, "Decoding IPC acknowledgement")?),
        ResponseType::Watch => {
            WireResponse::Watch(decode_body(body, "Decoding IPC watch response")?)
        }
        ResponseType::Error => {
            WireResponse::Error(decode_body(body, "Decoding IPC error response")?)
        }
    };
    Ok(response)
}

// Reuse small frames, but never retain a bulk transfer for an idle session.
pub(super) const RETAINED_BUFFER_LIMIT: usize = 256 * 1024;
const FRAME_GROWTH_SLACK: usize = 64 * 1024;

pub(super) fn release_large_buffer(buffer: &mut Vec<u8>) {
    if buffer.capacity() > RETAINED_BUFFER_LIMIT {
        *buffer = Vec::new();
    }
}

struct FrameWriter<'a> {
    bytes: &'a mut Vec<u8>,
    limit: usize,
}

impl Write for FrameWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > 4 + self.limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "IPC frame exceeds maximum size",
            ));
        }
        let required = self.bytes.len() + bytes.len();
        if required > self.bytes.capacity() && required > FRAME_GROWTH_SLACK {
            let target = required
                .saturating_add(FRAME_GROWTH_SLACK)
                .min(4 + self.limit);
            self.bytes.reserve_exact(target - self.bytes.len());
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encode_message_into<T: Serialize>(
    version: u8,
    message_type: u8,
    value: &T,
    bytes: &mut Vec<u8>,
    limit: usize,
) -> Result<()> {
    bytes.clear();
    bytes.resize(6, 0);
    bytes[4] = version;
    bytes[5] = message_type;
    let mut writer = FrameWriter { bytes, limit };
    postcard::to_io(value, &mut writer).context("Encoding IPC message")?;
    let length = u32::try_from(writer.bytes.len() - 4).context("IPC frame is too large")?;
    writer.bytes[..4].copy_from_slice(&length.to_le_bytes());
    Ok(())
}

pub(super) fn encode_request_frame_into(request: &WireRequest, bytes: &mut Vec<u8>) -> Result<()> {
    match request {
        WireRequest::Hello(value) => encode_message_into(
            PROTOCOL_VERSION,
            RequestType::Hello as u8,
            value,
            bytes,
            MAX_REQUEST,
        ),
        WireRequest::State(value) => encode_message_into(
            PROTOCOL_VERSION,
            RequestType::State as u8,
            value,
            bytes,
            MAX_REQUEST,
        ),
        WireRequest::Query(value) => encode_message_into(
            PROTOCOL_VERSION,
            RequestType::Query as u8,
            value,
            bytes,
            MAX_REQUEST,
        ),
        WireRequest::Command(value) => encode_message_into(
            PROTOCOL_VERSION,
            RequestType::Command as u8,
            value,
            bytes,
            MAX_REQUEST,
        ),
        WireRequest::Watch(value) => encode_message_into(
            PROTOCOL_VERSION,
            RequestType::Watch as u8,
            value,
            bytes,
            MAX_REQUEST,
        ),
    }
}

pub(super) fn encode_response_frame_into(
    response: &WireResponse,
    bytes: &mut Vec<u8>,
) -> Result<()> {
    match response {
        WireResponse::Hello(value) => encode_message_into(
            PROTOCOL_VERSION,
            ResponseType::Hello as u8,
            value,
            bytes,
            MAX_RESPONSE,
        ),
        WireResponse::State(value) => encode_message_into(
            PROTOCOL_VERSION,
            ResponseType::State as u8,
            value,
            bytes,
            MAX_RESPONSE,
        ),
        WireResponse::Query(value) => encode_message_into(
            PROTOCOL_VERSION,
            ResponseType::Query as u8,
            value,
            bytes,
            MAX_RESPONSE,
        ),
        WireResponse::Ack(value) => encode_message_into(
            PROTOCOL_VERSION,
            ResponseType::Ack as u8,
            value,
            bytes,
            MAX_RESPONSE,
        ),
        WireResponse::Watch(value) => encode_message_into(
            PROTOCOL_VERSION,
            ResponseType::Watch as u8,
            value,
            bytes,
            MAX_RESPONSE,
        ),
        WireResponse::Error(value) => encode_message_into(
            PROTOCOL_VERSION,
            ResponseType::Error as u8,
            value,
            bytes,
            MAX_RESPONSE,
        ),
    }
}
pub(super) async fn read_frame<S: AsyncRead + Unpin>(
    stream: &mut S,
    limit: usize,
    pending: &mut BytesMut,
) -> Result<Bytes> {
    let length = fill_frame(stream, limit, pending).await?;
    pending.advance(4);
    Ok(pending.split_to(length).freeze())
}

async fn fill_frame<S: AsyncRead + Unpin>(
    stream: &mut S,
    limit: usize,
    pending: &mut BytesMut,
) -> Result<usize> {
    while pending.len() < 4 {
        let need = 4 - pending.len();
        let read = stream
            .read_buf(&mut (&mut *pending).limit(need))
            .await
            .context("Reading IPC frame")?;
        if read == 0 {
            bail!("IPC connection closed while reading frame");
        }
    }
    let length = u32::from_le_bytes(pending[..4].try_into().unwrap()) as usize;
    if length > limit {
        bail!("IPC frame exceeds maximum size");
    }
    pending.reserve((4 + length).saturating_sub(pending.len()));
    while pending.len() < 4 + length {
        let need = 4 + length - pending.len();
        let read = stream
            .read_buf(&mut (&mut *pending).limit(need))
            .await
            .context("Reading IPC frame")?;
        if read == 0 {
            bail!("IPC connection closed while reading frame");
        }
    }
    Ok(length)
}

pub(super) async fn write_frame_buffered<S: AsyncWrite + Unpin>(
    stream: &mut S,
    value: &WireResponse,
    buffer: &mut Vec<u8>,
) -> Result<()> {
    encode_response_frame_into(value, buffer)?;
    // Finish accepted replies, including shutdown acknowledgements, but never
    // let a nonreading peer hold a session (or server teardown) indefinitely.
    let result = timeout(FRAME_WRITE_TIMEOUT, async {
        stream
            .write_all(buffer)
            .await
            .context("Writing IPC frame")?;
        stream.flush().await.context("Flushing IPC frame")
    })
    .await
    .context("IPC response write timed out");
    release_large_buffer(buffer);
    result?
}

pub(super) async fn write_error_frame<S: AsyncWrite + Unpin>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
    error: impl Into<String>,
) -> Result<()> {
    let response = error_frame(error);
    write_frame_buffered(stream, &response, buffer).await
}

pub(super) async fn write_request_frame_buffered<S: AsyncWrite + Unpin>(
    stream: &mut S,
    request: &WireRequest,
    buffer: &mut Vec<u8>,
    write_started: &mut bool,
) -> Result<()> {
    encode_request_frame_into(request, buffer)?;
    // Encoding failure is known not sent; any attempted socket write is unknown.
    *write_started = true;
    let result = timeout(FRAME_WRITE_TIMEOUT, async {
        stream
            .write_all(buffer)
            .await
            .context("Writing IPC frame")?;
        stream.flush().await.context("Flushing IPC frame")
    })
    .await
    .context("IPC request write timed out");
    release_large_buffer(buffer);
    result?
}

pub(super) async fn read_response_frame<S: AsyncRead + Unpin>(
    stream: &mut S,
    pending: &mut BytesMut,
) -> Result<WireResponse> {
    let length = fill_frame(stream, MAX_RESPONSE, pending).await?;
    let consumed = 4 + length;
    let result = decode_response_frame(&pending[4..consumed]);
    if pending.len() == consumed {
        // Decode before consuming so capacity still covers the whole allocation,
        // rather than just an empty split tail. No frame ownership escapes here.
        pending.clear();
        // A caller may have prebuffered multiple frames; reclaim any prefix
        // advanced past on earlier calls without allocating or copying payload.
        let _ = pending.try_reclaim(RETAINED_BUFFER_LIMIT + 1);
        if pending.capacity() > RETAINED_BUFFER_LIMIT {
            *pending = BytesMut::new();
        }
    } else {
        pending.advance(consumed);
    }
    result
}

pub(super) fn error_frame(error: impl Into<String>) -> WireResponse {
    WireResponse::Error(error.into())
}

pub(super) fn unpack_error(response: WireResponse) -> Result<WireResponse> {
    if let WireResponse::Error(error) = response {
        bail!("IPC server error: {error}");
    }
    Ok(response)
}

pub(super) fn validate_hello(response: HelloResponse) -> Result<HelloResponse> {
    ensure!(
        u32::from_le_bytes(response.instance_id) != UNKNOWN_INSTANCE,
        "IPC Hello returned instance 0"
    );
    Ok(response)
}

pub(super) fn unpack_hello(response: WireResponse) -> Result<HelloResponse> {
    let WireResponse::Hello(response) = unpack_error(response)? else {
        bail!("IPC response was not a Hello response")
    };
    validate_hello(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        pin::Pin,
        task::{Context as TaskContext, Poll},
    };

    #[tokio::test]
    async fn bulk_response_releases_buffers_before_idle_and_accepts_next_frame() {
        let payload = "X".repeat(6 * 1024 * 1024);
        let (mut sender, mut receiver) = tokio::io::duplex(4096);
        let send = async {
            let mut buffer = Vec::new();
            write_frame_buffered(
                &mut sender,
                &WireResponse::Error(payload.clone()),
                &mut buffer,
            )
            .await
            .unwrap();
            assert!(buffer.capacity() <= RETAINED_BUFFER_LIMIT);
            write_frame_buffered(
                &mut sender,
                &WireResponse::Error("next".into()),
                &mut buffer,
            )
            .await
            .unwrap();
        };
        let receive = async {
            let mut pending = BytesMut::new();
            let WireResponse::Error(actual) = read_response_frame(&mut receiver, &mut pending)
                .await
                .unwrap()
            else {
                panic!("error response")
            };
            assert_eq!(actual, payload);
            assert!(pending.capacity() <= RETAINED_BUFFER_LIMIT);
            let WireResponse::Error(actual) = read_response_frame(&mut receiver, &mut pending)
                .await
                .unwrap()
            else {
                panic!("error response")
            };
            assert_eq!(actual, "next");
        };
        tokio::join!(send, receive);
    }

    #[tokio::test]
    async fn subthreshold_responses_release_doubled_backing_before_idle() {
        let mut pending = BytesMut::new();
        let mut encoded = Vec::new();
        for (index, size) in [150 * 1024, 230 * 1024].into_iter().enumerate() {
            let payload = "X".repeat(size);
            encode_response_frame_into(&WireResponse::Error(payload.clone()), &mut encoded)
                .unwrap();
            assert!(encoded.len() < RETAINED_BUFFER_LIMIT);
            let mut stream = encoded.as_slice();
            fill_frame(&mut stream, MAX_RESPONSE, &mut pending)
                .await
                .unwrap();
            if index == 1 {
                // Both frames fit the limit individually, but reusing the first
                // allocation doubles its capacity beyond the idle bound.
                assert!(pending.capacity() > RETAINED_BUFFER_LIMIT);
            }
            assert!(matches!(
                read_response_frame(&mut stream, &mut pending).await.unwrap(),
                WireResponse::Error(actual) if actual == payload
            ));
            assert!(stream.is_empty());
            assert!(pending.is_empty());
            // An empty split tail can hide an oversized backing allocation.
            // Ask to reclaim enough to expose any such allocation, without
            // allocating or relying on a process-global allocator in the test.
            let _ = pending.try_reclaim(RETAINED_BUFFER_LIMIT + 1);
            assert!(pending.capacity() <= RETAINED_BUFFER_LIMIT);
        }
        encode_response_frame_into(&WireResponse::Error("next".into()), &mut encoded).unwrap();
        assert!(matches!(
            read_response_frame(&mut encoded.as_slice(), &mut pending)
                .await
                .unwrap(),
            WireResponse::Error(actual) if actual == "next"
        ));
    }

    #[tokio::test]
    async fn malformed_response_releases_backing_and_preserves_buffered_next_frame() {
        let mut encoded = Vec::new();
        encode_response_frame_into(&WireResponse::Error("invalid".into()), &mut encoded).unwrap();
        encoded[4] = PROTOCOL_VERSION.wrapping_add(1);
        let mut pending = BytesMut::with_capacity(RETAINED_BUFFER_LIMIT + 1);
        pending.extend_from_slice(&encoded);
        encode_response_frame_into(&WireResponse::Error("next".into()), &mut encoded).unwrap();
        pending.extend_from_slice(&encoded);
        let mut stream = &[][..];
        assert!(
            read_response_frame(&mut stream, &mut pending)
                .await
                .is_err()
        );
        assert_eq!(pending.as_ref(), encoded.as_slice());
        assert!(matches!(
            read_response_frame(&mut stream, &mut pending).await.unwrap(),
            WireResponse::Error(actual) if actual == "next"
        ));
        assert!(pending.is_empty());
        let _ = pending.try_reclaim(RETAINED_BUFFER_LIMIT + 1);
        assert!(pending.capacity() <= RETAINED_BUFFER_LIMIT);
    }

    #[test]
    fn bulk_frame_growth_is_bounded_and_limit_is_still_enforced() {
        let mut buffer = Vec::new();
        let payload = "X".repeat(6 * 1024 * 1024);
        encode_message_into(
            PROTOCOL_VERSION,
            0,
            &(payload.as_str(), 1u8),
            &mut buffer,
            MAX_RESPONSE,
        )
        .unwrap();
        assert!(buffer.capacity() <= buffer.len() + FRAME_GROWTH_SLACK);
        assert!(
            encode_message_into(PROTOCOL_VERSION, 0, &payload, &mut buffer, MAX_REQUEST).is_err()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_bulk_response_releases_encoding_buffer() {
        let (mut sender, _peer) = tokio::io::duplex(8);
        let mut buffer = Vec::new();
        let result = write_frame_buffered(
            &mut sender,
            &WireResponse::Error("X".repeat(1024 * 1024)),
            &mut buffer,
        )
        .await;
        assert!(
            result
                .unwrap_err()
                .downcast_ref::<tokio::time::error::Elapsed>()
                .is_some()
        );
        assert!(buffer.capacity() <= RETAINED_BUFFER_LIMIT);
    }

    #[tokio::test(start_paused = true)]
    async fn response_write_timeout_ends_a_partial_frame_when_peer_stops_reading() {
        let (mut stream, mut peer) = tokio::io::duplex(8);
        let mut encoded = Vec::new();
        let error = write_frame_buffered(
            &mut stream,
            &WireResponse::Error("response".into()),
            &mut encoded,
        )
        .await
        .unwrap_err();
        assert!(
            error
                .downcast_ref::<tokio::time::error::Elapsed>()
                .is_some()
        );
        drop(stream);
        let mut received = Vec::new();
        peer.read_to_end(&mut received).await.unwrap();
        assert!(received.len() < encoded.len());
        assert_eq!(received, encoded[..received.len()]);
    }

    struct PendingFlush;

    impl AsyncWrite for PendingFlush {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut TaskContext<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(bytes.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn response_write_deadline_includes_flush() {
        let error = write_frame_buffered(
            &mut PendingFlush,
            &WireResponse::Error("response".into()),
            &mut Vec::new(),
        )
        .await
        .unwrap_err();
        assert!(
            error
                .downcast_ref::<tokio::time::error::Elapsed>()
                .is_some()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_request_write_is_unknown_and_releases_bulk_buffer() {
        let (mut stream, mut peer) = tokio::io::duplex(8);
        let request = WireRequest::Command(super::super::protocol::WireCommand::CreatePlaylist {
            name: "X".repeat(1024 * 1024),
        });
        let mut buffer = Vec::new();
        let mut write_started = false;
        let error =
            write_request_frame_buffered(&mut stream, &request, &mut buffer, &mut write_started)
                .await
                .unwrap_err();
        assert!(
            error
                .downcast_ref::<tokio::time::error::Elapsed>()
                .is_some()
        );
        assert!(write_started);
        assert!(buffer.capacity() <= RETAINED_BUFFER_LIMIT);
        drop(stream);
        let mut received = Vec::new();
        peer.read_to_end(&mut received).await.unwrap();
        assert!(!received.is_empty());
        assert!(
            received.len() < 4 + u32::from_le_bytes(received[..4].try_into().unwrap()) as usize
        );
    }

    #[tokio::test(start_paused = true)]
    async fn request_write_deadline_includes_flush() {
        let mut write_started = false;
        let error = write_request_frame_buffered(
            &mut PendingFlush,
            &WireRequest::State(crate::response::StateSections::ALL),
            &mut Vec::new(),
            &mut write_started,
        )
        .await
        .unwrap_err();
        assert!(
            error
                .downcast_ref::<tokio::time::error::Elapsed>()
                .is_some()
        );
        assert!(write_started);
    }
}
