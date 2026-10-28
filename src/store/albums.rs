use super::rows::{LIBRARY_ROW_COLUMNS, TRACK_COLUMNS, library_row_from_row, track_from_row};
use super::tracks::{search_filter, search_query};
use super::{Store, queue::integer};
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

pub(super) fn eligible_member(alias: &str) -> String {
    eligible_title(&format!(
        "COALESCE({alias}.album_override,{alias}.raw_album)"
    ))
}

pub(super) fn representative(alias: &str) -> String {
    let directory = DIRECTORY.replace("path", &format!("{alias}.path"));
    format!(
        "({eligible} AND {alias}.id=(SELECT id FROM tracks INDEXED BY tracks_album_members WHERE {DIRECTORY}={directory} AND {TITLE}=COALESCE({alias}.album_override,{alias}.raw_album) ORDER BY {ORDER} LIMIT 1))",
        eligible = eligible_member(alias)
    )
}

impl Store {
    /// Transfer the collection atomically after Core validates its runtime tokens.
    pub(crate) fn transfer_collection(
        &self,
        source: &rivu_protocol::model::CollectionSource,
        destination: rivu_protocol::model::CollectionDestination,
    ) -> Result<usize> {
        use super::positions::insert_staged_tracks;
        use rivu_protocol::model::{CollectionDestination, CollectionSource};
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
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
                    "SELECT EXISTS(SELECT 1 FROM playlists WHERE id=?1 AND id>0)",
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
                    usize::try_from(insertion.index)?
                } else {
                    tx.query_row(
                        "SELECT COALESCE(MAX(position)+1,0) FROM playlist_entries WHERE playlist_id=0",
                        [],
                        |row| integer(row, 0),
                    )?
                };
                (0, index)
            }
            CollectionDestination::Playlist {
                playlist_id, index, ..
            } => {
                ensure!(playlist_id > 0, "Playlist not found");
                let total: usize = tx.query_row("SELECT (SELECT COALESCE(MAX(position)+1,0) FROM playlist_entries WHERE playlist_id=?1) FROM playlists WHERE id=?1", [playlist_id], |row| integer(row, 0))?;
                (
                    playlist_id,
                    index.map(usize::try_from).transpose()?.unwrap_or(total),
                )
            }
        };
        let count = insert_staged_tracks(&tx, scope, index)?;
        tx.commit()?;
        Ok(count)
    }

    pub fn albums_page(
        &self,
        query: Option<&str>,
        sort: AlbumSort,
        offset: usize,
        limit: usize,
    ) -> Result<Page<AlbumSummary>> {
        let filter = "?1 IS NULL OR instr(lower(title),lower(COALESCE(?1,'')))>0 OR EXISTS(SELECT 1 FROM album_artists WHERE directory=albums.directory AND title=albums.title AND instr(lower(artist),lower(COALESCE(?1,'')))>0)";
        let total =
            self.cached_count(super::CountDomain::Albums, &[query.unwrap_or("")], || {
                Ok(self.conn.query_row(
                    &format!("SELECT COUNT(*) FROM albums WHERE {filter}"),
                    [query],
                    |row| row.get::<_, i64>(0),
                )? as usize)
            })?;
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
        let total = self.cached_count(
            super::CountDomain::AlbumTracks,
            &[directory, &key.title, query.as_deref().unwrap_or("")],
            || {
                Ok(self.conn.query_row(
                    &format!(
                        "SELECT COUNT(*) FROM tracks INDEXED BY tracks_album_members WHERE {filter}"
                    ),
                    params![directory, key.title, query],
                    |row| row.get::<_, i64>(0),
                )? as usize)
            },
        )?;
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
                "UPDATE rivu_meta SET current_queue_entry_id=NULL WHERE singleton=1",
                [],
            )?;
            tx.execute("DELETE FROM playlist_entries WHERE playlist_id=0", [])?;
        }
        tx.execute(&format!("INSERT INTO playlist_entries(playlist_id,track_id,position) SELECT 0,id,(SELECT COALESCE(MAX(position)+1,0) FROM playlist_entries WHERE playlist_id=0)+row_number() OVER (ORDER BY {ORDER})-1 FROM tracks INDEXED BY tracks_album_members WHERE {filter} ORDER BY {ORDER}"), params![directory, key.title])?;
        tx.commit()?;
        Ok(())
    }
}
