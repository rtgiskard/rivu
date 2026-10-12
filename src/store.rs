//! Persistence over one SQLite connection and its bounded track cache.
//! Operation modules share this owner; transaction boundaries stay within operations.

mod cache;
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

pub struct Store {
    conn: Connection,
    track_cache: RefCell<cache::TrackCache>,
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
