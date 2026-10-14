use super::*;

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
        if !std::fs::metadata(&source.path).is_ok_and(|after| same_source_file(before, &after))
            || !self
                .store
                .upgrade_bitrate(source, bitrate, exact)
                .unwrap_or_else(|error| {
                    tracing::warn!(error = ?error, "playback_bitrate_not_saved");
                    false
                })
        {
            return false;
        }
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
            && self.persist_playback_bitrate(bitrate, true)
        {
            self.state.queue = self.store.queue_projection(self.queue_ids.len())?;
            self.state.library.revision = self.state.library.revision.wrapping_add(1);
        }
        Ok(())
    }

    pub(in crate::core) fn start(&mut self, queue_id: u64, record_history: bool) -> Result<()> {
        self.start_with_status(queue_id, record_history, PlaybackStatus::Playing)
    }
    fn start_with_status(
        &mut self,
        queue_id: u64,
        record_history: bool,
        status: PlaybackStatus,
    ) -> Result<()> {
        let track = self.track(self.store.queue_track_id(queue_id)?)?;
        if status != PlaybackStatus::Stopped && !track.path.is_file() {
            bail!("Missing audio file: {}", track.path.display());
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
            self.audio(AudioCommand::Load {
                path: track.path.clone(),
                decoder_hint: track.decoder_hint,
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
        self.save(false)
    }
    pub(in crate::core) fn count_play(&mut self) -> Result<()> {
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
        self.update_track_stats(track_id, stats)?;
        if let Some(playback) = self.playback.as_mut() {
            playback.counted = true;
        }
        Ok(())
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
        let stats = match self.store.mark_played(track_id, played_at) {
            Ok(stats) => stats,
            Err(error) => {
                // Rejected bookkeeping cannot be revived by another queued start
                // or counted from the output's final snapshot.
                self.playback = None;
                return Err(error);
            }
        };
        // Coalesce this content change with the existing start bookkeeping's
        // queue projection and library revision; unknown/no-op results add none.
        if let Some(bitrate) = info.bitrate_bps.filter(|bitrate| *bitrate > 0) {
            self.persist_playback_bitrate(bitrate, info.bitrate_exact);
        }
        self.update_track_stats(track_id, stats)?;
        let playback = self.playback.as_mut().expect("playback was checked above");
        playback.started = true;
        playback.last_heard_at = Some(played_at);
        self.state.playback.last_heard_at = Some(played_at);
        self.state.playback.duration = info
            .duration
            .filter(|duration| duration.is_finite() && *duration > 0.0)
            .or(self
                .state
                .playback
                .duration
                .filter(|duration| duration.is_finite() && *duration > 0.0));
        self.state.library.history = Arc::new(self.store.history(MAX_QUERY_ROWS)?);
        self.count_play()
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
    fn retiring_audio_event(&mut self, generation: u64, event: AudioEvent) -> Result<()> {
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
        while let Ok(event) = self.engine.events.try_recv() {
            self.retiring_audio_event(generation, event)?;
        }
        Ok(())
    }
    pub(in crate::core) fn finish_playback(
        &mut self,
        snapshot: Result<Option<(u64, f64)>>,
        natural: bool,
        bookkeeping: Result<()>,
    ) -> Result<()> {
        let generation = self.generation;
        // A request can beat a queued Started even though the worker has already
        // played audio. Acknowledged stops make that queued bookkeeping safe to
        // consume before generation fencing; errors still fence below.
        let bookkeeping = if snapshot.is_ok() {
            bookkeeping.and_then(|()| self.drain_playback_events(generation))
        } else {
            bookkeeping
        };
        // Fence queued events even when stopping or saving the count fails.
        self.generation = self.generation.wrapping_add(1);
        self.state.playback.status = PlaybackStatus::Stopped;
        self.state.playback.position = 0.0;
        self.state.playback.seek_revision = self.state.playback.seek_revision.wrapping_add(1);
        let snapshot = match snapshot {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.playback = None;
                return Err(error);
            }
        };
        let mut final_activity = None;
        if let Some(playback) = &mut self.playback {
            if let Some((snapshot_generation, heard)) = snapshot
                && snapshot_generation == generation
                && heard.is_finite()
            {
                // Late paused confirmations still count as heard audio, but
                // their activity timestamp cannot cross the pause boundary.
                let heard_at = playback
                    .paused_at
                    .map_or_else(now, |paused_at| now().min(paused_at));
                if playback.record_heard(heard, heard_at) {
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
            if playback.started && playback.activity_updated {
                final_activity = playback.last_heard_at.map(|at| (playback.track_id, at));
            }
        }
        let result = (|| -> Result<()> {
            bookkeeping?;
            if let Some((track_id, played_at)) = final_activity {
                let stats = self.store.mark_played(track_id, played_at)?;
                self.update_track_stats(track_id, stats)?;
                self.state.library.history = Arc::new(self.store.history(MAX_QUERY_ROWS)?);
            }
            self.count_play()
        })();
        self.playback = None;
        result
    }
    pub(in crate::core) fn stop_playback(&mut self, natural: bool) -> Result<()> {
        let commands = self.engine.commands.clone();
        let events = self.engine.events.clone();
        let generation = self.generation;
        let mut bookkeeping = Ok(());
        let snapshot = audio::stop_and_snapshot(&commands, &events, |event| {
            if bookkeeping.is_ok() {
                bookkeeping = self.retiring_audio_event(generation, event);
            }
        })
        .map(Some);
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
        if !seconds.is_finite() || seconds < 0.0 {
            bail!("Seek must be a finite nonnegative number");
        }
        if self.state.playback.status == PlaybackStatus::Stopped {
            bail!("Nothing is playing");
        }
        let end = self
            .state
            .current_track()
            .and_then(|track| track.cue.as_ref())
            .and_then(|cue| cue.end_seconds().map(|end| end - cue.start_seconds()));
        let seconds = end.map_or(seconds, |end| seconds.min(end));
        self.audio(AudioCommand::Seek(seconds))?;
        self.state.playback.position = seconds;
        self.state.playback.seek_revision = self.state.playback.seek_revision.wrapping_add(1);
        Ok(())
    }
}
