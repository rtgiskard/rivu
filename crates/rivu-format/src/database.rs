use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatabaseState {
    Empty,
    Unversioned,
    Versioned { schema_version: i64 },
}

/// Inspect metadata without creating, upgrading, or modifying the database.
pub fn inspect(conn: &Connection) -> Result<DatabaseState> {
    let populated: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name NOT GLOB 'sqlite_*')",
        [],
        |row| row.get(0),
    )?;
    if !populated {
        return Ok(DatabaseState::Empty);
    }
    let metadata: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='rivu_meta')",
        [],
        |row| row.get(0),
    )?;
    if !metadata {
        return Ok(DatabaseState::Unversioned);
    }
    let schema_version = conn
        .query_row(
            "SELECT format_version FROM rivu_meta WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .optional()
        .context("Read library database schema version")?;
    let Some(schema_version) = schema_version else {
        return Ok(DatabaseState::Unversioned);
    };
    Ok(DatabaseState::Versioned { schema_version })
}

/// Validate the current format without writes.
pub fn validate_current(conn: &Connection) -> Result<()> {
    match inspect(conn)? {
        DatabaseState::Versioned {
            schema_version: SCHEMA_VERSION,
        } => Ok(()),
        DatabaseState::Versioned { schema_version } => bail!(
            "Unsupported database version {schema_version}; back up the database and WAL before replacing it; rescanning cannot restore user state"
        ),
        DatabaseState::Empty | DatabaseState::Unversioned => bail!(
            "Unsupported library database; back up the database and WAL before replacing it; rescanning cannot restore user state"
        ),
    }
}

/// Read-only structural preflight for offline operations. This checks the fields
/// used by current readers and writers and the required indexes, not SQL spelling.
pub fn validate_structure(conn: &Connection) -> Result<()> {
    validate_current(conn)?;
    for (table, columns) in [
        (
            "rivu_meta",
            "singleton,format_version,current_queue_entry_id",
        ),
        (
            "tracks",
            "id,path,fingerprint,file_size,modified_ns,raw_title,raw_artist,raw_album,title_override,artist_override,album_override,duration,codec,channels,sample_rate,missing,play_count,last_played,cue_sheet,cue_number,cue_start_frame,cue_end_frame,probe_version,bitrate_bps,track_number,disc_number,bitrate_exact,bits_per_sample,release_date,favorite,relocation_independent,decoder_hint",
        ),
        ("playlists", "id,name"),
        ("playlist_entries", "id,playlist_id,track_id,position"),
        ("queue_entries", "id,track_id,position"),
        ("albums", "directory,title,track_count,artist"),
        ("album_artists", "directory,title,artist,track_count"),
    ] {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
            [table],
            |row| row.get(0),
        )?;
        ensure!(
            exists,
            "Unsupported library database: missing table {table}"
        );
        conn.prepare(&format!("SELECT {columns} FROM {table} LIMIT 0"))
            .with_context(|| format!("Unsupported library database: incompatible table {table}"))?;
    }
    let mut primary_key =
        conn.prepare("SELECT name FROM pragma_table_info(?1) WHERE pk>0 ORDER BY pk")?;
    for (table, columns) in [
        ("rivu_meta", &["singleton"][..]),
        ("tracks", &["id"][..]),
        ("playlists", &["id"][..]),
        ("playlist_entries", &["id"][..]),
        ("queue_entries", &["id"][..]),
        ("albums", &["directory", "title"][..]),
        ("album_artists", &["directory", "title", "artist"][..]),
    ] {
        let mut rows = primary_key.query([table])?;
        for expected in columns {
            let row = rows.next()?.with_context(|| {
                format!("Unsupported library database: incompatible primary key for {table}")
            })?;
            ensure!(
                row.get_ref(0)?.as_str()? == *expected,
                "Unsupported library database: incompatible primary key for {table}"
            );
        }
        ensure!(
            rows.next()?.is_none(),
            "Unsupported library database: incompatible primary key for {table}"
        );
    }
    let queue_singleton: bool = conn.query_row(
        "SELECT count(*)=1 AND count(CASE WHEN singleton=1 THEN 1 END)=1
         FROM (SELECT singleton FROM rivu_meta LIMIT 2)",
        [],
        |row| row.get(0),
    )?;
    ensure!(
        queue_singleton,
        "Unsupported library database: missing or incompatible rivu_meta singleton"
    );
    let mut index =
        conn.prepare("SELECT \"unique\",partial FROM pragma_index_list(?1) WHERE name=?2")?;
    for (table, name, unique, partial) in [
        ("tracks", "tracks_fingerprint", false, false),
        ("tracks", "tracks_path", true, false),
        ("tracks", "tracks_cue", true, true),
        ("tracks", "tracks_recent", false, true),
        ("tracks", "tracks_album", false, false),
        ("tracks", "tracks_most_played", false, false),
        ("tracks", "tracks_album_members", false, false),
        ("playlist_entries", "playlist_order", false, false),
        ("playlist_entries", "playlist_track", true, false),
        ("playlist_entries", "playlist_track_lookup", false, false),
        ("queue_entries", "queue_order", false, false),
        ("queue_entries", "queue_track", false, false),
        ("albums", "albums_title", false, false),
        ("albums", "albums_artist", false, false),
    ] {
        let actual = index
            .query_row([table, name], |row| {
                Ok((row.get::<_, bool>(0)?, row.get::<_, bool>(1)?))
            })
            .optional()?;
        ensure!(
            actual == Some((unique, partial)),
            "Unsupported library database: missing or incompatible index {name}"
        );
    }
    let mut index_keys =
        conn.prepare("SELECT cid,name FROM pragma_index_info(?1) ORDER BY seqno")?;
    for (name, columns) in [
        ("tracks_path", &[Some("path"), None][..]),
        ("tracks_cue", &[Some("cue_sheet"), Some("cue_number")][..]),
        (
            "playlist_track",
            &[Some("playlist_id"), Some("track_id")][..],
        ),
    ] {
        let mut rows = index_keys.query([name])?;
        for expected in columns {
            let row = rows.next()?.with_context(|| {
                format!("Unsupported library database: incompatible key columns for index {name}")
            })?;
            let matches = match expected {
                Some(expected) => row.get_ref(1)?.as_str().ok() == Some(*expected),
                None => {
                    row.get::<_, i64>(0)? == -2
                        && matches!(row.get_ref(1)?, rusqlite::types::ValueRef::Null)
                }
            };
            ensure!(
                matches,
                "Unsupported library database: incompatible key columns for index {name}"
            );
        }
        ensure!(
            rows.next()?.is_none(),
            "Unsupported library database: incompatible key columns for index {name}"
        );
    }
    // Expression metadata identifies expression keys, not their semantics. These
    // read-only prepares check consumer compatibility, not expression equivalence
    // or whether the planner uses every key; no CREATE SQL comparison or writes.
    // The CUE branches mirror the exact and descendant scopes in store::scan.
    for (name, query) in [
        (
            "tracks_path",
            "SELECT id FROM tracks INDEXED BY tracks_path WHERE path=?1 AND CASE WHEN cue_sheet IS NULL THEN 0 ELSE NULL END=0 AND missing=0",
        ),
        (
            "tracks_cue",
            "SELECT t.id FROM json_each(?1) AS scope
             CROSS JOIN tracks AS t INDEXED BY tracks_cue
             WHERE t.cue_sheet IS NOT NULL AND t.cue_sheet=scope.value
             UNION
             SELECT t.id FROM json_each(?2) AS scope
             CROSS JOIN tracks AS t INDEXED BY tracks_cue
             WHERE t.cue_sheet IS NOT NULL
               AND t.cue_sheet>=scope.value AND t.cue_sheet<(rtrim(scope.value,'/')||'0')",
        ),
        (
            "tracks_recent",
            "SELECT id,COALESCE(title_override,raw_title),play_count,last_played,favorite
             FROM tracks INDEXED BY tracks_recent WHERE last_played IS NOT NULL
             ORDER BY last_played DESC,id DESC LIMIT ?1",
        ),
    ] {
        conn.prepare(query).with_context(|| {
            format!("Unsupported library database: index {name} cannot serve current queries")
        })?;
    }
    for name in [
        "albums_insert",
        "albums_delete",
        "albums_update",
        "albums_artist_update",
    ] {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='trigger' AND name=?1 AND tbl_name='tracks')",
            [name],
            |row| row.get(0),
        )?;
        ensure!(
            exists,
            "Unsupported library database: missing trigger {name}"
        );
    }
    Ok(())
}

/// Create an empty database, or validate the exact current format.
pub fn ensure_current(conn: &Connection) -> Result<()> {
    if inspect(conn)? == DatabaseState::Empty {
        create_current(conn)
    } else {
        validate_current(conn)
    }
}
pub const SCHEMA_VERSION: i64 = 2;

const TRACK_TABLE: &str = "CREATE TABLE tracks(
    id INTEGER PRIMARY KEY AUTOINCREMENT,path TEXT NOT NULL,
    fingerprint BLOB CHECK(fingerprint IS NULL OR (typeof(fingerprint)='blob' AND length(fingerprint)=32)),
    file_size INTEGER NOT NULL DEFAULT 0,modified_ns INTEGER NOT NULL DEFAULT 0,
    raw_title TEXT NOT NULL DEFAULT '',raw_artist TEXT NOT NULL DEFAULT '',
    raw_album TEXT NOT NULL DEFAULT '',title_override TEXT,artist_override TEXT,
    album_override TEXT,duration REAL,codec TEXT NOT NULL DEFAULT '',
    channels INTEGER NOT NULL DEFAULT 0,sample_rate INTEGER NOT NULL DEFAULT 0,
    missing INTEGER NOT NULL DEFAULT 0,play_count INTEGER NOT NULL DEFAULT 0,
    last_played INTEGER,cue_sheet TEXT,cue_number INTEGER,
    cue_start_frame INTEGER,cue_end_frame INTEGER,
    probe_version INTEGER NOT NULL DEFAULT 0,
    bitrate_bps INTEGER,track_number INTEGER,disc_number INTEGER,
    bitrate_exact INTEGER NOT NULL DEFAULT 0 CHECK(bitrate_exact IN (0,1)),
    bits_per_sample INTEGER,release_date TEXT,
    favorite INTEGER NOT NULL DEFAULT 0 CHECK(favorite IN (0,1)),
    relocation_independent INTEGER NOT NULL DEFAULT 0 CHECK(relocation_independent IN (0,1)),
    decoder_hint INTEGER NOT NULL DEFAULT 0 CHECK(decoder_hint IN (0,1,2))
);";

const TRACK_INDEXES: &str = "
    CREATE INDEX tracks_fingerprint ON tracks(fingerprint);
    CREATE UNIQUE INDEX tracks_path ON tracks(path, CASE WHEN cue_sheet IS NULL THEN 0 ELSE NULL END);
    CREATE UNIQUE INDEX tracks_cue ON tracks(cue_sheet,cue_number) WHERE cue_sheet IS NOT NULL;
    CREATE INDEX tracks_recent ON tracks(last_played DESC,id DESC) WHERE last_played IS NOT NULL;
    CREATE INDEX tracks_album ON tracks(COALESCE(album_override,raw_album), CASE WHEN COALESCE(album_override,raw_album)<>'' THEN COALESCE(disc_number,4294967295) END, CASE WHEN COALESCE(album_override,raw_album)<>'' THEN COALESCE(track_number,4294967295) END, COALESCE(title_override,raw_title));
    CREATE INDEX tracks_most_played ON tracks(play_count DESC);";

/// Create the current format transactionally. The connection must have no user tables.
pub fn create_current(conn: &Connection) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(TRACK_TABLE)?;
    tx.execute_batch(
        "CREATE TABLE playlists(id INTEGER PRIMARY KEY AUTOINCREMENT,name TEXT NOT NULL);
        CREATE TABLE playlist_entries(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
            track_id INTEGER NOT NULL,position INTEGER NOT NULL
        );
        CREATE INDEX playlist_order ON playlist_entries(playlist_id,position);
        CREATE UNIQUE INDEX playlist_track ON playlist_entries(playlist_id,track_id);
        CREATE INDEX playlist_track_lookup ON playlist_entries(track_id,playlist_id);
        CREATE TABLE queue_entries(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
            position INTEGER NOT NULL
        );
        CREATE INDEX queue_order ON queue_entries(position);
        CREATE INDEX queue_track ON queue_entries(track_id);
        CREATE TABLE rivu_meta(
            singleton INTEGER PRIMARY KEY CHECK(singleton=1),
            format_version INTEGER NOT NULL,
            current_queue_entry_id INTEGER REFERENCES queue_entries(id) ON DELETE SET NULL
        );",
    )?;
    tx.execute(
        "INSERT INTO rivu_meta(singleton,format_version) VALUES(1,?1)",
        [SCHEMA_VERSION],
    )?;
    tx.execute_batch(TRACK_INDEXES)?;
    create_summary_schema(&tx)?;
    tx.commit()?;
    Ok(())
}
// Paths are logical absolute paths. Keep root '/' while stripping other trailing
// separators so keys agree with Path::parent(), without recursively merging discs.
pub const DIRECTORY: &str = "CASE WHEN rtrim(path,replace(path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(path,replace(path,'/','')),'/') END";
pub const TITLE: &str = "COALESCE(album_override,raw_album)";
pub const ORDER: &str = "disc_number IS NULL,disc_number,track_number IS NULL,track_number,COALESCE(title_override,raw_title),id";

// Rust str::trim uses Unicode White_Space, not SQLite's default ASCII space.
// Keep this predicate built-in-only so persistent triggers work on any connection;
// trimming decides membership without changing the original album identity.
pub fn eligible_title(title: &str) -> String {
    format!(
        "trim({title},char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288))<>''"
    )
}

// SQLite owns these derived tables, including writes from other connections. Only
// effective grouping/artist changes touch them; stats and representative ordering
// changes need no rebuild. Each changed track does indexed counter updates, not a
// scan of its album. The covering member index also bounds representative lookup.
fn create_summary_schema(conn: &Connection) -> Result<()> {
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_database_is_created_directly_in_the_current_contract() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        assert_eq!(inspect(&conn)?, DatabaseState::Empty);
        ensure_current(&conn)?;
        assert_eq!(
            inspect(&conn)?,
            DatabaseState::Versioned {
                schema_version: SCHEMA_VERSION,
            }
        );
        validate_structure(&conn)?;
        let changes = conn.total_changes();
        conn.execute_batch("PRAGMA query_only=ON;")?;
        ensure_current(&conn)?;
        assert_eq!(conn.total_changes(), changes);
        Ok(())
    }

    #[test]
    fn ordinary_paths_are_unique_while_cue_tracks_share_audio_paths() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        create_current(&conn)?;
        conn.execute_batch(
            "INSERT INTO tracks(id,path,cue_sheet,cue_number) VALUES
                (1,'/album/image.flac',NULL,NULL),
                (2,'/album/image.flac','/album/disc.cue',1),
                (3,'/album/image.flac','/album/disc.cue',2),
                (4,'/album/image.flac','/album/alternate.cue',1),
                (5,'/album-other/image.flac',NULL,NULL);",
        )?;
        for missing in [0, 1] {
            let error = conn
                .execute(
                    "INSERT INTO tracks(path,missing) VALUES('/album/image.flac',?1)",
                    [missing],
                )
                .unwrap_err();
            assert_eq!(
                error.sqlite_error_code(),
                Some(rusqlite::ErrorCode::ConstraintViolation)
            );
        }
        let error = conn
            .execute(
                "INSERT INTO tracks(path,cue_sheet,cue_number)
                 VALUES('/different.flac','/album/disc.cue',1)",
                [],
            )
            .unwrap_err();
        assert_eq!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::ConstraintViolation)
        );
        assert_eq!(
            conn.query_row(
                "SELECT id FROM tracks WHERE path=?1
                 AND CASE WHEN cue_sheet IS NULL THEN 0 ELSE NULL END=0 AND missing=0",
                ["/album/image.flac"],
                |row| row.get::<_, i64>(0),
            )?,
            1
        );
        let mut directory =
            conn.prepare("SELECT id FROM tracks WHERE path>=?1 AND path<?2 ORDER BY id")?;
        let ids = directory
            .query_map(["/album/", "/album0"], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        assert_eq!(ids, [1, 2, 3, 4]);
        Ok(())
    }

    #[test]
    fn path_uniqueness_follows_cue_identity_transitions() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        create_current(&conn)?;
        conn.execute_batch(
            "INSERT INTO tracks(id,path,cue_sheet,cue_number,play_count,favorite) VALUES
                (1,'/album/image.flac','/album/disc.cue',1,11,1),
                (2,'/album/image.flac','/album/disc.cue',2,22,0);
             UPDATE tracks SET cue_sheet=NULL,cue_number=NULL WHERE id=1;",
        )?;
        let error = conn
            .execute(
                "UPDATE tracks SET cue_sheet=NULL,cue_number=NULL WHERE id=2",
                [],
            )
            .unwrap_err();
        assert_eq!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::ConstraintViolation)
        );
        assert_eq!(
            conn.query_row(
                "SELECT cue_sheet,cue_number FROM tracks WHERE id=2",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )?,
            ("/album/disc.cue".into(), 2)
        );
        conn.execute_batch(
            "UPDATE tracks SET cue_sheet='/album/disc.cue',cue_number=3 WHERE id=1;
             UPDATE tracks SET cue_sheet=NULL,cue_number=NULL WHERE id=2;
             UPDATE tracks SET path='/album/other.flac' WHERE id=2;
             UPDATE tracks SET cue_sheet=NULL,cue_number=NULL WHERE id=1;",
        )?;
        let mut tracks = conn.prepare(
            "SELECT id,path,play_count,favorite FROM tracks
             WHERE cue_sheet IS NULL AND cue_number IS NULL ORDER BY id",
        )?;
        let rows = tracks
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, bool>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        assert_eq!(
            rows,
            [
                (1, "/album/image.flac".into(), 11, true),
                (2, "/album/other.flac".into(), 22, false),
            ]
        );
        Ok(())
    }

    #[test]
    fn current_metadata_tracks_queue_occurrence() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        create_current(&conn)?;
        conn.execute_batch(
            "INSERT INTO tracks(id,path) VALUES(1,'/retained.wav');
             INSERT INTO queue_entries(id,track_id,position) VALUES(41,1,0),(42,1,1);
             UPDATE rivu_meta SET current_queue_entry_id=42 WHERE singleton=1;
             DELETE FROM queue_entries WHERE id=41;",
        )?;
        let current = || {
            conn.query_row(
                "SELECT current_queue_entry_id FROM rivu_meta WHERE singleton=1",
                [],
                |row| row.get::<_, Option<i64>>(0),
            )
        };
        assert_eq!(current()?, Some(42));
        conn.execute("DELETE FROM queue_entries WHERE id=42", [])?;
        assert_eq!(current()?, None);
        assert!(
            conn.execute(
                "INSERT INTO rivu_meta(singleton,format_version) VALUES(2,2)",
                []
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn sqlite_named_user_tables_are_not_treated_as_an_empty_database() -> Result<()> {
        for name in ["sqlitex_data", "SQLiteX_data"] {
            let conn = Connection::open_in_memory()?;
            conn.execute_batch(&format!(
                "CREATE TABLE {name}(value BLOB); INSERT INTO {name} VALUES(X'0102FF');"
            ))?;
            let schema: i64 = conn.query_row("PRAGMA schema_version", [], |row| row.get(0))?;
            assert_eq!(inspect(&conn)?, DatabaseState::Unversioned);
            assert!(ensure_current(&conn).is_err());
            assert_eq!(
                conn.query_row("PRAGMA schema_version", [], |row| row.get::<_, i64>(0))?,
                schema
            );
            assert_eq!(
                conn.query_row(&format!("SELECT value FROM {name}"), [], |row| {
                    row.get::<_, Vec<u8>>(0)
                })?,
                [1, 2, 255]
            );
        }
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TABLE transient(id INTEGER PRIMARY KEY AUTOINCREMENT); DROP TABLE transient;",
        )?;
        assert_eq!(inspect(&conn)?, DatabaseState::Empty);
        ensure_current(&conn)?;
        validate_structure(&conn)?;
        Ok(())
    }

    #[test]
    fn unsupported_versions_are_rejected_before_any_schema_or_data_write() -> Result<()> {
        for version in [0, 1, SCHEMA_VERSION + 1] {
            let conn = Connection::open_in_memory()?;
            create_current(&conn)?;
            conn.execute(
                "UPDATE rivu_meta SET format_version=?1 WHERE singleton=1",
                [version],
            )?;
            let schema: i64 = conn.query_row("PRAGMA schema_version", [], |row| row.get(0))?;
            let changes = conn.total_changes();
            assert!(ensure_current(&conn).is_err());
            assert_eq!(conn.total_changes(), changes);
            assert_eq!(
                conn.query_row("PRAGMA schema_version", [], |row| row.get::<_, i64>(0))?,
                schema
            );
        }
        let conn = Connection::open_in_memory()?;
        conn.execute("CREATE TABLE unknown(value BLOB)", [])?;
        assert_eq!(inspect(&conn)?, DatabaseState::Unversioned);
        assert!(ensure_current(&conn).is_err());
        Ok(())
    }

    #[test]
    fn preflight_rejects_incompatible_path_index_properties() -> Result<()> {
        for definition in [
            "CREATE INDEX tracks_path ON tracks(path, CASE WHEN cue_sheet IS NULL THEN 0 ELSE NULL END)",
            "CREATE UNIQUE INDEX tracks_path ON tracks(path, CASE WHEN cue_sheet IS NULL THEN 0 ELSE NULL END) WHERE cue_sheet IS NULL",
            "CREATE UNIQUE INDEX tracks_path ON tracks(path) WHERE cue_sheet IS NULL",
        ] {
            let conn = Connection::open_in_memory()?;
            create_current(&conn)?;
            conn.execute_batch(&format!("DROP INDEX tracks_path; {definition};"))?;
            conn.execute_batch("PRAGMA query_only=ON;")?;
            assert!(validate_structure(&conn).is_err(), "{definition}");
        }
        Ok(())
    }

    #[test]
    fn preflight_rejects_missing_reader_columns() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        create_current(&conn)?;
        conn.execute_batch("ALTER TABLE tracks RENAME COLUMN favorite TO unsupported_favorite;")?;
        assert!(validate_structure(&conn).is_err());
        Ok(())
    }

    #[test]
    fn preflight_rejects_missing_or_incompatible_primary_keys() -> Result<()> {
        for (table, definition) in [
            (
                "albums",
                "directory TEXT NOT NULL,title TEXT NOT NULL,track_count INTEGER NOT NULL,artist TEXT",
            ),
            (
                "albums",
                "directory TEXT NOT NULL,title TEXT NOT NULL,track_count INTEGER NOT NULL,artist TEXT,PRIMARY KEY(title,directory)",
            ),
            (
                "album_artists",
                "directory TEXT NOT NULL,title TEXT NOT NULL,artist TEXT NOT NULL,track_count INTEGER NOT NULL",
            ),
            (
                "album_artists",
                "directory TEXT NOT NULL,title TEXT NOT NULL,artist TEXT NOT NULL,track_count INTEGER NOT NULL,PRIMARY KEY(directory,title)",
            ),
        ] {
            let conn = Connection::open_in_memory()?;
            create_current(&conn)?;
            conn.execute_batch(&format!(
                "DROP TABLE {table}; CREATE TABLE {table}({definition});"
            ))?;
            if table == "albums" {
                conn.execute_batch(
                    "CREATE INDEX albums_title ON albums(title,directory);
                     CREATE INDEX albums_artist ON albums(COALESCE(artist,'Unknown artist'),title,directory);",
                )?;
            }
            assert!(
                validate_structure(&conn).is_err(),
                "accepted incompatible {table} primary key: {definition}"
            );
        }
        Ok(())
    }

    #[test]
    fn preflight_rejects_wrong_unique_index_keys() -> Result<()> {
        for (name, definition) in [
            (
                "tracks_path",
                "ON tracks(raw_title, CASE WHEN cue_sheet IS NULL THEN 0 ELSE NULL END)",
            ),
            ("tracks_path", "ON tracks(path)"),
            ("tracks_path", "ON tracks(path,cue_sheet)"),
            (
                "tracks_path",
                "ON tracks(CASE WHEN cue_sheet IS NULL THEN 0 ELSE NULL END,path)",
            ),
            (
                "tracks_path",
                "ON tracks(path, CASE WHEN cue_sheet IS NULL THEN 0 ELSE NULL END,id)",
            ),
            (
                "tracks_cue",
                "ON tracks(cue_sheet,path) WHERE cue_sheet IS NOT NULL",
            ),
            (
                "playlist_track",
                "ON playlist_entries(playlist_id,position)",
            ),
        ] {
            let conn = Connection::open_in_memory()?;
            create_current(&conn)?;
            conn.execute_batch(&format!(
                "DROP INDEX {name}; CREATE UNIQUE INDEX {name} {definition};"
            ))?;
            assert!(
                validate_structure(&conn).is_err(),
                "accepted wrong keys for {name}"
            );
        }
        Ok(())
    }

    #[test]
    fn preflight_rejects_partial_indexes_unusable_by_current_consumers() -> Result<()> {
        for (name, definition) in [
            (
                "tracks_cue",
                "CREATE UNIQUE INDEX tracks_cue ON tracks(cue_sheet,cue_number) WHERE cue_sheet IS NULL",
            ),
            (
                "tracks_recent",
                "CREATE INDEX tracks_recent ON tracks(last_played DESC,id DESC) WHERE last_played IS NULL",
            ),
        ] {
            let conn = Connection::open_in_memory()?;
            create_current(&conn)?;
            conn.execute_batch(&format!("DROP INDEX {name}; {definition};"))?;
            assert!(
                validate_structure(&conn).is_err(),
                "accepted incompatible predicate for {name}"
            );
        }
        Ok(())
    }

    #[test]
    fn preflight_requires_queue_singleton_without_writes() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        create_current(&conn)?;
        conn.execute_batch("PRAGMA query_only=ON;")?;
        let changes = conn.total_changes();
        let schema: i64 = conn.query_row("PRAGMA schema_version", [], |row| row.get(0))?;
        validate_structure(&conn)?;
        assert_eq!(conn.total_changes(), changes);
        assert_eq!(
            conn.query_row("PRAGMA schema_version", [], |row| row.get::<_, i64>(0))?,
            schema
        );
        conn.execute_batch("PRAGMA query_only=OFF; DELETE FROM rivu_meta; PRAGMA query_only=ON;")?;
        let changes = conn.total_changes();
        assert!(validate_structure(&conn).is_err());
        assert_eq!(conn.total_changes(), changes);
        Ok(())
    }

    #[test]
    fn preflight_requires_summary_definitions_without_writes() -> Result<()> {
        for sql in [
            "DROP TRIGGER albums_insert",
            "DROP TRIGGER albums_delete",
            "DROP TRIGGER albums_update",
            "DROP TRIGGER albums_artist_update",
        ] {
            let conn = Connection::open_in_memory()?;
            create_current(&conn)?;
            conn.execute_batch(sql)?;
            let changes = conn.total_changes();
            let schema: i64 = conn.query_row("PRAGMA schema_version", [], |row| row.get(0))?;
            conn.execute_batch("PRAGMA query_only=ON;")?;
            assert!(validate_structure(&conn).is_err(), "{sql}");
            assert_eq!(conn.total_changes(), changes);
            assert_eq!(
                conn.query_row("PRAGMA schema_version", [], |row| row.get::<_, i64>(0))?,
                schema
            );
        }
        Ok(())
    }
}
