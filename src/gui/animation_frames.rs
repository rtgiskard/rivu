//! One window-local native frame gate for artwork and audio visualization.

use super::{album_artwork::AlbumArtwork, presentation::Presentation};
use gpui::{App, Context, Entity, Task, WeakEntity, Window};
use std::time::{Duration, Instant};

struct Slot<T> {
    owner: Option<WeakEntity<T>>,
    generation: u64,
    deadline: Option<Instant>,
    delivered: Option<Instant>,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Self {
            owner: None,
            generation: 0,
            deadline: None,
            delivered: None,
        }
    }
}

impl<T> Slot<T> {
    fn request(&mut self, owner: WeakEntity<T>, generation: u64, deadline: Instant) {
        self.owner = Some(owner);
        self.generation = generation;
        self.deadline = Some(deadline);
    }

    fn cancel(&mut self, generation: u64) {
        if self.generation == generation {
            self.owner = None;
            self.deadline = None;
            self.delivered = None;
        }
    }

    fn take_due(&mut self, now: Instant) -> Option<(WeakEntity<T>, u64)> {
        if self.deadline.is_none_or(|deadline| deadline > now) {
            return None;
        }
        self.deadline = None;
        self.delivered = Some(now);
        self.owner.take().map(|owner| (owner, self.generation))
    }
}

/// A shared epoch aligns equal rates without replacing either client's FPS.
/// Late delivery advances to the next grid boundary, never replaying a backlog.
fn deadline(epoch: Instant, now: Instant, fps: u16, delivered: Option<Instant>) -> Instant {
    let Some(delivered) = delivered.filter(|_| fps != 0) else {
        return now;
    };
    let interval = 1_000_000_000_u64.div_ceil(u64::from(fps));
    let tick = delivered.saturating_duration_since(epoch).as_nanos() / u128::from(interval) + 1;
    let next = epoch + Duration::from_nanos((tick * u128::from(interval)) as u64);
    next.max(now)
}

pub(super) struct AnimationFrames {
    epoch: Instant,
    artwork: Slot<AlbumArtwork>,
    presentation: Slot<Presentation>,
    timer: Option<Task<()>>,
    timer_deadline: Option<Instant>,
    native_pending: bool,
}

impl Default for AnimationFrames {
    fn default() -> Self {
        Self {
            epoch: Instant::now(),
            artwork: Slot::default(),
            presentation: Slot::default(),
            timer: None,
            timer_deadline: None,
            native_pending: false,
        }
    }
}

impl AnimationFrames {
    pub(super) fn request_artwork(
        &mut self,
        owner: WeakEntity<AlbumArtwork>,
        generation: u64,
        fps: u16,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let due = deadline(self.epoch, Instant::now(), fps, self.artwork.delivered);
        self.artwork.request(owner, generation, due);
        self.arm(window, cx);
    }

    pub(super) fn request_presentation(
        &mut self,
        owner: WeakEntity<Presentation>,
        generation: u64,
        fps: u16,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let due = deadline(self.epoch, Instant::now(), fps, self.presentation.delivered);
        self.presentation.request(owner, generation, due);
        self.arm(window, cx);
    }

    pub(super) fn cancel_artwork(&mut self, generation: u64) {
        self.artwork.cancel(generation);
        self.cancel_idle_timer();
    }

    pub(super) fn cancel_presentation(&mut self, generation: u64) {
        self.presentation.cancel(generation);
        self.cancel_idle_timer();
    }

    fn cancel_idle_timer(&mut self) {
        if self.next_deadline().is_none() {
            self.timer = None;
            self.timer_deadline = None;
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        match (self.artwork.deadline, self.presentation.deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn arm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.native_pending {
            return;
        }
        let Some(due) = self.next_deadline() else {
            self.cancel_idle_timer();
            return;
        };
        let now = Instant::now();
        if due > now {
            if self.timer_deadline == Some(due) {
                return;
            }
            self.timer_deadline = Some(due);
            let timer = cx.background_executor().timer(due.duration_since(now));
            self.timer = Some(cx.spawn_in(window, async move |this, cx| {
                timer.await;
                let _ = this.update_in(cx, |this, window, cx| {
                    this.timer = None;
                    this.timer_deadline = None;
                    this.arm(window, cx);
                });
            }));
            return;
        }
        self.timer = None;
        self.timer_deadline = None;
        self.native_pending = true;
        let scheduler = cx.entity();
        window.on_next_frame(move |window, cx| Self::deliver(scheduler, window, cx));
        // Scheduling is not invalidation. Only accepted animation changes notify.
    }

    fn deliver(scheduler: Entity<Self>, window: &mut Window, cx: &mut App) {
        let now = Instant::now();
        let (artwork, presentation) = scheduler.update(cx, |this, _| {
            // Keep the gate occupied while clients re-register during delivery.
            (this.artwork.take_due(now), this.presentation.take_due(now))
        });
        if let Some((owner, generation)) = artwork {
            let _ = owner.update(cx, |owner, cx| {
                owner.animation_frame(generation, now, window, cx)
            });
        }
        if let Some((owner, generation)) = presentation {
            let _ = owner.update(cx, |owner, cx| {
                owner.animation_frame(generation, now, window, cx)
            });
        }
        scheduler.update(cx, |this, cx| {
            this.native_pending = false;
            this.arm(window, cx);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_rates_align_after_different_delivery_times() {
        let epoch = Instant::now();
        let a = epoch + Duration::from_millis(3);
        let b = epoch + Duration::from_millis(19);
        assert_eq!(
            deadline(epoch, a, 30, Some(a)),
            deadline(epoch, b, 30, Some(b))
        );
        assert_ne!(
            deadline(epoch, a, 20, Some(a)),
            deadline(epoch, b, 30, Some(b))
        );
    }

    #[test]
    fn zero_fps_and_late_frames_never_replay_deadlines() {
        let epoch = Instant::now();
        let late = epoch + Duration::from_secs(10);
        assert_eq!(deadline(epoch, late, 0, Some(epoch)), late);
        assert_eq!(deadline(epoch, late, 30, Some(epoch)), late);
        assert!(deadline(epoch, late, 30, Some(late)) > late);
    }
}
