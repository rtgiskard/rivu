use super::*;
use anyhow::{bail, ensure};
use std::fs::{File, Metadata};
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

/// Keep the verified descriptor alive until the Core commit boundary.
pub(in crate::core) struct VerifiedTarget {
    file: File,
    identity: FileIdentity,
}

#[derive(Debug, PartialEq, Eq)]
struct FileIdentity {
    size: u64,
    modified: SystemTime,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    changed: (i64, i64),
    #[cfg(not(unix))]
    created: Option<SystemTime>,
}

impl FileIdentity {
    fn read(metadata: &Metadata) -> Result<Self> {
        ensure!(
            metadata.is_file(),
            "Relocation target is not an ordinary file"
        );
        Ok(Self {
            size: metadata.len(),
            modified: metadata
                .modified()
                .context("Reading target modification time")?,
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            changed: (metadata.ctime(), metadata.ctime_nsec()),
            #[cfg(not(unix))]
            created: metadata.created().ok(),
        })
    }

    fn modified_ns(&self) -> Result<i64> {
        let ns = match self.modified.duration_since(UNIX_EPOCH) {
            Ok(duration) => i128::try_from(duration.as_nanos())?,
            Err(error) => -i128::try_from(error.duration().as_nanos())?,
        };
        let ns = i64::try_from(ns).context("Target modification time is out of range")?;
        ensure!(
            ns != i64::MIN,
            "Target modification time cannot be verified"
        );
        Ok(ns)
    }
}

fn require_source_absent(path: &Path) -> Result<()> {
    check_source_absent(std::fs::symlink_metadata(path))
}

fn check_source_absent(metadata: std::io::Result<Metadata>) -> Result<()> {
    match metadata {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("Checking old file absence"),
        Ok(_) => bail!("Old file path exists again; relocation refused"),
    }
}

async fn verify_target(
    prepared: crate::store::PreparedRelocation,
    cancellation: library::ScanCancellation,
) -> Result<VerifiedTarget> {
    cancellation.check()?;
    let admitted = prepared.clone();
    let mut verified = tokio::task::spawn_blocking(move || -> Result<VerifiedTarget> {
        require_source_absent(&admitted.source.path)?;
        let file = rivu_format::input::open_regular_file(&admitted.target.path)
            .context("Opening relocation target")?;
        let metadata = file
            .metadata()
            .context("Reading relocation target metadata")?;
        let identity = FileIdentity::read(&metadata)?;
        ensure!(
            identity.size == admitted.target.file_size
                && identity.modified_ns()? == admitted.target.modified_ns,
            "Target metadata changed since the library scan; scan again before confirming"
        );
        let verified = VerifiedTarget { file, identity };
        verified.revalidate(&admitted.target.path)?;
        Ok(verified)
    })
    .await
    .context("Relocation admission worker panicked")??;
    let mut buffer = vec![0u8; 128 * 1024];
    let mut hasher = blake3::Hasher::new();
    let mut remaining = verified.identity.size;
    while remaining != 0 {
        let cancellation = cancellation.clone();
        // Bound each pool lease to 4 MiB, checking cancellation each 128 KiB.
        // The descriptor, scratch and hash state move, not copy, between leases.
        (verified, buffer, hasher, remaining) = tokio::task::spawn_blocking(move || {
            let mut budget = remaining.min(4 * 1024 * 1024);
            while budget != 0 {
                cancellation.check()?;
                let limit = budget.min(buffer.len() as u64) as usize;
                let count = match verified.file.read(&mut buffer[..limit]) {
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    result => result.context("Reading relocation target")?,
                };
                if count == 0 {
                    remaining = 0;
                    break;
                }
                remaining -= count as u64;
                budget -= count as u64;
                hasher.update(&buffer[..count]);
            }
            cancellation.check()?;
            Ok::<_, anyhow::Error>((verified, buffer, hasher, remaining))
        })
        .await
        .context("Relocation read worker panicked")??;
    }
    tokio::task::spawn_blocking(move || {
        cancellation.check()?;
        verified.revalidate(&prepared.target.path)?;
        ensure!(
            hasher.finalize().to_hex().as_str() == prepared.source.fingerprint,
            "Fresh target content does not match the old file fingerprint; relocation refused"
        );
        Ok(verified)
    })
    .await
    .context("Relocation target retirement worker panicked")?
}

impl VerifiedTarget {
    fn revalidate(&self, path: &Path) -> Result<()> {
        ensure!(
            FileIdentity::read(&self.file.metadata()?)? == self.identity
                && FileIdentity::read(&std::fs::metadata(path)?)? == self.identity,
            "Relocation target changed during verification; relocation refused"
        );
        Ok(())
    }
}

impl Core {
    fn require_relocation_playback_idle(&self, source_id: i64, target_id: i64) -> Result<()> {
        ensure!(
            !self.playback.as_ref().is_some_and(
                |playback| playback.track_id == source_id || playback.track_id == target_id
            ),
            "Stop playback of the old file and target before relocating"
        );
        ensure!(
            self.state.playback.status == PlaybackStatus::Stopped
                || !self
                    .state
                    .current_track
                    .as_ref()
                    .is_some_and(|track| track.id == source_id || track.id == target_id),
            "Stop playback of the old file and target before relocating"
        );
        Ok(())
    }

    pub(in crate::core) fn relocate_track(&mut self, source_id: i64, target_id: i64) -> Result<()> {
        ensure!(
            self.active_relocation.is_none(),
            "A relocation verification is already running"
        );
        self.require_relocation_playback_idle(source_id, target_id)?;
        if self.retired_playback.as_ref().is_some_and(|retired| {
            retired.stats.track_id == source_id || retired.stats.track_id == target_id
        }) {
            self.flush_deferred_playback()?;
        }
        let prepared = self.store.prepare_relocation(source_id, target_id)?;
        let id = self.next_relocation_id;
        let next_id = id.checked_add(1).context("Relocation task IDs exhausted")?;
        let cancellation = library::ScanCancellation::default();
        let worker_cancellation = cancellation.clone();
        let worker_prepared = prepared.clone();
        let worker = self
            .spawn_relocation_verification(id, verify_target(worker_prepared, worker_cancellation));
        let status = RelocationTaskStatus {
            id,
            source_id,
            target_id,
            phase: RelocationPhase::Verifying,
            error: None,
        };
        self.active_relocation = Some(RelocationTask {
            status: status.clone(),
            prepared,
            cancellation,
            worker: Some(worker),
        });
        self.next_relocation_id = next_id;
        self.state.system.relocation_task = Some(status);
        Ok(())
    }

    pub(in crate::core) fn spawn_relocation_verification(
        &self,
        id: u64,
        verify: impl Future<Output = Result<VerifiedTarget>> + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        let sender = self.relocation_tx.clone();
        let executor = self.executor.clone();
        self.executor.spawn(async move {
            // Await the actual verification future, including panics while polling.
            // Completion cannot retire admission before its resources have ended.
            let result = executor.spawn(verify).await.unwrap_or_else(|_| {
                Err(anyhow::anyhow!(
                    "Relocation verification worker panicked; relocation was not committed"
                ))
            });
            let _ = sender.send(RelocationFinished { id, result }).await;
        })
    }

    pub(in crate::core) fn cancel_relocation(&mut self, id: u64) -> Result<()> {
        if let Some(task) = self
            .active_relocation
            .as_ref()
            .filter(|task| task.status.id == id)
        {
            task.cancellation.cancel();
            return Ok(());
        }
        ensure!(
            self.relocation_results.iter().any(|status| status.id == id),
            "Unknown or expired relocation task"
        );
        Ok(())
    }

    pub(in crate::core) fn relocation_event(&mut self, event: RelocationFinished) {
        if !self
            .active_relocation
            .as_ref()
            .is_some_and(|task| task.status.id == event.id)
        {
            return;
        }
        let mut task = self.active_relocation.take().unwrap();
        if let Some(worker) = task.worker.take() {
            let _ = futures::executor::block_on(worker);
        }
        if task.cancellation.is_cancelled() {
            task.status.phase = RelocationPhase::Cancelled;
        } else {
            let outcome = event.result.and_then(|verified| {
                self.require_relocation_playback_idle(
                    task.status.source_id,
                    task.status.target_id,
                )?;
                require_source_absent(&task.prepared.source.path)?;
                verified.revalidate(&task.prepared.target.path)?;
                // Store rereads both identities, eligibility and all target conflicts
                // inside the transaction; current source user state is retained.
                self.store.commit_relocation(&task.prepared)
            });
            match outcome {
                Ok(()) => {
                    task.status.phase = RelocationPhase::Completed;
                    // The transaction is committed even if projection refresh fails.
                    if let Err(error) = self.apply_scan_impacts(crate::store::ScanImpacts {
                        changed: true,
                        library: true,
                        structure: true,
                        duplicate_groups: true,
                        duplicate_members: true,
                        ..crate::store::ScanImpacts::default()
                    }) {
                        task.status.error = Some(format!(
                            "Relocation committed; refreshing views failed; some projections may be stale: {error:#}"
                        ));
                    }
                }
                Err(error) => {
                    task.status.phase = RelocationPhase::Failed;
                    task.status.error = Some(format!("{error:#}"));
                }
            }
        }
        if let Some(error) = &task.status.error {
            self.state.system.last_error = Some(error.clone());
        }
        self.retain_relocation_result(task.status.clone());
        self.publish();
    }

    fn retain_relocation_result(&mut self, status: RelocationTaskStatus) {
        self.state.system.relocation_task = Some(status.clone());
        if self.relocation_results.len() == MAX_RETAINED_SCAN_RESULTS {
            self.relocation_results.pop_front();
        }
        self.relocation_results.push_back(status);
    }

    pub(in crate::core) fn shutdown_relocation(&mut self) {
        if let Some(mut task) = self.active_relocation.take() {
            task.cancellation.cancel();
            self.relocation_rx.close();
            // Closing wakes a supervisor awaiting bounded completion capacity.
            while self.relocation_rx.try_recv().is_ok() {}
            if let Some(worker) = task.worker.take() {
                let _ = futures::executor::block_on(worker);
            }
            // A cancelled successful verification still owns its descriptor in
            // the completion port. Drop it before retiring relocation admission.
            while self.relocation_rx.try_recv().is_ok() {}
            task.status.phase = RelocationPhase::Cancelled;
            self.retain_relocation_result(task.status.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{PreparedRelocation, RelocationIdentity};

    fn prepared() -> (tempfile::TempDir, PreparedRelocation) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("target.wav");
        std::fs::write(&path, b"actual file bytes").unwrap();
        let metadata = FileIdentity::read(&std::fs::metadata(&path).unwrap()).unwrap();
        let fingerprint = blake3::hash(b"actual file bytes").to_hex().to_string();
        let source = RelocationIdentity {
            id: 1,
            path: directory.path().join("missing.wav"),
            fingerprint: fingerprint.clone(),
            file_size: metadata.size,
            modified_ns: metadata.modified_ns().unwrap(),
        };
        let target = RelocationIdentity {
            id: 2,
            path,
            fingerprint,
            file_size: metadata.size,
            modified_ns: metadata.modified_ns().unwrap(),
        };
        (directory, PreparedRelocation { source, target })
    }

    #[tokio::test]
    async fn fresh_hash_rejects_matching_cached_hash_for_different_bytes() {
        let (_directory, mut prepared) = prepared();
        // Both database identities still agree; only reading the bytes detects this.
        prepared.source.fingerprint = blake3::hash(b"historical bytes").to_hex().to_string();
        prepared
            .target
            .fingerprint
            .clone_from(&prepared.source.fingerprint);
        let error = verify_target(prepared, library::ScanCancellation::default())
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("Fresh target content"));
    }

    #[tokio::test]
    async fn verification_requires_old_path_absent_and_observes_cancellation() {
        let (_directory, prepared) = prepared();
        let cancellation = library::ScanCancellation::default();
        cancellation.cancel();
        assert!(verify_target(prepared.clone(), cancellation).await.is_err());
        std::fs::write(&prepared.source.path, b"old file returned").unwrap();
        assert!(
            verify_target(prepared, library::ScanCancellation::default())
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("exists again")
        );
    }

    #[tokio::test]
    async fn verified_descriptor_rejects_path_replacement_before_commit() {
        let (_directory, prepared) = prepared();
        let verified = verify_target(prepared.clone(), library::ScanCancellation::default())
            .await
            .unwrap();
        let replacement = prepared.target.path.with_extension("replacement");
        std::fs::write(&replacement, b"actual file bytes").unwrap();
        std::fs::rename(&replacement, &prepared.target.path).unwrap();
        assert!(verified.revalidate(&prepared.target.path).is_err());
    }

    #[tokio::test]
    async fn verified_descriptor_rejects_in_place_write_before_commit() {
        let (_directory, prepared) = prepared();
        let verified = verify_target(prepared.clone(), library::ScanCancellation::default())
            .await
            .unwrap();
        std::fs::write(&prepared.target.path, b"changed bytes").unwrap();
        assert!(verified.revalidate(&prepared.target.path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn relocation_fifo_replacement_is_rejected_without_waiting_for_a_writer() {
        let (_directory, prepared) = prepared();
        std::fs::remove_file(&prepared.target.path).unwrap();
        let name =
            std::ffi::CString::new(prepared.target.path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: name is a live NUL-terminated path; mkfifo retains no pointer.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let (completed, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let result = runtime.block_on(verify_target(
                prepared,
                library::ScanCancellation::default(),
            ));
            let _ = completed.send(result);
        });
        // On timeout do not join: a FIFO-open regression must fail, not hang
        // while a test runtime waits for a stuck blocking filesystem operation.
        let error = receive
            .recv_timeout(Duration::from_secs(2))
            .expect("FIFO admission must not block")
            .err()
            .unwrap();
        worker.join().unwrap();
        assert!(format!("{error:#}").contains("not an ordinary file"));
    }
}
