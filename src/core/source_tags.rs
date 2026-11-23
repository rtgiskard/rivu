use super::*;
use anyhow::ensure;
use rivu_format::tags::{TagEncoding, TagValues};
use rivu_protocol::model::SourceTagRecovery;

const SAVED_SOURCE_SYNC_FAILED: &str =
    "File tags saved, but synchronizing the database failed; retry database synchronization only";

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
    tags: TagValues,
    expected: TagValues,
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

fn source_values(tags: rivu_metadata::Tags) -> TagValues {
    TagValues {
        title: tags.title.unwrap_or_default(),
        artist: tags.artist.unwrap_or_default(),
        album: tags.album.unwrap_or_default(),
        release_date: tags.release_date,
        track_number: tags.track_number,
        disc_number: tags.disc_number,
        replaygain_track_gain: tags.replaygain_track_gain,
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
                track.tag_view() == expected.as_view(),
                "Database tags changed; reload the details before saving"
            );
        }
        let path = track.path.clone();
        let (tags, expected) = save.unzip();
        let (tx, finished) = oneshot::channel();
        let workers = self.workers.clone();
        let worker = self.executor.spawn(async move {
            let result = workers
                .execute(move || -> Result<SourceTagFinished> {
                    if let Some(tags) = tags {
                        let patch = rivu_metadata::TagPatch {
                            title: Some(tags.title),
                            artist: Some(tags.artist),
                            album: Some(tags.album),
                            release_date: Some(tags.release_date.unwrap_or_default()),
                            track_number: Some(tags.track_number),
                            disc_number: Some(tags.disc_number),
                            replaygain_track_gain: Some(tags.replaygain_track_gain),
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
    ) -> Result<Option<String>> {
        let stored = self.track(track_id)?;
        self.preflight_tag_sync(&stored, tags, expected)?;
        let impact = self.store.sync_track_tags(track_id, tags)?;
        Ok(self.refresh_tag_commit(track_id, impact, false))
    }

    fn preflight_tag_sync(
        &self,
        stored: &Track,
        tags: &TagValues,
        expected: &TagValues,
    ) -> Result<()> {
        ensure!(
            tags.replaygain_track_gain.is_none_or(f64::is_finite),
            "ReplayGain must be finite"
        );
        ensure!(
            stored.tag_view() == expected.as_view(),
            "Database tags changed; reload the details before synchronizing"
        );
        // Reject a busy live metadata update before persisting tags or saved source facts.
        if self
            .transport
            .session
            .as_ref()
            .is_some_and(|playback| playback.loaded_track.id == stored.id)
            && self.state.current_track.as_ref().is_some_and(|track| {
                track.id == stored.id
                    && (track.title != tags.title
                        || track.artist != tags.artist
                        || track.album != tags.album
                        || track.replaygain_track_gain != tags.replaygain_track_gain)
            })
        {
            self.engine.ensure_command_capacity(1)?;
        }
        Ok(())
    }

    pub(in crate::core) fn commit_saved_tags(
        &mut self,
        track_id: i64,
        receipt_tags: Option<&TagValues>,
    ) -> Result<(TagValues, Option<String>)> {
        let pending = self
            .pending_written_source
            .as_ref()
            .filter(|pending| pending.track_id == track_id)
            .context("This track has no saved file awaiting database synchronization")?;
        ensure!(
            receipt_tags.is_none_or(|tags| *tags == pending.tags),
            "Recovery tags do not match the verified file save"
        );
        match self.synchronize_saved_tags() {
            Ok(impact) => {
                let tags = self
                    .pending_written_source
                    .take()
                    .expect("committed save")
                    .tags;
                if let Some(recovery) = self.state.system.source_tag_recovery.take()
                    && self
                        .state
                        .system
                        .last_error
                        .as_deref()
                        .and_then(|error| error.split_once(": "))
                        == Some((SAVED_SOURCE_SYNC_FAILED, recovery.error.as_str()))
                {
                    self.state.system.last_error = None;
                }
                // Source facts can invalidate the current track even when tags are unchanged.
                let warning = self.refresh_tag_commit(track_id, impact, true);
                Ok((tags, warning))
            }
            Err(error) => {
                let reason = format!("{error:#}");
                let recovery = &mut self.state.system.source_tag_recovery;
                if recovery.as_ref().is_none_or(|save| save.error != reason) {
                    let pending = self.pending_written_source.as_ref().expect("pending save");
                    *recovery = Some(Arc::new(SourceTagRecovery {
                        track_id,
                        tags: pending.tags.clone(),
                        error: reason,
                    }));
                }
                Err(error.context(SAVED_SOURCE_SYNC_FAILED))
            }
        }
    }

    fn synchronize_saved_tags(&mut self) -> Result<crate::store::TrackEditImpact> {
        let pending = self.pending_written_source.as_ref().expect("pending save");
        let track_id = pending.track_id;
        let stored = self.track(track_id)?;
        ensure!(
            stored.path == pending.path && stored.cue.is_none(),
            "Saved source identity changed; inspect the file before synchronizing"
        );
        self.preflight_tag_sync(&stored, &pending.tags, &pending.expected)?;
        let (changes, ids) = self
            .store
            .update_written_source(track_id, &pending.source)?;
        let impact = self.store.sync_track_tags(track_id, &pending.tags);
        // Source facts are already committed even if the tag write failed.
        self.accept_catalog_facts(changes, Some(&ids), None);
        self.projections.current_track |= changes.library
            && self
                .state
                .current_track
                .as_ref()
                .is_some_and(|track| ids.contains(&track.id));
        impact
    }

    fn refresh_tag_commit(
        &mut self,
        track_id: i64,
        impact: crate::store::TrackEditImpact,
        file_saved: bool,
    ) -> Option<String> {
        let error = self
            .finish_track_edit(
                track_id,
                impact,
                file_saved && self.projections.current_track,
            )
            .err()?;
        let committed = if file_saved {
            "File and database saved"
        } else {
            "Database tags saved"
        };
        let warning = format!("{committed}; refresh failed: {error:#}");
        self.state.system.last_error = Some(warning.clone());
        Some(warning)
    }

    pub(in crate::core) fn finish_source_tags(&mut self, mut result: SourceTagFinished) {
        let Some(mut task) = self.active_source_tags.take() else {
            return;
        };
        if let Err(error) = task.join() {
            result.result = Err(error);
        }
        let mut refresh_error = None;
        if result.result.is_ok() && result.file_saved {
            let track_id = task.track.id;
            self.pending_written_source = Some(PendingWrittenSource {
                track_id,
                path: task.track.path.clone(),
                source: result.source.take().expect("verified file source facts"),
                tags: result.tags.take().expect("verified file tags"),
                expected: task.expected.take().expect("save database baseline"),
            });
            match self.commit_saved_tags(track_id, None) {
                Ok((tags, warning)) => {
                    result.tags = Some(tags);
                    refresh_error = warning;
                }
                Err(error) => {
                    result.tags = self
                        .pending_written_source
                        .as_ref()
                        .map(|save| save.tags.clone());
                    result.result = Err(error);
                }
            }
        }
        if let Err(error) = &result.result {
            self.state.system.last_error = Some(format!("{error:#}"));
        }
        self.publish();
        if let Some(reply) = task.reply.take() {
            let source_tags = (result.tags.is_some()
                || result.file_saved
                || refresh_error.is_some())
            .then_some(SourceTagOutput {
                tags: result.tags,
                file_saved: result.file_saved,
                refresh_error,
            });
            let outcome = match result.result {
                Ok(()) => CommandOutcome::Succeeded(
                    source_tags.map_or(CommandOutput::None, CommandOutput::SourceTags),
                ),
                Err(error) => CommandOutcome::Failed {
                    error: format!("{error:#}"),
                    source_tags,
                },
            };
            let ack = CommandReceipt {
                revision: self.state.system.revision,
                outcome,
            };
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

    pub(in crate::core) fn ensure_source_tag_scan_allowed(&self) -> Result<()> {
        ensure!(
            self.pending_written_source.is_none(),
            "A saved file is awaiting database synchronization; synchronize it first"
        );
        ensure!(
            !self
                .active_source_tags
                .as_ref()
                .is_some_and(|task| task.expected.is_some()),
            "Source tags are being saved; this library operation was not executed"
        );
        Ok(())
    }

    /// No conflicting catalog operation may publish stale facts across an accepted write.
    pub(in crate::core) fn source_tag_control(&self, command: &Command) -> Result<()> {
        if let Some(pending) = &self.pending_written_source {
            let conflicting = match command {
                Command::QueueExternalPaths { .. }
                | Command::RelocateTrack { .. }
                | Command::OptimizeDatabase
                | Command::RemoveMissingTracks => true,
                Command::EditTrack { track_id, .. }
                | Command::SetReplayGain { track_id, .. }
                | Command::SyncTrackTags { track_id, .. }
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
            Command::QueueExternalPaths { .. }
            | Command::RelocateTrack { .. }
            | Command::OptimizeDatabase
            | Command::RemoveMissingTracks => true,
            Command::EditTrack { track_id, .. }
            | Command::SetReplayGain { track_id, .. }
            | Command::SyncTrackTags { track_id, .. }
            | Command::RetrySourceTagSync { track_id, .. }
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
