use anyhow::{Context, Result, ensure};
use rivu_format::database;
use rusqlite::Connection;

pub(super) fn validate(conn: &Connection) -> Result<()> {
    ensure!(
        database::inspect(conn)? == database::DatabaseState::Versioned { schema_version: 2 },
        "Expected database schema 2"
    );
    database::validate_common_structure(conn)?;
    let compatible: bool = conn.query_row(
        "SELECT NOT EXISTS(SELECT 1 FROM pragma_table_info('playlists') WHERE name='position')",
        [],
        |row| row.get(0),
    )?;
    ensure!(compatible, "Incompatible schema 2 catalog structure");
    Ok(())
}

pub(super) fn apply(conn: &Connection) -> Result<()> {
    validate(conn)?;
    tracing::info!("Adding playlist catalog positions in playlist-ID order");
    conn.execute_batch(
        "ALTER TABLE playlists ADD COLUMN position INTEGER NOT NULL DEFAULT 0 CHECK(position>=0);
         WITH ranked AS MATERIALIZED (
            SELECT id,row_number() OVER (ORDER BY id)-1 AS position FROM playlists WHERE id>0
         ) UPDATE playlists SET position=ranked.position FROM ranked WHERE playlists.id=ranked.id;",
    )
    .context("Persisting playlist catalog order")?;
    tracing::info!(
        schema_version = database::SCHEMA_VERSION,
        "Advancing format marker and validating target structure"
    );
    conn.execute(
        "UPDATE rivu_meta SET format_version=?1 WHERE singleton=1",
        [database::SCHEMA_VERSION],
    )?;
    tracing::info!("Verifying all schema objects against a fresh database");
    super::target_schema::validate(conn)
}
