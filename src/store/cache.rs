use super::Store;
use super::rows::{TRACK_COLUMNS, track_from_row};
use anyhow::Result;
use rivu_protocol::model::Track;
use rusqlite::OptionalExtension;
use std::collections::HashMap;

pub(super) struct TrackCache {
    pub(super) entries: HashMap<i64, (Track, u64)>,
    pub(super) capacity: usize,
    pub(super) clock: u64,
}

impl TrackCache {
    fn evict_oldest(&mut self) -> bool {
        let oldest = self
            .entries
            .iter()
            .min_by_key(|(_, (_, age))| *age)
            .map(|(&id, _)| id);
        oldest
            .map(|id| self.entries.remove(&id).is_some())
            .unwrap_or(false)
    }

    fn trim(&mut self) {
        while self.entries.len() > self.capacity {
            self.evict_oldest();
        }
    }
}

impl Store {
    pub(crate) fn set_track_cache_query_batch_size(&self, query_batch_size: u32) {
        let mut cache = self.track_cache.borrow_mut();
        cache.capacity = (query_batch_size as usize).saturating_mul(2);
        cache.trim();
        let capacity = cache.capacity;
        cache.entries.shrink_to(capacity);
    }

    pub fn track(&self, id: i64) -> Result<Option<Track>> {
        let mut cache = self.track_cache.borrow_mut();
        if cache.clock == u64::MAX {
            cache.entries.clear();
            cache.clock = 0;
        }
        cache.clock += 1;
        let age = cache.clock;
        if let Some((track, touched)) = cache.entries.get_mut(&id) {
            *touched = age;
            return Ok(Some(track.clone()));
        }
        let sql = format!("SELECT {TRACK_COLUMNS} FROM tracks WHERE id=?1");
        let track = self.conn.query_row(&sql, [id], track_from_row).optional()?;
        if let Some(track) = &track
            && cache.capacity != 0
        {
            // Evict before insertion so entries never transiently exceed capacity.
            cache.trim();
            if cache.entries.len() == cache.capacity {
                cache.evict_oldest();
            }
            cache.entries.insert(id, (track.clone(), age));
        }
        Ok(track)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CountDomain {
    Other,
    Directory,
    Albums,
    AlbumTracks,
    Queue,
    PlaylistEntries,
}

// Only narrow scalar totals are retained. A small LRU covers interleaved views
// without retaining rows or growing with the number of visited directories.
#[derive(Default)]
pub(super) struct ScalarCounts {
    changes: u64,
    external_revision: i64,
    entries: Vec<ScalarCount>,
}

struct ScalarCount {
    domain: CountDomain,
    keys: Vec<String>,
    identity: i64,
    total: usize,
}

impl Store {
    pub(crate) fn invalidate_scalar_counts(&self) {
        self.scalar_counts.borrow_mut().entries.clear();
    }

    pub(crate) fn invalidate_scalar_count_domains(&self, domains: &[CountDomain]) {
        self.scalar_counts
            .borrow_mut()
            .entries
            .retain(|entry| !domains.contains(&entry.domain));
    }

    pub(super) fn cached_count(
        &self,
        domain: CountDomain,
        keys: &[&str],
        read: impl FnOnce() -> Result<usize>,
    ) -> Result<usize> {
        self.cached_count_for_id(domain, 0, keys, read)
    }

    pub(super) fn cached_count_for_id(
        &self,
        domain: CountDomain,
        identity: i64,
        keys: &[&str],
        read: impl FnOnce() -> Result<usize>,
    ) -> Result<usize> {
        // A writer transaction must never publish a count that could outlive a
        // rollback. Read workers instead invalidate on accepted revision ownership.
        if !self.revision_owned_counts && !self.conn.is_autocommit() {
            return read();
        }
        let changes = self.conn.total_changes();
        // Read workers own accepted revisions. Writable owners must also see
        // commits made by independent scan/admission connections.
        let external_revision = if self.revision_owned_counts {
            0
        } else {
            self.conn
                .query_row("PRAGMA data_version", [], |row| row.get::<_, i64>(0))?
        };
        {
            let mut cache = self.scalar_counts.borrow_mut();
            if cache.changes != changes || cache.external_revision != external_revision {
                cache.entries.clear();
                cache.changes = changes;
                cache.external_revision = external_revision;
            }
            if let Some(index) = cache.entries.iter().position(|entry| {
                entry.domain == domain
                    && entry.identity == identity
                    && entry
                        .keys
                        .iter()
                        .map(String::as_str)
                        .eq(keys.iter().copied())
            }) {
                let entry = cache.entries.remove(index);
                let total = entry.total;
                cache.entries.push(entry);
                return Ok(total);
            }
        }
        let total = read()?;
        let mut cache = self.scalar_counts.borrow_mut();
        if cache.entries.len() == 16 {
            cache.entries.remove(0);
        }
        cache.entries.push(ScalarCount {
            domain,
            identity,
            keys: keys.iter().map(|key| (*key).to_owned()).collect(),
            total,
        });
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_totals_are_bounded_and_failed_reads_are_not_cached() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        for identity in 0..100 {
            assert_eq!(
                store
                    .cached_count_for_id(
                        CountDomain::PlaylistEntries,
                        identity,
                        &["filter"],
                        || {
                            Ok(store.conn.query_row(
                                "SELECT COUNT(*) FROM tracks WHERE id>=?1",
                                [identity],
                                |row| row.get::<_, i64>(0),
                            )? as usize)
                        }
                    )
                    .unwrap(),
                0
            );
            assert!(store.scalar_counts.borrow().entries.len() <= 16);
        }
        assert_eq!(store.scalar_counts.borrow().entries.len(), 16);
        assert!(
            store
                .cached_count(CountDomain::Queue, &[], || anyhow::bail!("unavailable"))
                .is_err()
        );
        assert_eq!(
            store
                .cached_count(CountDomain::Queue, &[], || Ok(7))
                .unwrap(),
            7
        );
        store.invalidate_scalar_counts();
        assert!(store.scalar_counts.borrow().entries.is_empty());
    }
}
