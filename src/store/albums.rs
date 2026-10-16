use super::rows::{LIBRARY_ROW_COLUMNS, library_row_from_row};
use super::tracks::{search_filter, search_query};
use super::{Store, queue::invalidate_structure};
use crate::model::{AlbumKey, AlbumSort, AlbumSummary, LibraryRow, MAX_QUERY_ROWS, Page};
use anyhow::{Context, Result, ensure};
use rusqlite::params;

// Paths are logical absolute paths. Keep root '/' while stripping other trailing
// separators so keys agree with Path::parent(), without recursively merging discs.
const DIRECTORY: &str = "CASE WHEN rtrim(path,replace(path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(path,replace(path,'/','')),'/') END";
const TITLE: &str = "COALESCE(album_override,raw_album)";
const ORDER: &str = "disc_number IS NULL,disc_number,track_number IS NULL,track_number,COALESCE(title_override,raw_title),id";

// A slash-terminated prefix has the exclusive BINARY upper bound ending in '0',
// including Unicode suffixes. Keep the parent check so child directories do not
// join the album. Callers force tracks_directory: title equality otherwise makes
// SQLite prefer tracks_album and scan every same-titled album.
fn album_filter() -> String {
    format!(
        "path>=(CASE WHEN ?1='/' THEN '/' ELSE ?1||'/' END) AND path<(CASE WHEN ?1='/' THEN '0' ELSE ?1||'0' END) AND {DIRECTORY}=?1 AND {TITLE}=?2"
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
        let groups = format!(
            "SELECT {DIRECTORY} AS directory,{TITLE} AS title,COUNT(*) AS track_count,CASE COUNT(DISTINCT NULLIF(COALESCE(artist_override,raw_artist),'')) WHEN 0 THEN NULL WHEN 1 THEN MAX(NULLIF(COALESCE(artist_override,raw_artist),'')) ELSE 'Various artists' END AS artist FROM tracks GROUP BY directory,title HAVING ?1 IS NULL OR instr(lower({TITLE}),lower(COALESCE(?1,'')))>0 OR MAX(instr(lower(COALESCE(artist_override,raw_artist)),lower(COALESCE(?1,''))))>0"
        );
        let total = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM ({groups})"),
            [query],
            |row| row.get::<_, i64>(0),
        )? as usize;
        let order = match sort {
            AlbumSort::Title => "title,directory",
            AlbumSort::Artist => "COALESCE(artist,'Unknown artist'),title,directory",
        };
        let mut statement = self.conn.prepare(&format!("SELECT directory,title,track_count,artist FROM ({groups}) ORDER BY {order} LIMIT ?2 OFFSET ?3"))?;
        let mut representative = self.conn.prepare(&format!(
            "SELECT id FROM tracks INDEXED BY tracks_directory WHERE {} ORDER BY {ORDER} LIMIT 1",
            album_filter()
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
            let id = representative.query_row(params![directory, key.title], |row| row.get(0))?;
            summaries.push(AlbumSummary {
                representative: self
                    .track(id)?
                    .context("Album representative disappeared")?,
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
            &format!("SELECT COUNT(*) FROM tracks INDEXED BY tracks_directory WHERE {filter}"),
            params![directory, key.title, query],
            |row| row.get::<_, i64>(0),
        )? as usize;
        let mut statement = self.conn.prepare(&format!("SELECT {LIBRARY_ROW_COLUMNS} FROM tracks INDEXED BY tracks_directory WHERE {filter} ORDER BY {ORDER} LIMIT ?4 OFFSET ?5"))?;
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
            &format!("SELECT COUNT(*) FROM tracks INDEXED BY tracks_directory WHERE {filter}"),
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
        tx.execute(&format!("INSERT INTO queue_entries(track_id,position) SELECT id,(SELECT COALESCE(MAX(position)+1,0) FROM queue_entries)+row_number() OVER (ORDER BY {ORDER})-1 FROM tracks INDEXED BY tracks_directory WHERE {filter} ORDER BY {ORDER}"), params![directory, key.title])?;
        invalidate_structure(&tx, true)?;
        tx.commit()?;
        Ok(())
    }
}
