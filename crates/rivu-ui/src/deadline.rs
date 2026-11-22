use std::time::Instant;

use gpui::{Context, Task};

/// One pending entity wake. Replacing or dropping it cancels the previous task.
#[derive(Default)]
pub(crate) struct DeadlineWake {
    armed: Option<(Instant, Task<()>)>,
}

impl DeadlineWake {
    pub(crate) fn is_armed_for(&self, deadline: Option<Instant>) -> bool {
        self.armed.as_ref().map(|(at, _)| *at) == deadline
    }

    pub(crate) fn cancel(&mut self) {
        self.armed = None;
    }

    pub(crate) fn arm<T: 'static>(
        &mut self,
        deadline: Option<Instant>,
        cx: &mut Context<T>,
        on_due: impl FnOnce(&mut T, Instant, &mut Context<T>) + 'static,
    ) {
        if self.is_armed_for(deadline) {
            return;
        }
        self.cancel();
        let Some(deadline) = deadline else {
            return;
        };
        let executor = cx.background_executor();
        let timer = executor.timer(deadline.saturating_duration_since(executor.now()));
        let task = cx.spawn(async move |this, cx| {
            timer.await;
            let _ = this.update(cx, |this, cx| {
                on_due(this, cx.background_executor().now(), cx);
            });
        });
        self.armed = Some((deadline, task));
    }
}
