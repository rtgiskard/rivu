use super::ScanCancellation;
use crate::execution::BlockingPool;
use anyhow::{Context, Result, ensure};
use std::io::Read;
use std::path::Path;

pub(super) const MAX_CUE_BYTES: usize = 1024 * 1024;
const INPUT_CHUNK_BYTES: usize = 256 * 1024;

/// Independent retained-data domains use checked reservations. Raw input limits
/// remain separate; scoped workspace releases its charge with its owner.
#[derive(Clone, Debug, Default)]
pub(crate) struct RetainedBudget(std::sync::Arc<std::sync::Mutex<usize>>);

pub(crate) const MAX_IMPORT_RETAINED_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug)]
struct RetainedLimit;

impl std::fmt::Display for RetainedLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("import exceeds retained data limit")
    }
}
impl std::error::Error for RetainedLimit {}

impl RetainedBudget {
    pub(crate) fn reserve(&self, bytes: usize) -> Result<()> {
        let mut used = self.0.lock().unwrap();
        let next = used
            .checked_add(bytes)
            .filter(|next| *next <= MAX_IMPORT_RETAINED_BYTES)
            .ok_or(RetainedLimit)?;
        *used = next;
        Ok(())
    }

    pub(crate) fn release(&self, bytes: usize) {
        let mut used = self.0.lock().unwrap();
        *used = used
            .checked_sub(bytes)
            .expect("retained reservation ownership");
    }

    pub(crate) fn scoped(&self) -> RetainedScope {
        RetainedScope {
            budget: self.clone(),
            bytes: 0,
        }
    }

    pub(crate) fn reserve_vec<T>(&self, values: &mut Vec<T>, additional: usize) -> Result<usize> {
        let required = values.len().checked_add(additional).ok_or(RetainedLimit)?;
        if required <= values.capacity() {
            return Ok(0);
        }
        let capacity = required.max(values.capacity().saturating_mul(2)).max(4);
        let bytes = (capacity - values.capacity())
            .checked_mul(std::mem::size_of::<T>())
            .ok_or(RetainedLimit)?;
        self.reserve(bytes)?;
        values.reserve_exact(capacity - values.len());
        Ok(bytes)
    }

    #[cfg(test)]
    pub(crate) fn remaining(&self) -> usize {
        MAX_IMPORT_RETAINED_BYTES - *self.0.lock().unwrap()
    }

    pub(super) fn is_limit(error: &anyhow::Error) -> bool {
        error.downcast_ref::<RetainedLimit>().is_some()
    }
}

#[derive(Debug)]
pub(crate) struct RetainedScope {
    budget: RetainedBudget,
    bytes: usize,
}

impl RetainedScope {
    pub(crate) fn reserve(&mut self, bytes: usize) -> Result<()> {
        self.budget.reserve(bytes)?;
        self.bytes += bytes;
        Ok(())
    }
    pub(crate) fn release(&mut self, bytes: usize) {
        self.budget.release(bytes);
        self.bytes -= bytes;
    }
    pub(crate) fn reserve_vec<T>(&mut self, values: &mut Vec<T>, additional: usize) -> Result<()> {
        self.bytes += self.budget.reserve_vec(values, additional)?;
        Ok(())
    }
}

impl Drop for RetainedScope {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}

pub(super) async fn read_input_async(
    workers: &BlockingPool,
    path: &Path,
    limit: usize,
    cancellation: &ScanCancellation,
) -> Result<Vec<u8>> {
    cancellation.check()?;
    let path = path.to_path_buf();
    let admission_cancellation = cancellation.clone();
    let (file, bytes) = workers
        .execute(move || -> Result<_> {
            admission_cancellation.check()?;
            let file = rivu_format::input::open_regular_file(&path)?;
            let metadata = file.metadata()?;
            ensure!(
                metadata.len() <= limit as u64,
                "input exceeds {limit} bytes"
            );
            Ok((file, Vec::with_capacity(metadata.len() as usize)))
        })
        .await
        .context("Playlist input admission worker panicked")??;
    read_chunks_async(workers, file, bytes, limit, cancellation).await
}

pub(super) async fn read_chunks_async<R: Read + Send + 'static>(
    workers: &BlockingPool,
    mut reader: R,
    mut bytes: Vec<u8>,
    limit: usize,
    cancellation: &ScanCancellation,
) -> Result<Vec<u8>> {
    loop {
        let cancellation = cancellation.clone();
        // One admitted descriptor and retained buffer move through bounded reads.
        // Always await the stage: cancellation/error closes the reader here, not
        // on the async driver or in a detached filesystem task.
        let (next, output) = workers
            .execute(move || -> Result<_> {
                cancellation.check()?;
                let mut read = |buffer: &mut [u8]| -> Result<usize> {
                    loop {
                        cancellation.check()?;
                        let result = reader.read(buffer);
                        cancellation.check()?;
                        match result {
                            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                                continue;
                            }
                            result => return Ok(result?),
                        }
                    }
                };
                let start = bytes.len();
                let count = if start == bytes.capacity() || start == limit {
                    // Probe EOF without growing a full retained buffer. A file that
                    // grew after admission gains at most one chunk of capacity.
                    let mut extra = [0];
                    let count = read(&mut extra)?;
                    ensure!(start + count <= limit, "input exceeds {limit} bytes");
                    if count != 0 {
                        let growth = INPUT_CHUNK_BYTES.min(limit - start);
                        bytes.reserve_exact(growth);
                        bytes.push(extra[0]);
                    }
                    count
                } else {
                    bytes.resize(
                        start
                            + INPUT_CHUNK_BYTES
                                .min(bytes.capacity() - start)
                                .min(limit - start),
                        0,
                    );
                    let count = read(&mut bytes[start..])?;
                    bytes.truncate(start + count);
                    count
                };
                if count == 0 {
                    drop(reader);
                    Ok((None, bytes))
                } else {
                    Ok((Some(reader), bytes))
                }
            })
            .await
            .context("Playlist input read worker panicked")??;
        bytes = output;
        match next {
            Some(next) => reader = next,
            None => return Ok(bytes),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct ObservedReader {
        input: std::io::Cursor<Vec<u8>>,
        maximum: Arc<AtomicUsize>,
        retired: Arc<AtomicBool>,
        fail: bool,
    }

    impl Read for ObservedReader {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            self.maximum.fetch_max(bytes.len(), Ordering::Relaxed);
            if self.fail {
                return Err(std::io::Error::other("injected read failure"));
            }
            self.input.read(bytes)
        }
    }

    impl Drop for ObservedReader {
        fn drop(&mut self) {
            self.retired.store(true, Ordering::Relaxed);
        }
    }

    #[test]
    fn reads_reuse_retained_capacity_and_retire_before_returning() -> Result<()> {
        futures::executor::block_on(async {
            let workers = &BlockingPool::new(4);
            let length = INPUT_CHUNK_BYTES * 2 + 17;
            for fail in [false, true] {
                let maximum = Arc::new(AtomicUsize::new(0));
                let retired = Arc::new(AtomicBool::new(false));
                let reader = ObservedReader {
                    input: std::io::Cursor::new(vec![b'x'; length]),
                    maximum: maximum.clone(),
                    retired: retired.clone(),
                    fail,
                };
                let bytes = Vec::with_capacity(length);
                let capacity = bytes.capacity();
                let result =
                    read_chunks_async(workers, reader, bytes, length, &ScanCancellation::default())
                        .await;
                assert!(retired.load(Ordering::Relaxed));
                assert_eq!(maximum.load(Ordering::Relaxed), INPUT_CHUNK_BYTES);
                if fail {
                    assert!(result.is_err());
                } else {
                    let bytes = result?;
                    assert_eq!(bytes, vec![b'x'; length]);
                    assert_eq!(bytes.capacity(), capacity);
                }
            }
            Ok(())
        })
    }

    #[test]
    fn interrupted_reads_retry_unless_cancelled_including_eof_probes() -> Result<()> {
        futures::executor::block_on(async {
            let workers = &BlockingPool::new(4);
            struct InterruptedReader {
                input: std::io::Cursor<Vec<u8>>,
                reads: Arc<AtomicUsize>,
                cancellation: ScanCancellation,
                cancel: bool,
            }

            impl Read for InterruptedReader {
                fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                    if self.reads.fetch_add(1, Ordering::Relaxed) == 0 {
                        if self.cancel {
                            self.cancellation.cancel();
                        }
                        return Err(std::io::ErrorKind::Interrupted.into());
                    }
                    self.input.read(buffer)
                }
            }

            for capacity in [0, 4] {
                for cancel in [false, true] {
                    let cancellation = ScanCancellation::default();
                    let reads = Arc::new(AtomicUsize::new(0));
                    let reader = InterruptedReader {
                        input: std::io::Cursor::new(b"song".to_vec()),
                        reads: reads.clone(),
                        cancellation: cancellation.clone(),
                        cancel,
                    };
                    let result = read_chunks_async(
                        workers,
                        reader,
                        Vec::with_capacity(capacity),
                        4,
                        &cancellation,
                    )
                    .await;
                    if cancel {
                        assert!(result.unwrap_err().to_string().contains("cancelled"));
                        assert_eq!(reads.load(Ordering::Relaxed), 1);
                    } else {
                        assert_eq!(result?, b"song");
                        assert!(reads.load(Ordering::Relaxed) > 1);
                    }
                }
            }
            Ok(())
        })
    }

    #[test]
    fn retained_scopes_release_workspace_and_overflow_does_not_change_accounting() {
        let budget = RetainedBudget::default();
        budget.reserve(100).unwrap();
        let remaining = budget.remaining();
        assert!(budget.reserve(usize::MAX).is_err());
        assert_eq!(budget.remaining(), remaining);
        {
            let mut workspace = budget.scoped();
            let mut values = Vec::<u64>::new();
            workspace.reserve_vec(&mut values, 7).unwrap();
            assert_eq!(budget.remaining(), remaining - values.capacity() * 8);
            workspace.reserve(300).unwrap();
            workspace.release(100);
            assert_eq!(budget.remaining(), remaining - values.capacity() * 8 - 200);
        }
        assert_eq!(budget.remaining(), remaining);
    }
}
