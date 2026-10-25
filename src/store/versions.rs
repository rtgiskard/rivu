use super::{Store, queue::integer};
use anyhow::{Result, ensure};
use rusqlite::OptionalExtension;
use std::collections::BTreeMap;

impl Store {
    /// Pin versions and result rows to one SQLite read snapshot.
    pub(crate) fn query_snapshot(&self) -> Result<rusqlite::Transaction<'_>> {
        Ok(self.conn.unchecked_transaction()?)
    }

    pub(crate) fn queue_revision(&self) -> Result<u64> {
        Ok(self.conn.query_row(
            "SELECT revision FROM queue_meta WHERE singleton=1",
            [],
            |row| integer(row, 0),
        )?)
    }

    pub(crate) fn album_revision(&self) -> Result<u64> {
        Ok(self.conn.query_row(
            "SELECT album_revision FROM collection_meta WHERE singleton=1",
            [],
            |row| integer(row, 0),
        )?)
    }

    pub(crate) fn playlist_entry_revisions(&self) -> Result<BTreeMap<i64, u64>> {
        Ok(self
            .conn
            .prepare("SELECT playlist_id,revision FROM playlist_versions ORDER BY playlist_id")?
            .query_map([], |row| Ok((row.get(0)?, integer(row, 1)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub(crate) fn playlist_entry_revision(&self, playlist_id: i64) -> Result<Option<u64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT revision FROM playlist_versions WHERE playlist_id=?1",
                [playlist_id],
                |row| integer(row, 0),
            )
            .optional()?)
    }

    pub(crate) fn check_playlist_entry_revision(
        &self,
        playlist_id: i64,
        expected: u64,
    ) -> Result<()> {
        ensure!(
            self.playlist_entry_revision(playlist_id)? == Some(expected),
            "Playlist entry revision changed"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rivu_protocol::model::AlbumKey;

    #[test]
    fn collection_read_snapshot_keeps_rows_and_versions_together_across_external_commit()
    -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("library.db");
        let store = Store::open(&path)?;
        store.conn.execute_batch("INSERT INTO tracks(id,path,raw_album,track_number) VALUES
            (1,'/music/one.wav','Album',1),(2,'/music/two.wav','Album',2),(3,'/music/child/three.wav','Album',0);")?;
        let key = AlbumKey {
            directory: "/music".into(),
            title: "Album".into(),
        };
        let before = store.album_revision()?;
        let snapshot = store.query_snapshot()?;
        assert_eq!(store.album_revision()?, before);
        let connection = rusqlite::Connection::open(&path)?;
        connection
            .execute_batch("UPDATE tracks SET track_number=3-track_number WHERE id IN(1,2)")?;
        let ids = || -> Result<Vec<i64>> {
            Ok(store
                .album_tracks_page(&key, None, 0, 20)?
                .rows
                .iter()
                .map(|row| row.id)
                .collect())
        };
        assert_eq!(ids()?, [1, 2]);
        assert_eq!(store.album_revision()?, before);
        snapshot.rollback()?;
        assert_eq!(ids()?, [2, 1]);
        assert_ne!(store.album_revision()?, before);
        Ok(())
    }
}
