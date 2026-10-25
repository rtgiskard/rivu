use super::rows::{LIBRARY_ROW_COLUMNS, TRACK_COLUMNS, library_row_from_row, track_from_row};
use super::tracks::{search_filter, search_query};
use super::{
    Store,
    queue::{integer, invalidate_structure},
};
use anyhow::{Context, Result, ensure};
use rivu_format::database::{DIRECTORY, ORDER, TITLE, eligible_title};
use rivu_protocol::model::{AlbumKey, AlbumSort, AlbumSummary, LibraryRow, MAX_QUERY_ROWS, Page};
use rusqlite::params;

// Exact parent and effective title match the leading member-index expressions;
// descendants and same-titled albums in other directories are separate ranges.
fn album_filter() -> String {
    format!(
        "{DIRECTORY}=?1 AND {TITLE}=?2 AND {}",
        eligible_title(TITLE)
    )
}

impl Store {
    /// Lock writers, validate durable tokens, and transfer in the same transaction.
    /// No GUI-sized ID snapshot or process-local version check is used.
    pub(crate) fn transfer_collection(
        &self,
        source: &rivu_protocol::model::CollectionSource,
        destination: rivu_protocol::model::CollectionDestination,
    ) -> Result<(usize, Vec<u64>)> {
        use super::positions::{PositionScope, insert_staged_tracks};
        use rivu_protocol::model::{CollectionDestination, CollectionSource};
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        match source {
            CollectionSource::Album { revision, .. } => {
                ensure!(
                    self.album_revision()? == *revision,
                    "Album collection changed"
                );
            }
            CollectionSource::Playlist {
                playlist_id,
                revision,
            } => {
                self.check_playlist_entry_revision(*playlist_id, *revision)?;
            }
        }
        if let CollectionDestination::Playlist {
            playlist_id,
            revision,
            ..
        } = destination
        {
            self.check_playlist_entry_revision(playlist_id, revision)?;
        }
        tx.execute_batch("CREATE TEMP TABLE insertion_tracks(ordinal INTEGER PRIMARY KEY,track_id INTEGER NOT NULL);")?;
        match source {
            CollectionSource::Album { key, .. } => {
                let directory = key
                    .directory
                    .to_str()
                    .context("Album directory is not UTF-8")?;
                tx.execute(&format!(
                    "INSERT INTO insertion_tracks SELECT row_number() OVER (ORDER BY {ORDER})-1,id FROM tracks INDEXED BY tracks_album_members WHERE {} ORDER BY {ORDER}",
                    album_filter(),
                ), params![directory, key.title])?;
                let count: i64 =
                    tx.query_row("SELECT COUNT(*) FROM insertion_tracks", [], |row| {
                        row.get(0)
                    })?;
                ensure!(count > 0, "Album is empty");
            }
            CollectionSource::Playlist { playlist_id, .. } => {
                let exists: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM playlists WHERE id=?1)",
                    [playlist_id],
                    |row| row.get(0),
                )?;
                ensure!(exists, "Playlist not found");
                tx.execute("INSERT INTO insertion_tracks SELECT row_number() OVER (ORDER BY position,id)-1,track_id FROM playlist_entries WHERE playlist_id=?1 ORDER BY position,id", [playlist_id])?;
            }
        }
        let missing: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM insertion_tracks s LEFT JOIN tracks t ON t.id=s.track_id WHERE t.id IS NULL)", [], |row| row.get(0))?;
        ensure!(!missing, "Collection contains a missing track identity");
        let (scope, index) = match destination {
            CollectionDestination::Queue { insertion } => {
                let index = if let Some(insertion) = insertion {
                    let revision: u64 = tx.query_row(
                        "SELECT revision FROM queue_meta WHERE singleton=1",
                        [],
                        |row| integer(row, 0),
                    )?;
                    ensure!(revision == insertion.revision, "Queue revision changed");
                    usize::try_from(insertion.index)?
                } else {
                    tx.query_row(
                        "SELECT COALESCE(MAX(position)+1,0) FROM queue_entries",
                        [],
                        |row| integer(row, 0),
                    )?
                };
                (PositionScope::Queue, index)
            }
            CollectionDestination::Playlist {
                playlist_id, index, ..
            } => {
                // Preserve the playlist's unique-track and first-occurrence policy.
                tx.execute("DELETE FROM insertion_tracks WHERE track_id IN (SELECT track_id FROM playlist_entries WHERE playlist_id=?1) OR ordinal NOT IN (SELECT MIN(ordinal) FROM insertion_tracks GROUP BY track_id)", [playlist_id])?;
                let total: usize = tx.query_row("SELECT (SELECT COALESCE(MAX(position)+1,0) FROM playlist_entries WHERE playlist_id=?1) FROM playlists WHERE id=?1", [playlist_id], |row| integer(row, 0))?;
                (
                    PositionScope::Playlist(playlist_id),
                    index.map(usize::try_from).transpose()?.unwrap_or(total),
                )
            }
        };
        let count = insert_staged_tracks(&tx, scope, index)?;
        if matches!(scope, PositionScope::Queue) {
            invalidate_structure(&tx, count != 0)?;
        }
        let ids = if matches!(
            destination,
            CollectionDestination::Queue { insertion: Some(_) }
        ) {
            let end = index
                .checked_add(count)
                .context("Queue insertion overflow")?;
            tx.prepare("SELECT id FROM queue_entries WHERE position>=?1 AND position<?2 ORDER BY position,id")?
                .query_map(params![i64::try_from(index)?, i64::try_from(end)?], |row| integer(row, 0))?
                .collect::<rusqlite::Result<Vec<u64>>>()?
        } else {
            Vec::new()
        };
        tx.commit()?;
        Ok((count, ids))
    }

    pub fn albums_page(
        &self,
        query: Option<&str>,
        sort: AlbumSort,
        offset: usize,
        limit: usize,
    ) -> Result<Page<AlbumSummary>> {
        let filter = "?1 IS NULL OR instr(lower(title),lower(COALESCE(?1,'')))>0 OR EXISTS(SELECT 1 FROM album_artists WHERE directory=albums.directory AND title=albums.title AND instr(lower(artist),lower(COALESCE(?1,'')))>0)";
        let total = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM albums WHERE {filter}"),
            [query],
            |row| row.get::<_, i64>(0),
        )? as usize;
        let order = match sort {
            AlbumSort::Title => "title,directory",
            AlbumSort::Artist => "COALESCE(artist,'Unknown artist'),title,directory",
        };
        let mut statement = self.conn.prepare(&format!("SELECT directory,title,track_count,artist FROM albums WHERE {filter} ORDER BY {order} LIMIT ?2 OFFSET ?3"))?;
        let mut representative = self.conn.prepare(&format!(
            "SELECT {TRACK_COLUMNS} FROM tracks INDEXED BY tracks_album_members WHERE {DIRECTORY}=?1 AND {TITLE}=?2 ORDER BY {ORDER} LIMIT 1"
        ))?;
        let mut rows = statement.query(params![
            query,
            limit.min(MAX_QUERY_ROWS) as i64,
            i64::try_from(offset)?
        ])?;
        let mut summaries = Vec::new();
        while let Some(row) = rows.next()? {
            let key = AlbumKey {
                directory: row.get::<_, String>(0)?.into(),
                title: row.get(1)?,
            };
            let directory = key
                .directory
                .to_str()
                .context("Album directory is not UTF-8")?;
            let representative =
                representative.query_row(params![directory, key.title], track_from_row)?;
            summaries.push(AlbumSummary {
                representative,
                key,
                track_count: row.get::<_, i64>(2)? as usize,
                artist: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
            });
        }
        Ok(Page {
            total,
            rows: summaries,
        })
    }

    pub fn album_tracks_page(
        &self,
        key: &AlbumKey,
        query: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<Page<LibraryRow>> {
        let directory = key
            .directory
            .to_str()
            .context("Album directory is not UTF-8")?;
        let query = search_query(query);
        let filter = format!("{} AND {}", album_filter(), search_filter("tracks", "?3"));
        let total = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM tracks INDEXED BY tracks_album_members WHERE {filter}"),
            params![directory, key.title, query],
            |row| row.get::<_, i64>(0),
        )? as usize;
        let mut statement = self.conn.prepare(&format!("SELECT {LIBRARY_ROW_COLUMNS} FROM tracks INDEXED BY tracks_album_members WHERE {filter} ORDER BY {ORDER} LIMIT ?4 OFFSET ?5"))?;
        let rows = statement
            .query_map(
                params![
                    directory,
                    key.title,
                    query,
                    limit.min(MAX_QUERY_ROWS) as i64,
                    i64::try_from(offset)?
                ],
                library_row_from_row,
            )?
            .collect::<rusqlite::Result<_>>()?;
        Ok(Page { total, rows })
    }

    pub(crate) fn queue_album(&self, key: &AlbumKey, replace: bool) -> Result<()> {
        let directory = key
            .directory
            .to_str()
            .context("Album directory is not UTF-8")?;
        let tx = self.conn.unchecked_transaction()?;
        let filter = album_filter();
        let count: i64 = tx.query_row(
            &format!("SELECT COUNT(*) FROM tracks INDEXED BY tracks_album_members WHERE {filter}"),
            params![directory, key.title],
            |row| row.get(0),
        )?;
        ensure!(count > 0, "Album is empty");
        if replace {
            tx.execute(
                "UPDATE queue_meta SET current_id=NULL WHERE singleton=1",
                [],
            )?;
            tx.execute("DELETE FROM queue_entries", [])?;
        }
        tx.execute(&format!("INSERT INTO queue_entries(track_id,position) SELECT id,(SELECT COALESCE(MAX(position)+1,0) FROM queue_entries)+row_number() OVER (ORDER BY {ORDER})-1 FROM tracks INDEXED BY tracks_album_members WHERE {filter} ORDER BY {ORDER}"), params![directory, key.title])?;
        invalidate_structure(&tx, true)?;
        tx.commit()?;
        Ok(())
    }
}
