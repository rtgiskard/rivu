use super::positions::{close_gaps, move_selection};
use super::rows::{LIBRARY_ROW_COLUMNS_QUALIFIED, library_row_at};
use super::tracks::{SOURCE_FILTER, search_filter, search_query};
use super::{Store, batch_limit};
use anyhow::{Context, Result, ensure};
use rivu_protocol::model::{
    Batch, BatchContinuation, MAX_QUERY_ROWS, Page, QueryStamp, QueueCursor, QueueRow, QueueState,
};
use rusqlite::{OptionalExtension, Transaction, params};
use std::path::PathBuf;

pub(super) fn integer<T: TryFrom<i64>>(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> rusqlite::Result<T> {
    let value = row.get::<_, i64>(index)?;
    T::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}

fn reorder_interval(tx: &Transaction<'_>, start: i64, ids: &[u64]) -> Result<bool> {
    if ids.is_empty() {
        return Ok(false);
    }
    let end = start
        .checked_add(i64::try_from(ids.len())?)
        .context("Queue range exceeds database positions")?;
    tx.execute_batch(
        "CREATE TEMP TABLE queue_order_update(id INTEGER PRIMARY KEY,position INTEGER NOT NULL);",
    )?;
    tx.execute("INSERT INTO queue_order_update(id,position) SELECT value,?2+CAST(key AS INTEGER) FROM json_each(?1)", params![serde_json::to_string(ids)?, start])?;
    let matched: usize = tx.query_row(
        "SELECT COUNT(*) FROM queue_order_update JOIN playlist_entries USING(id) WHERE playlist_entries.playlist_id=0 AND playlist_entries.position>=?1 AND playlist_entries.position<?2",
        params![start, end], |row| integer(row, 0),
    )?;
    ensure!(
        matched == ids.len(),
        "Queue entry not found in target range"
    );
    let changed = tx.execute("UPDATE playlist_entries SET position=(SELECT position FROM queue_order_update WHERE queue_order_update.id=playlist_entries.id) WHERE playlist_id=0 AND id IN (SELECT id FROM queue_order_update) AND position<>(SELECT position FROM queue_order_update WHERE queue_order_update.id=playlist_entries.id)", [])?;
    tx.execute_batch("DROP TABLE queue_order_update;")?;
    Ok(changed != 0)
}

fn queue_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<QueueRow> {
    Ok(QueueRow {
        id: integer(row, 0)?,
        track_id: row.get(1)?,
        position: integer(row, 2)?,
        offset: integer(row, 2)?,
        track: library_row_at(row, 3)?,
    })
}

impl Store {
    #[cfg(test)]
    pub fn queue_state(&self) -> Result<QueueState> {
        let total = self.conn.query_row(
            "SELECT COUNT(*) FROM playlist_entries WHERE playlist_id=0",
            [],
            |row| integer(row, 0),
        )?;
        self.queue_projection(total, 0)
    }

    pub(crate) fn queue_projection(&self, total: usize, revision: u64) -> Result<QueueState> {
        Ok(self.conn.query_row(
            "SELECT current_queue_entry_id,(SELECT position FROM playlist_entries WHERE playlist_id=0 AND id=current_queue_entry_id) FROM rivu_meta WHERE singleton=1",
            [], |row| Ok(QueueState {
                total,
                current_id: row.get::<_, Option<i64>>(0)?.map(|value| u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, value))).transpose()?,
                current_position: row.get::<_, Option<i64>>(1)?.map(|value| usize::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(1, value))).transpose()?,
                revision,
            }),
        )?)
    }

    pub(crate) fn queue_ids(&self) -> Result<Vec<u64>> {
        let mut statement = self
            .conn
            .prepare("SELECT id FROM playlist_entries WHERE playlist_id=0 ORDER BY position,id")?;
        Ok(statement
            .query_map([], |row| integer(row, 0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub(crate) fn extend_queue_ids(&self, ids: &mut Vec<u64>) -> Result<()> {
        let mut statement = self
            .conn
            .prepare("SELECT id FROM playlist_entries WHERE playlist_id=0 AND position>=?1 ORDER BY position,id")?;
        let mut rows = statement.query([ids.len() as i64])?;
        while let Some(row) = rows.next()? {
            ids.push(integer(row, 0)?);
        }
        Ok(())
    }

    pub(crate) fn queue_track_id(&self, id: u64) -> Result<i64> {
        let id = i64::try_from(id).context("Queue entry ID exceeds database range")?;
        self.conn
            .query_row(
                "SELECT track_id FROM playlist_entries WHERE playlist_id=0 AND id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?
            .context("Queue entry not found")
    }

    pub(crate) fn queued_track(&self, track_id: i64) -> Result<Option<u64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM playlist_entries WHERE playlist_id=0 AND track_id=?1 ORDER BY position,id LIMIT 1",
                [track_id],
                |row| integer(row, 0),
            )
            .optional()?)
    }

    pub(crate) fn set_queue_current(&self, current: Option<u64>) -> Result<()> {
        let current = current
            .map(i64::try_from)
            .transpose()
            .context("Queue entry ID exceeds database range")?;
        if let Some(id) = current {
            let exists: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM playlist_entries WHERE playlist_id=0 AND id=?1)",
                [id],
                |row| row.get(0),
            )?;
            ensure!(exists, "Queue entry not found");
        }
        let changed = self.conn.execute(
            "UPDATE rivu_meta SET current_queue_entry_id=?1 WHERE singleton=1 AND current_queue_entry_id IS NOT ?1",
            [current],
        )?;
        if changed == 0 {
            let saved: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM rivu_meta WHERE singleton=1 AND current_queue_entry_id IS ?1)",
                [current],
                |row| row.get(0),
            )?;
            ensure!(saved, "Queue current selection was not saved");
        }
        Ok(())
    }

    pub(crate) fn queue_page(
        &self,
        query: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<Page<QueueRow>> {
        let query = search_query(query);
        let filter = search_filter("t", "?1");
        let total = self.cached_count(super::CountDomain::Queue, &[query.as_deref().unwrap_or("")], || {
            if query.is_some() {
                Ok(self.conn.query_row(&format!("SELECT COUNT(*) FROM playlist_entries q JOIN tracks t ON t.id=q.track_id WHERE q.playlist_id=0 AND {filter}"), [query.as_deref()], |row| integer(row, 0))?)
            } else {
                Ok(self.conn.query_row("SELECT COALESCE(MAX(position)+1,0) FROM playlist_entries WHERE playlist_id=0", [], |row| integer(row, 0))?)
            }
        })?;
        if limit == 0 {
            return Ok(Page {
                total,
                rows: Vec::new(),
            });
        }
        let paging = if query.is_some() {
            "ORDER BY q.position,q.id LIMIT ?3 OFFSET ?2"
        } else {
            "AND q.position>=?2 ORDER BY q.position,q.id LIMIT ?3"
        };
        let mut statement = self.conn.prepare(&format!("SELECT q.id,q.track_id,q.position,{LIBRARY_ROW_COLUMNS_QUALIFIED} FROM playlist_entries q JOIN tracks t ON t.id=q.track_id WHERE q.playlist_id=0 AND {filter} {paging}"))?;
        let mut rows = statement
            .query_map(
                params![
                    query,
                    offset.min(i64::MAX as usize) as i64,
                    limit.min(MAX_QUERY_ROWS) as i64
                ],
                queue_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if query.is_some() {
            for (index, row) in rows.iter_mut().enumerate() {
                row.offset = offset + index;
            }
        }
        Ok(Page { total, rows })
    }

    pub fn queue_batch(
        &self,
        query: Option<&str>,
        after: Option<BatchContinuation<QueueCursor>>,
        limit: usize,
        revisions: QueryStamp,
    ) -> Result<Batch<QueueRow, BatchContinuation<QueueCursor>>> {
        let limit = batch_limit(limit)?;
        let query = search_query(query);
        let scope = revisions.continuation_scope(query.as_deref(), None);
        if after.is_some_and(|after| after.scope != scope) {
            return Err(rivu_protocol::response::QueryError::RevisionChanged.into());
        }
        let filter = search_filter("t", "?4");
        let mut statement = self.conn.prepare(&format!("SELECT q.id,q.track_id,q.position,{LIBRARY_ROW_COLUMNS_QUALIFIED} FROM playlist_entries q JOIN tracks t ON t.id=q.track_id WHERE q.playlist_id=0 AND (q.position,q.id)>(?1,?2) AND {filter} ORDER BY q.position,q.id LIMIT ?3"))?;
        let (position, id) = after.map_or((-1, 0), |cursor| {
            (
                cursor.cursor.position.min(i64::MAX as usize) as i64,
                cursor.cursor.id,
            )
        });
        let id = i64::try_from(id).context("Queue cursor ID exceeds database range")?;
        let start = after.map_or(0, |cursor| cursor.offset);
        let mut rows = statement
            .query_map(params![position, id, limit + 1, query], queue_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if query.is_some() {
            for (index, row) in rows.iter_mut().enumerate() {
                row.offset = start
                    .checked_add(index)
                    .context("Queue matching offset overflow")?;
            }
        }
        let next = if rows.len() > limit as usize {
            rows.pop();
            rows.last()
                .map(|row| {
                    Ok::<_, anyhow::Error>(BatchContinuation {
                        cursor: QueueCursor {
                            position: row.position,
                            id: row.id,
                        },
                        offset: start
                            .checked_add(rows.len())
                            .context("Queue matching offset overflow")?,
                        scope,
                    })
                })
                .transpose()?
        } else {
            None
        };
        Ok(Batch { rows, next })
    }

    pub fn queue_entries(&self, query: Option<&str>, ids: &[u64]) -> Result<Vec<QueueRow>> {
        ensure!(
            ids.len() <= MAX_QUERY_ROWS,
            "Queue entry query exceeds {MAX_QUERY_ROWS} IDs"
        );
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let query = search_query(query);
        // Apply identity restriction after ranking only when matches differ from
        // queue positions. Unfiltered reconciliation remains an indexed lookup.
        let sql = if query.is_some() {
            format!(
                "WITH matched AS (SELECT q.id AS queue_id,q.track_id AS queue_track_id,q.position AS queue_position,{LIBRARY_ROW_COLUMNS_QUALIFIED},row_number() OVER (ORDER BY q.position,q.id)-1 AS match_offset FROM playlist_entries q JOIN tracks t ON t.id=q.track_id WHERE q.playlist_id=0 AND {}) SELECT * FROM matched WHERE queue_id IN (SELECT value FROM json_each(?1)) ORDER BY queue_position,queue_id",
                search_filter("t", "?2")
            )
        } else {
            format!(
                "SELECT q.id,q.track_id,q.position,{LIBRARY_ROW_COLUMNS_QUALIFIED},q.position AS match_offset FROM playlist_entries q JOIN tracks t ON t.id=q.track_id WHERE q.playlist_id=0 AND q.id IN (SELECT value FROM json_each(?1)) AND ?2 IS NULL ORDER BY q.position,q.id"
            )
        };
        let mut statement = self.conn.prepare(&sql)?;
        Ok(statement
            .query_map(params![serde_json::to_string(ids)?, query], |row| {
                let mut result = queue_row(row)?;
                result.offset = row.get::<_, i64>("match_offset")? as usize;
                Ok(result)
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub(crate) fn enqueue_tracks(&self, ids: &[i64]) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let ids = self.validated_track_ids(ids)?;
        let changed = tx.execute("INSERT INTO playlist_entries(playlist_id,track_id,position) SELECT 0,value,(SELECT COALESCE(MAX(position)+1,0) FROM playlist_entries WHERE playlist_id=0)+CAST(key AS INTEGER) FROM json_each(?1) ORDER BY CAST(key AS INTEGER)", [ids])?;
        tx.commit()?;
        Ok(changed != 0)
    }

    pub(crate) fn enqueue_sources(
        &self,
        directories: &[PathBuf],
        ids: &[i64],
        other_roots: Option<&[PathBuf]>,
    ) -> Result<bool> {
        if directories.is_empty() && other_roots.is_none() {
            return self.enqueue_tracks(ids);
        }
        let tx = self.conn.unchecked_transaction()?;
        let (directories, ids) = self.source_parameters(directories, ids, other_roots)?;
        let changed = tx.execute(&format!("INSERT INTO playlist_entries(playlist_id,track_id,position) SELECT 0,tracks.id,(SELECT COALESCE(MAX(position)+1,0) FROM playlist_entries WHERE playlist_id=0)+row_number() OVER (ORDER BY tracks.id)-1 FROM tracks WHERE {SOURCE_FILTER} ORDER BY tracks.id"), params![directories, ids])?;
        tx.commit()?;
        Ok(changed != 0)
    }

    pub(crate) fn insert_queue_tracks(&self, ids: &[i64], index: usize) -> Result<Vec<u64>> {
        let ids = self.validated_track_ids(ids)?;
        self.insert_queue(&ids, None, index)
    }

    pub(crate) fn insert_queue_sources(
        &self,
        directories: &[PathBuf],
        ids: &[i64],
        other_roots: Option<&[PathBuf]>,
        index: usize,
    ) -> Result<Vec<u64>> {
        if directories.is_empty() && other_roots.is_none() {
            return self.insert_queue_tracks(ids, index);
        }
        let (directories, ids) = self.source_parameters(directories, ids, other_roots)?;
        self.insert_queue(&ids, Some(&directories), index)
    }

    fn insert_queue(&self, ids: &str, directories: Option<&str>, index: usize) -> Result<Vec<u64>> {
        let tx = self.conn.unchecked_transaction()?;
        let count = super::positions::insert_tracks(&tx, 0, index, ids, directories)?;
        let inserted = tx.prepare("SELECT id FROM playlist_entries WHERE playlist_id=0 AND position>=?1 AND position<?2 ORDER BY position,id")?
            .query_map(params![i64::try_from(index)?, i64::try_from(index.checked_add(count).context("Queue insertion overflow")?)?], |row| integer(row, 0))?
            .collect::<rusqlite::Result<Vec<u64>>>()?;
        tx.commit()?;
        Ok(inserted)
    }

    pub(crate) fn replace_queue_from_playlist(&self, playlist: i64) -> Result<()> {
        ensure!(playlist > 0, "Playlist not found");
        let tx = self.conn.unchecked_transaction()?;
        let nonempty: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM playlist_entries WHERE playlist_id=?1)",
            [playlist],
            |row| row.get(0),
        )?;
        ensure!(nonempty, "Playlist is empty or not found");
        tx.execute("DELETE FROM playlist_entries WHERE playlist_id=0", [])?;
        tx.execute("INSERT INTO playlist_entries(playlist_id,track_id,position) SELECT 0,track_id,row_number() OVER (ORDER BY position,id)-1 FROM playlist_entries WHERE playlist_id=?1 ORDER BY position,id", [playlist])?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn remove_queue_entries(&self, ids: &[u64]) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let ids = serde_json::to_string(ids)?;
        let missing: Option<i64> = tx.query_row("SELECT requested.value FROM json_each(?1) AS requested WHERE NOT EXISTS (SELECT 1 FROM playlist_entries AS e WHERE e.id=requested.value AND e.playlist_id=0) LIMIT 1", [&ids], |row| row.get(0)).optional()?;
        ensure!(
            missing.is_none(),
            "Queue entry not found: {}",
            missing.unwrap_or_default()
        );
        let positions = tx
            .prepare("SELECT position FROM playlist_entries WHERE playlist_id=0 AND id IN (SELECT value FROM json_each(?1)) ORDER BY position")?
            .query_map([&ids], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        tx.execute(
            "DELETE FROM playlist_entries WHERE playlist_id=0 AND id IN (SELECT value FROM json_each(?1))",
            [&ids],
        )?;
        close_gaps(&tx, &positions, 0)?;
        tx.commit()?;
        Ok(!positions.is_empty())
    }

    pub(crate) fn reorder_queue(&self, ids: &[u64]) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let total: usize = tx.query_row(
            "SELECT COALESCE(MAX(position)+1,0) FROM playlist_entries WHERE playlist_id=0",
            [],
            |row| integer(row, 0),
        )?;
        ensure!(
            total == ids.len(),
            "Queue order does not contain every entry"
        );
        let changed = reorder_interval(&tx, 0, ids)?;
        tx.commit()?;
        Ok(changed)
    }

    pub(crate) fn move_queue_entries(&self, selected: &[(i64, i64)], index: usize) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let insertion = i64::try_from(index).context("Queue target exceeds database range")?;
        ensure!(
            selected.windows(2).all(|pair| pair[0].1 < pair[1].1),
            "Queue selection is not in occurrence order"
        );
        let total: usize = tx.query_row(
            "SELECT COALESCE(MAX(position)+1,0) FROM playlist_entries WHERE playlist_id=0",
            [],
            |row| integer(row, 0),
        )?;
        ensure!(
            selected.len() <= total && index <= total - selected.len(),
            "Queue target out of range"
        );
        let matched: usize = tx.query_row(
            "SELECT COUNT(*) FROM json_each(?1) AS picked JOIN playlist_entries AS e ON e.id=json_extract(picked.value,'$[0]') AND e.position=json_extract(picked.value,'$[1]') WHERE e.playlist_id=0",
            [serde_json::to_string(selected)?], |row| integer(row, 0),
        )?;
        ensure!(
            matched == selected.len(),
            "Queue entry not found in selection"
        );
        let changed = move_selection(&tx, 0, selected, insertion)?;
        tx.commit()?;
        Ok(changed)
    }

    pub(crate) fn clear_queue(&self) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let changed = tx.execute("DELETE FROM playlist_entries WHERE playlist_id=0", [])?;
        tx.commit()?;
        Ok(changed != 0)
    }

    pub(crate) fn deduplicate_queue(&self) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        // Removed slots are the original non-first occurrences, even when the
        // playing occurrence is relocated to replace its track's first one.
        let positions = tx
            .prepare("WITH ranked AS (SELECT position,row_number() OVER (PARTITION BY track_id ORDER BY position,id) AS occurrence FROM playlist_entries WHERE playlist_id=0) SELECT position FROM ranked WHERE occurrence>1 ORDER BY position")?
            .query_map([], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if positions.is_empty() {
            return Ok(false);
        }
        // Keep the playing occurrence's identity, but move it to its track's first position.
        tx.execute_batch("UPDATE playlist_entries SET position=(SELECT MIN(other.position) FROM playlist_entries other WHERE other.playlist_id=0 AND other.track_id=playlist_entries.track_id) WHERE playlist_id=0 AND id=(SELECT current_queue_entry_id FROM rivu_meta) AND position<>(SELECT MIN(other.position) FROM playlist_entries other WHERE other.playlist_id=0 AND other.track_id=playlist_entries.track_id); WITH ranked AS (SELECT id,row_number() OVER (PARTITION BY track_id ORDER BY CASE WHEN id=(SELECT current_queue_entry_id FROM rivu_meta) THEN 0 ELSE 1 END,position,id) AS occurrence FROM playlist_entries WHERE playlist_id=0) DELETE FROM playlist_entries WHERE playlist_id=0 AND id IN (SELECT id FROM ranked WHERE occurrence>1);")?;
        close_gaps(&tx, &positions, 0)?;
        tx.commit()?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn fixture() -> Result<Store> {
        let store = Store::open(Path::new(":memory:"))?;
        store.conn.execute_batch(
            "INSERT INTO tracks(path) VALUES('/music/a'),('/music/b'),('/music/c'),('/other/d');",
        )?;
        Ok(store)
    }

    fn assert_compact(store: &Store) -> Result<()> {
        let rows = store.queue_page(None, 0, MAX_QUERY_ROWS)?.rows;
        assert!(
            rows.iter()
                .enumerate()
                .all(|(index, row)| row.position == index)
        );
        Ok(())
    }

    #[test]
    fn indexed_queue_insertion_preserves_order_duplicates_and_current_occurrence() -> Result<()> {
        for index in 0..=3 {
            let store = fixture()?;
            store.enqueue_tracks(&[1, 2, 2])?;
            let original = store.queue_ids()?;
            store.set_queue_current(Some(original[2]))?;
            let inserted = store.insert_queue_tracks(&[3, 1, 3], index)?;
            assert_eq!(inserted.len(), 3);
            let mut expected_ids = original.clone();
            expected_ids.splice(index..index, inserted);
            assert_eq!(store.queue_ids()?, expected_ids);
            let mut expected_tracks = vec![1, 2, 2];
            expected_tracks.splice(index..index, [3, 1, 3]);
            let rows = store.queue_page(None, 0, MAX_QUERY_ROWS)?.rows;
            assert_eq!(
                rows.iter().map(|row| row.track_id).collect::<Vec<_>>(),
                expected_tracks
            );
            assert!(
                rows.iter()
                    .enumerate()
                    .all(|(position, row)| row.position == position)
            );
            let after = store.queue_state()?;
            assert_eq!(after.current_id, Some(original[2]));
            assert_eq!(
                after.current_position,
                Some(2 + if index <= 2 { 3 } else { 0 })
            );
        }
        Ok(())
    }

    #[test]
    fn indexed_queue_sources_keep_union_order_and_reject_invalid_boundaries() -> Result<()> {
        let store = fixture()?;
        store.enqueue_tracks(&[4])?;
        store.insert_queue_sources(&["/music".into(), "/music".into()], &[3, 1, 3], None, 0)?;
        let state = store.queue_state()?;
        let order = store.queue_ids()?;
        assert_eq!(
            store
                .queue_page(None, 0, 10)?
                .rows
                .iter()
                .map(|row| row.track_id)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        for index in [state.total + 1, usize::MAX] {
            assert!(store.insert_queue_tracks(&[], index).is_err());
            assert!(store.insert_queue_sources(&[], &[], None, index).is_err());
        }
        assert!(store.insert_queue_tracks(&[1, 999], 1).is_err());
        assert!(
            store
                .insert_queue_sources(&["/music".into()], &[999], None, 1)
                .is_err()
        );
        assert!(store.insert_queue_tracks(&[], state.total)?.is_empty());
        assert!(
            store
                .insert_queue_sources(&["/absent".into()], &[], None, 0)?
                .is_empty()
        );
        assert_eq!(store.queue_state()?, state);
        assert_eq!(store.queue_ids()?, order);
        Ok(())
    }

    #[test]
    fn indexed_queue_insertions_roll_back_gap_entries_and_current() -> Result<()> {
        let store = fixture()?;
        store.enqueue_tracks(&[1, 2, 2])?;
        let order = store.queue_ids()?;
        store.set_queue_current(Some(order[2]))?;
        let before = store.queue_state()?;
        store.conn.execute_batch("CREATE TEMP TRIGGER reject_insertion BEFORE INSERT ON playlist_entries WHEN new.playlist_id=0 AND new.track_id=3 BEGIN SELECT RAISE(ABORT,'reject entry'); END;")?;
        assert!(store.insert_queue_tracks(&[3, 3], 1).is_err());
        assert!(
            store
                .insert_queue_sources(&["/music".into()], &[], None, 1)
                .is_err()
        );
        assert_eq!(store.queue_ids()?, order);
        assert_eq!(store.queue_state()?, before);
        store.conn.execute_batch("DROP TRIGGER reject_insertion;")?;
        assert_eq!(store.insert_queue_tracks(&[3], 1)?.len(), 1);
        Ok(())
    }

    #[test]
    fn structural_changes_return_effects_but_noops_do_not() -> Result<()> {
        let store = fixture()?;
        let before = store.queue_state()?;
        assert!(!store.enqueue_tracks(&[])?);
        assert!(!store.enqueue_sources(&[], &[], None)?);
        assert!(!store.enqueue_sources(&["/absent".into()], &[], None)?);
        assert!(!store.reorder_queue(&[])?);
        assert!(!store.move_queue_entries(&[], 0)?);
        assert!(!store.remove_queue_entries(&[])?);
        assert!(!store.clear_queue()?);
        assert!(!store.deduplicate_queue()?);
        assert_eq!(store.queue_state()?, before);

        assert!(store.enqueue_tracks(&[1, 2, 3])?);
        let ids = store.queue_ids()?;
        store.set_queue_current(Some(ids[1]))?;
        let before = store.queue_state()?;
        assert!(!store.move_queue_entries(&[(ids[1] as i64, 1)], 1)?);
        assert!(!store.reorder_queue(&ids)?);
        assert!(!store.move_queue_entries(&[(ids[1] as i64, 1), (ids[2] as i64, 2)], 1)?);
        assert!(!store.remove_queue_entries(&[])?);
        assert!(!store.deduplicate_queue()?);
        assert_eq!(store.queue_state()?, before);

        assert!(store.move_queue_entries(&[(ids[0] as i64, 0)], 2)?);
        assert_eq!(store.queue_ids()?, [ids[1], ids[2], ids[0]]);
        assert!(store.reorder_queue(&ids)?);
        assert!(store.move_queue_entries(&[(ids[2] as i64, 2)], 1)?);
        assert!(store.remove_queue_entries(&[ids[0]])?);
        assert!(store.enqueue_sources(&[], &[1], None)?);
        assert!(store.enqueue_tracks(&[2])?);
        assert!(store.deduplicate_queue()?);
        assert_compact(&store)?;
        assert_eq!(store.queue_state()?.current_id, Some(ids[1]));
        let playlist = store.create_playlist("Replacement")?;
        store.add_playlist(playlist, &[1, 3])?;
        store.replace_queue_from_playlist(playlist)?;
        assert_compact(&store)?;
        assert_eq!(store.queue_state()?.current_id, None);
        assert!(store.clear_queue()?);
        assert!(!store.clear_queue()?);
        Ok(())
    }

    #[test]
    fn queued_metadata_favorites_and_statistics_return_precise_effects() -> Result<()> {
        let store = fixture()?;
        store.enqueue_tracks(&[1, 1, 2])?;
        assert!(store.edit_track(1, Some("Title"), None, None, None)?.queue);
        assert!(store.set_favorite(&[1], true)?.queue);
        assert!(store.increment_play_count(1)?.queue);
        assert!(!store.mark_played(1, 123)?.queue);
        assert!(!store.edit_track(1, Some("Title"), None, None, None)?.queue);
        assert!(!store.set_favorite(&[1], true)?.queue);
        assert!(
            !store
                .edit_track(4, Some("Unqueued"), None, None, None)?
                .queue
        );
        assert!(!store.set_favorite(&[4], true)?.queue);
        assert!(!store.increment_play_count(4)?.queue);
        Ok(())
    }

    #[test]
    fn track_cascades_return_effects_compact_and_preserve_surviving_current() -> Result<()> {
        let store = fixture()?;
        store.enqueue_tracks(&[1, 2, 1, 3, 2])?;
        let ids = store.queue_ids()?;
        store.set_queue_current(Some(ids[3]))?;
        let before = store.queue_state()?;
        let removal = store.prepare_track_removal(&[4])?;
        assert!(!store.remove_prepared_tracks(removal)?.queue);
        assert!(!store.remove_missing_tracks()?.queue);
        assert_eq!(store.queue_state()?, before);
        let removal = store.prepare_track_removal(&[1])?;
        assert!(store.remove_prepared_tracks(removal)?.queue);
        assert_compact(&store)?;
        assert_eq!(store.queue_ids()?, [ids[1], ids[3], ids[4]]);
        assert_eq!(store.queue_state()?.current_id, Some(ids[3]));
        assert_eq!(store.queue_state()?.current_position, Some(1));
        store
            .conn
            .execute("UPDATE tracks SET missing=1 WHERE id=3", [])?;
        assert!(store.remove_missing_tracks()?.queue);
        assert_compact(&store)?;
        assert_eq!(store.queue_ids()?, [ids[1], ids[4]]);
        assert_eq!(store.queue_state()?.current_id, None);
        Ok(())
    }

    #[test]
    fn structural_failures_roll_back_order_and_current() -> Result<()> {
        let store = fixture()?;
        store.enqueue_tracks(&[1, 2, 1, 3])?;
        let ids = store.queue_ids()?;
        store.set_queue_current(Some(ids[2]))?;
        let before = store.queue_state()?;
        let rows = store.queue_page(None, 0, 10)?;
        assert!(
            store
                .reorder_queue(&[ids[0], ids[0], ids[2], ids[3]])
                .is_err()
        );
        assert!(store.remove_queue_entries(&[ids[0], 999]).is_err());
        assert!(store.enqueue_tracks(&[1, 999]).is_err());
        assert_eq!(store.queue_state()?, before);
        assert_eq!(store.queue_page(None, 0, 10)?, rows);
        let playlist = store.create_playlist("Replacement")?;
        store.add_playlist(playlist, &[2, 3])?;
        store.conn.execute_batch(
            "CREATE TEMP TRIGGER reject_queue_insert AFTER INSERT ON playlist_entries WHEN new.playlist_id=0 BEGIN SELECT RAISE(ABORT,'injected failure'); END;
             CREATE TEMP TRIGGER reject_queue_update AFTER UPDATE ON playlist_entries WHEN new.playlist_id=0 BEGIN SELECT RAISE(ABORT,'injected failure'); END;
             CREATE TEMP TRIGGER reject_queue_delete AFTER DELETE ON playlist_entries WHEN old.playlist_id=0 BEGIN SELECT RAISE(ABORT,'injected failure'); END;",
        )?;
        assert!(store.enqueue_tracks(&[1]).is_err());
        assert!(store.enqueue_sources(&[], &[1], None).is_err());
        assert!(store.move_queue_entries(&[(ids[0] as i64, 0)], 3).is_err());
        assert!(
            store
                .reorder_queue(&[ids[3], ids[2], ids[1], ids[0]])
                .is_err()
        );
        assert!(
            store
                .move_queue_entries(&[(ids[0] as i64, 0), (ids[2] as i64, 2)], 2)
                .is_err()
        );
        assert!(store.remove_queue_entries(&[ids[2]]).is_err());
        assert!(store.deduplicate_queue().is_err());
        assert!(store.clear_queue().is_err());
        assert!(store.replace_queue_from_playlist(playlist).is_err());
        assert!(store.remove_tracks(&[1]).is_err());
        assert_eq!(store.queue_state()?, before);
        assert_eq!(store.queue_page(None, 0, 10)?, rows);
        assert!(store.track(1)?.is_some());
        store
            .conn
            .execute("UPDATE tracks SET missing=1 WHERE id=1", [])?;
        let rows = store.queue_page(None, 0, 10)?;
        assert!(store.remove_missing_tracks().is_err());
        assert_eq!(store.queue_state()?, before);
        assert_eq!(store.queue_page(None, 0, 10)?, rows);
        assert!(store.track(1)?.is_some());
        Ok(())
    }
}
