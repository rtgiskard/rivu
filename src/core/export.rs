use super::*;
use anyhow::ensure;

/// Exactly one export retains a read snapshot; caller timeout never retires it.
pub(in crate::core) struct ExportTask {
    pub(in crate::core) finished: oneshot::Receiver<Result<()>>,
    pub(in crate::core) reply: Option<oneshot::Sender<CoreResponse>>,
    cancellation: library::ScanCancellation,
    worker: Option<tokio::task::JoinHandle<()>>,
}

impl ExportTask {
    fn join(&mut self) -> Result<()> {
        if let Some(worker) = self.worker.take() {
            futures::executor::block_on(worker).map_err(|_| {
                anyhow::anyhow!(
                    "Playlist export worker panicked; inspect the destination before retrying"
                )
            })?;
        }
        Ok(())
    }
}

impl Drop for ExportTask {
    fn drop(&mut self) {
        self.cancellation.cancel();
        let _ = self.join();
    }
}

struct ExportSource {
    store: Option<Store>,
    playlist_id: i64,
    after: Option<PlaylistEntryCursor>,
    rows: std::vec::IntoIter<PlaylistExportTrack>,
    done: bool,
    cancellation: library::ScanCancellation,
}

impl ExportSource {
    async fn next(&mut self) -> Option<Result<PlaylistExportTrack>> {
        loop {
            if let Err(error) = self.cancellation.check() {
                return Some(Err(error));
            }
            if let Some(track) = self.rows.next() {
                return Some(Ok(track));
            }
            if self.done {
                return None;
            }
            let store = self.store.take()?;
            let playlist_id = self.playlist_id;
            let after = self.after;
            // The same connection/read transaction moves through short bounded
            // queries. Never hold a blocking worker while awaiting file writes.
            let batch = tokio::task::spawn_blocking(move || {
                let batch = store.playlist_export_batch(playlist_id, after, MAX_QUERY_ROWS);
                (store, batch)
            })
            .await;
            let (store, batch) = match batch {
                Ok(batch) => batch,
                Err(error) => return Some(Err(error.into())),
            };
            self.store = Some(store);
            match batch {
                Ok(batch) => {
                    self.after = batch.next;
                    self.done = batch.next.is_none();
                    self.rows = batch.rows.into_iter();
                }
                Err(error) => return Some(Err(error)),
            }
        }
    }
}

async fn export_snapshot(
    database: &Path,
    playlist_id: i64,
    path: &Path,
    cancellation: &library::ScanCancellation,
) -> Result<()> {
    cancellation.check()?;
    let database = database.to_path_buf();
    let source_cancellation = cancellation.clone();
    // Admission and the first bounded query share one blocking dispatch.
    let mut source = tokio::task::spawn_blocking(move || -> Result<ExportSource> {
        source_cancellation.check()?;
        let store = Store::open_export(&database, playlist_id)?;
        let batch = store.playlist_export_batch(playlist_id, None, MAX_QUERY_ROWS)?;
        Ok(ExportSource {
            store: Some(store),
            playlist_id,
            after: batch.next,
            rows: batch.rows.into_iter(),
            done: batch.next.is_none(),
            cancellation: source_cancellation,
        })
    })
    .await
    .context("Playlist export snapshot worker panicked")??;
    let tracks = futures::stream::unfold(&mut source, |source| async move {
        source.next().await.map(|track| (track, source))
    });
    let result = std::panic::AssertUnwindSafe(library::export_m3u(path, tracks, cancellation))
        .catch_unwind()
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("Playlist export worker panicked")));
    // Close the pinned read transaction off the driver, including after unwind.
    let store = source.store.take();
    let retired = tokio::task::spawn_blocking(move || drop(store))
        .await
        .context("Playlist export snapshot retirement panicked");
    retired.and(result)
}

impl Core {
    pub(in crate::core) fn export_playlist(&mut self, playlist_id: i64, path: &Path) -> Result<()> {
        ensure!(
            self.active_export.is_none(),
            "A playlist export is already running"
        );
        ensure!(
            !self.database_maintenance_active(),
            "Database maintenance is in progress"
        );
        let database = self.store.database_path()?;
        let path = path.to_path_buf();
        let cancellation = library::ScanCancellation::default();
        let worker_cancellation = cancellation.clone();
        let (finished_tx, finished) = oneshot::channel();
        let executor = self.executor.clone();
        let worker = self.executor.spawn(async move {
            let started = Instant::now();
            let job = executor.spawn(async move {
                export_snapshot(&database, playlist_id, &path, &worker_cancellation).await
            });
            // Joining, rather than a catch_unwind around a future constructor,
            // observes panics while polling and guarantees resources have ended.
            let result = job.await.unwrap_or_else(|_| {
                Err(anyhow::anyhow!(
                    "Playlist export worker panicked; inspect the destination before retrying"
                ))
            });
            tracing::debug!(
                elapsed_ms = started.elapsed().as_millis(),
                success = result.is_ok(),
                "playlist_export_finished"
            );
            let _ = finished_tx.send(result);
        });
        self.active_export = Some(ExportTask {
            finished,
            reply: None,
            cancellation,
            worker: Some(worker),
        });
        Ok(())
    }

    pub(in crate::core) fn finish_export(
        &mut self,
        result: Result<()>,
    ) -> (Option<oneshot::Sender<CoreResponse>>, Result<()>) {
        let Some(mut task) = self.active_export.take() else {
            return (None, Err(anyhow::anyhow!("No playlist export is running")));
        };
        let result = task.join().and(result);
        (task.reply.take(), result)
    }

    pub(in crate::core) fn shutdown_export(
        &mut self,
    ) -> Option<(Option<oneshot::Sender<CoreResponse>>, Result<()>)> {
        let task = self.active_export.as_mut()?;
        task.cancellation.cancel();
        let result = task.join().and_then(|()| {
            task.finished.try_recv().unwrap_or_else(|_| Err(anyhow::anyhow!(
                "Playlist export worker ended without a result; inspect the destination before retrying"
            )))
        });
        Some(self.finish_export(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn export_snapshot(
        database: &Path,
        playlist_id: i64,
        path: &Path,
        cancellation: &library::ScanCancellation,
    ) -> Result<()> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(super::export_snapshot(
                database,
                playlist_id,
                path,
                cancellation,
            ))
    }

    #[test]
    fn empty_playlist_exports_but_unknown_and_orphaned_playlists_preserve_destination() -> Result<()>
    {
        let directory = tempfile::tempdir()?;
        let database = directory.path().join("library.db");
        let store = Store::open(&database)?;
        let id = store.create_playlist("Empty")?;
        let path = directory.path().join("playlist.m3u8");
        let cancellation = library::ScanCancellation::default();
        std::fs::write(&path, b"original\n")?;
        let error = export_snapshot(&database, i64::MAX, &path, &cancellation).unwrap_err();
        assert!(error.to_string().contains("Playlist not found"));
        assert_eq!(std::fs::read(&path)?, b"original\n");

        export_snapshot(&database, id, &path, &cancellation)?;
        assert_eq!(std::fs::read(&path)?, b"#EXTM3U\n");

        let writer = rusqlite::Connection::open(&database)?;
        writer.execute(
            "INSERT INTO playlist_entries(playlist_id,track_id,position) VALUES(?1,999,0)",
            [id],
        )?;
        let error = export_snapshot(&database, id, &path, &cancellation).unwrap_err();
        assert!(error.to_string().contains("missing library track"));
        assert_eq!(std::fs::read(&path)?, b"#EXTM3U\n");
        Ok(())
    }

    #[test]
    fn export_projection_writes_overrides_and_complete_cue_runs() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let database = directory.path().join("library.db");
        let store = Store::open(&database)?;
        let song = directory.path().join("#song.wav");
        let audio = directory.path().join("audio.wav");
        let sheet = directory.path().join("album.cue");
        std::fs::write(&song, b"playlist fixture")?;
        std::fs::write(&audio, b"playlist fixture")?;
        std::fs::write(
            &sheet,
            "FILE \"audio.wav\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n  TRACK 02 AUDIO\n    INDEX 01 00:01:00\n",
        )?;
        let writer = rusqlite::Connection::open(&database)?;
        writer.execute(
            "INSERT INTO tracks(id,path,raw_title,title_override,raw_artist,artist_override,duration)
             VALUES(1,?1,'Raw title','Chosen'||char(13)||char(10)||'Title','Raw artist','',1.6)",
            [song.to_str().unwrap()],
        )?;
        writer.execute(
            "INSERT INTO tracks(id,path,cue_sheet,cue_number,cue_start_frame,cue_end_frame)
             VALUES(2,?1,?2,1,0,75),(3,?1,?2,2,75,NULL)",
            rusqlite::params![audio.to_str().unwrap(), sheet.to_str().unwrap()],
        )?;
        let id = store.create_playlist_with_tracks("Projection", &[1, 2, 3])?;
        let path = directory.path().join("playlist.m3u8");
        export_snapshot(&database, id, &path, &library::ScanCancellation::default())?;
        assert_eq!(
            std::fs::read_to_string(&path)?,
            "#EXTM3U\n#EXTINF:2,Chosen  Title\n./#song.wav\nalbum.cue\n"
        );
        let imported = library::import_playlist(&path)?;
        assert_eq!(imported.len(), 3);
        assert_eq!(imported[0].path, song);
        assert_eq!(imported[0].cue_track, None);
        assert_eq!(imported[1].path, sheet);
        assert_eq!(imported[1].cue_track, Some(1));
        assert_eq!(imported[2].path, sheet);
        assert_eq!(imported[2].cue_track, Some(2));
        Ok(())
    }

    #[tokio::test]
    async fn async_pages_keep_one_snapshot_across_concurrent_playlist_edits() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let database = directory.path().join("library.db");
        let store = Store::open(&database)?;
        let playlist_id = store.create_playlist("Paged snapshot")?;
        let total = MAX_QUERY_ROWS + 3;
        let writer = rusqlite::Connection::open(&database)?;
        writer.execute(
            "WITH RECURSIVE ids(id) AS (SELECT 1 UNION ALL SELECT id+1 FROM ids WHERE id<?1)
             INSERT INTO tracks(id,path,raw_title) SELECT id,'/music/'||id||'.wav','Track '||id FROM ids",
            [total as i64],
        )?;
        writer.execute(
            "INSERT INTO playlist_entries(playlist_id,track_id,position) SELECT ?1,id,id-1 FROM tracks",
            [playlist_id],
        )?;
        let snapshot =
            tokio::task::spawn_blocking(move || Store::open_export(&database, playlist_id))
                .await??;
        let mut source = ExportSource {
            store: Some(snapshot),
            playlist_id,
            after: None,
            rows: Vec::new().into_iter(),
            done: false,
            cancellation: library::ScanCancellation::default(),
        };
        assert_eq!(source.next().await.unwrap()?.title, "Track 1");
        writer.execute(
            "UPDATE tracks SET raw_title='Edited' WHERE id=?1",
            [total as i64],
        )?;
        writer.execute(
            "DELETE FROM playlist_entries WHERE playlist_id=?1 AND track_id=?2",
            rusqlite::params![playlist_id, total as i64],
        )?;
        for id in 2..=total {
            let track = source.next().await.context("snapshot lost a row")??;
            assert_eq!(track.path, PathBuf::from(format!("/music/{id}.wav")));
            assert_eq!(track.title, format!("Track {id}"));
        }
        assert!(source.next().await.is_none());
        tokio::task::spawn_blocking(move || drop(source)).await?;
        Ok(())
    }
}
