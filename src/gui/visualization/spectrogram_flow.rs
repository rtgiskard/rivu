//! Display-only flow: fill the viewport, hold its far edge while history compacts,
//! then smoothly open the outflow. Audio buckets and their timestamps never move.
use std::time::Duration;

// Peak derivative of the unit quintic. This makes `speed` a viewport-relative
// upper bound on the filling front's speed rather than an arbitrary duration.
const FILL_DISTANCE: f64 = 1.875;
const SPEED_RESPONSE: f64 = 0.2;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Mapping {
    coverage: f64,
    span: f64,
}

impl Mapping {
    pub(super) fn scale(self) -> f64 {
        if self.span > 0.0 {
            self.coverage / self.span
        } else {
            0.0
        }
    }

    pub(super) fn position(self, time: Duration, latest: Duration) -> f32 {
        (1.0 + (time.as_secs_f64() - latest.as_secs_f64()) * self.scale()) as f32
    }

    pub(super) fn visible_start(self, latest: Duration) -> Duration {
        let scale = self.scale();
        if scale > 0.0 {
            Duration::from_secs_f64((latest.as_secs_f64() - 1.0 / scale).max(0.0))
        } else {
            latest
        }
    }
}

#[derive(Default)]
pub(super) struct SpectrogramFlow {
    origin: Option<Duration>,
    last_time: Option<Duration>,
    phase: f64,
    speed: f64,
    // Latch the release once it starts: editing the threshold must not suddenly
    // pull already-outgoing history back into the viewport.
    release: Option<(f64, f64)>,
    mapping: Mapping,
}

impl SpectrogramFlow {
    pub(super) fn mapping(&self) -> Mapping {
        self.mapping
    }

    pub(super) fn advance(
        &mut self,
        now: Duration,
        first_interval: Duration,
        limit: f64,
        speed: f32,
        accumulation: u32,
    ) {
        let speed = f64::from(speed);
        let origin = *self.origin.get_or_insert(first_interval.min(now));
        let Some(previous) = self.last_time else {
            self.last_time = Some(now);
            self.speed = speed;
            return;
        };
        if now <= previous {
            return;
        }
        self.last_time = Some(now);
        if self.phase < 1.0 {
            // Do not replay a long hidden animation in one draw. Audio time
            // itself remains bounded by accepted data.
            let dt = now.saturating_sub(previous).as_secs_f64().min(0.1);
            let distance = if self.speed == speed {
                speed * dt
            } else {
                let decay = (-dt / SPEED_RESPONSE).exp();
                let distance = speed * dt + (self.speed - speed) * SPEED_RESPONSE * (1.0 - decay);
                self.speed = speed + (self.speed - speed) * decay;
                distance
            };
            self.phase = (self.phase + distance / FILL_DISTANCE).min(1.0);
        }
        let age = now.saturating_sub(origin).as_secs_f64().max(1e-9);
        if self.phase >= 1.0 && self.release.is_none() {
            // Wait until both the requested accumulation and the velocity
            // bound permit release. Very short histories necessarily retain
            // their ordinary audio-time scroll speed (1 / limit).
            let threshold = (limit * f64::from(accumulation) / 100.0)
                .max(1.0 / speed)
                .min(limit);
            if age >= threshold {
                self.release = Some((age, age.min(limit)));
            }
        }
        self.remap_at_age(age, limit);
    }

    /// Reproject a changed capacity without advancing the frozen audio clock or
    /// changing fill progress, speed response, or the latched release.
    pub(super) fn remap(&mut self, limit: f64) {
        let (Some(now), Some(origin)) = (self.last_time, self.origin) else {
            return;
        };
        let age = now.saturating_sub(origin).as_secs_f64().max(1e-9);
        self.remap_at_age(age, limit);
    }

    fn remap_at_age(&mut self, age: f64, limit: f64) {
        let mut span = age.min(limit);
        if let Some((start, held_span)) = self.release {
            let held_span = held_span.min(limit);
            let duration = 2.0 * (limit - held_span);
            span = if duration <= 1e-9 || age >= start + duration {
                limit
            } else {
                let u = ((age - start) / duration).clamp(0.0, 1.0);
                // Integrate 1 - smootherstep(u). Span growth changes from
                // 1 to 0 with continuous acceleration, reaching exactly the
                // existing history limit without retaining any extra audio.
                let integral = u - 2.5 * u.powi(4) + 3.0 * u.powi(5) - u.powi(6);
                held_span + duration * integral
            };
        }
        self.mapping = Mapping {
            coverage: smootherstep(self.phase),
            span: span.max(1e-9),
        };
    }
}

fn smootherstep(u: f64) -> f64 {
    u * u * u * (10.0 + u * (-15.0 + 6.0 * u))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tick(flow: &mut SpectrogramFlow, t: f64, speed: f32, accumulation: u32) -> Mapping {
        flow.advance(
            Duration::from_secs_f64(t),
            Duration::ZERO,
            20.0,
            speed,
            accumulation,
        );
        flow.mapping()
    }

    #[test]
    fn front_fills_gradually_stops_then_history_leaves_without_reversing() {
        let mut flow = SpectrogramFlow::default();
        tick(&mut flow, 0.0, 0.75, 75);
        let mut last = 1.0;
        let mut last_velocity = 0.0;
        for i in 1..=120 {
            let t = i as f64 / 60.0;
            let mapping = tick(&mut flow, t, 0.75, 75);
            let front = f64::from(mapping.position(Duration::ZERO, Duration::from_secs_f64(t)));
            let velocity = (last - front) * 60.0;
            assert!((-1e-5..=0.751).contains(&velocity));
            assert!((velocity - last_velocity).abs() < 0.04);
            last = front;
            last_velocity = velocity;
        }
        assert!(last > 0.0, "front must not reach the far edge immediately");
        for i in 121..=1800 {
            let t = i as f64 / 60.0;
            let mapping = tick(&mut flow, t, 0.75, 75);
            let front = f64::from(mapping.position(Duration::ZERO, Duration::from_secs_f64(t)));
            assert!(front <= last + 2e-6, "history must not flow backwards");
            if (3.0..=14.0).contains(&t) {
                assert!(
                    front.abs() < 1e-6,
                    "far edge must stay blocked while accumulating"
                );
            }
            if t >= 27.0 {
                assert!((mapping.scale() - 1.0 / 20.0).abs() < 1e-9);
                assert!(front < 0.0, "oldest history must have left the viewport");
            }
            last = front;
        }
    }

    #[test]
    fn speed_edits_and_publication_gaps_do_not_restart_or_teleport_the_front() {
        let mut flow = SpectrogramFlow::default();
        for i in 0..=60 {
            tick(&mut flow, i as f64 / 60.0, 0.75, 75);
        }
        let before = flow.mapping();
        tick(&mut flow, 1.0, 2.0, 75);
        assert_eq!(flow.mapping(), before);
        let mut previous = before.coverage;
        for i in 61..=90 {
            let next = tick(&mut flow, i as f64 / 60.0, 2.0, 75);
            assert!(next.coverage >= previous);
            assert!((next.coverage - previous) * 60.0 <= 2.001);
            previous = next.coverage;
        }
        tick(&mut flow, 10.0, 2.0, 75);
        assert!(flow.mapping().coverage - previous <= 0.201);
        let frozen = flow.mapping();
        tick(&mut flow, 9.0, 2.0, 75);
        assert_eq!(
            flow.mapping(),
            frozen,
            "stale clocks must not rewind motion"
        );
    }

    #[test]
    fn controls_change_fill_speed_and_release_threshold_without_clock_restarts() {
        let mut slow = SpectrogramFlow::default();
        let mut fast = SpectrogramFlow::default();
        let mut low = SpectrogramFlow::default();
        let mut high = SpectrogramFlow::default();
        for i in 0..=720 {
            let t = i as f64 / 60.0;
            tick(&mut slow, t, 0.3, 75);
            tick(&mut fast, t, 1.2, 75);
            tick(&mut low, t, 0.75, 25);
            tick(&mut high, t, 0.75, 90);
            if i == 60 {
                assert!(slow.mapping().coverage < fast.mapping().coverage);
            }
        }
        assert!(low.mapping().span < high.mapping().span);
        let before = slow.mapping();
        tick(&mut slow, 12.0, 2.0, 10);
        assert_eq!(
            slow.mapping(),
            before,
            "configuration alone must not move history"
        );
        let next = tick(&mut slow, 12.0 + 1.0 / 60.0, 2.0, 10);
        assert!(
            next.position(Duration::ZERO, Duration::from_secs_f64(12.0 + 1.0 / 60.0))
                <= before.position(Duration::ZERO, Duration::from_secs(12))
        );
    }

    #[test]
    fn paused_capacity_round_trip_preserves_fill_and_release_progress() {
        for end_tick in [60, 600, 1080, 2400] {
            let mut resized = SpectrogramFlow::default();
            let mut control = SpectrogramFlow::default();
            for i in 0..=end_tick {
                let time = i as f64 / 60.0;
                tick(&mut resized, time, 0.75, 75);
                tick(&mut control, time, 0.75, 75);
            }
            let frozen = resized.mapping();
            resized.remap(10.0);
            resized.remap(20.0);
            assert_eq!(resized.mapping(), frozen);
            let next = (end_tick + 1) as f64 / 60.0;
            assert_eq!(
                tick(&mut resized, next, 0.75, 95),
                tick(&mut control, next, 0.75, 95),
                "capacity edits must not restart filling or release"
            );
        }
    }

    #[test]
    fn frozen_audio_and_long_running_scroll_do_not_accumulate_lag() {
        let mut flow = SpectrogramFlow::default();
        for i in 0..=2400 {
            tick(&mut flow, i as f64 / 60.0, 0.75, 75);
        }
        let frozen = flow.mapping();
        for _ in 0..30 {
            tick(&mut flow, 40.0, 0.75, 75);
        }
        assert_eq!(flow.mapping(), frozen);
        let next = tick(&mut flow, 40.1, 0.75, 75);
        assert_eq!(
            next.position(Duration::from_millis(40100), Duration::from_millis(40100)),
            1.0
        );
        assert!(
            (next.position(Duration::from_millis(20100), Duration::from_millis(40100))).abs()
                < 1e-6
        );
    }
}
