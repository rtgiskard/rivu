use super::albums::album_filter;
use super::{Store, queue::integer};
use anyhow::{Context, Result, ensure};
use rivu_format::database::ORDER;
use rusqlite::params;

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
}
