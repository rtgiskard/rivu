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

pub(super) async fn run<T, F: Future<Output = T>>(
    handle: FrontendHandle,
    cx: &mut AsyncApp,
    operation: impl FnOnce(FrontendHandle) -> F,
    mut on_timeout: impl FnMut(QueryResponse, &mut AsyncApp),
) -> T {
    // A composed operation reports its first failure only. The original future
    // remains owned here; dropping logical demand cannot drop its reply wait.
    let (sender, timeout) = oneshot::channel();
    let sender = Mutex::new(Some(sender));
    let handle = handle.with_query_timeout(move |response| {
        if let Some(sender) = sender.lock().take() {
            let _ = sender.send(response);
        }
    });
    let worker = operation(handle);
    futures::pin_mut!(timeout, worker);
    // Poll the notice first so a reply arriving just after the deadline cannot
    // skip logical failure delivery when both futures are ready in one UI turn.
    match select(timeout, worker).await {
        Either::Left((notice, worker)) => {
            if let Ok(response) = notice {
                on_timeout(response, cx);
            }
            worker.await
        }
        Either::Right((result, _)) => result,
    }
}
#[cfg(test)]
mod tests {
    use super::ReadReservation;

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
