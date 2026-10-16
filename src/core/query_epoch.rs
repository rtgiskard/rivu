//! Owner-scoped cancellation for queued Core reads.

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
