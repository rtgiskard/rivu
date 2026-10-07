use super::*;

impl Core {
    pub(in crate::core) fn start(&mut self, queue_id: u64, record_history: bool) -> Result<()> {
        let entry = self
            .state
            .queue
            .entries
            .iter()
            .find(|entry| entry.id == queue_id)
            .context("Queue entry not found")?;
        let track = self.track(entry.track_id)?;
        if !track.path.is_file() {
            bail!("Missing audio file: {}", track.path.display());
        }
        self.stop()?;
        self.audio(AudioCommand::Load {
            path: track.path.clone(),
            range: track.cue.as_ref().map(|cue| audio::PlaybackRange {
                start_seconds: cue.start_seconds(),
                end_seconds: cue.end_seconds(),
            }),
            generation: self.generation,
            start_seconds: 0.0,
            paused: false,
        })?;
        self.playback = Some(PlaybackStats {
            track_id: track.id,
            heard: 0.0,
            position: None,
            started: false,
            counted: false,
            last_heard_at: None,
            activity_updated: false,
            paused_at: None,
        });
        self.state.playback.last_heard_at = None;
        self.state.queue.current_id = Some(queue_id);
        self.state.playback.duration = track.duration;
        self.state.current_track = Some(Arc::new(track));
        self.state.playback.status = PlaybackStatus::Playing;
        self.state.system.last_error = None;
        if record_history {
            self.played.truncate(self.played_cursor);
            self.played.push(queue_id);
            self.played_cursor = self.played.len();
            let keep = self.state.system.config.queue_limit as usize;
            if self.played.len() > keep {
                let drop_count = self.played.len() - keep;
                self.played.drain(..drop_count);
                self.played_cursor = self.played_cursor.saturating_sub(drop_count);
            }
        }
        self.shuffle_bag.retain(|id| *id != queue_id);
        self.queue_dirty = true;
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
        self.store.increment_play_count(track_id)?;
        self.update_track_stats(track_id, None, true)?;
        if let Some(playback) = self.playback.as_mut() {
            playback.counted = true;
        }
        Ok(())
    }
    pub(in crate::core) fn playback_started(&mut self, info: library::MediaInfo) -> Result<()> {
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
        if let Err(error) = self.store.mark_played(track_id, played_at) {
            // Rejected bookkeeping must not be revived by another queued start
            // or counted from the successful audio output's final snapshot.
            self.playback = None;
            return Err(error);
        }
        self.update_track_stats(track_id, Some(played_at), false)?;
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
    pub(in crate::core) fn drain_playback_events(&mut self, generation: u64) -> Result<()> {
        while let Ok(event) = self.engine.events.try_recv() {
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
                // The output is retiring: terminal events must never advance
                // the queue, recursively stop the worker, or revive playback.
                _ => {}
            }
        }
        Ok(())
    }
    pub(in crate::core) fn finish_playback(
        &mut self,
        snapshot: Result<Option<(u64, f64)>>,
        natural: bool,
    ) -> Result<()> {
        let generation = self.generation;
        // A request can beat a queued Started even though the worker has already
        // played audio. Acknowledged stops make that queued bookkeeping safe to
        // consume before generation fencing; errors still fence below.
        let bookkeeping = if snapshot.is_ok() {
            self.drain_playback_events(generation)
        } else {
            Ok(())
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
                self.store.mark_played(track_id, played_at)?;
                self.update_track_stats(track_id, Some(played_at), false)?;
                self.state.library.history = Arc::new(self.store.history(MAX_QUERY_ROWS)?);
            }
            self.count_play()
        })();
        self.playback = None;
        result
    }
    pub(in crate::core) fn stop(&mut self) -> Result<()> {
        let snapshot = self.engine.stop_and_snapshot().map(Some);
        self.finish_playback(snapshot, false)
    }
    pub(in crate::core) fn advance(&mut self, natural: bool) -> Result<()> {
        if self.state.queue.entries.is_empty() {
            return self.stop();
        }
        let current = self.state.queue.current_id;
        if natural
            && self.state.playback.repeat == RepeatMode::One
            && let Some(id) = current
        {
            return self.start(id, true);
        }
        if self.state.playback.shuffle {
            if !natural && self.played_cursor < self.played.len() {
                let id = self.played[self.played_cursor];
                self.start(id, false)?;
                self.played_cursor += 1;
                return Ok(());
            }
            if self.shuffle_bag.is_empty() {
                let visited = self
                    .played
                    .iter()
                    .copied()
                    .collect::<std::collections::HashSet<_>>();
                self.shuffle_bag = self
                    .state
                    .queue
                    .entries
                    .iter()
                    .filter(|q| Some(q.id) != current && !visited.contains(&q.id))
                    .map(|q| q.id)
                    .collect();
                if self.shuffle_bag.is_empty() && self.state.playback.repeat == RepeatMode::All {
                    self.played.clear();
                    self.played_cursor = 0;
                    self.shuffle_bag = self
                        .state
                        .queue
                        .entries
                        .iter()
                        .filter(|q| Some(q.id) != current)
                        .map(|q| q.id)
                        .collect();
                    if self.shuffle_bag.is_empty() {
                        self.shuffle_bag = self.state.queue.entries.iter().map(|q| q.id).collect();
                    }
                }
                self.shuffle_bag.shuffle(&mut rand::rng());
            }
            if let Some(&id) = self.shuffle_bag.last() {
                return self.start(id, true);
            }
        } else {
            let index =
                current.and_then(|id| self.state.queue.entries.iter().position(|q| q.id == id));
            let next = index.map_or(0, |index| index + 1);
            if let Some(entry) = self.state.queue.entries.get(next) {
                return self.start(entry.id, true);
            }
            if self.state.playback.repeat == RepeatMode::All {
                return self.start(self.state.queue.entries[0].id, true);
            }
        }
        self.stop()
    }
    pub(in crate::core) fn previous(&mut self) -> Result<()> {
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
            self.start(self.played[index], false)?;
            self.played_cursor = index + 1;
            return Ok(());
        }
        let index = self
            .state
            .queue
            .current_id
            .and_then(|id| self.state.queue.entries.iter().position(|q| q.id == id));
        if let Some(index) = index {
            if index > 0 {
                return self.start(self.state.queue.entries[index - 1].id, true);
            }
            return self.seek(0.0);
        }
        if let Some(entry) = self.state.queue.entries.first() {
            return self.start(entry.id, true);
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
        let seconds = self
            .state
            .playback
            .duration
            .map_or(seconds, |duration| seconds.min(duration));
        self.audio(AudioCommand::Seek(seconds))?;
        self.state.playback.position = seconds;
        self.state.playback.seek_revision = self.state.playback.seek_revision.wrapping_add(1);
        Ok(())
    }
}
