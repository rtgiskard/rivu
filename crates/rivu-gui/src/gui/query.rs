//! Delivers a logical waiting failure while the asynchronous read still owns its slot.

use crate::frontend::FrontendHandle;
use futures::{
    channel::oneshot,
    future::{Either, select},
};
use gpui::AsyncApp;
use parking_lot::Mutex;
use rivu_protocol::response::QueryResponse;

/// One physical worker slot. Logical cancellation and publication freshness stay
/// with the feature owner; only the matching worker may retire this reservation.
#[derive(Default)]
pub(super) struct ReadReservation {
    next: u64,
    worker: Option<u64>,
}

impl ReadReservation {
    pub(super) fn pending(&self) -> bool {
        self.worker.is_some()
    }

    pub(super) fn begin(&mut self) -> Option<u64> {
        if self.pending() {
            return None;
        }
        self.next = self.next.wrapping_add(1);
        self.worker = Some(self.next);
        Some(self.next)
    }

    pub(super) fn owns(&self, worker: u64) -> bool {
        self.worker == Some(worker)
    }

    pub(super) fn finish(&mut self, worker: u64) -> bool {
        if !self.owns(worker) {
            return false;
        }
        self.worker = None;
        true
    }
}

/// Returns the worker result and whether `on_timeout` was called before retirement.
pub(super) async fn run<T, F: Future<Output = T>>(
    handle: FrontendHandle,
    cx: &mut AsyncApp,
    operation: impl FnOnce(FrontendHandle) -> F,
    on_timeout: impl FnOnce(QueryResponse, &mut AsyncApp),
) -> (T, bool) {
    // A composed operation reports its first failure only. The original future
    // remains owned here; dropping logical demand cannot drop its reply wait.
    let (sender, timeout) = oneshot::channel();
    let sender = Mutex::new(Some(sender));
    let handle = handle.with_query_timeout(move |response| {
        if let Some(sender) = sender.lock().take() {
            let _ = sender.send(response);
        }
    });
    finish_read(timeout, operation(handle), |response| {
        on_timeout(response, cx)
    })
    .await
}

async fn finish_read<T>(
    timeout: oneshot::Receiver<QueryResponse>,
    worker: impl Future<Output = T>,
    on_timeout: impl FnOnce(QueryResponse),
) -> (T, bool) {
    let notify = |notice: Option<QueryResponse>| notice.map(on_timeout).is_some();
    // Deliver an already queued notice before polling the worker again.
    match select(timeout, std::pin::pin!(worker)).await {
        Either::Left((notice, worker)) => {
            let timed_out = notify(notice.ok());
            (worker.await, timed_out)
        }
        Either::Right((result, mut timeout)) => {
            // The worker can send the notice and finish in the same poll.
            (result, notify(timeout.try_recv().ok().flatten()))
        }
    }
}
#[cfg(test)]
mod tests {
    use super::{ReadReservation, finish_read};
    use futures::{FutureExt, channel::oneshot};
    use rivu_protocol::response::{QueryError, QueryResponse};

    #[test]
    fn timeout_notice_precedes_retirement_even_if_worker_finishes_in_the_same_poll() {
        for finish_immediately in [false, true] {
            let (notice, timeout) = oneshot::channel();
            let (release, released) = oneshot::channel();
            let published = std::cell::Cell::new(false);
            let worker = async move {
                notice
                    .send(QueryResponse {
                        revisions: Default::default(),
                        result: Err(QueryError::Message("timed out".into())),
                    })
                    .unwrap();
                if !finish_immediately {
                    released.await.unwrap();
                }
                42
            };
            let mut read = std::pin::pin!(finish_read(timeout, worker, |_| published.set(true)));
            if !finish_immediately {
                assert!(read.as_mut().now_or_never().is_none());
                // The first poll sent the notice; the second must deliver it while the worker waits.
                assert!(read.as_mut().now_or_never().is_none());
                assert!(published.get());
                release.send(()).unwrap();
            }
            assert_eq!(read.now_or_never(), Some((42, true)));
            assert!(published.get());
        }
    }

    #[test]
    fn only_matching_worker_can_retire_a_reservation() {
        let mut reservation = ReadReservation::default();
        let first = reservation.begin().unwrap();
        assert!(reservation.begin().is_none());
        assert!(!reservation.finish(first.wrapping_add(1)));
        assert!(reservation.pending());
        assert!(reservation.finish(first));
        let second = reservation.begin().unwrap();
        assert_ne!(first, second);
        assert!(!reservation.finish(first));
        assert!(reservation.owns(second));
        assert!(reservation.finish(second));
    }
}
