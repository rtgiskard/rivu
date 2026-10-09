use super::{
    HelloResponse, MAX_REQUEST, MAX_RESPONSE, PROTOCOL_VERSION, RequestType, ResponseType,
    UNKNOWN_INSTANCE, WireRequest, WireResponse,
};
use anyhow::{Context, Result, bail, ensure};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

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
    pending.advance(4);
    Ok(pending.split_to(length).freeze())
}

pub(super) async fn write_frame_buffered<S: AsyncWrite + Unpin>(
    stream: &mut S,
    value: &WireResponse,
    buffer: &mut Vec<u8>,
) -> Result<()> {
    encode_response_frame_into(value, buffer)?;
    stream
        .write_all(buffer)
        .await
        .context("Writing IPC frame")?;
    stream.flush().await.context("Flushing IPC frame")?;
    Ok(())
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
) -> Result<()> {
    encode_request_frame_into(request, buffer)?;
    stream
        .write_all(buffer)
        .await
        .context("Writing IPC frame")?;
    stream.flush().await.context("Flushing IPC frame")?;
    Ok(())
}

pub(super) async fn read_response_frame<S: AsyncRead + Unpin>(
    stream: &mut S,
    pending: &mut BytesMut,
) -> Result<WireResponse> {
    let bytes = read_frame(stream, MAX_RESPONSE, pending).await?;
    decode_response_frame(&bytes)
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
