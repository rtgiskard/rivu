//! Persistence over one SQLite connection and its bounded track cache.
//! Operation modules share this owner; transaction boundaries stay within operations.
//! Queue occurrences live in the system playlist (ID 0); public playlist operations
//! expose only positive IDs, while shared position helpers accept either collection.

mod admission;
mod albums;
mod cache;
mod catalog;
mod collections;
mod duplicates;
mod playlists;
mod positions;
mod queue;
mod rows;
mod scan;
mod schema;
mod tracks;

use anyhow::{Result, bail};
use rusqlite::Connection;
use std::cell::RefCell;

pub(crate) use cache::CountDomain;
pub(crate) use catalog::CatalogChanges;
pub(crate) use duplicates::PreparedRelocation;
#[cfg(test)]
pub(crate) use duplicates::RelocationIdentity;
#[cfg(any(test, feature = "test-support"))]
pub(crate) use scan::TrackIdentity;
pub(crate) use tracks::{PlaybackSource, TrackRemovalImpact};

pub(crate) struct Store {
    conn: Connection,
    // Read-only owners invalidate each count on its published data-domain revision.
    revision_owned_counts: bool,
    track_cache: RefCell<cache::TrackCache>,
    duplicate_group_count_cache: RefCell<Option<duplicates::DuplicateGroupCount>>,
    scalar_counts: RefCell<cache::ScalarCounts>,
}

/// A borrowed SQLite snapshot. Query workers explicitly finish it before
/// releasing their maintenance lease; Drop also closes early-return paths.
pub(crate) struct ReadSnapshot<'a> {
    store: &'a Store,
    active: bool,
}

impl std::ops::Deref for ReadSnapshot<'_> {
    type Target = Store;
    fn deref(&self) -> &Store {
        self.store
    }
}

impl ReadSnapshot<'_> {
    /// A failed rollback requires retiring the connection before its read lease.
    pub(crate) fn finish(mut self) -> Result<()> {
        self.active = false;
        self.store.conn.execute_batch("ROLLBACK")?;
        Ok(())
    }
}

impl Drop for ReadSnapshot<'_> {
    fn drop(&mut self) {
        if self.active
            && let Err(error) = self.store.conn.execute_batch("ROLLBACK")
        {
            tracing::error!(%error, "read_snapshot_rollback_failed");
        }
    }
}

impl Store {
    fn configure_connection(conn: &Connection) -> Result<()> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // Query shapes are finite and parameter-bound; retain a bounded working
        // set across requests, never one statement per filter/path value.
        conn.set_prepared_statement_cache_capacity(64);
        // SQLite's built-in lower() is ASCII-only; preserve frontend Unicode search.
        conn.create_scalar_function(
            "lower",
            1,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8
                | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
            |ctx| {
                let value = ctx
                    .get_raw(0)
                    .as_str()
                    .map_err(|error| rusqlite::Error::UserFunctionError(error.into()))?;
                Ok(value.to_lowercase())
            },
        )?;
        Ok(())
    }

    pub(crate) fn playlist_ids(&self) -> Result<Vec<i64>> {
        Ok(self
            .conn
            .prepare_cached("SELECT id FROM playlists WHERE id>0 ORDER BY position,id")?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Validate the playlist once and pin its read snapshot before any export I/O.
    pub(crate) fn open_export(path: &std::path::Path, playlist_id: i64) -> Result<Self> {
        let store = Self::open_read_snapshot(path)?;
        let exists = store.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM playlists WHERE id=?1 AND id>0)",
            [playlist_id],
            |row| row.get::<_, bool>(0),
        )?;
        anyhow::ensure!(exists, "Playlist not found");
        Ok(store)
    }

    /// Pin an isolated read transaction; no schema writes or shared cache ownership.
    pub(crate) fn open_read_snapshot(path: &std::path::Path) -> Result<Self> {
        let store = Self::open_reader(path)?;
        store.pin_read_snapshot()?;
        Ok(store)
    }

    /// Reuse the connection, but never carry a read transaction between requests.
    /// The owner must invalidate each count when its published data domain changes.
    pub(crate) fn open_reader(path: &std::path::Path) -> Result<Self> {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Self::configure_connection(&conn)?;
        Ok(Self {
            conn,
            revision_owned_counts: true,
            track_cache: RefCell::new(cache::TrackCache {
                entries: std::collections::HashMap::new(),
                capacity: 0,
                clock: 0,
            }),
            duplicate_group_count_cache: RefCell::new(None),
            scalar_counts: RefCell::new(cache::ScalarCounts::default()),
        })
    }

    /// Interrupt read-only SQL cooperatively; always detach before the borrowed
    /// callback leaves its stack frame, including unwinding from `read`.
    pub(crate) fn with_read_cancellation<T>(
        &self,
        cancelled: impl Fn() -> bool,
        read: impl FnOnce() -> T,
    ) -> T {
        assert!(
            self.revision_owned_counts,
            "SQL cancellation requires a read-only store"
        );
        unsafe extern "C" fn check<F: Fn() -> bool>(context: *mut std::ffi::c_void) -> i32 {
            // SAFETY: the registration borrows F for the synchronous read below;
            // SQLite invokes this callback only on the executing connection thread.
            let cancelled = unsafe { &*context.cast::<F>() };
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(cancelled)) {
                Ok(cancelled) => i32::from(cancelled),
                Err(payload) => {
                    // Even a panic payload with a panicking destructor must not
                    // unwind through SQLite's C callback boundary.
                    std::mem::forget(payload);
                    1
                }
            }
        }
        struct Registration<'a>(&'a Connection);
        impl Drop for Registration<'_> {
            fn drop(&mut self) {
                // SAFETY: this guard retains the connection and removes the
                // borrowed callback before its captured state is destroyed.
                unsafe {
                    rusqlite::ffi::sqlite3_progress_handler(
                        self.0.handle(),
                        0,
                        None,
                        std::ptr::null_mut(),
                    )
                };
            }
        }
        fn register<F: Fn() -> bool>(connection: &Connection, cancelled: &F) {
            // SAFETY: caller retains both arguments until Registration drops.
            unsafe {
                rusqlite::ffi::sqlite3_progress_handler(
                    connection.handle(),
                    1024,
                    Some(check::<F>),
                    std::ptr::from_ref(cancelled).cast_mut().cast(),
                );
            }
        }
        register(&self.conn, &cancelled);
        let _registration = Registration(&self.conn);
        read()
    }

    /// On error, retire this connection: transaction rollback may also have failed.
    pub(crate) fn read_snapshot(&self) -> Result<ReadSnapshot<'_>> {
        self.pin_read_snapshot()?;
        Ok(ReadSnapshot {
            store: self,
            active: true,
        })
    }

    fn pin_read_snapshot(&self) -> Result<()> {
        self.conn.execute_batch("BEGIN DEFERRED")?;
        // BEGIN is lazy: the first actual read establishes the WAL snapshot.
        if let Err(error) = self
            .conn
            .prepare_cached("SELECT COUNT(*) FROM sqlite_schema")
            .and_then(|mut statement| statement.query_row([], |row| row.get::<_, i64>(0)))
        {
            self.conn.execute_batch("ROLLBACK")?;
            return Err(error.into());
        }
        Ok(())
    }

    /// Background jobs open their own connection; never share this connection or caches.
    pub(crate) fn database_path(&self) -> Result<std::path::PathBuf> {
        self.conn
            .path()
            .filter(|path| !path.is_empty() && *path != ":memory:")
            .map(std::path::PathBuf::from)
            .ok_or_else(|| {
                anyhow::anyhow!("Background database operations require a file-backed library")
            })
    }

    pub(crate) fn invalidate_duplicate_group_count(&self) {
        *self.duplicate_group_count_cache.borrow_mut() = None;
    }

    pub(crate) fn invalidate_catalog_caches(
        &self,
        impacts: CatalogChanges,
        playlist_created: bool,
    ) {
        let dependencies = impacts.dependencies(playlist_created);
        if dependencies.clear_track_cache {
            self.track_cache.borrow_mut().entries.clear();
        }
        if dependencies.clear_catalog_counts {
            self.invalidate_scalar_count_domains(&[
                CountDomain::FilteredLibrary,
                CountDomain::Other,
                CountDomain::Directory,
                CountDomain::Albums,
                CountDomain::AlbumTracks,
            ]);
        }
        if dependencies.clear_queue_count {
            self.invalidate_scalar_count_domains(&[CountDomain::Queue]);
        }
        if dependencies.clear_playlist_count && !self.revision_owned_counts {
            self.invalidate_scalar_count_domains(&[CountDomain::PlaylistEntries]);
        }
        if dependencies.clear_duplicate_group_count {
            self.invalidate_duplicate_group_count();
        }
    }

    /// A background commit invalidates both cached tracks and cached counts.
    pub(crate) fn invalidate_caches(&self) {
        self.track_cache.borrow_mut().entries.clear();
        self.invalidate_scalar_counts();
        self.invalidate_duplicate_group_count();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TrackStats {
    pub play_count: u64,
    pub last_played: Option<i64>,
    pub changed: bool,
    pub queue: bool,
}

impl TrackStats {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            play_count: row.get::<_, i64>(0)? as u64,
            last_played: row.get(1)?,
            changed: false,
            queue: false,
        })
    }
}

fn batch_limit(limit: usize) -> Result<i64> {
    if limit == 0 {
        bail!("batch limit must be at least 1");
    }
    if limit > rivu_protocol::model::MAX_QUERY_ROWS {
        bail!(
            "batch limit must be at most {}; got {}",
            rivu_protocol::model::MAX_QUERY_ROWS,
            limit
        );
    }
    Ok(limit as i64)
}

fn changed(n: usize, k: &str, id: i64) -> Result<()> {
    if n == 0 {
        bail!("{k} {id} does not exist")
    }
    Ok(())
}

#[cfg(test)]
mod tests;
