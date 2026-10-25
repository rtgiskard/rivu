//! Delivers a logical waiting failure while the asynchronous read still owns its slot.

use crate::core::CoreHandle;
use futures::{
    channel::oneshot,
    future::{Either, select},
};
use gpui::AsyncApp;
use parking_lot::Mutex;
use rivu_protocol::response::QueryResponse;

pub(super) async fn run<T, F: Future<Output = T>>(
    handle: CoreHandle,
    cx: &mut AsyncApp,
    operation: impl FnOnce(CoreHandle) -> F,
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
