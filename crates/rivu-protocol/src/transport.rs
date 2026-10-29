use super::wire::{
    HelloResponse, MAX_REQUEST, MAX_RESPONSE, PROTOCOL_VERSION, RequestTag, ResponseTag,
    UNKNOWN_INSTANCE, WireRequest, WireResponse,
};
use anyhow::{Context, Result, bail, ensure};
use bytes::{Buf, BufMut, BytesMut};
use futures_lite::{
    future,
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
};
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Elapsed;

impl std::fmt::Display for Elapsed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("deadline has elapsed")
    }
}

impl std::error::Error for Elapsed {}

/// Bound the entire future, including partial I/O and flushing, by one deadline.
pub async fn timeout<T>(
    duration: Duration,
    operation: impl Future<Output = T>,
) -> Result<T, Elapsed> {
    timeout_at(Instant::now() + duration, operation).await
}

pub async fn timeout_at<T>(
    deadline: Instant,
    operation: impl Future<Output = T>,
) -> Result<T, Elapsed> {
    future::or(async { Ok(operation.await) }, async {
        async_io::Timer::at(deadline).await;
        Err(Elapsed)
    })
    .await
}

const BRIDGE_ALLOWANCE: Duration = Duration::from_secs(1);
const FRAME_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const TRANSPORT_ALLOWANCE: Duration = Duration::from_secs(1);

pub fn bridge_timeout(operation_timeout: Duration) -> Duration {
    operation_timeout + BRIDGE_ALLOWANCE
}

pub fn response_timeout(operation_timeout: Duration) -> Duration {
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

pub fn decode_request_frame(bytes: &[u8]) -> Result<WireRequest> {
    let (message_type, body) = decode_message_header(bytes, MAX_REQUEST)?;
    let message_type = RequestTag::try_from(message_type)?;
    let request = match message_type {
        RequestTag::Hello => WireRequest::Hello(decode_body(body, "Decoding IPC Hello request")?),
        RequestTag::State => WireRequest::State(decode_body(body, "Decoding IPC state request")?),
        RequestTag::Query => WireRequest::Query(decode_body(body, "Decoding IPC query request")?),
        RequestTag::Command => {
            WireRequest::Command(decode_body(body, "Decoding IPC command request")?)
        }
        RequestTag::Watch => WireRequest::Watch(decode_body(body, "Decoding IPC watch request")?),
        RequestTag::WaitTask => {
            WireRequest::WaitTask(decode_body(body, "Decoding IPC task wait request")?)
        }
    };
    Ok(request)
}

pub fn decode_response_frame(bytes: &[u8]) -> Result<WireResponse> {
    let (message_type, body) = decode_message_header(bytes, MAX_RESPONSE)?;
    let message_type = ResponseTag::try_from(message_type)?;
    let response = match message_type {
        ResponseTag::Hello => {
            WireResponse::Hello(decode_body(body, "Decoding IPC Hello response")?)
        }
        ResponseTag::State => {
            WireResponse::State(decode_body(body, "Decoding IPC state response")?)
        }
        ResponseTag::Query => {
            WireResponse::Query(decode_body(body, "Decoding IPC query response")?)
        }
        ResponseTag::Ack => WireResponse::Ack(decode_body(body, "Decoding IPC acknowledgement")?),
        ResponseTag::Watch => {
            WireResponse::Watch(decode_body(body, "Decoding IPC watch response")?)
        }
        ResponseTag::Task => WireResponse::Task(decode_body(body, "Decoding IPC task response")?),
        ResponseTag::Error => {
            WireResponse::Error(decode_body(body, "Decoding IPC error response")?)
        }
    };
    Ok(response)
}

// Reuse small frames, but never retain a bulk transfer for an idle session.
pub const RETAINED_BUFFER_LIMIT: usize = 256 * 1024;
const FRAME_GROWTH_THRESHOLD: usize = 64 * 1024;

pub fn release_large_buffer(buffer: &mut Vec<u8>) {
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
        if required > self.bytes.capacity() && required > FRAME_GROWTH_THRESHOLD {
            let target = required
                .max(self.bytes.capacity().saturating_mul(2))
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

impl FrameWriter<'_> {
    fn encode<T: Serialize>(&mut self, message_type: u8, value: &T) -> Result<()> {
        self.bytes.clear();
        self.bytes.resize(6, 0);
        self.bytes[4] = PROTOCOL_VERSION;
        self.bytes[5] = message_type;
        postcard::to_io(value, &mut *self).context("Encoding IPC message")?;
        let length = u32::try_from(self.bytes.len() - 4).context("IPC frame is too large")?;
        self.bytes[..4].copy_from_slice(&length.to_le_bytes());
        Ok(())
    }
}

pub fn encode_request_frame_into(request: &WireRequest, bytes: &mut Vec<u8>) -> Result<()> {
    let mut writer = FrameWriter {
        bytes,
        limit: MAX_REQUEST,
    };
    match request {
        WireRequest::Hello(value) => writer.encode(RequestTag::Hello as u8, value),
        WireRequest::State(value) => writer.encode(RequestTag::State as u8, value),
        WireRequest::Query(value) => writer.encode(RequestTag::Query as u8, value),
        WireRequest::Command(value) => writer.encode(RequestTag::Command as u8, value),
        WireRequest::Watch(value) => writer.encode(RequestTag::Watch as u8, value),
        WireRequest::WaitTask(value) => writer.encode(RequestTag::WaitTask as u8, value),
    }
}

pub fn encode_response_frame_into(response: &WireResponse, bytes: &mut Vec<u8>) -> Result<()> {
    let mut writer = FrameWriter {
        bytes,
        limit: MAX_RESPONSE,
    };
    match response {
        WireResponse::Hello(value) => writer.encode(ResponseTag::Hello as u8, value),
        WireResponse::State(value) => writer.encode(ResponseTag::State as u8, value),
        WireResponse::Query(value) => writer.encode(ResponseTag::Query as u8, value),
        WireResponse::Ack(value) => writer.encode(ResponseTag::Ack as u8, value),
        WireResponse::Watch(value) => writer.encode(ResponseTag::Watch as u8, value),
        WireResponse::Task(value) => writer.encode(ResponseTag::Task as u8, value),
        WireResponse::Error(value) => writer.encode(ResponseTag::Error as u8, value),
    }
}
/// Read and consume one request, retaining only reusable receive storage.
///
/// The outer error reports framing or I/O failure before a complete frame was
/// available. The inner error reports decoding of a completed, consumed frame,
/// allowing the server to reply with an invalid-request error.
pub async fn read_request_frame<S: AsyncRead + Unpin>(
    stream: &mut S,
    pending: &mut BytesMut,
) -> Result<Result<WireRequest>> {
    read_decoded_frame(stream, MAX_REQUEST, pending, decode_request_frame).await
}

async fn read_decoded_frame<S: AsyncRead + Unpin, T>(
    stream: &mut S,
    limit: usize,
    pending: &mut BytesMut,
    decode: impl FnOnce(&[u8]) -> Result<T>,
) -> Result<Result<T>> {
    let length = fill_frame(stream, limit, pending).await?;
    let consumed = 4 + length;
    let result = decode(&pending[4..consumed]);
    if pending.len() == consumed {
        // Decode into owned data before consuming; no frame allocation escapes.
        pending.clear();
        // Earlier pipelined frames may have advanced the visible start. Reclaim
        // that prefix without allocating to inspect the actual backing capacity.
        let _ = pending.try_reclaim(RETAINED_BUFFER_LIMIT + 1);
        if pending.capacity() > RETAINED_BUFFER_LIMIT {
            *pending = BytesMut::new();
        }
    } else {
        pending.advance(consumed);
    }
    Ok(result)
}

async fn fill_frame<S: AsyncRead + Unpin>(
    stream: &mut S,
    limit: usize,
    pending: &mut BytesMut,
) -> Result<usize> {
    while pending.len() < 4 {
        let need = 4 - pending.len();
        let read = read_into_pending(stream, pending, need)
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
        let read = read_into_pending(stream, pending, need)
            .await
            .context("Reading IPC frame")?;
        if read == 0 {
            bail!("IPC connection closed while reading frame");
        }
    }
    Ok(length)
}

async fn read_into_pending<S: AsyncRead + Unpin>(
    stream: &mut S,
    pending: &mut BytesMut,
    need: usize,
) -> io::Result<usize> {
    // Futures IO requires initialized memory. Initialize only a bounded chunk
    // of the reserved frame, and expose received bytes only after a ready read.
    // Dropping a pending read therefore leaves all framing progress intact.
    let count = need.min(FRAME_GROWTH_THRESHOLD);
    pending.reserve(count);
    let spare = pending.chunk_mut();
    // SAFETY: chunk_mut supplies at least the reserved count writable bytes;
    // zero them before constructing the initialized slice required by AsyncRead.
    let bytes = unsafe {
        std::ptr::write_bytes(spare.as_mut_ptr(), 0, count);
        std::slice::from_raw_parts_mut(spare.as_mut_ptr(), count)
    };
    let read = stream.read(bytes).await?;
    assert!(
        read <= count,
        "AsyncRead returned more bytes than its buffer"
    );
    // SAFETY: the count check bounds these initialized bytes to spare capacity.
    unsafe { pending.advance_mut(read) };
    Ok(read)
}

pub async fn write_frame_buffered<S: AsyncWrite + Unpin>(
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

pub async fn write_error_frame<S: AsyncWrite + Unpin>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
    error: impl Into<String>,
) -> Result<()> {
    let response = error_frame(error);
    write_frame_buffered(stream, &response, buffer).await
}

pub async fn write_request_frame_buffered<S: AsyncWrite + Unpin>(
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

pub async fn read_response_frame<S: AsyncRead + Unpin>(
    stream: &mut S,
    pending: &mut BytesMut,
) -> Result<WireResponse> {
    read_decoded_frame(stream, MAX_RESPONSE, pending, decode_response_frame).await?
}

pub fn error_frame(error: impl Into<String>) -> WireResponse {
    WireResponse::Error(error.into())
}

pub fn unpack_error(response: WireResponse) -> Result<WireResponse> {
    if let WireResponse::Error(error) = response {
        bail!("IPC server error: {error}");
    }
    Ok(response)
}

pub fn validate_hello(response: HelloResponse) -> Result<HelloResponse> {
    ensure!(
        u32::from_le_bytes(response.instance_id) != UNKNOWN_INSTANCE,
        "IPC Hello returned instance 0"
    );
    Ok(response)
}

pub fn unpack_hello(response: WireResponse) -> Result<HelloResponse> {
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

    #[test]
    fn bulk_response_releases_buffers_before_idle_and_accepts_next_frame() {
        async_io::block_on(async {
            let payload = "X".repeat(6 * 1024 * 1024);
            let (mut sender, mut receiver) =
                async_io::Async::<std::os::unix::net::UnixStream>::pair().unwrap();
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
            future::zip(send, receive).await;
        });
    }

    #[test]
    fn subthreshold_responses_release_doubled_backing_before_idle() {
        async_io::block_on(async {
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
        });
    }

    #[test]
    fn malformed_response_releases_backing_and_preserves_buffered_next_frame() {
        async_io::block_on(async {
            let mut encoded = Vec::new();
            encode_response_frame_into(&WireResponse::Error("invalid".into()), &mut encoded)
                .unwrap();
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
        });
    }

    #[test]
    fn bulk_request_limit_is_enforced() {
        let mut buffer = Vec::new();
        let payload = "X".repeat(6 * 1024 * 1024);
        assert!(
            FrameWriter {
                bytes: &mut buffer,
                limit: MAX_REQUEST
            }
            .encode(0, &payload)
            .is_err()
        );
    }

    #[test]
    fn stalled_bulk_response_releases_encoding_buffer() {
        async_io::block_on(async {
            let mut sender = LimitedWriter::default();
            let mut buffer = Vec::new();
            let result = write_frame_buffered(
                &mut sender,
                &WireResponse::Error("X".repeat(1024 * 1024)),
                &mut buffer,
            )
            .await;
            assert!(result.unwrap_err().downcast_ref::<Elapsed>().is_some());
            assert!(buffer.capacity() <= RETAINED_BUFFER_LIMIT);
        });
    }

    #[test]
    fn response_write_timeout_ends_a_partial_frame_when_peer_stops_reading() {
        async_io::block_on(async {
            let mut stream = LimitedWriter::default();
            let mut encoded = Vec::new();
            let error = write_frame_buffered(
                &mut stream,
                &WireResponse::Error("response".into()),
                &mut encoded,
            )
            .await
            .unwrap_err();
            assert!(error.downcast_ref::<Elapsed>().is_some());
            assert!(!stream.bytes.is_empty());
            assert!(stream.bytes.len() < encoded.len());
            assert_eq!(stream.bytes, encoded[..stream.bytes.len()]);
        });
    }

    #[derive(Default)]
    struct LimitedWriter {
        bytes: Vec<u8>,
    }

    impl AsyncWrite for LimitedWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _: &mut TaskContext<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            let count = bytes.len().min(8 - self.bytes.len());
            if count == 0 {
                return Poll::Pending;
            }
            self.bytes.extend_from_slice(&bytes[..count]);
            Poll::Ready(Ok(count))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
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
        fn poll_close(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn response_write_deadline_includes_flush() {
        async_io::block_on(async {
            let error = write_frame_buffered(
                &mut PendingFlush,
                &WireResponse::Error("response".into()),
                &mut Vec::new(),
            )
            .await
            .unwrap_err();
            assert!(error.downcast_ref::<Elapsed>().is_some());
        });
    }

    #[test]
    fn stalled_request_write_is_unknown_and_releases_bulk_buffer() {
        async_io::block_on(async {
            let mut stream = LimitedWriter::default();
            let request = WireRequest::Command(super::super::wire::WireCommand::CreatePlaylist {
                name: "X".repeat(1024 * 1024),
            });
            let mut buffer = Vec::new();
            let mut write_started = false;
            let error = write_request_frame_buffered(
                &mut stream,
                &request,
                &mut buffer,
                &mut write_started,
            )
            .await
            .unwrap_err();
            assert!(error.downcast_ref::<Elapsed>().is_some());
            assert!(write_started);
            assert!(buffer.capacity() <= RETAINED_BUFFER_LIMIT);
            assert!(!stream.bytes.is_empty());
            assert!(
                stream.bytes.len()
                    < 4 + u32::from_le_bytes(stream.bytes[..4].try_into().unwrap()) as usize
            );
        });
    }

    #[test]
    fn request_write_deadline_includes_flush() {
        async_io::block_on(async {
            let mut write_started = false;
            let error = write_request_frame_buffered(
                &mut PendingFlush,
                &WireRequest::State(crate::response::StateSections::ALL),
                &mut Vec::new(),
                &mut write_started,
            )
            .await
            .unwrap_err();
            assert!(error.downcast_ref::<Elapsed>().is_some());
            assert!(write_started);
        });
    }

    fn payload_request(payload: String) -> WireRequest {
        WireRequest::Query(super::super::wire::WireQuery::PlaylistSummaries {
            query: Some(payload),
            offset: 0,
            limit: 1,
        })
    }

    fn request_payload(request: WireRequest) -> String {
        let WireRequest::Query(super::super::wire::WireQuery::PlaylistSummaries {
            query: Some(payload),
            ..
        }) = request
        else {
            panic!("query request with a payload")
        };
        payload
    }

    #[test]
    fn warmed_small_requests_reuse_receive_storage() {
        async_io::block_on(async {
            let first_payload = "X".repeat(1024);
            let mut encoded = Vec::new();
            encode_request_frame_into(&payload_request(first_payload.clone()), &mut encoded)
                .unwrap();
            let mut pending = BytesMut::new();
            let first = read_request_frame(&mut encoded.as_slice(), &mut pending)
                .await
                .unwrap()
                .unwrap();
            let pointer = pending.as_ptr();
            let capacity = pending.capacity();
            assert!(capacity >= encoded.len());
            assert!(capacity <= RETAINED_BUFFER_LIMIT);
            for payload in ["Y".repeat(1024), "Z".repeat(128)] {
                encode_request_frame_into(&payload_request(payload.clone()), &mut encoded).unwrap();
                let request = read_request_frame(&mut encoded.as_slice(), &mut pending)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(request_payload(request), payload);
                assert!(pending.is_empty());
                assert_eq!(pending.as_ptr(), pointer);
                assert_eq!(pending.capacity(), capacity);
            }
            assert_eq!(request_payload(first), first_payload);
        });
    }

    #[test]
    fn warmed_small_responses_reuse_receive_storage() {
        async_io::block_on(async {
            let first_payload = "X".repeat(1024);
            let mut encoded = Vec::new();
            encode_response_frame_into(&WireResponse::Error(first_payload.clone()), &mut encoded)
                .unwrap();
            let mut pending = BytesMut::new();
            let first = read_response_frame(&mut encoded.as_slice(), &mut pending)
                .await
                .unwrap();
            let pointer = pending.as_ptr();
            let capacity = pending.capacity();
            assert!(capacity >= encoded.len());
            assert!(capacity <= RETAINED_BUFFER_LIMIT);
            for payload in ["Y".repeat(1024), "Z".repeat(128)] {
                encode_response_frame_into(&WireResponse::Error(payload.clone()), &mut encoded)
                    .unwrap();
                assert!(matches!(
                    read_response_frame(&mut encoded.as_slice(), &mut pending)
                        .await
                        .unwrap(),
                    WireResponse::Error(actual) if actual == payload
                ));
                assert!(pending.is_empty());
                assert_eq!(pending.as_ptr(), pointer);
                assert_eq!(pending.capacity(), capacity);
            }
            assert!(matches!(first, WireResponse::Error(actual) if actual == first_payload));
        });
    }

    #[test]
    fn requests_release_bulk_and_subthreshold_growth_before_idle() {
        async_io::block_on(async {
            let mut pending = BytesMut::new();
            let mut encoded = Vec::new();
            for (index, size) in [RETAINED_BUFFER_LIMIT + 1024, 150 * 1024, 230 * 1024]
                .into_iter()
                .enumerate()
            {
                let payload = "X".repeat(size);
                encode_request_frame_into(&payload_request(payload.clone()), &mut encoded).unwrap();
                let mut stream = encoded.as_slice();
                fill_frame(&mut stream, MAX_REQUEST, &mut pending)
                    .await
                    .unwrap();
                if index != 1 {
                    // The last frame fits the limit, but growth doubles the
                    // preceding small allocation beyond the retention bound.
                    assert!(pending.capacity() > RETAINED_BUFFER_LIMIT);
                }
                if index > 0 {
                    assert!(encoded.len() < RETAINED_BUFFER_LIMIT);
                }
                let request = read_request_frame(&mut stream, &mut pending)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(request_payload(request), payload);
                assert!(stream.is_empty());
                assert!(pending.is_empty());
                let _ = pending.try_reclaim(RETAINED_BUFFER_LIMIT + 1);
                assert!(pending.capacity() <= RETAINED_BUFFER_LIMIT);
            }
        });
    }

    #[test]
    fn pipelined_requests_preserve_payloads_and_release_advanced_backing() {
        async_io::block_on(async {
            for invalid in [false, true] {
                let first_payload = "X".repeat(160 * 1024);
                let next_payload = "Y".repeat(16 * 1024);
                let mut encoded = Vec::new();
                encode_request_frame_into(&payload_request(first_payload.clone()), &mut encoded)
                    .unwrap();
                if invalid {
                    encoded[4] = PROTOCOL_VERSION.wrapping_add(1);
                }
                let mut pending = BytesMut::with_capacity(300 * 1024);
                pending.extend_from_slice(&encoded);
                encode_request_frame_into(&payload_request(next_payload.clone()), &mut encoded)
                    .unwrap();
                pending.extend_from_slice(&encoded);
                assert!(pending.capacity() > RETAINED_BUFFER_LIMIT);
                let mut stream = &[][..];
                let first = read_request_frame(&mut stream, &mut pending).await.unwrap();
                assert_eq!(first.is_err(), invalid);
                assert_eq!(pending.as_ref(), encoded.as_slice());
                // Advancing the first frame hides the oversized allocation.
                assert!(pending.capacity() <= RETAINED_BUFFER_LIMIT);
                let next = read_request_frame(&mut stream, &mut pending)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(request_payload(next), next_payload);
                assert!(pending.is_empty());
                assert!(!pending.try_reclaim(RETAINED_BUFFER_LIMIT + 1));
                assert_eq!(pending.capacity(), 0);
                if !invalid {
                    assert_eq!(request_payload(first.unwrap()), first_payload);
                }
            }
        });
    }

    #[test]
    fn pipelined_responses_preserve_payloads_and_release_advanced_backing() {
        async_io::block_on(async {
            for invalid in [false, true] {
                let first_payload = "X".repeat(160 * 1024);
                let next_payload = "Y".repeat(16 * 1024);
                let mut encoded = Vec::new();
                encode_response_frame_into(
                    &WireResponse::Error(first_payload.clone()),
                    &mut encoded,
                )
                .unwrap();
                if invalid {
                    encoded[4] = PROTOCOL_VERSION.wrapping_add(1);
                }
                let mut pending = BytesMut::with_capacity(300 * 1024);
                pending.extend_from_slice(&encoded);
                encode_response_frame_into(
                    &WireResponse::Error(next_payload.clone()),
                    &mut encoded,
                )
                .unwrap();
                pending.extend_from_slice(&encoded);
                assert!(pending.capacity() > RETAINED_BUFFER_LIMIT);
                let mut stream = &[][..];
                let first = read_response_frame(&mut stream, &mut pending).await;
                assert_eq!(first.is_err(), invalid);
                assert_eq!(pending.as_ref(), encoded.as_slice());
                assert!(pending.capacity() <= RETAINED_BUFFER_LIMIT);
                assert!(matches!(
                    read_response_frame(&mut stream, &mut pending).await.unwrap(),
                    WireResponse::Error(actual) if actual == next_payload
                ));
                assert!(pending.is_empty());
                assert!(!pending.try_reclaim(RETAINED_BUFFER_LIMIT + 1));
                assert_eq!(pending.capacity(), 0);
                if !invalid {
                    assert!(
                        matches!(first.unwrap(), WireResponse::Error(actual) if actual == first_payload)
                    );
                }
            }
        });
    }

    #[test]
    fn request_reader_distinguishes_framing_and_decoding_errors() {
        async_io::block_on(async {
            let mut pending = BytesMut::new();
            let oversized = ((MAX_REQUEST + 1) as u32).to_le_bytes();
            assert!(
                read_request_frame(&mut oversized.as_slice(), &mut pending)
                    .await
                    .is_err()
            );
            assert_eq!(pending.as_ref(), oversized.as_slice());
            pending.clear();
            let invalid = [1, 0, 0, 0, PROTOCOL_VERSION];
            assert!(
                read_request_frame(&mut invalid.as_slice(), &mut pending)
                    .await
                    .unwrap()
                    .is_err()
            );
            assert!(pending.is_empty());
        });
    }

    #[test]
    fn cancelling_a_partial_read_preserves_only_received_bytes() {
        struct PartialRead<'a>(&'a [u8]);

        impl AsyncRead for PartialRead<'_> {
            fn poll_read(
                mut self: Pin<&mut Self>,
                _: &mut TaskContext<'_>,
                buffer: &mut [u8],
            ) -> Poll<io::Result<usize>> {
                if self.0.is_empty() {
                    return Poll::Pending;
                }
                let count = buffer.len().min(self.0.len());
                buffer[..count].copy_from_slice(&self.0[..count]);
                self.0 = &self.0[count..];
                Poll::Ready(Ok(count))
            }
        }

        async_io::block_on(async {
            let mut encoded = Vec::new();
            encode_response_frame_into(&WireResponse::Error("response".into()), &mut encoded)
                .unwrap();
            let mut stream = PartialRead(&encoded[..7]);
            let mut pending = BytesMut::new();
            assert!(
                future::poll_once(read_response_frame(&mut stream, &mut pending))
                    .await
                    .is_none()
            );
            assert_eq!(pending.as_ref(), &encoded[..7]);
            let mut rest = &encoded[7..];
            let response = read_response_frame(&mut rest, &mut pending).await.unwrap();
            assert!(matches!(response, WireResponse::Error(actual) if actual == "response"));
            assert!(pending.is_empty());
        });
    }

    #[test]
    fn absolute_deadline_expires_pending_work_without_resetting() {
        async_io::block_on(async {
            let deadline = Instant::now();
            assert_eq!(
                timeout_at(deadline, future::pending::<()>()).await,
                Err(Elapsed)
            );
            // Like the previous transport, immediately ready work wins a tie.
            assert_eq!(timeout_at(deadline, future::ready(17)).await, Ok(17));
        });
    }
}
