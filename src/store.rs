//! Persistence over one SQLite connection and its bounded track cache.
//! Operation modules share this owner; transaction boundaries stay within operations.

mod admission;
mod albums;
mod cache;
mod duplicates;
#[cfg(test)]
mod other_tracks_tests;
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

pub(crate) use duplicates::PreparedRelocation;
#[cfg(test)]
pub(crate) use duplicates::RelocationIdentity;
pub(crate) use scan::ScanImpacts;
pub(crate) use tracks::PlaybackSource;

pub(crate) struct Store {
    conn: Connection,
    track_cache: RefCell<cache::TrackCache>,
    count_cache: RefCell<Option<tracks::TrackCount>>,
}

impl Store {
    /// Validate the playlist once and pin its read snapshot before any export I/O.
    pub(crate) fn open_export(path: &std::path::Path, playlist_id: i64) -> Result<Self> {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("BEGIN DEFERRED")?;
        let exists = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM playlists WHERE id=?1)",
            [playlist_id],
            |row| row.get::<_, bool>(0),
        )?;
        anyhow::ensure!(exists, "Playlist not found");
        Ok(Self {
            conn,
            track_cache: RefCell::new(cache::TrackCache {
                entries: std::collections::HashMap::new(),
                capacity: 0,
                clock: 0,
            }),
            count_cache: RefCell::new(None),
        })
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

    /// A background commit invalidates both cached tracks and cached counts.
    pub(crate) fn invalidate_caches(&self) {
        self.track_cache.borrow_mut().entries.clear();
        *self.count_cache.borrow_mut() = None;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TrackStats {
    pub play_count: u64,
    pub last_played: Option<i64>,
}

impl TrackStats {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            play_count: row.get::<_, i64>(0)? as u64,
            last_played: row.get(1)?,
        })
    }
}

fn batch_limit(limit: usize) -> Result<i64> {
    if limit == 0 {
        bail!("batch limit must be at least 1");
    }
    if limit > crate::model::MAX_QUERY_ROWS {
        bail!(
            "batch limit must be at most {}; got {}",
            crate::model::MAX_QUERY_ROWS,
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
