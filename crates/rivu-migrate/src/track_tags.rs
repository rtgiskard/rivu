use anyhow::{Context, Result, ensure};
use rivu_format::database;
use rusqlite::Connection;

pub(super) fn apply(conn: &Connection) -> Result<()> {
    ensure!(
        super::target_schema::validate_tag_source(conn)? == 4,
        "Tag cutover requires schema 4"
    );
    // SQLite preserves an explicitly quoted rename target. Match the fresh schema
    // instead of weakening target verification to ignore identifier quoting.
    tracing::info!("Materializing effective tags without modifying audio files");
    conn.execute_batch(
        "DROP TRIGGER albums_insert;
         DROP TRIGGER albums_delete;
         DROP TRIGGER albums_update;
         DROP TRIGGER albums_artist_update;
         DROP INDEX tracks_album;
         DROP INDEX tracks_album_members;
         ALTER TABLE tracks RENAME COLUMN raw_title TO \"title\";
         ALTER TABLE tracks RENAME COLUMN raw_artist TO \"artist\";
         ALTER TABLE tracks RENAME COLUMN raw_album TO \"album\";
         UPDATE tracks SET title=COALESCE(title_override,title),
             artist=COALESCE(artist_override,artist),album=COALESCE(album_override,album);
         ALTER TABLE tracks DROP COLUMN title_override;
         ALTER TABLE tracks DROP COLUMN artist_override;
         ALTER TABLE tracks DROP COLUMN album_override;",
    )
    .context("Replacing raw tags and overrides with effective tag values")?;
    // Derived album contents already describe these exact effective values.
    // Install the current indexed maintenance logic without rebuilding user state.
    let fresh = Connection::open_in_memory()?;
    database::create_current(&fresh)?;
    let mut objects = fresh.prepare(
        "SELECT sql FROM sqlite_schema WHERE name IN (
            'tracks_album','tracks_album_members','albums_insert','albums_delete',
            'albums_update','albums_artist_update') ORDER BY type,name",
    )?;
    let mut rows = objects.query([])?;
    while let Some(row) = rows.next()? {
        conn.execute_batch(row.get_ref(0)?.as_str()?)?;
    }
    conn.execute(
        "UPDATE rivu_meta SET format_version=?1 WHERE singleton=1",
        [database::SCHEMA_VERSION],
    )?;
    super::target_schema::validate(conn)
}
