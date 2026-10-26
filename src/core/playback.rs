use super::*;
use anyhow::bail;

fn modified_ns(metadata: &std::fs::Metadata) -> Option<i64> {
    let modified = metadata.modified().ok()?;
    let ns = match modified.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos() as i128,
        Err(error) => -(error.duration().as_nanos() as i128),
    };
    i64::try_from(ns).ok().filter(|ns| *ns != i64::MIN)
}

fn same_source_file(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    if !after.is_file()
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
    {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec()
    }
    #[cfg(not(unix))]
    {
        before.created().ok() == after.created().ok()
    }
}

impl Core {
    pub(in crate::core) fn playback_metadata(track: &Track) -> Arc<audio::PlaybackMetadata> {
        Arc::new(audio::PlaybackMetadata {
            title: track.title.clone(),
            artist: track.artist.clone(),
            album: track.album.clone(),
        })
    }

    pub(in crate::core) fn audio_event(&mut self, event: AudioEvent) -> Result<()> {
        match event {
            AudioEvent::Started { generation, info } if generation == self.generation => {
                if let Err(error) = self.playback_started(info) {
                    self.stop().with_context(|| format!("{error:#}"))?;
                    return Err(error);
                }
            }
            AudioEvent::Progress {
                generation,
                position_seconds,
                listened_seconds,
            } if generation == self.generation => {
                self.playback_progress(position_seconds, listened_seconds)?;
            }
            AudioEvent::Ended {
                generation,
                bitrate_bps,
            } if generation == self.generation => {
                self.playback_ended_bitrate(bitrate_bps)?;
                let status = self.state.playback.status;
                self.stop_playback(true)?;
                self.advance_with_status(true, status)?;
            }
            AudioEvent::SeekRejected {
                revision,
                snapshot,
                message,
            } if snapshot.generation == self.generation
                && revision == self.state.playback.seek_revision =>
            {
                self.state.playback.position = snapshot.position_seconds;
                // Publish the correction to clock consumers without reusing an intent ID.
                self.state.playback.seek_revision = revision.wrapping_add(1);
                self.state.system.last_error = Some(message);
                self.playback_progress(snapshot.position_seconds, snapshot.listened_seconds)?;
            }
            AudioEvent::Failed {
                generation,
                message,
            } if generation == self.generation => {
                self.stop().with_context(|| message.clone())?;
                self.state.system.last_error = Some(message);
            }
            AudioEvent::OutputSettingsFailed {
                request,
                device,
                auto_mix,
                message,
            } if request == self.output_settings_request
                && device == self.state.system.selected_device
                && auto_mix == self.state.system.config.pipewire_auto_mix =>
            {
                self.state.system.last_error = Some(message);
            }
            AudioEvent::EqualizerSettingsFailed {
                generation,
                config,
                message,
            } if generation.is_none_or(|value| value == self.generation)
                && *config == self.state.system.config.equalizer =>
            {
                self.state.system.last_error = Some(message);
            }
            AudioEvent::DecoderStopped { generation } if generation == self.generation => {
                // The worker has already stopped and may still be sending
                // events while handling Configure; do not wait on a command.
                self.finish_playback(Ok(None), false, Ok(()))?;
            }
            _ => {}
        }
        Ok(())
    }
    pub(in crate::core) fn playback_source(
        &self,
        track: &Track,
    ) -> Result<Option<(crate::store::PlaybackSource, std::fs::Metadata)>> {
        if track.bitrate_bps.is_some() && track.bitrate_exact {
            return Ok(None);
        }
        let Some(source) = self.store.playback_source(track)? else {
            return Ok(None);
        };
        let Ok(metadata) = std::fs::metadata(&source.path) else {
            return Ok(None);
        };
        Ok((metadata.is_file()
            && source.file_size == metadata.len()
            && Some(source.modified_ns) == modified_ns(&metadata))
        .then_some((source, metadata)))
    }

    fn persist_playback_bitrate(&mut self, bitrate: u64, exact: bool) -> bool {
        let Some(playback) = self.playback.as_ref() else {
            return false;
        };
        let track_id = playback.track_id;
        let Some((source, before)) = playback.source.as_ref() else {
            return false;
        };
        if self.database_maintenance_active() {
            if exact
                || !self
                    .pending_bitrate
                    .is_some_and(|(_, pending_exact)| pending_exact)
            {
                self.pending_bitrate = Some((bitrate, exact));
            }
            self.playback_write_deferred = true;
            return false;
        }
        if !std::fs::metadata(&source.path).is_ok_and(|after| same_source_file(before, &after)) {
            return false;
        }
        let changed = match self.store.upgrade_bitrate(source, bitrate, exact) {
            Ok(changed) => changed,
            Err(error) => {
                tracing::warn!(error = ?error, "playback_bitrate_not_saved");
                return false;
            }
        };
        if !changed {
            return false;
        }
        let library = &mut self.state.library;
        library.bitrate_revision = library.bitrate_revision.wrapping_add(1);
        if let Some(current) = self.state.current_track.as_mut()
            && current.id == track_id
        {
            let current = Arc::make_mut(current);
            current.bitrate_bps = Some(bitrate);
            current.bitrate_exact = exact;
        }
        true
    }

    pub(in crate::core) fn playback_ended_bitrate(&mut self, bitrate: Option<u64>) -> Result<()> {
        // Only the worker's actual full-source EOF can supply this value.
        if self
            .playback
            .as_ref()
            .is_some_and(|playback| playback.started)
            && self
                .state
                .current_track
                .as_ref()
                .is_some_and(|track| track.cue.is_none())
            && let Some(bitrate) = bitrate.filter(|bitrate| *bitrate > 0)
        {
            self.persist_playback_bitrate(bitrate, true);
        }
        Ok(())
    }

    pub(in crate::core) fn start(&mut self, queue_id: u64, record_history: bool) -> Result<()> {
        self.start_with_status(queue_id, record_history, PlaybackStatus::Playing)
    }
    pub(in crate::core) fn start_with_status(
        &mut self,
        queue_id: u64,
        record_history: bool,
        status: PlaybackStatus,
    ) -> Result<()> {
        let track = self.track(self.store.queue_track_id(queue_id)?)?;
        if status != PlaybackStatus::Stopped && !track.path.is_file() {
            bail!("Missing audio file: {}", track.path.display());
        }
        // Independent controls do not depend on database recovery. Only a new
        // playback owner must settle the previous producer before replacing it.
        if self.retired_playback.is_some() {
            self.flush_deferred_playback()?;
        }
        self.stop()?;
        // Retiring Started/Ended bookkeeping can enrich this same track in the store.
        // Keep the missing-file preflight above, but load the authoritative snapshot now.
        let track = self.track(track.id)?;
        if status != PlaybackStatus::Stopped {
            let source = self.playback_source(&track).unwrap_or_else(|error| {
                tracing::warn!(error = ?error, "playback_bitrate_identity_unavailable");
                None
            });
            self.engine.send(AudioCommand::Load {
                path: track.path.clone(),
                decoder_hint: track.decoder_hint,
                metadata: Self::playback_metadata(&track),
                range: track.cue.as_ref().map(|cue| audio::PlaybackRange {
                    start_seconds: cue.start_seconds(),
                    end_seconds: cue.end_seconds(),
                }),
                generation: self.generation,
                start_seconds: 0.0,
                paused: status == PlaybackStatus::Paused,
            })?;
            self.playback = Some(PlaybackStats {
                track_id: track.id,
                source,
                heard: 0.0,
                position: None,
                started: false,
                counted: false,
                last_heard_at: None,
                activity_updated: false,
                paused_at: (status == PlaybackStatus::Paused).then(now),
            });
        }
        self.state.playback.last_heard_at = None;
        self.state.queue.current_id = Some(queue_id);
        self.state.playback.duration = track.duration;
        self.state.current_track = Some(Arc::new(track));
        self.state.playback.status = status;
        self.state.system.last_error = None;
        if record_history {
            self.record_queue_history(queue_id);
        }
        self.save_queue()
    }
    pub(in crate::core) fn count_play(&mut self) -> Result<()> {
        if self.database_maintenance_active() {
            self.playback_write_deferred = true;
            return Ok(());
        }
        let Some(duration) = self
            .state
            .playback
            .duration
            .filter(|duration| duration.is_finite() && *duration > 0.0)
        else {
            return Ok(());
        };
        let Some(track_id) = self.playback.as_ref().and_then(|playback| {
            (playback.started
                && !playback.counted
                && playback.heard
                    > duration * self.state.system.config.play_count_threshold_percent / 100.0)
                .then_some(playback.track_id)
        }) else {
            return Ok(());
        };
        let stats = self.store.increment_play_count(track_id)?;
        if let Some(playback) = self.playback.as_mut() {
            playback.counted = true;
        }
        self.update_track_stats(track_id, stats)
    }
    pub(in crate::core) fn playback_started(&mut self, info: MediaInfo) -> Result<()> {
        let track_id = self.playback.as_ref().map(|playback| playback.track_id);
        let Some(track_id) = track_id else {
            return Ok(());
        };
        if self
            .playback
            .as_ref()
            .is_some_and(|playback| playback.started)
        {
            return Ok(());
        }
        let played_at = now();
        let played_at = self
            .playback
            .as_ref()
            .and_then(|playback| playback.paused_at)
            .map_or(played_at, |paused_at| played_at.min(paused_at));
        let stats = if self.database_maintenance_active() {
            self.playback_write_deferred = true;
            None
        } else {
            match self.store.mark_played(track_id, played_at) {
                Ok(stats) => Some(stats),
                Err(error) => {
                    // A rejected start cannot be revived by a later output snapshot.
                    self.playback = None;
                    return Err(error);
                }
            }
        };
        let playback = self.playback.as_mut().expect("playback was checked above");
        playback.started = true;
        playback.last_heard_at = Some(played_at);
        if stats.is_none() {
            playback.activity_updated = true;
        }
        self.state.playback.last_heard_at = Some(played_at);
        self.state.playback.duration = info
            .duration
            .filter(|duration| duration.is_finite() && *duration > 0.0)
            .or(self
                .state
                .playback
                .duration
                .filter(|duration| duration.is_finite() && *duration > 0.0));
        let result = (|| -> Result<()> {
            if let Some(stats) = stats {
                self.playback_history_pending |= stats.changed;
                self.update_track_stats(track_id, stats)?;
                self.refresh_playback_history().context(
                    "Playback start committed, but refreshing history failed; previous history retained",
                )?;
            }
            self.count_play()
        })();
        // Persist bitrate independently even if a projection refresh failed
        // after the playback statistics committed.
        if let Some(bitrate) = info.bitrate_bps.filter(|bitrate| *bitrate > 0) {
            self.persist_playback_bitrate(bitrate, info.bitrate_exact);
        }
        result
    }
    pub(in crate::core) fn playback_progress(&mut self, position: f64, heard: f64) -> Result<()> {
        if let Some(playback) = &mut self.playback
            && playback.started
        {
            if position.is_finite() {
                self.state.playback.position = position;
                playback.position = Some(position);
            }
            if heard.is_finite() && heard > playback.heard {
                let heard_at = playback
                    .paused_at
                    .map_or_else(now, |paused_at| now().min(paused_at));
                if playback.record_heard(heard, heard_at) {
                    self.state.playback.last_heard_at = playback.last_heard_at;
                }
            }
            self.count_play()?;
        }
        Ok(())
    }
    pub(in crate::core) fn retiring_audio_event(
        &mut self,
        generation: u64,
        event: AudioEvent,
    ) -> Result<()> {
        match event {
            AudioEvent::Started {
                generation: event_generation,
                info,
            } if event_generation == generation => self.playback_started(info)?,
            AudioEvent::Progress {
                generation: event_generation,
                position_seconds,
                listened_seconds,
            } if event_generation == generation => {
                self.playback_progress(position_seconds, listened_seconds)?;
            }
            AudioEvent::Ended {
                generation: event_generation,
                bitrate_bps,
            } if event_generation == generation => self.playback_ended_bitrate(bitrate_bps)?,
            // Retiring terminal events cannot advance, recursively stop, or revive playback.
            _ => {}
        }
        Ok(())
    }

    pub(in crate::core) fn drain_playback_events(&mut self, generation: u64) -> Result<()> {
        let mut result = Ok(());
        while let Ok(event) = self
            .engine
            .events
            .as_mut()
            .expect("Core owns audio events")
            .try_recv()
        {
            result = result.and(self.retiring_audio_event(generation, event));
        }
        result
    }
    pub(in crate::core) fn finish_playback(
        &mut self,
        snapshot: Result<Option<audio::AudioSnapshot>>,
        natural: bool,
        bookkeeping: Result<()>,
    ) -> Result<()> {
        let generation = self.generation;
        // A request can beat a queued Started even though the worker has already
        // played audio. Acknowledged stops make that queued bookkeeping safe to
        // consume before generation fencing; errors still fence below.
        let bookkeeping = if snapshot.is_ok() {
            let drained = self.drain_playback_events(generation);
            bookkeeping.and(drained)
        } else {
            bookkeeping
        };
        // Fence queued events even when stopping or saving the count fails.
        self.generation = self.generation.wrapping_add(1);
        self.processing.audition = None;
        self.processing.audition_generation = None;
        self.state.processing_audition = None;
        self.state.playback.status = PlaybackStatus::Stopped;
        self.state.playback.position = 0.0;
        self.state.playback.seek_revision = self.state.playback.seek_revision.wrapping_add(1);
        let (snapshot, stopped) = match snapshot {
            Ok(snapshot) => (snapshot, Ok(())),
            Err(error) => (None, Err(error)),
        };
        if let Some(playback) = &mut self.playback {
            if let Some(snapshot) = snapshot
                && snapshot.generation == generation
                && snapshot.listened_seconds.is_finite()
            {
                // Late paused confirmations still count as heard audio, but
                // their activity timestamp cannot cross the pause boundary.
                let heard_at = playback
                    .paused_at
                    .map_or_else(now, |paused_at| now().min(paused_at));
                if playback.record_heard(snapshot.listened_seconds, heard_at) {
                    self.state.playback.last_heard_at = playback.last_heard_at;
                }
            }
            if natural
                && !self
                    .state
                    .playback
                    .duration
                    .is_some_and(|duration| duration.is_finite() && duration > 0.0)
            {
                // Only backend-confirmed position at EOF can resolve an unknown
                // duration; a seek target or accumulated heard time cannot.
                self.state.playback.duration = playback.position.filter(|position| *position > 0.0);
            }
        }
        if let Some(stats) = self.playback.take() {
            debug_assert!(self.retired_playback.is_none());
            self.retired_playback = Some(RetiredPlayback {
                stats,
                duration: self.state.playback.duration,
            });
            self.playback_write_deferred = true;
        }
        if stopped.is_err() || bookkeeping.is_err() {
            self.pending_navigation = None;
        }
        // Retain the same bounded producer owner on ordinary stop failures as
        // during maintenance. Neither a projection error nor an activity write
        // may skip an independently eligible count or discard confirmed facts.
        let statistics = self.flush_deferred_playback();
        stopped.and(bookkeeping).and(statistics)
    }
    pub(in crate::core) fn stop_playback(&mut self, natural: bool) -> Result<()> {
        let mut stop = self.engine.stop_request();
        let generation = self.generation;
        let mut bookkeeping = Ok(());
        // Only the persistent synchronous business owner enters this wait.
        // The application executor remains free, and the stop handle is the sole audio
        // event consumer until it returns the receiver (also on timeout).
        let snapshot = futures::executor::block_on(stop.wait(|event| {
            if let Err(error) = self.retiring_audio_event(generation, event)
                && bookkeeping.is_ok()
            {
                bookkeeping = Err(error);
            }
        }))
        .map(Some);
        self.engine.restore_events(stop.into_events());
        self.finish_playback(snapshot, natural, bookkeeping)
    }

    pub(in crate::core) fn stop(&mut self) -> Result<()> {
        self.stop_playback(false)
    }
    pub(in crate::core) fn advance(&mut self, natural: bool) -> Result<()> {
        self.advance_with_status(natural, PlaybackStatus::Playing)
    }
    pub(in crate::core) fn navigate(&mut self, direction: Navigation) -> Result<()> {
        // Read the owner state here, after earlier queued operations have run.
        let status = self.state.playback.status;
        match direction {
            Navigation::Next => self.advance_with_status(false, status),
            Navigation::Previous => self.previous_with_status(status),
        }
    }
    pub(in crate::core) fn advance_with_status(
        &mut self,
        natural: bool,
        status: PlaybackStatus,
    ) -> Result<()> {
        if self.database_maintenance_active() {
            if natural {
                self.pending_navigation = Some(status);
                return Ok(());
            }
            bail!("Database maintenance is in progress; navigation was not executed");
        }
        if self.queue_ids.is_empty() {
            return self.stop();
        }
        let current = self.state.queue.current_id;
        if natural
            && self.state.playback.repeat == RepeatMode::One
            && let Some(id) = current
        {
            return self.start_with_status(id, true, status);
        }
        if self.state.playback.shuffle {
            // Every Next is an independent draw over occurrences, including current.
            let id = self
                .random_queue_id(&mut rand::rng())
                .expect("queue is nonempty");
            return self.start_with_status(id, true, status);
        }
        let index = self.state.queue.current_position;
        let next = index.map_or(0, |index| index + 1);
        if let Some(id) = self.queue_ids.get(next).copied() {
            return self.start_with_status(id, true, status);
        }
        if self.state.playback.repeat == RepeatMode::All {
            return self.start_with_status(self.queue_ids[0], true, status);
        }
        if status == PlaybackStatus::Paused {
            Ok(())
        } else {
            self.stop()
        }
    }
    pub(in crate::core) fn previous(&mut self) -> Result<()> {
        self.previous_with_status(PlaybackStatus::Playing)
    }
    fn previous_with_status(&mut self, status: PlaybackStatus) -> Result<()> {
        if self.state.playback.position > 3.0 {
            return self.seek(0.0);
        }
        if self.state.playback.shuffle
            && let Some(index) =
                self.played_cursor
                    .checked_sub(if self.state.queue.current_id.is_some() {
                        2
                    } else {
                        1
                    })
        {
            self.start_with_status(self.played[index], false, status)?;
            self.played_cursor = index + 1;
            return Ok(());
        }
        let index = self.state.queue.current_position;
        if let Some(index) = index {
            if index > 0 {
                return self.start_with_status(self.queue_ids[index - 1], true, status);
            }
            return if status == PlaybackStatus::Stopped {
                Ok(())
            } else {
                self.seek(0.0)
            };
        }
        if let Some(id) = self.queue_ids.first().copied() {
            return self.start_with_status(id, true, status);
        }
        bail!("Queue is empty")
    }
    pub(in crate::core) fn seek(&mut self, seconds: f64) -> Result<()> {
        audio::validate_seek_position(seconds)?;
        if self.state.playback.status == PlaybackStatus::Stopped {
            bail!("Nothing is playing");
        }
        let end = self
            .state
            .current_track()
            .and_then(|track| track.cue.as_ref())
            .and_then(|cue| cue.end_seconds().map(|end| end - cue.start_seconds()));
        let seconds = end.map_or(seconds, |end| seconds.min(end));
        let revision = self.state.playback.seek_revision.wrapping_add(1);
        self.engine.send(AudioCommand::Seek { seconds, revision })?;
        self.state.playback.position = seconds;
        self.state.playback.seek_revision = revision;
        Ok(())
    }

    pub(in crate::core) fn replace_history(
        &mut self,
        history: Vec<rivu_protocol::model::HistoryEntry>,
    ) {
        self.state.library.history = Arc::new(history);
        self.playback_history_pending = false;
    }

    fn refresh_playback_history(&mut self) -> Result<()> {
        if self.playback_history_pending {
            let history = self.store.history(MAX_QUERY_ROWS)?;
            self.replace_history(history);
        }
        Ok(())
    }

    /// Replay only retained producer facts, never timed-out user commands.
    pub(in crate::core) fn flush_deferred_playback(&mut self) -> Result<()> {
        if self.database_maintenance_active() {
            return Ok(());
        }
        let retired = self.retired_playback.take();
        let retired_duration = retired.as_ref().map(|playback| playback.duration);
        if let Some(retired) = retired {
            debug_assert!(self.playback.is_none());
            self.playback = Some(retired.stats);
        }
        let duration = self.state.playback.duration;
        if let Some(retired_duration) = retired_duration {
            self.state.playback.duration = retired_duration;
        }
        let result = (|| -> Result<()> {
            if !std::mem::take(&mut self.playback_write_deferred) && retired_duration.is_none() {
                return Ok(());
            }
            // The count's committed bit precedes projection reads. Attempt both
            // producer facts even if one projection fails; never count twice.
            let count = self.count_play();
            let activity = if let Some((track_id, at)) =
                self.playback.as_ref().and_then(|playback| {
                    playback
                        .started
                        .then_some((playback.track_id, playback.last_heard_at))
                        .and_then(|(id, at)| at.map(|at| (id, at)))
                }) {
                self.store.mark_played(track_id, at).and_then(|stats| {
                    self.playback_history_pending |= stats.changed;
                    let projection = self.update_track_stats(track_id, stats);
                    let history = self.refresh_playback_history();
                    projection.and(history)
                })
            } else {
                Ok(())
            };
            if let Some((bitrate, exact)) = self.pending_bitrate.take() {
                self.persist_playback_bitrate(bitrate, exact);
            }
            count.and(activity)
        })();
        if retired_duration.is_some() {
            if let Some(stats) = self.playback.take()
                && result.is_err()
            {
                // Park facts on failure; explicit commands and shutdown recover
                // them before another playback can replace this bounded owner.
                self.retired_playback = Some(RetiredPlayback {
                    stats,
                    duration: self.state.playback.duration,
                });
            }
            self.state.playback.duration = duration;
        }
        if result.is_err() {
            self.pending_navigation = None;
        }
        result?;
        if let Some(status) = self.pending_navigation.take()
            && !self.state.system.shutting_down
        {
            self.advance_with_status(true, status)?;
        }
        Ok(())
    }
}
