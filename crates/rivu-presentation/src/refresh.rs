use std::time::{Duration, Instant};

/// Maximum display-only lifetime after data loses authority.
pub const REFRESH_RETENTION: Duration = Duration::from_millis(200);

/// One continuous stale interval. Only fresh authority or an explicit identity
/// discard may clear it; superseding requests and expiry never restart it.
#[derive(Default)]
pub struct RefreshDeadline {
    deadline: Option<Instant>,
    finished: bool,
}

impl RefreshDeadline {
    pub fn begin(&mut self, now: Instant) {
        self.deadline.get_or_insert(now + REFRESH_RETENTION);
    }

    /// The pending expiry wake, absent once the interval has been expired.
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline.filter(|_| !self.finished)
    }

    pub fn visible(&self, now: Instant) -> bool {
        !self.finished && self.deadline.is_some_and(|deadline| now < deadline)
    }

    pub fn expired(&self, now: Instant) -> bool {
        self.deadline.is_some_and(|deadline| now >= deadline)
    }

    /// Finish an elapsed interval once, without allowing stale data to reopen it.
    /// The owner must release its display-only payload when this returns true.
    pub fn expire(&mut self, now: Instant) -> bool {
        if self.finished || !self.expired(now) {
            return false;
        }
        self.finished = true;
        true
    }

    /// Restore fresh authority, or discard the previous display identity.
    pub fn clear(&mut self) {
        self.deadline = None;
        self.finished = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_invalidation_cannot_extend_stale_display() {
        let start = Instant::now();
        let mut retention = RefreshDeadline::default();
        assert_eq!(retention.deadline(), None);
        assert!(!retention.visible(start));
        assert!(!retention.expire(start));
        retention.begin(start);
        retention.begin(start + Duration::from_millis(150));
        let deadline = start + REFRESH_RETENTION;
        assert_eq!(retention.deadline(), Some(deadline));
        assert!(retention.visible(deadline - Duration::from_nanos(1)));
        assert!(!retention.expire(deadline - Duration::from_nanos(1)));
        assert!(!retention.visible(deadline));
        assert!(retention.expired(deadline));
        assert!(retention.expire(deadline));
        assert_eq!(retention.deadline(), None);
        assert!(!retention.expire(deadline));
        retention.begin(start + Duration::from_secs(1));
        assert_eq!(retention.deadline(), None);
        assert!(!retention.visible(start + Duration::from_secs(1)));
        assert!(retention.expired(start + Duration::from_secs(1)));
        retention.clear();
        retention.begin(start + Duration::from_secs(1));
        assert!(retention.visible(start + Duration::from_secs(1)));
        assert_eq!(
            retention.deadline(),
            Some(start + Duration::from_secs(1) + REFRESH_RETENTION)
        );
    }
}
