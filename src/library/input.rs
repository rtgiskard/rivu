use super::ScanCancellation;
use anyhow::{Result, ensure};
use std::{
    fs::{self, OpenOptions},
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

pub(super) const MAX_CUE_BYTES: usize = 1024 * 1024;

pub(super) fn read_input(
    path: &Path,
    limit: usize,
    cancellation: &ScanCancellation,
) -> Result<Vec<u8>> {
    cancellation.check()?;
    // O_NONBLOCK prevents a FIFO substituted between metadata and open from hanging.
    ensure!(
        fs::metadata(path)?.is_file(),
        "input is not an ordinary file"
    );
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "input is not an ordinary file");
    ensure!(
        metadata.len() <= limit as u64,
        "input exceeds {limit} bytes"
    );
    read_chunks(&mut file, limit, cancellation)
}

pub(super) fn read_chunks(
    reader: &mut impl Read,
    limit: usize,
    cancellation: &ScanCancellation,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut chunk = [0; 64 * 1024];
    loop {
        cancellation.check()?;
        let count = reader.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        ensure!(bytes.len() + count <= limit, "input exceeds {limit} bytes");
        bytes.extend_from_slice(&chunk[..count]);
    }
    cancellation.check()?;
    Ok(bytes)
}
