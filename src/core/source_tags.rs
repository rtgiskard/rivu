use super::*;
use anyhow::ensure;
use rivu_format::tags::{TagEncoding, TagValues};

/// One explicit file operation per Core. Its worker outlives caller timeouts.
pub(in crate::core) struct SourceTagTask {
    pub(in crate::core) finished: oneshot::Receiver<SourceTagFinished>,
    pub(in crate::core) reply: Option<oneshot::Sender<CoreResponse>>,
    track: Track,
    expected: Option<TagValues>,
    worker: Option<async_executor::Task<()>>,
}

pub(in crate::core) struct PendingWrittenSource {
    track_id: i64,
    path: PathBuf,
    source: rivu_metadata::WrittenSource,
}

pub(in crate::core) struct SourceTagFinished {
    tags: Option<TagValues>,
    file_saved: bool,
    source: Option<rivu_metadata::WrittenSource>,
    result: Result<()>,
}

impl SourceTagFinished {
    pub(in crate::core) fn failed(error: anyhow::Error) -> Self {
        Self {
            tags: None,
            file_saved: false,
            source: None,
            result: Err(error),
        }
    }
}

impl SourceTagTask {
    fn join(&mut self) -> Result<()> {
        super::retire_optional_worker(
            &mut self.worker,
            "Source tag worker panicked; inspect the file before retrying",
        )
    }
}

impl Drop for SourceTagTask {
    fn drop(&mut self) {
        let _ = self.join();
    }
}

pub(super) fn track_tags(track: &Track) -> TagValues {
    TagValues {
        title: track.title.clone(),
        artist: track.artist.clone(),
        album: track.album.clone(),
        release_date: track.release_date.clone(),
        track_number: track.track_number,
        disc_number: track.disc_number,
    }
}

fn source_values(tags: rivu_metadata::Tags) -> TagValues {
    TagValues {
        title: tags.title.unwrap_or_default(),
        artist: tags.artist.unwrap_or_default(),
        album: tags.album.unwrap_or_default(),
        release_date: tags.release_date,
        track_number: tags.track_number,
        disc_number: tags.disc_number,
    }
}

impl Core {
    pub(in crate::core) fn start_source_tags(
        &mut self,
        track_id: i64,
        encoding: TagEncoding,
        save: Option<(TagValues, TagValues)>,
    ) -> Result<()> {
        ensure!(
            self.active_source_tags.is_none(),
            "A source tag operation is already running"
        );
        ensure!(
            self.active_scan.is_none()
                && self.active_external_admission.is_none()
                && self.active_relocation.is_none()
                && !self.database_maintenance_active(),
            "Library maintenance is in progress; source tags were not accessed"
        );
        let track = self.track(track_id)?;
        ensure!(
            track.cue.is_none(),
            "CUE segment tags belong to the CUE sheet, not the shared audio file"
        );
        ensure!(!track.missing, "Source file is missing");
        ensure!(
            save.is_none() || self.pending_written_source.is_none(),
            "A saved file is awaiting database synchronization; synchronize it before saving another file"
        );
        if let Some((_, expected)) = &save {
            ensure!(
                track_tags(&track) == *expected,
                "Database tags changed; reload the details before saving"
            );
        }
        let path = track.path.clone();
        let expected = save.as_ref().map(|(_, expected)| expected.clone());
        let (tx, finished) = oneshot::channel();
        let workers = self.workers.clone();
        let worker = self.executor.spawn(async move {
            let result = workers
                .execute(move || -> Result<SourceTagFinished> {
                    if let Some((tags, _)) = save {
                        let patch = rivu_metadata::TagPatch {
                            title: Some(tags.title),
                            artist: Some(tags.artist),
                            album: Some(tags.album),
                            release_date: Some(tags.release_date.unwrap_or_default()),
                            track_number: Some(tags.track_number),
                            disc_number: Some(tags.disc_number),
                        };
                        let (report, source) = rivu_metadata::edit_file_with_source(&path, &patch)?;
                        Ok(SourceTagFinished {
                            tags: Some(source_values(report.tags)),
                            file_saved: true,
                            source: Some(source),
                            result: Ok(()),
                        })
                    } else {
                        let mut file = rivu_utils::regular_file::open_regular_file(&path)?;
                        let tags = rivu_metadata::read_tags_with_encoding(&mut file, encoding)?
                            .context("Source tag reading is not supported for this container")?;
                        Ok(SourceTagFinished {
                            tags: Some(source_values(tags)),
                            file_saved: false,
                            source: None,
                            result: Ok(()),
                        })
                    }
                })
                .await;
            let finished = result
                .and_then(|result| result)
                .unwrap_or_else(SourceTagFinished::failed);
            let _ = tx.send(finished);
        });
        self.active_source_tags = Some(SourceTagTask {
            finished,
            reply: None,
            track,
            expected,
            worker: Some(worker),
        });
        Ok(())
    }

    pub(in crate::core) fn sync_track_tags(
        &mut self,
        track_id: i64,
        tags: &TagValues,
        expected: &TagValues,
    ) -> Result<()> {
        ensure!(
            track_tags(&self.track(track_id)?) == *expected,
            "Database tags changed; reload the details before synchronizing"
        );
        if let Some(pending) = &self.pending_written_source
            && pending.track_id == track_id
        {
            let track = self.track(track_id)?;
            ensure!(
                track.path == pending.path && track.cue.is_none(),
                "Saved source identity changed; inspect the file before synchronizing"
            );
            let changes = self
                .store
                .update_written_source(track_id, &pending.source)?;
            self.pending_written_source = None;
            self.accept_catalog_facts(changes, Some(&[track_id]), None);
            self.projections.current_track |= changes.library
                && self
                    .state
                    .current_track
                    .as_ref()
                    .is_some_and(|track| track.id == track_id);
        }
        // Only title/artist/album can require an audio-worker metadata command.
        if self.state.current_track.as_ref().is_some_and(|track| {
            track.id == track_id
                && (track.title != tags.title
                    || track.artist != tags.artist
                    || track.album != tags.album)
        }) {
            self.engine.ensure_command_capacity(1)?;
        }
        let impact = self.store.sync_track_tags(track_id, tags)?;
        let changes = crate::store::CatalogChanges {
            changed: impact.changed,
            library: impact.library,
            queue: impact.queue,
            albums: impact.albums,
            duplicate_members: impact.member_facts,
            history: impact.history,
            ..Default::default()
        };
        self.accept_catalog_facts(changes, Some(&[track_id]), impact.playlists.as_deref());
        if impact.library || self.projections.current_track {
            self.transport.history_pending |= changes.history;
            let current = self.refresh_affected_queue_tracks(&[track_id]);
            let catalog = self.refresh_catalog_projections();
            let errors: Vec<_> = [current, catalog]
                .into_iter()
                .filter_map(Result::err)
                .collect();
            if !errors.is_empty() {
                return Err(projections::ProjectionSyncError(errors).into());
            }
        }
        Ok(())
    }

    pub(in crate::core) fn finish_source_tags(&mut self, mut result: SourceTagFinished) {
        let Some(mut task) = self.active_source_tags.take() else {
            return;
        };
        if let Err(error) = task.join() {
            result.result = Err(error);
        }
        if result.result.is_ok() && result.file_saved {
            result.result = (|| {
                let track = self.track(task.track.id)?;
                ensure!(track.path == task.track.path && track.cue == task.track.cue,
                    "File saved, but the library source identity changed");
                self.pending_written_source = Some(PendingWrittenSource {
                    track_id: task.track.id,
                    path: task.track.path.clone(),
                    source: result.source.take().context("Saved file omitted verified source facts")?,
                });
                self.sync_track_tags(task.track.id,
                    result.tags.as_ref().context("Saved file omitted verified tags")?,
                    task.expected.as_ref().context("Saved file omitted database baseline")?)
            })().context("File tags saved, but synchronizing the database failed; retry database synchronization only");
        }
        if let Err(error) = &result.result {
            self.state.system.last_error = Some(format!("{error:#}"));
        }
        self.publish();
        let mut ack = Ack::from_result(result.result, self.state.system.revision);
        ack.source_tags = result.tags;
        ack.source_file_saved = result.file_saved;
        if let Some(reply) = task.reply.take() {
            let _ = reply.send(CoreResponse::Ack(ack));
        }
    }

    pub(in crate::core) fn shutdown_source_tags(&mut self) {
        let Some(task) = self.active_source_tags.as_mut() else {
            return;
        };
        let joined = task.join();
        let finished = match task.finished.try_recv() {
            Ok(Some(result)) => result,
            _ => SourceTagFinished::failed(anyhow::anyhow!(
                "Source tag worker ended without a result; inspect the file before retrying"
            )),
        };
        if let Err(error) = joined {
            self.finish_source_tags(SourceTagFinished::failed(error));
        } else {
            self.finish_source_tags(finished);
        }
    }

    /// No scan/relocation may publish stale source facts across an accepted write.
    pub(in crate::core) fn source_tag_control(&self, command: &Command) -> Result<()> {
        if let Some(pending) = &self.pending_written_source {
            let conflicting = match command {
                Command::Scan { .. }
                | Command::CheckOtherTracks { .. }
                | Command::QueueExternalPaths { .. }
                | Command::RelocateTrack { .. }
                | Command::OptimizeDatabase
                | Command::RemoveMissingTracks => true,
                Command::EditTrack { track_id, .. }
                | Command::KeepTrackIndependent { track_id } => *track_id == pending.track_id,
                Command::RemoveTracks { track_ids } => track_ids.contains(&pending.track_id),
                _ => false,
            };
            ensure!(
                !conflicting,
                "A saved file is awaiting database synchronization; synchronize it first"
            );
        }
        let Some(task) = &self.active_source_tags else {
            return Ok(());
        };
        if task.expected.is_none() {
            return Ok(());
        }
        let conflicting = match command {
            Command::Scan { .. }
            | Command::CheckOtherTracks { .. }
            | Command::QueueExternalPaths { .. }
            | Command::RelocateTrack { .. }
            | Command::OptimizeDatabase
            | Command::RemoveMissingTracks => true,
            Command::EditTrack { track_id, .. }
            | Command::SyncTrackTags { track_id, .. }
            | Command::KeepTrackIndependent { track_id } => *track_id == task.track.id,
            Command::RemoveTracks { track_ids } => track_ids.contains(&task.track.id),
            _ => false,
        };
        ensure!(
            !conflicting,
            "Source tags are being saved; this library operation was not executed"
        );
        Ok(())
    }
}
