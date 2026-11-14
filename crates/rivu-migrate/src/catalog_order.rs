use anyhow::{Context, Result};
use rusqlite::Connection;

pub(super) fn apply(conn: &Connection) -> Result<()> {
    anyhow::ensure!(
        super::target_schema::validate_tag_source(conn)? == 2,
        "Catalog order migration requires schema 2"
    );
    tracing::info!("Adding playlist catalog positions in playlist-ID order");
    conn.execute_batch(
        "ALTER TABLE playlists ADD COLUMN position INTEGER NOT NULL DEFAULT 0 CHECK(position>=0);
         WITH ranked AS MATERIALIZED (
            SELECT id,row_number() OVER (ORDER BY id)-1 AS position FROM playlists WHERE id>0
         ) UPDATE playlists SET position=ranked.position FROM ranked WHERE playlists.id=ranked.id;",
    )
    .context("Persisting playlist catalog order")?;
    tracing::info!(
        schema_version = 4,
        "Advancing format marker and validating target structure"
    );
    conn.execute(
        "UPDATE rivu_meta SET format_version=?1 WHERE singleton=1",
        [4],
    )?;
    tracing::info!("Verifying catalog-order migration source for tag cutover");
    super::target_schema::validate_tag_source(conn).map(|_| ())
}
