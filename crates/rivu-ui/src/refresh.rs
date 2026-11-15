use std::time::Instant;

use gpui::{Context, Task};

/// One cancellable wake per feature, independent of query completion or frames.
#[derive(Default)]
pub struct RefreshWake {
    timer: Option<Task<()>>,
    wake_at: Option<Instant>,
}

impl RefreshWake {
    pub fn schedule<T: 'static>(
        &mut self,
        deadline: Option<Instant>,
        cx: &mut Context<T>,
        expire: impl Fn(&mut T, Instant, &mut Context<T>) -> bool + 'static,
    ) {
        if self.wake_at == deadline {
            return;
        }
        self.timer = None;
        self.wake_at = deadline;
        let Some(deadline) = deadline else {
            return;
        };
        let timer = cx
            .background_executor()
            .timer(deadline.saturating_duration_since(Instant::now()));
        self.timer = Some(cx.spawn(async move |this, cx| {
            timer.await;
            let _ = this.update(cx, |this, cx| {
                if expire(this, deadline.max(Instant::now()), cx) {
                    cx.notify();
                }
            });
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rivu_presentation::refresh::{REFRESH_RETENTION, RefreshDeadline};
    use std::time::Duration;

    #[gpui::test]
    fn stalled_refresh_expires_without_another_publication(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext;

        #[derive(Default)]
        struct View {
            text: Option<&'static str>,
            retention: RefreshDeadline,
            wake: RefreshWake,
        }

        let view = cx.update(|cx| cx.new(|_| View::default()));
        let start = Instant::now() - REFRESH_RETENTION;
        view.update(cx, |view, cx| {
            view.text = Some("old result");
            view.retention.begin(start);
            view.wake
                .schedule(view.retention.deadline(), cx, |view, now, _| {
                    if view.retention.expire(now) {
                        return view.text.take().is_some();
                    }
                    false
                });
        });
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.text), None);

        view.update(cx, |view, cx| {
            view.text = Some("another old result");
            view.retention.clear();
            view.retention
                .begin(Instant::now() + Duration::from_secs(60));
            view.wake
                .schedule(view.retention.deadline(), cx, |view, _, _| {
                    view.text = None;
                    true
                });
        });
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            view.text = Some("fresh result");
            view.retention.clear();
            view.wake.schedule(None, cx, |_, _, _| false);
        });
        cx.executor().advance_clock(Duration::from_secs(61));
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.text),
            Some("fresh result")
        );
    }
}
