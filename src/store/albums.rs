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
        let filter = "?1 IS NULL OR instr(lower(p.title),lower(COALESCE(?1,'')))>0 OR EXISTS(SELECT 1 FROM album_artists WHERE directory=p.directory AND title=p.title AND instr(lower(artist),lower(COALESCE(?1,'')))>0)";
        let total =
            self.cached_count(super::CountDomain::Albums, &[query.unwrap_or("")], || {
                Ok(self
                    .conn
                    .prepare_cached(&format!("SELECT COUNT(*) FROM albums AS p WHERE {filter}"))?
                    .query_row([query], |row| row.get::<_, i64>(0))? as usize)
            })?;
        let sql_offset = i64::try_from(offset)?;
        if limit == 0 || offset >= total {
            return Ok(Page {
                total,
                rows: Vec::new(),
            });
        }
        let order = match sort {
            AlbumSort::Title => "p.title,p.directory",
            AlbumSort::Artist => "COALESCE(p.artist,'Unknown artist'),p.title,p.directory",
        };
        // Unary + removes the directory column's TEXT affinity so SQLite can
        // seek the directory expression in tracks_album_members.
        let mut statement = self.conn.prepare_cached(&format!(
            "WITH page AS (
                SELECT directory,title,track_count,artist FROM albums AS p
                WHERE {filter} ORDER BY {order} LIMIT ?2 OFFSET ?3
             )
             SELECT p.directory,p.title,p.track_count,p.artist,r.id,r.path,r.fingerprint
             FROM page AS p LEFT JOIN tracks AS r ON r.id=(
                SELECT id FROM tracks INDEXED BY tracks_album_members
                WHERE {DIRECTORY}=+p.directory AND {TITLE}=p.title ORDER BY {ORDER} LIMIT 1
             ) ORDER BY {order}"
        ))?;
        let mut rows =
            statement.query(params![query, limit.min(MAX_QUERY_ROWS) as i64, sql_offset])?;
        let mut summaries = Vec::new();
        while let Some(row) = rows.next()? {
            let key = AlbumKey {
                directory: row.get::<_, String>(0)?.into(),
                title: row.get(1)?,
            };
            let representative = ArtworkSource {
                id: row
                    .get::<_, Option<i64>>(4)?
                    .ok_or(rusqlite::Error::QueryReturnedNoRows)?,
                path: row.get::<_, String>(5)?.into(),
                fingerprint: fingerprint_from_row(row, 6)?,
            };
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
        let member_filter = || format!("{} AND {}", album_filter(), search_filter!("tracks", "?3"));
        let mut filter = None;
        let total = if let Some(query) = &query {
            self.cached_count(
                super::CountDomain::AlbumTracks,
                &[directory, &key.title, query],
                || {
                    let filter = filter.get_or_insert_with(member_filter);
                    Ok(self.conn.prepare_cached(&format!(
                        "SELECT COUNT(*) FROM tracks INDEXED BY tracks_album_members WHERE {filter}"
                    ))?.query_row(params![directory, key.title, query], |row| row.get::<_, i64>(0))? as usize)
                },
            )?
        } else {
            self.conn.prepare_cached(
                "SELECT COALESCE((SELECT track_count FROM albums WHERE directory=?1 AND title=?2),0)",
            )?.query_row(params![directory, key.title], |row| row.get::<_, i64>(0))? as usize
        };
        let offset = i64::try_from(offset)?;
        if limit == 0 {
            return Ok(Page {
                total,
                rows: Vec::new(),
            });
        }
        let filter = filter.unwrap_or_else(member_filter);
        let mut statement = self.conn.prepare_cached(&format!("SELECT {LIBRARY_ROW_COLUMNS} FROM tracks INDEXED BY tracks_album_members WHERE {filter} ORDER BY {ORDER} LIMIT ?4 OFFSET ?5"))?;
        let rows = statement
            .query_map(
                params![
                    directory,
                    key.title,
                    query,
                    limit.min(MAX_QUERY_ROWS) as i64,
                    offset
                ],
                library_row_from_row,
            )?
            .collect::<rusqlite::Result<_>>()?;
        Ok(Page { total, rows })
    }

    pub(crate) fn album_track_ids(&self, key: &AlbumKey) -> Result<Vec<i64>> {
        let directory = key
            .directory
            .to_str()
            .context("Album directory is not UTF-8")?;
        let mut statement = self.conn.prepare_cached(&format!(
            "SELECT id FROM tracks INDEXED BY tracks_album_members WHERE {} ORDER BY {ORDER}",
            album_filter(),
        ))?;
        Ok(statement
            .query_map(params![directory, key.title], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?)
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

    pub(crate) fn replace_queue_with_album(&self, key: &AlbumKey) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE rivu_meta SET current_queue_entry_id=NULL WHERE singleton=1",
            [],
        )?;
        super::queue::delete_queue_rows(&tx)?;
        ensure!(insert_album_members(&tx, key, 0, 0)? != 0, "Album is empty");
        tx.commit()?;
        Ok(())
    }
}

pub(super) fn insert_album_members(
    tx: &rusqlite::Transaction<'_>,
    key: &AlbumKey,
    playlist_id: i64,
    start: i64,
) -> Result<usize> {
    let directory = key
        .directory
        .to_str()
        .context("Album directory is not UTF-8")?;
    Ok(tx.execute(
        &format!(
            "INSERT INTO playlist_entries(playlist_id,track_id,position)
            SELECT ?3,id,?4+(row_number() OVER (ORDER BY {ORDER})-1)
            FROM tracks INDEXED BY tracks_album_members WHERE {} ORDER BY {ORDER}",
            album_filter()
        ),
        params![directory, key.title, playlist_id, start],
    )?)
}
