use super::Store;
use super::rows::{LIBRARY_ROW_COLUMNS, fingerprint_from_row, library_row_from_row};
use super::tracks::{search_filter, search_query};
use anyhow::{Context, Result, ensure};
use rivu_format::database::{DIRECTORY, ORDER, TITLE, eligible_title};
use rivu_protocol::model::{
    AlbumKey, AlbumSort, AlbumSummary, ArtworkSource, LibraryRow, MAX_QUERY_ROWS, Page,
};
use rusqlite::params;

// Exact parent and effective title match the leading member-index expressions;
// descendants and same-titled albums in other directories are separate ranges.
pub(super) fn album_filter() -> String {
    format!(
        "{DIRECTORY}=?1 AND {TITLE}=?2 AND {}",
        eligible_title(TITLE)
    )
}

pub(super) fn eligible_member(alias: &str) -> String {
    eligible_title(&format!("{alias}.album"))
}

pub(super) fn representative(alias: &str) -> String {
    let directory = DIRECTORY.replace("path", &format!("{alias}.path"));
    format!(
        "({eligible} AND {alias}.id=(SELECT id FROM tracks INDEXED BY tracks_album_members WHERE {DIRECTORY}={directory} AND {TITLE}={alias}.album ORDER BY {ORDER} LIMIT 1))",
        eligible = eligible_member(alias)
    )
}

impl Store {
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
                Ok(self
                    .conn
                    .prepare_cached(&format!("SELECT COUNT(*) FROM albums WHERE {filter}"))?
                    .query_row([query], |row| row.get::<_, i64>(0))? as usize)
            })?;
        let order = match sort {
            AlbumSort::Title => "title,directory",
            AlbumSort::Artist => "COALESCE(artist,'Unknown artist'),title,directory",
        };
        let mut statement = self.conn.prepare_cached(&format!("SELECT directory,title,track_count,artist FROM albums WHERE {filter} ORDER BY {order} LIMIT ?2 OFFSET ?3"))?;
        let mut representative = self.conn.prepare_cached(&format!(
            "SELECT id,path,fingerprint FROM tracks INDEXED BY tracks_album_members WHERE {DIRECTORY}=?1 AND {TITLE}=?2 ORDER BY {ORDER} LIMIT 1"
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
                representative.query_row(params![directory, key.title], |row| {
                    Ok(ArtworkSource {
                        id: row.get(0)?,
                        path: row.get::<_, String>(1)?.into(),
                        fingerprint: fingerprint_from_row(row, 2)?,
                    })
                })?;
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
        let total =
            self.cached_count(
                super::CountDomain::AlbumTracks,
                &[directory, &key.title, query.as_deref().unwrap_or("")],
                || {
                    Ok(self.conn.prepare_cached(&format!(
                    "SELECT COUNT(*) FROM tracks INDEXED BY tracks_album_members WHERE {filter}"
                ))?.query_row(params![directory, key.title, query],
                |row| row.get::<_, i64>(0),)? as usize)
                },
            )?;
        let mut statement = self.conn.prepare_cached(&format!("SELECT {LIBRARY_ROW_COLUMNS} FROM tracks INDEXED BY tracks_album_members WHERE {filter} ORDER BY {ORDER} LIMIT ?4 OFFSET ?5"))?;
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

    pub(crate) fn album_has_tracks(&self, key: &AlbumKey) -> Result<bool> {
        let directory = key
            .directory
            .to_str()
            .context("Album directory is not UTF-8")?;
        Ok(self
            .conn
            .prepare_cached(&format!(
                "SELECT EXISTS(SELECT 1 FROM tracks INDEXED BY tracks_album_members WHERE {})",
                album_filter()
            ))?
            .query_row(params![directory, key.title], |row| row.get(0))?)
    }

    pub(crate) fn queue_album(&self, key: &AlbumKey, replace: bool) -> Result<()> {
        let directory = key
            .directory
            .to_str()
            .context("Album directory is not UTF-8")?;
        let tx = self.conn.unchecked_transaction()?;
        let filter = album_filter();
        ensure!(self.album_has_tracks(key)?, "Album is empty");
        if replace {
            tx.execute(
                "UPDATE rivu_meta SET current_queue_entry_id=NULL WHERE singleton=1",
                [],
            )?;
            super::queue::delete_queue_rows(&tx)?;
        }
        tx.execute(&format!("INSERT INTO playlist_entries(playlist_id,track_id,position) SELECT 0,id,(SELECT COALESCE(MAX(position)+1,0) FROM playlist_entries WHERE playlist_id=0)+row_number() OVER (ORDER BY {ORDER})-1 FROM tracks INDEXED BY tracks_album_members WHERE {filter} ORDER BY {ORDER}"), params![directory, key.title])?;
        tx.commit()?;
        Ok(())
    }
}
