use super::positions::{PositionScope, close_gaps};
use super::rows::{LIBRARY_ROW_COLUMNS_QUALIFIED, library_row_at};
use super::tracks::SOURCE_FILTER;
use super::{Store, batch_limit};
use crate::model::{Batch, MAX_QUERY_ROWS, Page, QueueCursor, QueueRow, QueueState};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};
use std::path::PathBuf;

fn integer<T: TryFrom<i64>>(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<T> {
    let value = row.get::<_, i64>(index)?;
    T::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}

fn queue_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<QueueRow> {
    Ok(QueueRow {
        id: integer(row, 0)?,
        track_id: row.get(1)?,
        position: integer(row, 2)?,
        track: library_row_at(row, 3)?,
    })
}

impl Store {
    pub fn queue_state(&self) -> Result<QueueState> {
        let total = self
            .conn
            .query_row("SELECT COUNT(*) FROM queue_entries", [], |row| {
                integer(row, 0)
            })?;
        self.queue_projection(total)
    }

    pub(crate) fn queue_projection(&self, total: usize) -> Result<QueueState> {
        Ok(self.conn.query_row(
            "SELECT current_id,(SELECT position FROM queue_entries WHERE id=current_id),revision FROM queue_meta WHERE singleton=1",
            [], |row| Ok(QueueState {
                total,
                current_id: row.get::<_, Option<i64>>(0)?.map(|value| u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, value))).transpose()?,
                current_position: row.get::<_, Option<i64>>(1)?.map(|value| usize::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(1, value))).transpose()?,
                revision: integer(row, 2)?,
            }),
        )?)
    }

    pub(crate) fn queue_ids(&self) -> Result<Vec<u64>> {
        let mut statement = self
            .conn
            .prepare("SELECT id FROM queue_entries ORDER BY position,id")?;
        Ok(statement
            .query_map([], |row| integer(row, 0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub(crate) fn extend_queue_ids(&self, ids: &mut Vec<u64>) -> Result<()> {
        let mut statement = self
            .conn
            .prepare("SELECT id FROM queue_entries WHERE position>=?1 ORDER BY position,id")?;
        let mut rows = statement.query([ids.len() as i64])?;
        while let Some(row) = rows.next()? {
            ids.push(integer(row, 0)?);
        }
        Ok(())
    }

    pub(crate) fn queue_track_id(&self, id: u64) -> Result<i64> {
        let id = i64::try_from(id).context("Queue entry ID exceeds database range")?;
        Ok(self
            .conn
            .query_row(
                "SELECT track_id FROM queue_entries WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?
            .context("Queue entry not found")?)
    }

    pub(crate) fn queued_track(&self, track_id: i64) -> Result<Option<u64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM queue_entries WHERE track_id=?1 ORDER BY position,id LIMIT 1",
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
        self.conn.execute(
            "UPDATE queue_meta SET current_id=?1 WHERE singleton=1 AND current_id IS NOT ?1",
            [current],
        )?;
        Ok(())
    }

    pub fn queue_page(&self, offset: usize, limit: usize) -> Result<Page<QueueRow>> {
        let total = self.queue_state()?.total;
        let mut statement = self.conn.prepare(&format!("SELECT q.id,q.track_id,q.position,{LIBRARY_ROW_COLUMNS_QUALIFIED} FROM queue_entries q JOIN tracks t ON t.id=q.track_id WHERE q.position>=?1 ORDER BY q.position,q.id LIMIT ?2"))?;
        let rows = statement
            .query_map(
                params![
                    offset.min(i64::MAX as usize) as i64,
                    limit.min(MAX_QUERY_ROWS) as i64
                ],
                queue_row,
            )?
            .collect::<rusqlite::Result<_>>()?;
        Ok(Page { total, rows })
    }

    pub fn queue_batch(
        &self,
        after: Option<QueueCursor>,
        limit: usize,
    ) -> Result<Batch<QueueRow, QueueCursor>> {
        let limit = batch_limit(limit)?;
        let mut statement = self.conn.prepare(&format!("SELECT q.id,q.track_id,q.position,{LIBRARY_ROW_COLUMNS_QUALIFIED} FROM queue_entries q JOIN tracks t ON t.id=q.track_id WHERE (q.position,q.id)>(?1,?2) ORDER BY q.position,q.id LIMIT ?3"))?;
        let (position, id) = after.map_or((-1, 0), |cursor| {
            (cursor.position.min(i64::MAX as usize) as i64, cursor.id)
        });
        let id = i64::try_from(id).context("Queue cursor ID exceeds database range")?;
        let mut rows = statement
            .query_map(params![position, id, limit + 1], queue_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let next = if rows.len() > limit as usize {
            rows.pop();
            rows.last().map(|row| QueueCursor {
                position: row.position,
                id: row.id,
            })
        } else {
            None
        };
        Ok(Batch { rows, next })
    }

    pub fn queue_entries(&self, ids: &[u64]) -> Result<Vec<QueueRow>> {
        ensure!(
            ids.len() <= MAX_QUERY_ROWS,
            "Queue entry query exceeds {MAX_QUERY_ROWS} IDs"
        );
        let mut statement = self.conn.prepare(&format!("SELECT q.id,q.track_id,q.position,{LIBRARY_ROW_COLUMNS_QUALIFIED} FROM queue_entries q JOIN tracks t ON t.id=q.track_id WHERE q.id IN (SELECT value FROM json_each(?1)) ORDER BY q.position,q.id"))?;
        Ok(statement
            .query_map([serde_json::to_string(ids)?], queue_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub(crate) fn enqueue_tracks(&self, ids: &[i64]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        self.validate_tracks(ids)?;
        tx.execute("INSERT INTO queue_entries(track_id,position) SELECT value,(SELECT COALESCE(MAX(position)+1,0) FROM queue_entries)+CAST(key AS INTEGER) FROM json_each(?1) ORDER BY CAST(key AS INTEGER)", [serde_json::to_string(ids)?])?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn enqueue_sources(&self, directories: &[PathBuf], ids: &[i64]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let (directories, ids) = self.source_parameters(directories, ids)?;
        tx.execute(&format!("INSERT INTO queue_entries(track_id,position) SELECT tracks.id,(SELECT COALESCE(MAX(position)+1,0) FROM queue_entries)+row_number() OVER (ORDER BY tracks.id)-1 FROM tracks WHERE {SOURCE_FILTER} ORDER BY tracks.id"), params![directories, ids])?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn replace_queue_from_playlist(&self, playlist: i64) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM playlist_entries WHERE playlist_id=?1",
            [playlist],
            |row| row.get(0),
        )?;
        ensure!(count > 0, "Playlist is empty or not found");
        tx.execute("DELETE FROM queue_entries", [])?;
        tx.execute("INSERT INTO queue_entries(track_id,position) SELECT track_id,row_number() OVER (ORDER BY position,id)-1 FROM playlist_entries WHERE playlist_id=?1 ORDER BY position,id", [playlist])?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn remove_queue_entries(&self, ids: &[u64]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let ids = serde_json::to_string(ids)?;
        let missing: Option<i64> = tx.query_row("SELECT value FROM json_each(?1) WHERE value NOT IN (SELECT id FROM queue_entries) LIMIT 1", [&ids], |row| row.get(0)).optional()?;
        ensure!(
            missing.is_none(),
            "Queue entry not found: {}",
            missing.unwrap_or_default()
        );
        let positions = tx
            .prepare("SELECT position FROM queue_entries WHERE id IN (SELECT value FROM json_each(?1)) ORDER BY position")?
            .query_map([&ids], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        tx.execute(
            "DELETE FROM queue_entries WHERE id IN (SELECT value FROM json_each(?1))",
            [&ids],
        )?;
        close_gaps(&tx, &positions, PositionScope::Queue)?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn reorder_queue(&self, ids: &[u64]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let total: usize = tx.query_row("SELECT COUNT(*) FROM queue_entries", [], |row| {
            integer(row, 0)
        })?;
        ensure!(
            total == ids.len(),
            "Queue order does not contain every entry"
        );
        tx.execute_batch("CREATE TEMP TABLE queue_order_update(id INTEGER PRIMARY KEY,position INTEGER NOT NULL);")?;
        tx.execute("INSERT INTO queue_order_update(id,position) SELECT value,CAST(key AS INTEGER) FROM json_each(?1)", [serde_json::to_string(ids)?])?;
        let matched: usize = tx.query_row(
            "SELECT COUNT(*) FROM queue_order_update JOIN queue_entries USING(id)",
            [],
            |row| integer(row, 0),
        )?;
        ensure!(matched == total, "Queue entry not found");
        tx.execute_batch("UPDATE queue_entries SET position=(SELECT position FROM queue_order_update WHERE queue_order_update.id=queue_entries.id); DROP TABLE queue_order_update;")?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn clear_queue(&self) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM queue_entries", [])?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn deduplicate_queue(&self) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        // Removed slots are the original non-first occurrences, even when the
        // playing occurrence is relocated to replace its track's first one.
        let positions = tx
            .prepare("WITH ranked AS (SELECT position,row_number() OVER (PARTITION BY track_id ORDER BY position,id) AS occurrence FROM queue_entries) SELECT position FROM ranked WHERE occurrence>1 ORDER BY position")?
            .query_map([], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        // Keep the playing occurrence's identity, but move it to its track's first position.
        tx.execute_batch("UPDATE queue_entries SET position=(SELECT MIN(other.position) FROM queue_entries other WHERE other.track_id=queue_entries.track_id) WHERE id=(SELECT current_id FROM queue_meta); WITH ranked AS (SELECT id,row_number() OVER (PARTITION BY track_id ORDER BY CASE WHEN id=(SELECT current_id FROM queue_meta) THEN 0 ELSE 1 END,position,id) AS occurrence FROM queue_entries) DELETE FROM queue_entries WHERE id IN (SELECT id FROM ranked WHERE occurrence>1);")?;
        close_gaps(&tx, &positions, PositionScope::Queue)?;
        tx.commit()?;
        Ok(())
    }
}
