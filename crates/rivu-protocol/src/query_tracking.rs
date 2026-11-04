//! Query identity, freshness, and owner-scoped cancellation across consumers and Core.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

/// One consumer's reusable read generation, independent of every other owner.
#[derive(Debug, Default)]
pub struct QueryEpoch {
    generation: u64,
    shared: Option<Arc<AtomicU64>>,
}

impl QueryEpoch {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn cancel(&mut self) {
        self.advance();
    }

    pub fn advance(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if let Some(shared) = &self.shared {
            shared.store(self.generation, Ordering::Release);
        }
    }

    /// Allocate once on first submission; later tickets only clone the Arc.
    pub fn ticket(&mut self) -> QueryTicket {
        let shared = self
            .shared
            .get_or_insert_with(|| Arc::new(AtomicU64::new(self.generation)));
        QueryTicket {
            shared: Arc::clone(shared),
            generation: self.generation,
        }
    }
}

impl Drop for QueryEpoch {
    fn drop(&mut self) {
        self.advance();
    }
}

#[derive(Clone, Debug)]
pub struct QueryTicket {
    shared: Arc<AtomicU64>,
    generation: u64,
}

impl QueryTicket {
    pub fn is_current(&self) -> bool {
        self.shared.load(Ordering::Acquire) == self.generation
    }
}

/// Value receipt for a submission to its issuing tracker.
/// A receipt covers identity and freshness; page demand stays with the consumer.
/// It must not be compared or accepted across different tracker owners.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuerySubmission {
    generation: u64,
}

/// Owns query identity and freshness, never payload or business operations.
/// Page demand and the physical in-flight worker gate stay with the consumer.
#[derive(Debug, Default)]
pub struct QueryTracker {
    identity: Option<crate::model::Query>,
    stamp: Option<crate::model::QueryStamp>,
    generation: QueryEpoch,
}

impl QueryTracker {
    pub fn sync(
        &mut self,
        query: &crate::model::Query,
        revisions: crate::response::QueryRevisions,
    ) -> bool {
        let stamp = query.stamp(revisions);
        let identity_changed = !self.matches_identity(query);
        if !identity_changed && self.stamp == Some(stamp) {
            return false;
        }
        if identity_changed {
            self.identity = Some(query.clone());
        }
        self.stamp = Some(stamp);
        self.cancel();
        true
    }

    /// Synchronize demand and capture a value receipt; only identity changes clone the query.
    pub fn submit(
        &mut self,
        query: &crate::model::Query,
        revisions: crate::response::QueryRevisions,
    ) -> QuerySubmission {
        self.sync(query, revisions);
        QuerySubmission {
            generation: self.generation.generation(),
        }
    }

    /// Accept only the issuing tracker's current demand and response version.
    pub fn accepts_submission(
        &self,
        submission: QuerySubmission,
        revisions: crate::response::QueryRevisions,
    ) -> bool {
        submission.generation == self.generation.generation()
            && self
                .identity
                .as_ref()
                .is_some_and(|query| self.stamp == Some(query.stamp(revisions)))
    }

    pub fn generation(&self) -> u64 {
        self.generation.generation()
    }

    pub fn stamp(&self) -> Option<crate::model::QueryStamp> {
        self.stamp
    }

    pub fn matches_identity(&self, query: &crate::model::Query) -> bool {
        self.identity
            .as_ref()
            .is_some_and(|current| current.identity() == query.identity())
    }

    /// Retiring submissions does not invalidate a still-current cached payload.
    pub fn cancel(&mut self) {
        self.generation.cancel();
    }

    pub fn ticket(&mut self) -> QueryTicket {
        self.generation.ticket()
    }

    pub fn accepts(
        &self,
        query: &crate::model::Query,
        generation: u64,
        revisions: crate::response::QueryRevisions,
    ) -> bool {
        self.generation.generation() == generation
            && self.matches_identity(query)
            && self.stamp == Some(query.stamp(revisions))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelling_or_dropping_an_epoch_invalidates_its_tickets() {
        let mut epoch = QueryEpoch::default();
        epoch.advance();
        let first = epoch.ticket();
        assert!(first.is_current());
        epoch.cancel();
        assert!(!first.is_current());
        let next = epoch.ticket();
        assert!(next.is_current());
        drop(epoch);
        assert!(!next.is_current());
    }

    #[test]
    fn identity_changes_and_cancellation_retire_previous_submissions() {
        let mut tracker = QueryTracker::default();
        let revisions = crate::response::QueryRevisions::default();
        let original = crate::model::Query::Track { track_id: 1 };
        let replacement = crate::model::Query::Track { track_id: 2 };

        let first = tracker.submit(&original, revisions);
        assert!(tracker.accepts_submission(first, revisions));
        let changed = tracker.submit(&replacement, revisions);
        assert!(!tracker.accepts_submission(first, revisions));
        assert!(tracker.accepts_submission(changed, revisions));

        tracker.cancel();
        assert!(!tracker.accepts_submission(changed, revisions));
        let resubmitted = tracker.submit(&replacement, revisions);
        assert!(!tracker.accepts_submission(changed, revisions));
        assert!(tracker.accepts_submission(resubmitted, revisions));
    }

    #[test]
    fn submissions_select_relevant_response_revisions() {
        let mut tracker = QueryTracker::default();
        let query = crate::model::Query::LibraryStats;
        let revisions = crate::response::QueryRevisions {
            library: 10,
            ..Default::default()
        };
        let first = tracker.submit(&query, revisions);
        let unrelated_change = crate::response::QueryRevisions {
            structure: 20,
            ..revisions
        };
        assert!(tracker.accepts_submission(first, unrelated_change));
        let unchanged = tracker.submit(&query, unrelated_change);
        assert!(tracker.accepts_submission(first, revisions));
        assert!(tracker.accepts_submission(unchanged, unrelated_change));

        let relevant_change = crate::response::QueryRevisions {
            library: 11,
            ..unrelated_change
        };
        assert!(!tracker.accepts_submission(first, relevant_change));
        let refreshed = tracker.submit(&query, relevant_change);
        assert!(!tracker.accepts_submission(first, revisions));
        assert!(!tracker.accepts_submission(unchanged, relevant_change));
        assert!(!tracker.accepts_submission(refreshed, revisions));
        assert!(tracker.accepts_submission(refreshed, relevant_change));
    }

    #[test]
    fn page_offsets_do_not_retire_owner_submissions() {
        let mut tracker = QueryTracker::default();
        let revisions = crate::response::QueryRevisions::default();
        let first_page = crate::model::Query::PlaylistSummaries {
            query: Some("favorites".into()),
            offset: 0,
            limit: 25,
        };
        let next_page = crate::model::Query::PlaylistSummaries {
            query: Some("favorites".into()),
            offset: 25,
            limit: 25,
        };

        let first = tracker.submit(&first_page, revisions);
        let next = tracker.submit(&next_page, revisions);
        assert!(tracker.accepts_submission(first, revisions));
        assert!(tracker.accepts_submission(next, revisions));
        let repeated = tracker.submit(&first_page, revisions);
        assert!(tracker.accepts_submission(first, revisions));
        assert!(tracker.accepts_submission(next, revisions));
        assert!(tracker.accepts_submission(repeated, revisions));
    }
}
