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
    pub(crate) fn set_track_cache_page_size(&self, page_size: u32) {
        let mut cache = self.track_cache.borrow_mut();
        cache.capacity = (page_size as usize).saturating_mul(2);
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
