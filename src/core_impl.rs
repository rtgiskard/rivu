use super::*;
use anyhow::bail;
use crossbeam_channel::{Receiver, select};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

impl Core {
    pub(super) fn run(&mut self, requests: Receiver<Request>) {
        let config_ticks = crossbeam_channel::tick(Duration::from_millis(100));
        loop {
            select! {
                recv(requests) -> request => {
                    let Ok(request) = request else { break; };
                    let shutdown = matches!(request.command, Command::Shutdown);
                    let changed = !matches!(request.command, Command::Status | Command::Overview | Command::ShowWindow);
                    let result = self.command(request.command);
                    if let Err(error) = &result { self.state.last_error = Some(format!("{error:#}")); }
                    if changed || result.is_err() { self.publish(); }
                    if let Some(reply) = request.reply {
                        let value = if request.ack {
                            CoreResponse::Ack(Ack {
                                ok: result.is_ok(),
                                error: result.as_ref().err().map(|e| format!("{e:#}")),
                                revision: self.state.revision,
                            })
                        } else {
                            CoreResponse::State(Box::new(StateResponse {
                                ok: result.is_ok(),
                                error: result.err().map(|e| format!("{e:#}")),
                                state: self.state.clone(),
                            }))
                        };
                        let _ = reply.send(value);
                    }
                    if shutdown { break; }
                }
                recv(self.engine.events) -> event => {
                    let Ok(event) = event else { self.state.last_error = Some("Audio worker stopped unexpectedly".into()); self.publish(); break; };
                    if let Err(error) = self.audio_event(event) { self.state.last_error = Some(format!("{error:#}")); }
                    self.publish();
                }
                recv(self.scan_rx) -> scan => {
                    if let Ok(scan) = scan {
                        if let Err(error) = self.finish_scan(scan) { self.state.last_error = Some(format!("{error:#}")); }
                        self.state.scanning = false;
                        self.publish();
                    }
                }
                recv(config_ticks) -> _ => {
                    if self.config_dirty
                        && self.config_last_saved.elapsed() >= Duration::from_millis(250)
                        && self.save(false).is_ok()
                    {
                        self.publish();
                    }
            }
                }
        }

        if let Err(error) = self.stop() {
            self.state.last_error = Some(format!("Saving play count: {error:#}"));
        }
        let _ = self.save(true);
        let _ = self.engine.send(AudioCommand::Shutdown);
        for worker in self.scan_workers.drain(..) {
            let _ = worker.join();
        }
        self.state.status = PlaybackStatus::Stopped;
        self.state.shutting_down = true;
        self.publish();
        if let Some(callback) = self.gui_opener.read().clone() {
            callback();
        }
    }

    pub(super) fn publish(&mut self) {
        if let Some(library) = self.library.snapshot() {
            self.state.library = library;
            self.state.library_revision = self.state.library_revision.wrapping_add(1);
        }
        self.state.revision = self.state.revision.wrapping_add(1);
        *self.shared.write() = self.state.clone();
        self.subscribers.write().retain(|sender| {
            !matches!(
                sender.try_send(()),
                Err(crossbeam_channel::TrySendError::Disconnected(_))
            )
        });
        let callback = self.wakeup.read().clone();
        if let Some(callback) = callback {
            callback();
        }
    }
    pub(super) fn request_raise(&self) -> Result<()> {
        let opener = self.gui_opener.read();
        let callback = opener
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Rivu has no graphical host"))?;
        self.raise_requested.store(true, Ordering::Release);
        callback();
        Ok(())
    }
    pub(super) fn save(&mut self, force_config: bool) -> Result<()> {
        let config_changed = self.state.config.volume != self.state.volume
            || self.state.config.shuffle != self.state.shuffle
            || self.state.config.repeat != self.state.repeat
            || self.state.config.output_device != self.state.selected_device;
        if self.config_dirty
            && config_changed
            && (force_config || self.config_last_saved.elapsed() >= Duration::from_millis(250))
        {
            let mut config = self.state.config.as_ref().clone();
            config.volume = self.state.volume;
            config.shuffle = self.state.shuffle;
            config.repeat = self.state.repeat;
            config.output_device.clone_from(&self.state.selected_device);
            config.save(&self.state.config_path)?;
            self.state.config = Arc::new(config);
            self.config_last_saved = Instant::now();
            self.config_dirty = false;
        } else if !config_changed {
            self.config_dirty = false;
        }
        if self.queue_dirty {
            let saved = Saved {
                queue: self.state.queue.as_ref().clone(),
                current: self.state.current_queue_id,
            };
            self.store
                .set_setting("playback", &serde_json::to_string(&saved)?)?;
            self.queue_dirty = false;
        }
        Ok(())
    }
    pub(super) fn reload_library(&mut self, structure_changed: bool) -> Result<()> {
        let tracks = self.store.tracks()?;
        self.state.library = self.library.replace(tracks);
        self.state.library_revision = self.state.library_revision.wrapping_add(1);
        if structure_changed {
            self.state.library_structure_revision =
                self.state.library_structure_revision.wrapping_add(1);
        }
        Ok(())
    }

    pub(super) fn update_track_stats(
        &mut self,
        track_id: i64,
        played_at: Option<i64>,
        increment_play_count: bool,
    ) -> Result<()> {
        self.library.update(track_id, |track| {
            if let Some(played_at) = played_at {
                track.last_played = Some(played_at);
            }
            if increment_play_count {
                track.play_count = track.play_count.saturating_add(1);
            }
        })
    }
    pub(super) fn reload(&mut self, structure_changed: bool) -> Result<()> {
        self.reload_library(structure_changed)?;
        self.state.playlists = Arc::new(self.store.playlists()?);
        self.state.history = Arc::new(self.store.history(200)?);
        Ok(())
    }
    pub(super) fn audio(&self, command: AudioCommand) -> Result<()> {
        self.engine
            .commands
            .send(command)
            .context("Audio worker unavailable")
    }
    pub(super) fn track(&self, id: i64) -> Result<&Track> {
        let index = self
            .library
            .tracks()
            .binary_search_by_key(&id, |track| track.id)
            .ok()
            .context("Track not found")?;
        Ok(&self.library.tracks()[index])
    }
    pub(super) fn enqueue(&mut self, ids: &[i64]) -> Result<()> {
        for id in ids {
            self.track(*id)?;
        }
        let queue = Arc::make_mut(&mut self.state.queue);
        for id in ids {
            queue.push(QueueEntry {
                id: self.next_queue_id,
                track_id: *id,
            });
            self.next_queue_id += 1;
        }
        self.shuffle_bag.clear();
        if !ids.is_empty() {
            self.queue_dirty = true;
        }
        Ok(())
    }
    pub(super) fn randomize_queue(&mut self, rng: &mut impl rand::Rng) {
        if self.state.queue.len() > 1 {
            Arc::make_mut(&mut self.state.queue).shuffle(rng);
        }
    }
    pub(super) fn deduplicate_queue(&mut self) {
        if self.state.queue.len() < 2 {
            return;
        }
        let current = self.state.current_queue_id.and_then(|id| {
            self.state
                .queue
                .iter()
                .find(|entry| entry.id == id)
                .map(|entry| (entry.id, entry.track_id))
        });
        let mut seen = std::collections::HashSet::with_capacity(self.state.queue.len());
        Arc::make_mut(&mut self.state.queue).retain_mut(|entry| {
            if !seen.insert(entry.track_id) {
                return false;
            }
            // Keep the current entry's identity at this track's first position,
            // even when playback had selected a later duplicate.
            if let Some((id, track_id)) = current
                && entry.track_id == track_id
            {
                entry.id = id;
            }
            true
        });
        self.prune_queue_history();
    }
    pub(super) fn prune_queue_history(&mut self) {
        let queue = &self.state.queue;
        self.shuffle_bag
            .retain(|id| queue.iter().any(|entry| entry.id == *id));
        let mut index = 0;
        let mut cursor = 0;
        self.played.retain(|id| {
            let keep = queue.iter().any(|entry| entry.id == *id);
            if keep && index < self.played_cursor {
                cursor += 1;
            }
            index += 1;
            keep
        });
        self.played_cursor = cursor;
    }
    pub(super) fn start(&mut self, queue_id: u64, record_history: bool) -> Result<()> {
        let entry = self
            .state
            .queue
            .iter()
            .find(|entry| entry.id == queue_id)
            .context("Queue entry not found")?;
        let track = self.track(entry.track_id)?.clone();
        if !track.path.is_file() {
            bail!("Missing audio file: {}", track.path.display());
        }
        self.stop()?;
        self.audio(AudioCommand::Load {
            path: track.path,
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
        });
        self.state.current_queue_id = Some(queue_id);
        self.state.duration = track.duration;
        self.state.status = PlaybackStatus::Playing;
        self.state.last_error = None;
        if record_history {
            self.played.truncate(self.played_cursor);
            self.played.push(queue_id);
            self.played_cursor = self.played.len();
        }
        self.shuffle_bag.retain(|id| *id != queue_id);
        self.queue_dirty = true;
        self.save(false)
    }
    pub(super) fn count_play(&mut self) -> Result<()> {
        let Some(duration) = self
            .state
            .duration
            .filter(|duration| duration.is_finite() && *duration > 0.0)
        else {
            return Ok(());
        };
        let Some(track_id) = self.playback.as_ref().and_then(|playback| {
            (playback.started
                && !playback.counted
                && playback.heard
                    > duration * self.state.config.play_count_threshold_percent / 100.0)
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
    pub(super) fn playback_started(&mut self, info: library::MediaInfo) -> Result<()> {
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
        if let Err(error) = self.store.mark_played(track_id, played_at) {
            // Rejected bookkeeping must not be revived by another queued start
            // or counted from the successful audio output's final snapshot.
            self.playback = None;
            return Err(error);
        }
        self.update_track_stats(track_id, Some(played_at), false)?;
        let playback = self.playback.as_mut().expect("playback was checked above");
        playback.started = true;
        self.state.duration = info
            .duration
            .filter(|duration| duration.is_finite() && *duration > 0.0)
            .or(self
                .state
                .duration
                .filter(|duration| duration.is_finite() && *duration > 0.0));
        self.state.history = Arc::new(self.store.history(200)?);
        self.count_play()
    }
    pub(super) fn playback_progress(&mut self, position: f64, heard: f64) -> Result<()> {
        if let Some(playback) = &mut self.playback
            && playback.started
        {
            if position.is_finite() {
                self.state.position = position;
                playback.position = Some(position);
            }
            if heard.is_finite() {
                playback.heard = playback.heard.max(heard);
            }
            self.count_play()?;
        }
        Ok(())
    }
    pub(super) fn drain_playback_events(&mut self, generation: u64) -> Result<()> {
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
    pub(super) fn finish_playback(
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
        self.state.status = PlaybackStatus::Stopped;
        self.state.position = 0.0;
        self.state.seek_revision = self.state.seek_revision.wrapping_add(1);
        let snapshot = snapshot?;
        if let Some(playback) = &mut self.playback {
            if let Some((snapshot_generation, heard)) = snapshot
                && snapshot_generation == generation
                && heard.is_finite()
            {
                playback.heard = playback.heard.max(heard);
            }
            if natural
                && !self
                    .state
                    .duration
                    .is_some_and(|duration| duration.is_finite() && duration > 0.0)
            {
                // Only backend-confirmed position at EOF can resolve an unknown
                // duration; a seek target or accumulated heard time cannot.
                self.state.duration = playback.position.filter(|position| *position > 0.0);
            }
        }
        bookkeeping?;
        self.count_play()?;
        self.playback = None;
        Ok(())
    }
    pub(super) fn stop(&mut self) -> Result<()> {
        let snapshot = self.engine.stop_and_snapshot().map(Some);
        self.finish_playback(snapshot, false)
    }
    pub(super) fn advance(&mut self, natural: bool) -> Result<()> {
        if self.state.queue.is_empty() {
            return self.stop();
        }
        let current = self.state.current_queue_id;
        if natural
            && self.state.repeat == RepeatMode::One
            && let Some(id) = current
        {
            return self.start(id, true);
        }
        if self.state.shuffle {
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
                    .iter()
                    .filter(|q| Some(q.id) != current && !visited.contains(&q.id))
                    .map(|q| q.id)
                    .collect();
                if self.shuffle_bag.is_empty() && self.state.repeat == RepeatMode::All {
                    self.played.clear();
                    self.played_cursor = 0;
                    self.shuffle_bag = self
                        .state
                        .queue
                        .iter()
                        .filter(|q| Some(q.id) != current)
                        .map(|q| q.id)
                        .collect();
                    if self.shuffle_bag.is_empty() {
                        self.shuffle_bag = self.state.queue.iter().map(|q| q.id).collect();
                    }
                }
                self.shuffle_bag.shuffle(&mut rand::rng());
            }
            if let Some(&id) = self.shuffle_bag.last() {
                return self.start(id, true);
            }
        } else {
            let index = current.and_then(|id| self.state.queue.iter().position(|q| q.id == id));
            let next = index.map_or(0, |index| index + 1);
            if let Some(entry) = self.state.queue.get(next) {
                return self.start(entry.id, true);
            }
            if self.state.repeat == RepeatMode::All {
                return self.start(self.state.queue[0].id, true);
            }
        }
        self.stop()
    }
    pub(super) fn previous(&mut self) -> Result<()> {
        if self.state.position > 3.0 {
            return self.seek(0.0);
        }
        if self.state.shuffle
            && let Some(index) =
                self.played_cursor
                    .checked_sub(if self.state.current_queue_id.is_some() {
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
            .current_queue_id
            .and_then(|id| self.state.queue.iter().position(|q| q.id == id));
        if let Some(index) = index {
            if index > 0 {
                return self.start(self.state.queue[index - 1].id, true);
            }
            return self.seek(0.0);
        }
        if let Some(entry) = self.state.queue.first() {
            return self.start(entry.id, true);
        }
        bail!("Queue is empty")
    }
    pub(super) fn seek(&mut self, seconds: f64) -> Result<()> {
        if !seconds.is_finite() || seconds < 0.0 {
            bail!("Seek must be a finite nonnegative number");
        }
        if self.state.status == PlaybackStatus::Stopped {
            bail!("Nothing is playing");
        }
        let seconds = self
            .state
            .duration
            .map_or(seconds, |duration| seconds.min(duration));
        self.audio(AudioCommand::Seek(seconds))?;
        self.state.position = seconds;
        self.state.seek_revision = self.state.seek_revision.wrapping_add(1);
        Ok(())
    }
    pub(super) fn scan(
        &mut self,
        paths: Vec<PathBuf>,
        import: Option<(String, Vec<M3uItem>)>,
        force: bool,
    ) -> Result<()> {
        if self.state.scanning {
            bail!("A library scan is already running");
        }
        if paths.is_empty() {
            bail!("No files or directories supplied");
        }
        let known = if force {
            Vec::new()
        } else {
            self.store.known_files()?
        };
        let ffmpeg_enabled = self.state.config.ffmpeg_enabled;
        let sender = self.scan_tx.clone();
        let worker = thread::Builder::new()
            .name("rivu-scan".into())
            .spawn(move || {
                let result = library::scan_paths(&paths, &known, ffmpeg_enabled);
                let _ = sender.send(ScanFinished { result, import });
            })?;
        self.scan_workers.retain(|worker| !worker.is_finished());
        self.scan_workers.push(worker);
        self.state.scanning = true;
        self.state.last_error = None;
        self.state.scan_message = "Reading audio metadata…".into();
        Ok(())
    }
    pub(super) fn finish_scan(&mut self, scan: ScanFinished) -> Result<()> {
        let result = scan.result?;
        self.store.apply_scan(&result)?;
        self.state.scan_message = result.summary();
        if !result.errors().is_empty() {
            self.state.last_error = Some(result.errors().join("\n"));
        }
        self.reload(true)?;
        if let Some((name, items)) = scan.import {
            let mut ids = Vec::with_capacity(items.len());
            let by_source: std::collections::HashMap<(&Path, Option<u32>), i64> = self
                .state
                .library
                .iter()
                .filter(|track| !track.missing)
                .map(|track| {
                    let key = track
                        .cue
                        .as_ref()
                        .map_or((track.path.as_path(), None), |cue| {
                            (cue.sheet.as_path(), Some(cue.number))
                        });
                    (key, track.id)
                })
                .collect();
            for item in items {
                if let Ok(path) = item.path.canonicalize()
                    && let Some(id) = by_source.get(&(path.as_path(), item.cue_track))
                {
                    ids.push(*id);
                }
            }
            if ids.is_empty() {
                bail!("Playlist contains no supported, accessible audio tracks");
            }
            let playlist_id = self.store.create_playlist(&name)?;
            self.store.add_playlist(playlist_id, &ids)?;
            self.state.playlists = Arc::new(self.store.playlists()?);
        }
        Ok(())
    }
    pub(super) fn audio_event(&mut self, event: AudioEvent) -> Result<()> {
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
            AudioEvent::Ended { generation } if generation == self.generation => {
                let snapshot = self.engine.stop_and_snapshot().map(Some);
                self.finish_playback(snapshot, true)?;
                self.advance(true)?;
            }
            AudioEvent::Failed {
                generation,
                message,
            } if generation == self.generation => {
                self.stop().with_context(|| message.clone())?;
                self.state.last_error = Some(message);
            }
            AudioEvent::DecoderStopped { generation } if generation == self.generation => {
                // The worker has already stopped and may still be sending
                // events while handling Configure; do not wait on a command.
                self.finish_playback(Ok(None), false)?;
            }
            _ => {}
        }
        Ok(())
    }
    pub(super) fn remove_queue_entries(&mut self, queue_ids: &[u64]) -> Result<()> {
        if queue_ids.is_empty() {
            return Ok(());
        }
        let ids = queue_ids.iter().copied().collect::<HashSet<_>>();
        let found = self
            .state
            .queue
            .iter()
            .filter(|entry| ids.contains(&entry.id))
            .count();
        if found != ids.len() {
            let missing = ids
                .iter()
                .find(|id| !self.state.queue.iter().any(|entry| entry.id == **id))
                .copied()
                .expect("bulk queue validation mismatch");
            bail!("Queue entry not found: {missing}");
        }

        if self
            .state
            .current_queue_id
            .is_some_and(|id| ids.contains(&id))
        {
            self.stop()?;
            self.state.current_queue_id = None;
        }
        Arc::make_mut(&mut self.state.queue).retain(|entry| !ids.contains(&entry.id));
        self.prune_queue_history();
        Ok(())
    }

    pub(super) fn move_queue_entries(&mut self, queue_ids: &[u64], index: usize) -> Result<()> {
        if queue_ids.is_empty() {
            return Ok(());
        }
        let ids = queue_ids.iter().copied().collect::<HashSet<_>>();
        // Validate before make_mut so an invalid request cannot even detach
        // the shared queue snapshot, let alone mutate playback state.
        let found = self
            .state
            .queue
            .iter()
            .filter(|entry| ids.contains(&entry.id))
            .count();
        if found != ids.len() {
            let missing = ids
                .iter()
                .find(|id| !self.state.queue.iter().any(|entry| entry.id == **id))
                .copied()
                .expect("bulk queue validation mismatch");
            bail!("Queue entry not found: {missing}");
        }

        let queue = Arc::make_mut(&mut self.state.queue);
        let mut selected = Vec::with_capacity(ids.len());
        selected.extend(queue.extract_if(.., |entry| ids.contains(&entry.id)));
        let insertion = index.min(queue.len());
        queue.splice(insertion..insertion, selected);
        Ok(())
    }

    pub(super) fn command(&mut self, command: Command) -> Result<()> {
        let persist_queue = matches!(
            &command,
            Command::Enqueue { .. }
                | Command::RemoveQueue { .. }
                | Command::RemoveQueueEntries { .. }
                | Command::MoveQueue { .. }
                | Command::MoveQueueEntries { .. }
                | Command::ClearQueue
                | Command::RandomizeQueue
                | Command::DeduplicateQueue
                | Command::RemoveTracks { .. }
                | Command::RemoveMissingTracks
        );
        let persist_config = matches!(
            &command,
            Command::Volume { .. }
                | Command::Shuffle { .. }
                | Command::Repeat { .. }
                | Command::Device { .. }
                | Command::Configure { .. }
        );
        match command {
            Command::Status | Command::Overview => return Ok(()),
            Command::Scan { paths, force } => self.scan(paths, None, force)?,
            Command::Play { track_id } => {
                self.track(track_id)?;
                let existing = self
                    .state
                    .queue
                    .iter()
                    .find(|q| q.track_id == track_id)
                    .map(|q| q.id);
                let id = if let Some(id) = existing {
                    id
                } else {
                    self.enqueue(&[track_id])?;
                    self.state.queue.last().unwrap().id
                };
                self.start(id, true)?;
            }
            Command::PlayQueue { queue_id } => self.start(queue_id, true)?,
            Command::PlayPlaylist { playlist_id } => {
                let ids = self
                    .state
                    .playlists
                    .iter()
                    .find(|p| p.id == playlist_id)
                    .context("Playlist not found")?
                    .entries
                    .iter()
                    .map(|e| e.track_id)
                    .collect::<Vec<_>>();
                if ids.is_empty() {
                    bail!("Playlist is empty");
                }
                self.stop()?;
                self.state.queue = Arc::new(Vec::new());
                self.state.current_queue_id = None;
                self.played.clear();
                self.played_cursor = 0;
                self.enqueue(&ids)?;
                self.start(self.state.queue[0].id, true)?;
            }
            Command::Resume => {
                if self.state.status == PlaybackStatus::Stopped {
                    let id = self
                        .state
                        .current_queue_id
                        .or_else(|| self.state.queue.first().map(|q| q.id))
                        .context("Queue is empty")?;
                    self.start(id, true)?;
                } else {
                    self.audio(AudioCommand::Pause(false))?;
                    self.state.status = PlaybackStatus::Playing;
                }
            }
            Command::Pause => {
                if self.state.status == PlaybackStatus::Playing {
                    self.audio(AudioCommand::Pause(true))?;
                    self.state.status = PlaybackStatus::Paused;
                }
            }
            Command::Toggle => {
                return self.command(if self.state.status == PlaybackStatus::Playing {
                    Command::Pause
                } else {
                    Command::Resume
                });
            }
            Command::Stop => self.stop()?,
            Command::Next => self.advance(false)?,
            Command::Previous => self.previous()?,
            Command::Seek { seconds } => self.seek(seconds)?,
            Command::Volume { value } => {
                if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                    bail!("Volume must be between 0 and 1");
                }
                self.audio(AudioCommand::Volume(value))?;
                self.state.volume = value;
            }
            Command::Enqueue { track_ids } => self.enqueue(&track_ids)?,
            Command::RemoveQueue { queue_id } => {
                if !self.state.queue.iter().any(|q| q.id == queue_id) {
                    bail!("Queue entry not found");
                }
                if self.state.current_queue_id == Some(queue_id) {
                    self.stop()?;
                    self.state.current_queue_id = None;
                }
                Arc::make_mut(&mut self.state.queue).retain(|q| q.id != queue_id);
                self.prune_queue_history();
            }
            Command::RemoveQueueEntries { queue_ids } => self.remove_queue_entries(&queue_ids)?,
            Command::MoveQueue { queue_id, index } => {
                let queue = Arc::make_mut(&mut self.state.queue);
                let old = queue
                    .iter()
                    .position(|q| q.id == queue_id)
                    .context("Queue entry not found")?;
                if index >= queue.len() {
                    bail!("Queue target position out of range");
                }
                let entry = queue.remove(old);
                queue.insert(index, entry);
            }
            Command::MoveQueueEntries { queue_ids, index } => {
                self.move_queue_entries(&queue_ids, index)?;
            }
            Command::ClearQueue => {
                self.stop()?;
                self.state.queue = Arc::new(Vec::new());
                self.state.current_queue_id = None;
                self.played.clear();
                self.played_cursor = 0;
                self.shuffle_bag.clear();
            }
            Command::RandomizeQueue => self.randomize_queue(&mut rand::rng()),
            Command::DeduplicateQueue => self.deduplicate_queue(),
            Command::Shuffle { enabled } => {
                self.state.shuffle = enabled;
                self.shuffle_bag.clear();
                self.played.clear();
                self.played_cursor = 0;
                if let Some(id) = self.state.current_queue_id {
                    self.played.push(id);
                    self.played_cursor = 1;
                }
            }
            Command::Repeat { mode } => self.state.repeat = mode,
            Command::CreatePlaylist { name } => {
                self.store.create_playlist(&name)?;
                self.state.playlists = Arc::new(self.store.playlists()?);
            }
            Command::CreatePlaylistWithTracks { name, track_ids } => {
                let playlist_id = self.store.create_playlist(&name)?;
                self.store.add_playlist(playlist_id, &track_ids)?;
                self.state.playlists = Arc::new(self.store.playlists()?);
            }
            Command::RenamePlaylist { playlist_id, name } => {
                self.store.rename_playlist(playlist_id, &name)?;
                self.state.playlists = Arc::new(self.store.playlists()?);
            }
            Command::DeletePlaylist { playlist_id } => {
                self.store.delete_playlist(playlist_id)?;
                self.state.playlists = Arc::new(self.store.playlists()?);
            }
            Command::AddPlaylist {
                playlist_id,
                track_ids,
            } => {
                self.store.add_playlist(playlist_id, &track_ids)?;
                self.state.playlists = Arc::new(self.store.playlists()?);
            }
            Command::RemovePlaylistEntry { entry_id } => {
                self.store.remove_playlist_entry(entry_id)?;
                self.state.playlists = Arc::new(self.store.playlists()?);
            }
            Command::MovePlaylistEntry { entry_id, index } => {
                self.store.move_playlist_entry(entry_id, index)?;
                self.state.playlists = Arc::new(self.store.playlists()?);
            }
            Command::ImportPlaylist { path, name } => {
                if self.state.scanning {
                    bail!("A library scan is already running");
                }
                let items = library::import_playlist(&path)?;
                let paths = items
                    .iter()
                    .map(|item| item.path.clone())
                    .collect::<Vec<_>>();
                let name = name.unwrap_or_else(|| {
                    path.file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                });
                self.scan(paths, Some((name, items)), false)?;
            }
            Command::ExportPlaylist { playlist_id, path } => {
                let playlist = self
                    .state
                    .playlists
                    .iter()
                    .find(|p| p.id == playlist_id)
                    .context("Playlist not found")?;
                library::export_m3u(&path, playlist, &self.state.library)?;
            }
            Command::EditTrack {
                track_id,
                title,
                artist,
                album,
            } => {
                self.store.edit_track(track_id, &title, &artist, &album)?;
                self.library.update(track_id, |track| {
                    track.title = title;
                    track.artist = artist;
                    track.album = album;
                })?;
            }
            Command::RemoveTracks { track_ids } => {
                for id in &track_ids {
                    self.track(*id)?;
                }
                if self
                    .state
                    .current_track()
                    .is_some_and(|track| track_ids.contains(&track.id))
                {
                    self.stop()?;
                    self.state.current_queue_id = None;
                }
                self.store.remove_tracks(&track_ids)?;
                Arc::make_mut(&mut self.state.queue).retain(|q| !track_ids.contains(&q.track_id));
                self.prune_queue_history();
                self.reload(true)?;
            }
            Command::SetFavorite {
                track_ids,
                favorite,
            } => {
                self.store.set_favorite(&track_ids, favorite)?;
                for track_id in track_ids {
                    self.library
                        .update(track_id, |track| track.favorite = favorite)?;
                }
            }
            Command::RemoveMissingTracks => {
                let track_ids = self
                    .state
                    .library
                    .iter()
                    .filter(|track| track.missing)
                    .map(|track| track.id)
                    .collect();
                return self.command(Command::RemoveTracks { track_ids });
            }
            Command::Device { name } => {
                if let Some(name) = &name
                    && !self.state.devices.contains(name)
                {
                    bail!("Output device not found: {name}");
                }
                self.audio(AudioCommand::OutputSettings {
                    device: name.clone(),
                    auto_mix: self.state.config.pipewire_auto_mix,
                })?;
                self.state.selected_device = name;
            }
            Command::Analysis { enabled } => {
                self.audio(AudioCommand::Analysis(enabled))?;
                return Ok(());
            }
            Command::DismissError => {
                self.state.last_error = None;
                return Ok(());
            }
            Command::Configure { config } => {
                config.validate()?;
                if let Some(device) = &config.output_device
                    && !self.state.devices.contains(device)
                {
                    bail!("Output device not found: {device}");
                }
                let enabling_ffmpeg = config.ffmpeg_enabled && !self.state.config.ffmpeg_enabled;
                let ffmpeg_status = if !config.ffmpeg_enabled {
                    "disabled".to_owned()
                } else if enabling_ffmpeg {
                    audio::ffmpeg_status()
                        .context("FFmpeg extension audio decoding is unavailable")?
                } else {
                    self.state.ffmpeg_status.clone()
                };
                config.save(&self.state.config_path)?;
                self.audio(AudioCommand::MediaReadBuffer(config.media_read_buffer_mb))?;
                self.audio(AudioCommand::Volume(config.volume))?;
                self.audio(AudioCommand::AnalysisSettings((&config).into()))?;
                self.audio(AudioCommand::FfmpegEnabled(config.ffmpeg_enabled))?;
                self.audio(AudioCommand::OutputSettings {
                    device: config.output_device.clone(),
                    auto_mix: config.pipewire_auto_mix,
                })?;
                self.state.selected_device = config.output_device.clone();
                self.state.volume = config.volume;
                if self.state.shuffle != config.shuffle {
                    self.shuffle_bag.clear();
                    self.played.clear();
                    self.played_cursor = 0;
                }
                self.state.shuffle = config.shuffle;
                self.state.repeat = config.repeat;
                self.state.ffmpeg_status = ffmpeg_status;
                self.state.config = Arc::new(config);
            }
            Command::ShowWindow => return self.request_raise(),
            Command::MprisStatus { status } => {
                self.state.mpris_status = status;
                return Ok(());
            }
            Command::SeekQueue { queue_id, seconds } => {
                if self.state.current_queue_id == Some(queue_id) {
                    self.seek(seconds)?;
                }
                return Ok(());
            }
            Command::OptimizeDatabase => {
                self.state.database_optimization = None;
                if self.state.status != PlaybackStatus::Stopped || self.state.scanning {
                    bail!("Database optimization requires stopped playback and no active scan");
                }
                self.state.database_optimization = Some(self.store.optimize()?);
                // Saving playback here would immediately create new WAL pages
                // after maintenance has truncated them.
                return Ok(());
            }
            Command::Shutdown => {
                self.state.shutting_down = true;
                self.stop()?;
            }
        }
        self.queue_dirty |= persist_queue;
        self.config_dirty |= persist_config;
        self.save(false)
    }
}
