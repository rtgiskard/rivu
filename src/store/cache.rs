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
        let track = self
            .conn
            .prepare_cached(&sql)?
            .query_row([id], track_from_row)
            .optional()?;
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
    FilteredLibrary,
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
    playlist_metadata: Option<(std::sync::Arc<rivu_protocol::model::MetadataRevisions>, u64)>,
    entries: Vec<ScalarCount>,
    #[cfg(test)]
    reads: usize,
}

enum ScalarScope<'a> {
    Keys {
        identity: i64,
        keys: &'a [&'a str],
    },
    FilteredLibrary {
        query: Option<&'a str>,
        favorite: Option<bool>,
        missing: Option<bool>,
        prefix: Option<&'a str>,
    },
}

enum OwnedScalarScope {
    Keys {
        identity: i64,
        keys: Vec<String>,
    },
    FilteredLibrary {
        query: Option<String>,
        favorite: Option<bool>,
        missing: Option<bool>,
        prefix: Option<String>,
    },
}

impl ScalarScope<'_> {
    fn matches(&self, cached: &OwnedScalarScope) -> bool {
        match (self, cached) {
            (
                Self::Keys { identity, keys },
                OwnedScalarScope::Keys {
                    identity: cached_identity,
                    keys: cached_keys,
                },
            ) => {
                identity == cached_identity
                    && cached_keys
                        .iter()
                        .map(String::as_str)
                        .eq(keys.iter().copied())
            }
            (
                Self::FilteredLibrary {
                    query,
                    favorite,
                    missing,
                    prefix,
                },
                OwnedScalarScope::FilteredLibrary {
                    query: cached_query,
                    favorite: cached_favorite,
                    missing: cached_missing,
                    prefix: cached_prefix,
                },
            ) => {
                *query == cached_query.as_deref()
                    && favorite == cached_favorite
                    && missing == cached_missing
                    && *prefix == cached_prefix.as_deref()
            }
            _ => false,
        }
    }

    fn into_owned(self) -> OwnedScalarScope {
        match self {
            Self::Keys { identity, keys } => OwnedScalarScope::Keys {
                identity,
                keys: keys.iter().map(|key| (*key).to_owned()).collect(),
            },
            Self::FilteredLibrary {
                query,
                favorite,
                missing,
                prefix,
            } => OwnedScalarScope::FilteredLibrary {
                query: query.map(str::to_owned),
                favorite,
                missing,
                prefix: prefix.map(str::to_owned),
            },
        }
    }
}

struct ScalarCount {
    domain: CountDomain,
    scope: OwnedScalarScope,
    total: usize,
}

impl Store {
    #[cfg(test)]
    pub(crate) fn scalar_count_reads(&self) -> usize {
        self.scalar_counts.borrow().reads
    }

    pub(crate) fn invalidate_scalar_counts(&self) {
        self.scalar_counts.borrow_mut().entries.clear();
    }

    pub(crate) fn invalidate_scalar_count_domains(&self, domains: &[CountDomain]) {
        self.scalar_counts
            .borrow_mut()
            .entries
            .retain(|entry| !domains.contains(&entry.domain));
    }

    pub(crate) fn invalidate_playlist_count_revisions(
        &self,
        previous: &std::collections::BTreeMap<i64, u64>,
        current: &std::collections::BTreeMap<i64, u64>,
        metadata: &std::sync::Arc<rivu_protocol::model::MetadataRevisions>,
        library: u64,
    ) {
        let mut cache = self.scalar_counts.borrow_mut();
        if std::ptr::eq(previous, current)
            && cache
                .playlist_metadata
                .as_ref()
                .is_some_and(|(old, old_library)| {
                    std::sync::Arc::ptr_eq(old, metadata) && *old_library == library
                })
        {
            return;
        }
        let old_metadata = cache
            .playlist_metadata
            .replace((std::sync::Arc::clone(metadata), library));
        cache.entries.retain(|entry| {
            if entry.domain != CountDomain::PlaylistEntries {
                return true;
            }
            let OwnedScalarScope::Keys { identity, .. } = &entry.scope else {
                return false;
            };
            previous
                .get(identity)
                .is_some_and(|revision| current.get(identity) == Some(revision))
                && old_metadata.as_ref().is_some_and(|(old, old_library)| {
                    old.playlist_revision(*identity, *old_library)
                        == metadata.playlist_revision(*identity, library)
                })
        });
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
        self.cached_scalar_count(domain, ScalarScope::Keys { identity, keys }, read)
    }

    pub(super) fn cached_filtered_library_count(
        &self,
        query: Option<&str>,
        favorite: Option<bool>,
        missing: Option<bool>,
        prefix: Option<&str>,
        read: impl FnOnce() -> Result<usize>,
    ) -> Result<usize> {
        self.cached_scalar_count(
            CountDomain::FilteredLibrary,
            ScalarScope::FilteredLibrary {
                query,
                favorite,
                missing,
                prefix,
            },
            read,
        )
    }

    fn cached_scalar_count(
        &self,
        domain: CountDomain,
        scope: ScalarScope<'_>,
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
                .prepare_cached("PRAGMA data_version")?
                .query_row([], |row| row.get::<_, i64>(0))?
        };
        {
            let mut cache = self.scalar_counts.borrow_mut();
            if cache.changes != changes || cache.external_revision != external_revision {
                cache.entries.clear();
                cache.changes = changes;
                cache.external_revision = external_revision;
            }
            if let Some(index) = cache
                .entries
                .iter()
                .position(|entry| entry.domain == domain && scope.matches(&entry.scope))
            {
                let entry = cache.entries.remove(index);
                let total = entry.total;
                cache.entries.push(entry);
                return Ok(total);
            }
        }
        #[cfg(test)]
        {
            self.scalar_counts.borrow_mut().reads += 1;
        }
        let total = read()?;
        let mut cache = self.scalar_counts.borrow_mut();
        // Retain only the latest filtered-library scope, within the shared cap.
        if domain == CountDomain::FilteredLibrary {
            cache.entries.retain(|entry| entry.domain != domain);
        }
        if cache.entries.len() == 16 {
            cache.entries.remove(0);
        }
        cache.entries.push(ScalarCount {
            domain,
            scope: scope.into_owned(),
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
