use std::time::Instant;

use gpui::Context;

use crate::deadline::DeadlineWake;

/// One cancellable wake per feature, independent of query completion or frames.
#[derive(Default)]
pub struct RefreshWake {
    wake: DeadlineWake,
}

impl RefreshWake {
    /// Expire retained data now, then arm the next wake.
    pub fn sync_expiring<T: 'static>(
        owner: &mut T,
        cx: &mut Context<T>,
        locate: fn(&mut T) -> &mut Self,
        next: fn(&T) -> Option<Instant>,
        expire: fn(&mut T, Instant, &mut Context<T>) -> bool,
    ) {
        if expire(owner, cx.background_executor().now(), cx) {
            cx.notify();
        }
        Self::sync(owner, cx, locate, next, expire);
    }

    /// Sync after a model change. The expiry callback must consume all deadlines
    /// through `now`; the next deadline is read again after cleanup.
    pub fn sync<T: 'static>(
        owner: &mut T,
        cx: &mut Context<T>,
        locate: fn(&mut T) -> &mut Self,
        next: fn(&T) -> Option<Instant>,
        expire: fn(&mut T, Instant, &mut Context<T>) -> bool,
    ) {
        let deadline = next(owner);
        locate(owner).wake.arm(deadline, cx, move |owner, now, cx| {
            if expire(owner, now, cx) {
                cx.notify();
            }
            Self::sync(owner, cx, locate, next, expire);
        });
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

        impl View {
            fn sync(&mut self, cx: &mut Context<Self>) {
                RefreshWake::sync(
                    self,
                    cx,
                    |view| &mut view.wake,
                    |view| view.retention.deadline(),
                    |view, now, _| view.retention.expire(now) && view.text.take().is_some(),
                );
            }
        }

        let view = cx.update(|cx| cx.new(|_| View::default()));
        let start = cx.executor().now();
        view.update(cx, |view, cx| {
            view.text = Some("old result");
            view.retention.begin(start);
            view.sync(cx);
        });
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.text), Some("old result"));
        cx.executor().advance_clock(REFRESH_RETENTION);
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.text), None);

        view.update(cx, |view, cx| {
            view.text = Some("another old result");
            view.retention.clear();
            view.retention
                .begin(cx.background_executor().now() + Duration::from_secs(60));
            view.sync(cx);
        });
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            view.text = Some("fresh result");
            view.retention.clear();
            view.sync(cx);
        });
        cx.executor().advance_clock(Duration::from_secs(61));
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.text),
            Some("fresh result")
        );
    }

    #[gpui::test]
    fn refresh_rearms_for_next_payload_without_publication(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext;

        #[derive(Default)]
        struct View {
            deadlines: [Option<Instant>; 2],
            last_wake: Option<Instant>,
            wake: RefreshWake,
        }

        let view = cx.update(|cx| cx.new(|_| View::default()));
        let start = cx.executor().now();
        cx.executor().advance_clock(Duration::from_secs(61));
        view.update(cx, |view, cx| {
            view.deadlines = [
                Some(start + Duration::from_secs(60)),
                Some(start + Duration::from_secs(120)),
            ];
            RefreshWake::sync(
                view,
                cx,
                |view| &mut view.wake,
                |view| view.deadlines.into_iter().flatten().min(),
                |view, now, _| {
                    view.last_wake = Some(now);
                    for deadline in &mut view.deadlines {
                        if deadline.is_some_and(|at| at <= now) {
                            *deadline = None;
                        }
                    }
                    true
                },
            );
        });
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.last_wake),
            Some(start + Duration::from_secs(61)),
        );
        assert_eq!(
            view.read_with(cx, |view, _| view.deadlines.map(|at| at.is_some())),
            [false, true],
        );
        cx.executor().advance_clock(Duration::from_secs(59));
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.deadlines), [None; 2]);
        assert_eq!(
            view.read_with(cx, |view, _| view.last_wake),
            Some(start + Duration::from_secs(120)),
        );
    }
}
