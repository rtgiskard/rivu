use super::*;
use anyhow::ensure;

/// Exactly one export retains a read snapshot; caller timeout never retires it.
pub(in crate::core) struct ExportTask {
    pub(in crate::core) finished: Receiver<Result<()>>,
    pub(in crate::core) reply: Option<Sender<CoreResponse>>,
    cancellation: library::ScanCancellation,
    worker: Option<JoinHandle<()>>,
}

impl ExportTask {
    fn join(&mut self) -> Result<()> {
        if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| {
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

fn export_snapshot(
    database: &Path,
    playlist_id: i64,
    path: &Path,
    cancellation: &library::ScanCancellation,
) -> Result<()> {
    let store = Store::open_export(database, playlist_id)?;
    let mut after = None;
    let mut rows = Vec::new().into_iter();
    let mut done = false;
    let tracks = std::iter::from_fn(|| {
        loop {
            if let Some(track) = rows.next() {
                return Some(Ok(track));
            }
            if done {
                return None;
            }
            if let Err(error) = cancellation.check() {
                done = true;
                return Some(Err(error));
            }
            let batch = match store.playlist_export_batch(playlist_id, after, MAX_QUERY_ROWS) {
                Ok(batch) => batch,
                Err(error) => {
                    done = true;
                    return Some(Err(error));
                }
            };
            after = batch.next;
            done = after.is_none();
            rows = batch.rows.into_iter();
        }
    });
    library::export_m3u(path, tracks, cancellation)
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
        let (finished_tx, finished) = bounded(1);
        let worker = thread::Builder::new()
            .name("rivu-playlist-export".into())
            .spawn(move || {
                let started = Instant::now();
                let result = export_snapshot(&database, playlist_id, &path, &worker_cancellation);
                tracing::debug!(
                    elapsed_ms = started.elapsed().as_millis(),
                    success = result.is_ok(),
                    "playlist_export_finished"
                );
                // A single completion fits even when shutdown is joining this worker.
                let _ = finished_tx.try_send(result);
            })?;
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
    ) -> (Option<Sender<CoreResponse>>, Result<()>) {
        let Some(mut task) = self.active_export.take() else {
            return (None, Err(anyhow::anyhow!("No playlist export is running")));
        };
        let result = task.join().and(result);
        (task.reply.take(), result)
    }

    pub(in crate::core) fn shutdown_export(
        &mut self,
    ) -> Option<(Option<Sender<CoreResponse>>, Result<()>)> {
        let task = self.active_export.as_mut()?;
        task.cancellation.cancel();
        let result = task.join().and_then(|()| {
            task.finished.recv().unwrap_or_else(|_| Err(anyhow::anyhow!(
                "Playlist export worker ended without a result; inspect the destination before retrying"
            )))
        });
        Some(self.finish_export(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
