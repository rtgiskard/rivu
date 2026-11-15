use anyhow::{Context, Result};
use rivu_format::database;
use rusqlite::Connection;

pub(super) fn apply(conn: &Connection) -> Result<()> {
    super::target_schema::validate_replaygain_source(conn)?;
    conn.execute_batch("ALTER TABLE tracks ADD COLUMN replaygain_track_gain REAL;")
        .context("Adding ReplayGain metadata without modifying audio files")?;
    conn.execute(
        "UPDATE rivu_meta SET format_version=?1 WHERE singleton=1",
        [database::SCHEMA_VERSION],
    )?;
    super::target_schema::validate(conn)
}
