use super::*;
use std::sync::{atomic::AtomicU64, mpsc};

// Four retained outputs, one active result and eight buffered events bound a
// retirement wave. Only a successfully spawned worker submits a barrier, and it
// waits before producing another result. Core never waits for mailbox capacity.
const CAPACITY: usize = 16;

enum Work {
    Retire(Box<ProcessingResult>),
    Publish {
        id: u64,
        result: Box<ProcessingResult>,
    },
    Barrier(mpsc::Sender<std::result::Result<(), String>>),
}

pub(super) struct Reclaimer {
    sender: Option<mpsc::SyncSender<Work>>,
    worker: Option<JoinHandle<()>>,
    bytes: Arc<AtomicU64>,
    #[cfg(test)]
    pub(super) fail_spawns: bool,
}

impl Reclaimer {
    pub(super) fn new(events: Sender<Event>) -> Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(CAPACITY);
        let bytes = Arc::new(AtomicU64::new(0));
        let pending = bytes.clone();
        let worker = thread::Builder::new()
            .name("rivu-processing-reclaim".into())
            .spawn(move || {
                let mut failures = Vec::new();
                let mut failure = None::<String>;
                while let Ok(work) = receiver.recv() {
                    match work {
                        Work::Retire(result) => {
                            let bytes = result.cache_bytes;
                            match fs::remove_dir_all(result.directory.path()) {
                                Ok(()) => {
                                    drop(result);
                                    pending.fetch_sub(bytes, Ordering::Release);
                                }
                                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                    drop(result);
                                    pending.fetch_sub(bytes, Ordering::Release);
                                }
                                Err(error) => {
                                    let error =
                                        format!("Processing cache reclamation failed: {error}");
                                    tracing::error!(error = %error);
                                    failure = Some(error);
                                    // Keep ownership and its charged bytes. Later
                                    // admission fails instead of accumulating more PCM.
                                    failures.push(result);
                                }
                            }
                        }
                        Work::Publish { id, result } => {
                            // Failed delivery destroys the unpublished output here,
                            // not on Core, including when shutdown closes events.
                            if let Some(error) = &failure {
                                pending.fetch_add(result.cache_bytes, Ordering::Release);
                                failures.push(result);
                                let _ = events.send_blocking(Event::Finished {
                                    id,
                                    result: Err(anyhow::anyhow!(error.clone())),
                                });
                            } else {
                                let _ = events.send_blocking(Event::ReadyToPublish { id, result });
                            }
                        }
                        Work::Barrier(reply) => {
                            let _ = reply.send(failure.clone().map_or(Ok(()), Err));
                        }
                    }
                }
            })?;
        Ok(Self {
            sender: Some(sender),
            worker: Some(worker),
            bytes,
            #[cfg(test)]
            fail_spawns: false,
        })
    }

    fn send(&self, work: Work) {
        // Only a single admitted job can produce a retirement wave. A full
        // mailbox would violate that ownership bound, rather than request a
        // blocking fallback on the business thread.
        assert!(
            self.sender.as_ref().unwrap().try_send(work).is_ok(),
            "processing retirement ownership bound exceeded"
        );
    }

    pub(super) fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Acquire)
    }

    pub(super) fn retire(&self, result: Box<ProcessingResult>) {
        self.bytes.fetch_add(result.cache_bytes, Ordering::Release);
        self.send(Work::Retire(result));
    }

    pub(super) fn publish_after_retirement(&self, id: u64, result: Box<ProcessingResult>) {
        self.send(Work::Publish { id, result });
    }

    pub(super) fn spawn(
        &self,
        name: &str,
        work: impl FnOnce(Result<()>) + Send + 'static,
    ) -> Result<JoinHandle<()>> {
        let (sender, receiver) = mpsc::channel::<std::result::Result<(), String>>();
        #[cfg(test)]
        if self.fail_spawns {
            return Err(std::io::Error::other("injected processing worker spawn failure").into());
        }
        let worker = thread::Builder::new().name(name.into()).spawn(move || {
            let retired = receiver
                .recv()
                .context("Processing reclaimer stopped")
                .and_then(|result| result.map_err(anyhow::Error::msg));
            work(retired);
        })?;
        // Failed spawns must not queue orphaned barriers: retries can otherwise
        // exhaust the bounded mailbox while retirement is blocked.
        self.send(Work::Barrier(sender));
        Ok(worker)
    }

    pub(super) fn shutdown(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for Reclaimer {
    fn drop(&mut self) {
        self.shutdown();
    }
}
