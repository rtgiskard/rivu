use super::rows::{LIBRARY_ROW_COLUMNS, TRACK_COLUMNS, library_row_from_row, track_from_row};
use super::tracks::{search_filter, search_query};
use super::{Store, queue::invalidate_structure};
use crate::model::{AlbumKey, AlbumSort, AlbumSummary, LibraryRow, MAX_QUERY_ROWS, Page};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, params};

// Paths are logical absolute paths. Keep root '/' while stripping other trailing
// separators so keys agree with Path::parent(), without recursively merging discs.
const DIRECTORY: &str = "CASE WHEN rtrim(path,replace(path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(path,replace(path,'/','')),'/') END";
const TITLE: &str = "COALESCE(album_override,raw_album)";
const ORDER: &str = "disc_number IS NULL,disc_number,track_number IS NULL,track_number,COALESCE(title_override,raw_title),id";

// Rust str::trim uses Unicode White_Space, not SQLite's default ASCII space.
// Keep this predicate built-in-only so persistent triggers work on any connection;
// trimming decides membership without changing the original album identity.
fn eligible_title(title: &str) -> String {
    format!(
        "trim({title},char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288))<>''"
    )
}

// SQLite owns these derived tables, including writes from other connections. Only
// effective grouping/artist changes touch them; stats and representative ordering
// changes need no rebuild. Each changed track does indexed counter updates, not a
// scan of its album. The covering member index also bounds representative lookup.
pub(super) fn create_summary_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(&format!(
        "CREATE TABLE albums(
            directory TEXT NOT NULL,title TEXT NOT NULL,
            track_count INTEGER NOT NULL,artist TEXT,
            PRIMARY KEY(directory,title)
        ) WITHOUT ROWID;
        CREATE INDEX albums_title ON albums(title,directory);
        CREATE INDEX albums_artist ON albums(COALESCE(artist,'Unknown artist'),title,directory);
        CREATE TABLE album_artists(
            directory TEXT NOT NULL,title TEXT NOT NULL,artist TEXT NOT NULL,
            track_count INTEGER NOT NULL,
            PRIMARY KEY(directory,title,artist)
        ) WITHOUT ROWID;
        CREATE INDEX tracks_album_members ON tracks({DIRECTORY},{TITLE},{ORDER});"
    ))?;
    install_summary_triggers(conn)
}

fn install_summary_triggers(conn: &Connection) -> Result<()> {
    let directory = |row: &str| DIRECTORY.replace("path", &format!("{row}.path"));
    let title = |row: &str| format!("COALESCE({row}.album_override,{row}.raw_album)");
    let artist = |row: &str| format!("COALESCE({row}.artist_override,{row}.raw_artist)");
    let refresh_artist = |row: &str| {
        let d = directory(row);
        let t = title(row);
        let value = format!(
            "CASE WHEN EXISTS(SELECT 1 FROM album_artists
                WHERE directory={d} AND title={t} LIMIT 1 OFFSET 1)
             THEN 'Various artists'
             ELSE (SELECT artist FROM album_artists
                WHERE directory={d} AND title={t} LIMIT 1) END"
        );
        format!(
            "UPDATE albums SET artist={value}
             WHERE directory={d} AND title={t} AND artist IS NOT ({value});"
        )
    };
    let add_artist = format!(
        "INSERT INTO album_artists(directory,title,artist,track_count)
            SELECT {d},{t},{a},1 WHERE {eligible} AND {a}<>''
            ON CONFLICT(directory,title,artist) DO UPDATE SET track_count=track_count+1;",
        d = directory("new"),
        t = title("new"),
        a = artist("new"),
        eligible = eligible_title(&title("new")),
    );
    let remove_artist = format!(
        "UPDATE album_artists SET track_count=track_count-1
            WHERE directory={d} AND title={t} AND artist={a};
         DELETE FROM album_artists WHERE directory={d} AND title={t} AND artist={a}
            AND track_count=0;",
        d = directory("old"),
        t = title("old"),
        a = artist("old"),
    );
    let add = format!(
        "INSERT INTO albums(directory,title,track_count) SELECT {d},{t},1 WHERE {eligible}
            ON CONFLICT(directory,title) DO UPDATE SET track_count=track_count+1;
         {add_artist}
         {refresh}",
        d = directory("new"),
        t = title("new"),
        refresh = refresh_artist("new"),
        eligible = eligible_title(&title("new")),
    );
    let remove = format!(
        "UPDATE albums SET track_count=track_count-1 WHERE directory={d} AND title={t};
         {remove_artist}
         DELETE FROM albums WHERE directory={d} AND title={t} AND track_count=0;
         {refresh}",
        d = directory("old"),
        t = title("old"),
        refresh = refresh_artist("old"),
    );
    conn.execute_batch(&format!(
        "CREATE TRIGGER albums_insert AFTER INSERT ON tracks BEGIN {add} END;
         CREATE TRIGGER albums_delete AFTER DELETE ON tracks BEGIN {remove} END;
         CREATE TRIGGER albums_update AFTER UPDATE OF path,raw_album,album_override,
            raw_artist,artist_override ON tracks
         WHEN {old_d} IS NOT {new_d} OR {old_t} IS NOT {new_t}
         BEGIN {remove} {add} END;
         CREATE TRIGGER albums_artist_update AFTER UPDATE OF path,raw_album,album_override,
            raw_artist,artist_override ON tracks
         WHEN {old_d} IS {new_d} AND {old_t} IS {new_t}
            AND {old_a} IS NOT {new_a} AND {eligible}
         BEGIN {remove_artist} {add_artist} {refresh} END;",
        old_d = directory("old"),
        new_d = directory("new"),
        old_t = title("old"),
        new_t = title("new"),
        old_a = artist("old"),
        new_a = artist("new"),
        eligible = eligible_title(&title("new")),
        refresh = refresh_artist("new"),
    ))?;
    conn.execute(
        "INSERT INTO schema_meta(key,value) VALUES('album_trigger_revision',2)
        ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [],
    )?;
    Ok(())
}

pub(super) fn ensure_summary_triggers(conn: &Connection) -> Result<()> {
    let current: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM schema_meta WHERE key='album_trigger_revision' AND value=2)",
        [],
        |row| row.get(0),
    )?;
    if current {
        return Ok(());
    }
    // Only trigger definitions change; existing summary rows remain authoritative.
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(
        "DROP TRIGGER albums_insert; DROP TRIGGER albums_delete; DROP TRIGGER albums_update;
         DROP TRIGGER IF EXISTS albums_artist_update;",
    )?;
    install_summary_triggers(&tx)?;
    tx.commit()?;
    Ok(())
}

// Exact parent and effective title match the leading member-index expressions;
// descendants and same-titled albums in other directories are separate ranges.
fn album_filter() -> String {
    format!(
        "{DIRECTORY}=?1 AND {TITLE}=?2 AND {}",
        eligible_title(TITLE)
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
