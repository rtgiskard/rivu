//! Shared byte bounds for length-prefixed postcard messages.
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::io::{self, Write};

const PREFIX_SIZE: usize = 4;
const GROWTH_THRESHOLD: usize = 64 * 1024;

struct BoundedWriter<'a> {
    bytes: &'a mut Vec<u8>,
    limit: usize,
}

impl Write for BoundedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let required = self.bytes.len().saturating_add(bytes.len());
        if required > PREFIX_SIZE + self.limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Frame exceeds maximum size",
            ));
        }
        if required > self.bytes.capacity() && required > GROWTH_THRESHOLD {
            let target = required
                .max(self.bytes.capacity().saturating_mul(2))
                .min(PREFIX_SIZE + self.limit);
            self.bytes.reserve_exact(target - self.bytes.len());
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The header is part of the body budget. Failed encoding cannot be sent.
pub(crate) fn encode<T: Serialize>(
    value: &T,
    header: &[u8],
    limit: usize,
    bytes: &mut Vec<u8>,
) -> Result<()> {
    bytes.clear();
    ensure!(header.len() <= limit, "Frame header exceeds maximum size");
    bytes.extend_from_slice(&[0; PREFIX_SIZE]);
    let result: Result<()> = (|| {
        let mut writer = BoundedWriter {
            bytes: &mut *bytes,
            limit,
        };
        writer.write_all(header)?;
        postcard::to_io(value, &mut writer)?;
        let length =
            u32::try_from(writer.bytes.len() - PREFIX_SIZE).context("Frame is too large")?;
        writer.bytes[..PREFIX_SIZE].copy_from_slice(&length.to_le_bytes());
        Ok(())
    })();
    if result.is_err() {
        bytes.clear();
        crate::transport::release_large_buffer(bytes);
    }
    result
}

pub(crate) fn decode_length(prefix: [u8; PREFIX_SIZE], limit: usize) -> Result<usize> {
    let length = u32::from_le_bytes(prefix) as usize;
    ensure!(length <= limit, "Frame exceeds maximum size");
    Ok(length)
}

pub(crate) fn validate_encoded(bytes: &[u8], limit: usize) -> Result<()> {
    ensure!(bytes.len() >= PREFIX_SIZE, "Frame prefix is truncated");
    let length = decode_length(bytes[..PREFIX_SIZE].try_into().unwrap(), limit)?;
    ensure!(length == bytes.len() - PREFIX_SIZE, "Invalid frame prefix");
    Ok(())
}
