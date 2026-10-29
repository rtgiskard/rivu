//! Query identity, freshness, and owner-scoped cancellation across consumers and Core.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

/// One consumer's reusable read generation, independent of every other owner.
#[derive(Debug, Default)]
pub(crate) struct QueryEpoch {
    generation: u64,
    shared: Option<Arc<AtomicU64>>,
}

impl QueryEpoch {
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn cancel(&mut self) {
        self.advance();
    }

    pub(crate) fn advance(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if let Some(shared) = &self.shared {
            shared.store(self.generation, Ordering::Release);
        }
    }

    /// Allocate once on first submission; later tickets only clone the Arc.
    pub(crate) fn ticket(&mut self) -> QueryTicket {
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
pub(crate) struct QueryTicket {
    shared: Arc<AtomicU64>,
    generation: u64,
}

impl QueryTicket {
    pub(crate) fn is_current(&self) -> bool {
        self.shared.load(Ordering::Acquire) == self.generation
    }
}

/// Owns query identity and freshness, never payload or business operations.
/// Page demand and the physical in-flight worker gate stay with the consumer.
#[derive(Debug, Default)]
pub(crate) struct QueryTracker {
    identity: Option<rivu_protocol::model::Query>,
    stamp: Option<rivu_protocol::model::QueryStamp>,
    generation: QueryEpoch,
}

impl QueryTracker {
    pub(crate) fn sync(
        &mut self,
        query: &rivu_protocol::model::Query,
        revisions: rivu_protocol::response::QueryRevisions,
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

    pub(crate) fn generation(&self) -> u64 {
        self.generation.generation()
    }

    pub(crate) fn stamp(&self) -> Option<rivu_protocol::model::QueryStamp> {
        self.stamp
    }

    pub(crate) fn matches_identity(&self, query: &rivu_protocol::model::Query) -> bool {
        self.identity
            .as_ref()
            .is_some_and(|current| current.identity() == query.identity())
    }

    /// Retiring submissions does not invalidate a still-current cached payload.
    pub(crate) fn cancel(&mut self) {
        self.generation.cancel();
    }

    pub(crate) fn ticket(&mut self) -> QueryTicket {
        self.generation.ticket()
    }

    pub(crate) fn accepts(
        &self,
        query: &rivu_protocol::model::Query,
        generation: u64,
        revisions: rivu_protocol::response::QueryRevisions,
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
    fn epochs_allocate_lazily_and_reuse_their_generation_cell() {
        let mut epoch = QueryEpoch::default();
        epoch.advance();
        assert_eq!(epoch.generation(), 1);
        assert!(epoch.shared.is_none());
        let first = epoch.ticket();
        let repeated = epoch.ticket();
        assert!(Arc::ptr_eq(&first.shared, &repeated.shared));
        assert!(first.is_current());
        epoch.cancel();
        assert!(!first.is_current());
        let next = epoch.ticket();
        assert!(next.is_current());
        assert!(Arc::ptr_eq(&first.shared, &next.shared));
        drop(epoch);
        assert!(!next.is_current());
    }
}
