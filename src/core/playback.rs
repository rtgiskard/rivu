use super::*;
use anyhow::bail;

/// One owner for occurrence intent, navigation and the physical listening source.
/// QueueState remains the constant-size publication; no queue is retained here.
#[derive(Default)]
pub(super) struct PlaybackOwner {
    pub(super) generation: u64,
    // Audio seek identity is independent of the published clock correction revision.
    pub(super) progress_seek_revision: u64,
    pub(super) session: Option<PlaybackSession>,
    pub(super) retired: Option<RetiredPlayback>,
    pub(super) write_deferred: bool,
    pub(super) history_pending: bool,
    pub(super) pending_navigation: Option<PlaybackStatus>,
    pub(super) projection_stale: bool,
    pub(super) selection_pending: bool,
    pub(super) history: Vec<u64>,
    pub(super) history_cursor: usize,
    pub(super) processing_resume_generation: Option<u64>,
    pub(super) output_prior_underruns: u64,
    pub(super) output_prior_missing_seconds: f64,
    live_source: Option<u64>,
    physically_closed_source: Option<u64>,
    autoplay: Option<Autoplay>,
}

impl PlaybackOwner {
    pub(super) fn new(generation: u64) -> Self {
        Self {
            generation,
            ..Self::default()
        }
    }

    pub(super) fn admit_source(&mut self) {
        self.processing_resume_generation = None;
        self.progress_seek_revision = 0;
        self.live_source = Some(self.generation);
        self.physically_closed_source = None;
    }

    fn reject_start(&mut self) {
        // Listening was rejected before its first durable fact. Keep the live
        // source context until physical retirement, but never let a queued
        // Started or final cached snapshot revive this rejected session.
        debug_assert!(self.session.as_ref().is_none_or(|session| !session.started));
        self.session = None;
    }

    fn confirm_source_closed(&mut self, generation: u64) {
        if self.live_source == Some(generation) {
            self.physically_closed_source = self.live_source.take();
        }
    }

    pub(super) fn reset_history(&mut self) {
        self.history.clear();
        self.history_cursor = 0;
    }

    pub(super) fn record_history(&mut self, id: u64) {
        const MAX_NAVIGATION_HISTORY: usize = 4096;
        self.history.truncate(self.history_cursor);
        self.history.push(id);
        if self.history.len() > MAX_NAVIGATION_HISTORY {
            self.history
                .drain(..self.history.len() - MAX_NAVIGATION_HISTORY);
        }
        self.history_cursor = self.history.len();
    }

    pub(super) fn select_occurrence(&mut self, id: u64, history: QueueHistory) {
        self.physically_closed_source = None;
        self.selection_pending = true;
        match history {
            QueueHistory::Record => self.record_history(id),
            QueueHistory::Preserve => {}
            QueueHistory::Restore(cursor) => self.history_cursor = cursor,
        }
    }

    pub(super) fn replace_occurrences(&mut self) {
        self.selection_pending = false;
        self.physically_closed_source = None;
        self.reset_history();
    }
}

/// Repeat All search after natural advancement. Occurrences, not tracks, are
/// excluded, so duplicates remain independent choices.
#[derive(Default)]
struct Autoplay {
    attempted: std::collections::HashSet<u64>,
    skipped: usize,
    last_failure: Option<String>,
}

impl Autoplay {
    fn notice(&self, exhausted: bool) -> Option<String> {
        self.last_failure.as_ref().map(|reason| {
            let outcome = if exhausted {
                "; no playable tracks found"
            } else {
                ""
            };
            format!(
                "Skipped {} unavailable queue occurrence(s){outcome}. Last failure: {reason}",
                self.skipped
            )
        })
    }
}

pub(super) enum QueueHistory {
    Record,
    Preserve,
    Restore(usize),
}

pub(super) struct SourceReplacement {
    pub(super) generation: u64,
    pub(super) heard: f64,
    pub(super) position: f64,
    pub(super) status: PlaybackStatus,
}

pub(super) struct PlaybackSession {
    // The source admitted by Load, shared with the initial current-track projection.
    // Library refreshes may update metadata but cannot replace this session's source.
    pub(super) loaded_track: Arc<Track>,
    pub(super) source: Option<(crate::store::PlaybackSource, std::fs::Metadata)>,
    pub(super) duration: Option<f64>,
    pub(super) pending_bitrate: Option<(u64, bool)>,
    pub(super) heard: f64,
    pub(super) started: bool,
    pub(super) counted: bool,
    pub(super) last_heard_at: Option<i64>,
    // A successful activity write owns this timestamp, independently of history projection.
    pub(super) activity_committed_at: Option<i64>,
    // Failed active writes are retried by a related producer event, not every business event.
    pub(super) activity_pending: bool,
    pub(super) history_removed: bool,
    pub(super) paused_at: Option<i64>,
}
impl PlaybackSession {
    fn count_pending(&self, threshold_percent: f64) -> bool {
        self.started
            && !self.counted
            && self
                .duration
                .is_some_and(|duration| self.heard > duration * threshold_percent / 100.0)
    }

    fn accept_duration(&mut self, duration: Option<f64>, replace: bool) {
        if replace || self.duration.is_none() {
            self.duration = duration
                .filter(|duration| duration.is_finite() && *duration > 0.0)
                .or(self.duration);
        }
    }

    fn accept_progress(&mut self, heard: f64) -> bool {
        self.started && self.record_confirmed_heard(heard)
    }

    fn record_confirmed_heard(&mut self, heard: f64) -> bool {
        let heard_at = self.paused_at.map_or_else(now, |at| now().min(at));
        self.record_heard(heard, heard_at)
    }

    pub(super) fn record_heard(&mut self, heard: f64, heard_at: i64) -> bool {
        if !heard.is_finite() || heard <= self.heard {
            return false;
        }
        self.heard = heard;
        self.last_heard_at = Some(
            self.last_heard_at
                .map_or(heard_at, |previous| previous.max(heard_at)),
        );
        true
    }
}
pub(super) struct RetiredPlayback {
    pub(super) stats: PlaybackSession,
    /// The fenced source still owes its final heard-time snapshot.
    pub(super) pending_close: Option<u64>,
}

fn modified_ns(metadata: &std::fs::Metadata) -> Option<i64> {
    rivu_utils::regular_file::modified_unix_ns(metadata.modified().ok()?)
        .filter(|ns| *ns != i64::MIN)
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

// Persistence takes the source's facts explicitly. Core owns publication and
// navigation, neither of which may be borrowed by a retired source.
fn accept_session_start(
    store: &Store,
    session: &mut PlaybackSession,
    info: &MediaInfo,
    deferred: bool,
) -> Result<Option<crate::store::TrackStats>> {
    if session.started {
        return Ok(None);
    }
    let played_at = session.paused_at.map_or_else(now, |at| now().min(at));
    let stats = if session.history_removed || deferred {
        None
    } else {
        Some(store.mark_played(session.loaded_track.id, played_at)?)
    };
    // A failed initial write must not accept any listening facts.
    session.started = true;
    session.last_heard_at = Some(played_at);
    session.activity_committed_at = stats.as_ref().map(|_| played_at);
    session.accept_duration(info.duration, true);
    Ok(stats)
}

fn persist_session_count(
    store: &Store,
    session: &mut PlaybackSession,
    threshold_percent: f64,
) -> Result<Option<crate::store::TrackStats>> {
    if !session.count_pending(threshold_percent) {
        return Ok(None);
    }
    let stats = store.increment_play_count(session.loaded_track.id)?;
    // Commit precedes publication: projection failure must never recount.
    session.counted = true;
    Ok(Some(stats))
}

fn persist_session_activity(
    store: &Store,
    session: &mut PlaybackSession,
) -> Result<Option<crate::store::TrackStats>> {
    let at = session.last_heard_at.filter(|at| {
        session.started
            && !session.history_removed
            && session
                .activity_committed_at
                .is_none_or(|committed| committed < *at)
    });
    let Some(at) = at else {
        session.activity_pending = false;
        return Ok(None);
    };
    session.activity_pending = true;
    let stats = store.mark_played(session.loaded_track.id, at)?;
    session.activity_committed_at = Some(at);
    session.activity_pending = false;
    Ok(Some(stats))
}

fn persist_session_bitrate(
    store: &Store,
    session: &mut PlaybackSession,
    deferred: bool,
) -> Option<(u64, bool)> {
    if deferred {
        return None;
    }
    let (bitrate, exact) = session.pending_bitrate.take()?;
    let (source, before) = session.source.as_ref()?;
    if !std::fs::metadata(&source.path).is_ok_and(|after| same_source_file(before, &after)) {
        return None;
    }
    match store.upgrade_bitrate(source, bitrate, exact) {
        Ok(true) => Some((bitrate, exact)),
        Ok(false) => None,
        Err(error) => {
            tracing::warn!(error = ?error, "playback_bitrate_not_saved");
            None
        }
    }
}

fn accept_session_bitrate(session: &mut PlaybackSession, bitrate: u64, exact: bool) {
    if bitrate > 0 && (exact || !session.pending_bitrate.is_some_and(|(_, exact)| exact)) {
        session.pending_bitrate = Some((bitrate, exact));
    }
}

fn accept_session_end(session: &mut PlaybackSession, duration: Option<f64>, bitrate: Option<u64>) {
    if !session.started {
        return;
    }
    session.accept_duration(duration, false);
    if session.loaded_track.cue.is_none()
        && let Some(bitrate) = bitrate
    {
        accept_session_bitrate(session, bitrate, true);
    }
}

impl Core {
    fn accept_output_diagnostics(
        &mut self,
        mut state: Arc<rivu_protocol::model::AudioOutputState>,
    ) {
        let current_source = state.source_generation == self.transport.generation;
        let closing_source = !state.active
            && self.state.playback.status == PlaybackStatus::Stopped
            && self.transport.live_source == Some(state.source_generation)
            && self
                .state
                .playback
                .audio_output
                .as_ref()
                .is_some_and(|old| old.source_generation == state.source_generation);
        if (!current_source && !closing_source)
            || self
                .state
                .playback
                .audio_output
                .as_ref()
                .is_some_and(|old| {
                    old.source_generation == state.source_generation
                        && (old.segment > state.segment
                            || (old.segment == state.segment && !old.active && state.active))
                })
        {
            return;
        }
        let totals = Arc::make_mut(&mut state);
        totals.session_underruns = totals
            .session_underruns
            .saturating_add(self.transport.output_prior_underruns);
        totals.session_missing_seconds += self.transport.output_prior_missing_seconds;
        self.state.playback.audio_output = Some(state);
    }

    pub(in crate::core) fn playback_metadata(track: &Track) -> Arc<audio::PlaybackMetadata> {
        Arc::new(audio::PlaybackMetadata {
            title: track.title.clone(),
            artist: track.artist.clone(),
            album: track.album.clone(),
            replaygain_track_gain: track.replaygain_track_gain,
        })
    }

    fn audio_settings_failed(&mut self, event: AudioEvent) {
        match event {
            AudioEvent::OutputSettingsFailed {
                request,
                device,
                auto_mix,
                message,
            } if request == self.output_settings_request
                && device == self.preferences.live().output_device
                && auto_mix == self.preferences.settings().pipewire_auto_mix =>
            {
                self.state.system.last_error = Some(message);
            }
            AudioEvent::EqualizerSettingsFailed {
                generation,
                config,
                message,
            } if generation.is_none_or(|value| value == self.transport.generation)
                && *config == self.playback_equalizer() =>
            {
                self.state.system.last_error = Some(message);
            }
            _ => {}
        }
    }

    pub(in crate::core) fn audio_event(&mut self, event: AudioEvent) -> Result<()> {
        match event {
            AudioEvent::OutputDiagnostics(state) => self.accept_output_diagnostics(state),
            AudioEvent::Started { generation, info } if generation == self.transport.generation => {
                self.transport.processing_resume_generation = None;
                let accepting_start = self
                    .transport
                    .session
                    .as_ref()
                    .is_some_and(|session| !session.started);
                if let Err(error) = self.playback_started(info) {
                    self.stop().with_context(|| format!("{error:#}"))?;
                    return Err(error);
                }
                if accepting_start && let Some(autoplay) = self.transport.autoplay.as_mut() {
                    // A real start ends this search; a later read failure of
                    // this naturally reached track starts a fresh one.
                    *autoplay = Autoplay::default();
                }
            }
            AudioEvent::Progress {
                generation,
                seek_revision,
                position_seconds,
                listened_seconds,
            } if generation == self.transport.generation => {
                // Heard time is cumulative; only position loses authority after a seek.
                let position = (seek_revision == self.transport.progress_seek_revision)
                    .then_some(position_seconds);
                self.playback_progress(position, listened_seconds)?;
            }
            AudioEvent::Ended {
                generation,
                completed_duration,
                bitrate_bps,
            } if generation == self.transport.generation => {
                // The worker claims EOF against seek admission before closing.
                // A current-generation Ended is therefore authoritative for navigation.
                self.playback_ended(completed_duration, bitrate_bps);
                let status = self.state.playback.status;
                self.transport.autoplay = None;
                self.stop_playback()?;
                self.advance_with_status(true, status)?;
            }
            AudioEvent::SeekRejected {
                revision,
                snapshot,
                message,
            } if snapshot.generation == self.transport.generation
                && revision == self.state.playback.seek_revision =>
            {
                self.state.playback.position = snapshot.position_seconds;
                // Publish the correction to clock consumers without reusing an intent ID.
                self.state.playback.seek_revision = revision.wrapping_add(1);
                self.state.system.last_error = Some(message);
                self.playback_progress(Some(snapshot.position_seconds), snapshot.listened_seconds)?;
            }
            AudioEvent::Failed {
                generation,
                kind,
                message,
            } if generation == self.transport.generation => {
                let resume_failed = self.transport.processing_resume_generation == Some(generation);
                if kind == audio::AudioFailureKind::Source
                    && self.transport.autoplay.is_some()
                    && self.preferences.live().repeat == RepeatMode::All
                    && !resume_failed
                {
                    let status = self.state.playback.status;
                    let failed = self.state.queue.current_id;
                    let path = self.state.current_track().map(|track| track.path.clone());
                    self.stop_for_replacement()
                        .with_context(|| message.clone())?;
                    // An absent or empty music folder affects every track
                    // below it: stop rather than walk the queue.
                    if let Some(path) = &path
                        && let Some(root) = self.unavailable_root(path)
                    {
                        self.transport.autoplay = None;
                        self.state.system.last_error = Some(format!(
                            "Music folder unavailable: {}; cannot open {}",
                            root.display(),
                            path.display()
                        ));
                        return Ok(());
                    }
                    let autoplay = self.transport.autoplay.as_mut().expect("search is active");
                    autoplay.attempted.extend(failed);
                    autoplay.skipped += 1;
                    autoplay.last_failure = Some(path.map_or_else(
                        || message.clone(),
                        |path| format!("{message} ({})", path.display()),
                    ));
                    self.state.system.last_error = autoplay.notice(false);
                    self.continue_autoplay(status)?;
                } else {
                    self.stop().with_context(|| message.clone())?;
                    if resume_failed {
                        self.processing_bypass(true)?;
                    }
                    self.state.system.last_error = Some(message);
                }
            }
            event @ (AudioEvent::OutputSettingsFailed { .. }
            | AudioEvent::EqualizerSettingsFailed { .. }) => self.audio_settings_failed(event),
            event @ (AudioEvent::Started { generation, .. }
            | AudioEvent::Progress { generation, .. }
            | AudioEvent::Ended { generation, .. })
                if self
                    .transport
                    .retired
                    .as_ref()
                    .is_some_and(|retired| retired.pending_close == Some(generation)) =>
            {
                self.retired_audio_event(event)?;
            }
            AudioEvent::DecoderStopped { snapshot }
                if self.transport.live_source == Some(snapshot.generation) =>
            {
                // A late physical close may settle an uncertain stop. It cannot
                // retire a newer source, advance navigation or fence twice.
                self.transport.confirm_source_closed(snapshot.generation);
                if snapshot.generation == self.transport.generation {
                    self.finish_playback(Ok(Some(snapshot)), Ok(()))?;
                } else {
                    self.accept_retired_snapshot(snapshot);
                    self.state.playback.stopped_source_generation =
                        self.transport.physically_closed_source;
                    self.flush_deferred_playback()?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    pub(in crate::core) fn playback_source(
        &self,
        track: &Track,
        track_metadata: Option<std::fs::Metadata>,
    ) -> Result<Option<(crate::store::PlaybackSource, std::fs::Metadata)>> {
        if track.bitrate_bps.is_some() && track.bitrate_exact {
            return Ok(None);
        }
        let Some(source) = self.store.playback_source(track)? else {
            return Ok(None);
        };
        let metadata = track_metadata.or_else(|| std::fs::metadata(&source.path).ok());
        let Some(metadata) = metadata else {
            return Ok(None);
        };
        Ok((metadata.is_file()
            && source.file_size == metadata.len()
            && Some(source.modified_ns) == modified_ns(&metadata))
        .then_some((source, metadata)))
    }

    fn publish_playback_bitrate(&mut self, track_id: i64, bitrate: u64, exact: bool) {
        let library = &mut self.state.library;
        library.bitrate_revision = library.bitrate_revision.wrapping_add(1);
        if let Some(current) = self.state.current_track.as_mut()
            && current.id == track_id
        {
            let current = Arc::make_mut(current);
            current.bitrate_bps = Some(bitrate);
            current.bitrate_exact = exact;
        }
    }

    fn playback_ended(&mut self, duration: Option<f64>, bitrate: Option<u64>) {
        let deferred = self.database_maintenance_active();
        let Some(session) = self
            .transport
            .session
            .as_mut()
            .filter(|session| session.started)
        else {
            return;
        };
        // EOF resolves only unknown duration, never a seek or final snapshot.
        accept_session_end(session, duration, bitrate);
        self.state.playback.duration = session.duration;
        let track_id = session.loaded_track.id;
        let bitrate = persist_session_bitrate(&self.store, session, deferred);
        self.transport.write_deferred |= session.pending_bitrate.is_some();
        if let Some((bitrate, exact)) = bitrate {
            self.publish_playback_bitrate(track_id, bitrate, exact);
        }
    }

    pub(in crate::core) fn start(&mut self, queue_id: u64, history: QueueHistory) -> Result<()> {
        self.start_with_status(queue_id, history, PlaybackStatus::Playing)
    }
    pub(in crate::core) fn start_with_status(
        &mut self,
        queue_id: u64,
        history: QueueHistory,
        status: PlaybackStatus,
    ) -> Result<()> {
        self.transport.autoplay = None;
        self.start_occurrence(queue_id, history, status, true)
    }

    /// The configured root containing `path` when it is absent, unreadable or
    /// empty, as with an unmounted removable or network volume.
    fn unavailable_root(&self, path: &std::path::Path) -> Option<std::path::PathBuf> {
        let roots = library::normalize_roots(&self.preferences.settings().library_roots).ok()?;
        let root = roots
            .into_iter()
            .filter(|root| path.starts_with(root))
            .max_by_key(|root| root.as_os_str().len())?;
        let available = std::fs::read_dir(&root)
            .is_ok_and(|mut entries| entries.next().is_some_and(|entry| entry.is_ok()));
        (!available).then_some(root)
    }

    /// Manual starts check for a missing file up front. Autoplay leaves it to
    /// the decoder, so every source failure reaches the same `Failed` handler.
    fn start_occurrence(
        &mut self,
        queue_id: u64,
        history: QueueHistory,
        status: PlaybackStatus,
        check_missing: bool,
    ) -> Result<()> {
        self.ensure_queue_control_ready()?;
        let (track_id, position) = self
            .store
            .queue_location(queue_id)?
            .context("Queue entry not found")?;
        let track = self.track(track_id)?;
        // Independent controls do not depend on database recovery. Only a new
        // playback owner must settle the previous producer before replacing it.
        if self.transport.retired.is_some() {
            self.flush_deferred_playback()?;
        }
        self.stop_for_replacement()?;
        anyhow::ensure!(
            self.transport.retired.is_none(),
            "Previous audio source has not confirmed its final stop"
        );
        // Retiring Started/Ended bookkeeping can enrich this same track in the store.
        let track = Arc::new(self.track(track.id)?);
        let check_file = check_missing && status != PlaybackStatus::Stopped;
        let track_metadata = if check_file {
            std::fs::metadata(&track.path).ok()
        } else {
            None
        };
        if check_file
            && !track_metadata
                .as_ref()
                .is_some_and(std::fs::Metadata::is_file)
        {
            let path = track.path.display().to_string();
            let root = self.unavailable_root(&track.path);
            self.accept_loaded_occurrence(
                queue_id,
                position,
                track,
                PlaybackStatus::Stopped,
                history,
            )?;
            if let Some(root) = root {
                bail!(
                    "Music folder unavailable: {}; cannot open {path}",
                    root.display()
                );
            }
            bail!("Missing audio file: {path}");
        }
        if status != PlaybackStatus::Stopped {
            let source = self
                .playback_source(&track, track_metadata)
                .unwrap_or_else(|error| {
                    tracing::warn!(error = ?error, "playback_bitrate_identity_unavailable");
                    None
                });
            self.engine.send(AudioCommand::Load {
                path: track.path.clone(),
                decoder_hint: track.decoder_hint,
                metadata: Self::playback_metadata(&track),
                range: track
                    .cue
                    .as_ref()
                    .map(|cue| rivu_protocol::audio::PlaybackRange {
                        start_seconds: cue.start_seconds(),
                        end_seconds: cue.end_seconds(),
                    }),
                generation: self.transport.generation,
                start_seconds: 0.0,
                paused: status == PlaybackStatus::Paused,
            })?;
            self.transport.admit_source();
            self.transport.session = Some(PlaybackSession {
                loaded_track: Arc::clone(&track),
                source,
                duration: track
                    .duration
                    .filter(|duration| duration.is_finite() && *duration > 0.0),
                pending_bitrate: None,
                heard: 0.0,
                started: false,
                counted: false,
                last_heard_at: None,
                activity_committed_at: None,
                activity_pending: false,
                history_removed: false,
                paused_at: (status == PlaybackStatus::Paused).then(now),
            });
        }
        self.accept_loaded_occurrence(queue_id, position, track, status, history)
    }
    pub(in crate::core) fn count_play(&mut self) -> Result<()> {
        if self.database_maintenance_active() {
            self.transport.write_deferred = true;
            return Ok(());
        }
        let Some(session) = self.transport.session.as_mut() else {
            return Ok(());
        };
        let track_id = session.loaded_track.id;
        let stats = persist_session_count(
            &self.store,
            session,
            self.preferences.settings().play_count_threshold_percent,
        )?;
        if let Some(stats) = stats {
            self.project_playback_stats(track_id, stats);
        }
        Ok(())
    }
    pub(in crate::core) fn playback_started(&mut self, info: MediaInfo) -> Result<()> {
        let deferred = self.database_maintenance_active();
        let Some(session) = self
            .transport
            .session
            .as_mut()
            .filter(|session| !session.started)
        else {
            return Ok(());
        };
        let stats = match accept_session_start(&self.store, session, &info, deferred) {
            Ok(stats) => stats,
            Err(error) => {
                self.transport.reject_start();
                return Err(error);
            }
        };
        let track_id = session.loaded_track.id;
        self.state.playback.last_heard_at = session.last_heard_at;
        self.state.playback.duration = session.duration;
        if let Some(bitrate) = info.bitrate_bps {
            accept_session_bitrate(session, bitrate, info.bitrate_exact);
        }
        let bitrate = persist_session_bitrate(&self.store, session, deferred);
        self.transport.write_deferred |= deferred;
        if let Some(stats) = stats {
            self.transport.history_pending |= stats.changed;
            self.project_playback_stats(track_id, stats);
            self.refresh_playback_history();
        }
        let result = self.count_play();
        // Bitrate publication is independent of statistics projection failures.
        if let Some((bitrate, exact)) = bitrate {
            self.publish_playback_bitrate(track_id, bitrate, exact);
        }
        result
    }
    pub(in crate::core) fn playback_progress(
        &mut self,
        position: Option<f64>,
        heard: f64,
    ) -> Result<()> {
        if let Some(session) = self.transport.session.as_mut()
            && session.started
        {
            if let Some(position) = position.filter(|position| position.is_finite()) {
                self.state.playback.position = position;
            }
            if session.accept_progress(heard) {
                self.state.playback.last_heard_at = session.last_heard_at;
            }
            if session.activity_pending {
                // A producer event grants one recovery attempt. Unrelated business
                // events cannot turn a failed activity write into an endless loop.
                self.flush_deferred_playback()?;
            } else {
                self.count_play()?;
            }
        }
        Ok(())
    }
    fn accept_retired_snapshot(&mut self, snapshot: audio::AudioSnapshot) {
        let Some(retired) = self
            .transport
            .retired
            .as_mut()
            .filter(|retired| retired.pending_close == Some(snapshot.generation))
        else {
            return;
        };
        if retired
            .stats
            .record_confirmed_heard(snapshot.listened_seconds)
        {
            self.state.playback.last_heard_at = retired.stats.last_heard_at;
        }
        retired.pending_close = None;
        self.transport.write_deferred = true;
    }

    fn retired_audio_event(&mut self, event: AudioEvent) -> Result<()> {
        let deferred = self.database_maintenance_active();
        let retired = self
            .transport
            .retired
            .as_mut()
            .expect("retired producer was checked");
        let session = &mut retired.stats;
        let stats = match event {
            AudioEvent::Started { info, .. } if !session.started => {
                let stats = match accept_session_start(&self.store, session, &info, deferred) {
                    Ok(stats) => stats,
                    Err(error) => {
                        // Initial durable admission failed; later events cannot revive it.
                        self.transport.retired = None;
                        return Err(error);
                    }
                };
                self.state.playback.last_heard_at = session.last_heard_at;
                if let Some(bitrate) = info.bitrate_bps {
                    accept_session_bitrate(session, bitrate, info.bitrate_exact);
                }
                stats
            }
            AudioEvent::Progress {
                listened_seconds, ..
            } => {
                if session.accept_progress(listened_seconds) {
                    self.state.playback.last_heard_at = session.last_heard_at;
                }
                None
            }
            AudioEvent::Ended {
                completed_duration,
                bitrate_bps,
                ..
            } => {
                accept_session_end(session, completed_duration, bitrate_bps);
                None
            }
            _ => None,
        };
        let track_id = session.loaded_track.id;
        let bitrate = persist_session_bitrate(&self.store, session, deferred);
        self.transport.write_deferred = true;
        if let Some(stats) = stats {
            self.transport.history_pending |= stats.changed;
            self.project_playback_stats(track_id, stats);
        }
        if let Some((bitrate, exact)) = bitrate {
            self.publish_playback_bitrate(track_id, bitrate, exact);
        }
        // Retired events never publish duration/position or run transport controls.
        self.flush_deferred_playback()
    }

    pub(in crate::core) fn retiring_audio_event(
        &mut self,
        generation: u64,
        event: AudioEvent,
    ) -> Result<()> {
        match event {
            AudioEvent::OutputDiagnostics(state) => self.accept_output_diagnostics(state),
            event @ (AudioEvent::Started { generation, .. }
            | AudioEvent::Progress { generation, .. }
            | AudioEvent::Ended { generation, .. })
                if self
                    .transport
                    .retired
                    .as_ref()
                    .is_some_and(|retired| retired.pending_close == Some(generation)) =>
            {
                self.retired_audio_event(event)?
            }
            AudioEvent::DecoderStopped { snapshot } => {
                self.transport.confirm_source_closed(snapshot.generation);
                if snapshot.generation == generation {
                    self.playback_progress(
                        Some(snapshot.position_seconds),
                        snapshot.listened_seconds,
                    )?;
                } else {
                    self.accept_retired_snapshot(snapshot);
                }
            }
            AudioEvent::Started {
                generation: event_generation,
                info,
            } if event_generation == generation => self.playback_started(info)?,
            AudioEvent::Progress {
                generation: event_generation,
                position_seconds,
                listened_seconds,
                ..
            } if event_generation == generation => {
                self.playback_progress(Some(position_seconds), listened_seconds)?;
            }
            AudioEvent::Ended {
                generation: event_generation,
                completed_duration,
                bitrate_bps,
            } if event_generation == generation => {
                self.playback_ended(completed_duration, bitrate_bps);
            }
            // Settings failures describe the current desired configuration, not
            // terminal playback control. Admit them before fencing this source.
            event @ (AudioEvent::OutputSettingsFailed { .. }
            | AudioEvent::EqualizerSettingsFailed { .. }) => self.audio_settings_failed(event),
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
        bookkeeping: Result<()>,
    ) -> Result<()> {
        let generation = self.transport.generation;
        // A request can beat a queued Started even though the worker has already
        // played audio. Acknowledged stops make that queued bookkeeping safe to
        // consume before generation fencing; errors still fence below.
        let bookkeeping = if snapshot.is_ok() {
            let drained = self.drain_playback_events(generation);
            bookkeeping.and(drained)
        } else {
            bookkeeping
        };
        if let Some(snapshot) = snapshot
            .as_ref()
            .ok()
            .and_then(|snapshot| snapshot.as_ref())
        {
            self.transport.confirm_source_closed(snapshot.generation);
            self.accept_retired_snapshot(*snapshot);
        }
        // A physical acknowledgement and a statistics commit are independent facts.
        // Unknown stop outcomes fence events but never manufacture a close receipt.
        let had_source = self.transport.live_source == Some(generation)
            || self.transport.session.is_some()
            || self.transport.physically_closed_source == Some(generation);
        if had_source {
            self.transport.generation = self.transport.generation.wrapping_add(1);
            self.state.playback.seek_revision = self.state.playback.seek_revision.wrapping_add(1);
        }
        self.clear_processing_audition("Audition resume cancelled because playback stopped");
        self.transport.processing_resume_generation = None;
        self.state.playback.status = PlaybackStatus::Stopped;
        self.state.playback.position = 0.0;
        self.state.playback.stopped_source_generation = self.transport.physically_closed_source;
        let pending_close = snapshot.as_ref().err().and(self.transport.live_source);
        let (snapshot, stopped) = match snapshot {
            Ok(snapshot) => (snapshot, Ok(())),
            Err(error) => (None, Err(error)),
        };
        if let Some(playback) = &mut self.transport.session
            && let Some(snapshot) = snapshot
            && snapshot.generation == generation
            && snapshot.listened_seconds.is_finite()
        {
            // Late paused confirmations still count as heard audio, but
            // their activity timestamp cannot cross the pause boundary.
            if playback.record_confirmed_heard(snapshot.listened_seconds) {
                self.state.playback.last_heard_at = playback.last_heard_at;
            }
        }
        if let Some(stats) = self.transport.session.take() {
            debug_assert!(self.transport.retired.is_none());
            self.transport.retired = Some(RetiredPlayback {
                stats,
                pending_close,
            });
            self.transport.write_deferred = true;
        }
        if stopped.is_err() || bookkeeping.is_err() {
            self.transport.pending_navigation = None;
        }
        // Retain the same bounded producer owner on ordinary stop failures as
        // during maintenance. Neither a projection error nor an activity write
        // may skip an independently eligible count or discard confirmed facts.
        let statistics = self.flush_deferred_playback();
        stopped.and(bookkeeping).and(statistics)
    }
    pub(in crate::core) fn stop_playback(&mut self) -> Result<()> {
        self.cancel_pending_processing_resume("Audition resume cancelled because playback stopped");
        let mut stop = self.engine.stop_request();
        let generation = self.transport.generation;
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
        self.finish_playback(snapshot, bookkeeping)
    }

    pub(in crate::core) fn stop(&mut self) -> Result<()> {
        self.transport.autoplay = None;
        self.stop_for_replacement()
    }

    fn stop_for_replacement(&mut self) -> Result<()> {
        // Stop relinquishes pending audition intent even without a physical source.
        // Prepared graph handoff consumes its intent before establishing a source.
        self.pending_processing_audition = None;
        self.transport.pending_navigation = None;
        let result = self.stop_playback();
        if result.is_err() {
            self.transport.autoplay = None;
        }
        result
    }

    pub(in crate::core) fn prepare_source_replacement(&mut self) -> Result<SourceReplacement> {
        let generation = self.transport.generation;
        let mut bookkeeping = Ok(());
        let mut stop = self.engine.stop_request();
        let snapshot = futures::executor::block_on(stop.wait(|event| {
            let result = self.retiring_audio_event(generation, event);
            if bookkeeping.is_ok() {
                bookkeeping = result;
            }
        }));
        self.engine.restore_events(stop.into_events());
        let snapshot = match snapshot {
            Ok(snapshot) if snapshot.generation == generation => {
                self.transport.confirm_source_closed(generation);
                snapshot
            }
            Ok(snapshot) => {
                self.finish_playback(
                    Ok(Some(snapshot)),
                    Err(anyhow::anyhow!(
                        "Audio stop snapshot belongs to a different playback generation"
                    )),
                )?;
                bail!("Audio source replacement was rejected");
            }
            Err(error) => {
                self.finish_playback(Err(error), bookkeeping)?;
                bail!("Audio source replacement could not stop its source");
            }
        };
        let drained = self.drain_playback_events(generation);
        let progress =
            self.playback_progress(Some(snapshot.position_seconds), snapshot.listened_seconds);
        if let Err(error) = bookkeeping.and(drained).and(progress) {
            self.finish_playback(Ok(Some(snapshot)), Err(error))?;
            bail!("Audio source replacement bookkeeping failed");
        }
        Ok(SourceReplacement {
            generation: generation.wrapping_add(1),
            heard: self
                .transport
                .session
                .as_ref()
                .map_or(0.0, |session| session.heard),
            position: self.state.playback.position,
            status: self.state.playback.status,
        })
    }

    pub(in crate::core) fn commit_source_replacement(
        &mut self,
        command: AudioCommand,
    ) -> Result<()> {
        if let Err(error) = self.engine.send_with_rejection(command, |command| {
            if let AudioCommand::GraphAudition { runner, .. } = command {
                self.processing.retire_runner(*runner);
            }
        }) {
            return self.finish_playback(Ok(None), Err(error));
        }
        if let Some(state) = self.state.playback.audio_output.take() {
            self.transport.output_prior_underruns = state.session_underruns;
            self.transport.output_prior_missing_seconds = state.session_missing_seconds;
        }
        self.transport.autoplay = None;
        self.transport.generation = self.transport.generation.wrapping_add(1);
        self.transport.admit_source();
        self.state.playback.seek_revision = self.state.playback.seek_revision.wrapping_add(1);
        self.state.playback.stopped_source_generation = None;
        Ok(())
    }

    pub(in crate::core) fn resume(&mut self) -> Result<()> {
        if self.state.playback.status == PlaybackStatus::Playing {
            return Ok(());
        }
        if self.state.playback.status == PlaybackStatus::Stopped {
            self.ensure_queue_control_ready()?;
            let id = match self.state.queue.current_id {
                Some(id) => id,
                None => match self.store.queue_id_at(0)? {
                    Some(id) => id,
                    None => return Ok(()),
                },
            };
            let history = if self
                .transport
                .history_cursor
                .checked_sub(1)
                .and_then(|index| self.transport.history.get(index))
                == Some(&id)
            {
                QueueHistory::Preserve
            } else {
                QueueHistory::Record
            };
            return self.start(id, history);
        }
        self.engine.send(AudioCommand::Pause(false))?;
        if let Some(session) = self.transport.session.as_mut() {
            session.paused_at = None;
        }
        self.state.playback.status = PlaybackStatus::Playing;
        Ok(())
    }

    pub(in crate::core) fn pause(&mut self) -> Result<()> {
        if self.state.playback.status != PlaybackStatus::Playing {
            return Ok(());
        }
        self.engine.send(AudioCommand::Pause(true))?;
        if let Some(session) = self.transport.session.as_mut() {
            session.paused_at.get_or_insert(now());
        }
        self.state.playback.status = PlaybackStatus::Paused;
        Ok(())
    }
    /// Load the next untried occurrence: a random or following position, then
    /// the first untried one after it. Failures return through `Failed`, so
    /// each attempt yields to the event loop.
    fn continue_autoplay(&mut self, status: PlaybackStatus) -> Result<()> {
        if self.database_maintenance_active() {
            self.transport.pending_navigation = Some(status);
            return Ok(());
        }
        let result = self.load_autoplay(status);
        if result.is_err() {
            self.transport.autoplay = None;
        }
        result
    }

    fn load_autoplay(&mut self, status: PlaybackStatus) -> Result<()> {
        self.ensure_queue_control_ready()?;
        let total = self.state.queue.total;
        let first = if total == 0 {
            0
        } else if self.preferences.live().shuffle {
            use rand::RngExt;
            rand::rng().random_range(0..total)
        } else {
            self.state
                .queue
                .current_position
                .map_or(0, |index| (index + 1) % total)
        };
        let autoplay = self.transport.autoplay.as_mut().expect("search is active");
        let mut candidate = None;
        if total != 0 {
            let id = self
                .store
                .queue_id_at(first)?
                .context("Queue positions are inconsistent")?;
            if !autoplay.attempted.contains(&id) {
                candidate = Some(id);
            } else {
                // Stream indexed ranges in wrap order; stop at the first untried ID.
                for span in [first + 1..total, 0..first] {
                    if !span.is_empty() {
                        candidate = self
                            .store
                            .first_untried_queue_id(span, &autoplay.attempted)?;
                        if candidate.is_some() {
                            break;
                        }
                    }
                }
            }
        }
        let Some(id) = candidate else {
            let notice = autoplay.notice(true);
            self.stop()?;
            self.state.system.last_error = notice;
            return Ok(());
        };
        let notice = autoplay.notice(false);
        autoplay.attempted.insert(id);
        self.start_occurrence(id, QueueHistory::Record, status, false)?;
        // Selection admission clears last_error; keep the aggregate notice.
        if notice.is_some() {
            self.state.system.last_error = notice;
        }
        Ok(())
    }

    pub(in crate::core) fn advance_with_status(
        &mut self,
        natural: bool,
        status: PlaybackStatus,
    ) -> Result<()> {
        if !natural {
            self.transport.autoplay = None;
        }
        if self.database_maintenance_active() {
            if natural {
                self.transport.pending_navigation = Some(status);
                return Ok(());
            }
            bail!("Database maintenance is in progress; navigation was not executed");
        }
        self.ensure_queue_control_ready()?;
        if self.state.queue.total == 0 {
            return if natural { self.stop() } else { Ok(()) };
        }
        let current = self.state.queue.current_id;
        if natural
            && status != PlaybackStatus::Stopped
            && self.preferences.live().repeat == RepeatMode::All
        {
            // Repeat One stops on its failed source; only Repeat All skips.
            self.transport.autoplay = Some(Autoplay::default());
            return self.continue_autoplay(status);
        }
        if natural
            && self.preferences.live().repeat == RepeatMode::One
            && let Some(id) = current
        {
            return self.start_with_status(id, QueueHistory::Record, status);
        }
        if self.preferences.live().shuffle {
            // Every Next is an independent draw over occurrences, including current.
            let id = self
                .random_queue_id(&mut rand::rng())?
                .context("Queue positions are inconsistent")?;
            return self.start_with_status(id, QueueHistory::Record, status);
        }
        let index = self.state.queue.current_position;
        let next = index.map_or(0, |index| index + 1);
        if let Some(id) = self.store.queue_id_at(next)? {
            return self.start_with_status(id, QueueHistory::Record, status);
        }
        if self.preferences.live().repeat == RepeatMode::All {
            let id = self.store.queue_id_at(0)?.context("Queue is empty")?;
            return self.start_with_status(id, QueueHistory::Record, status);
        }
        if natural { self.stop() } else { Ok(()) }
    }
    pub(in crate::core) fn previous(&mut self) -> Result<()> {
        let status = self.state.playback.status;
        self.transport.autoplay = None;
        self.ensure_queue_control_ready()?;
        if self.preferences.live().shuffle {
            if let Some(index) = self.transport.history_cursor.checked_sub(
                if self.state.queue.current_id.is_some() {
                    2
                } else {
                    1
                },
            ) {
                return self.start_with_status(
                    self.transport.history[index],
                    QueueHistory::Restore(index + 1),
                    status,
                );
            }
            return Ok(());
        }
        let index = self.state.queue.current_position;
        if let Some(index) = index {
            if index > 0 {
                let id = self
                    .store
                    .queue_id_at(index - 1)?
                    .context("Queue entry not found")?;
                return self.start_with_status(id, QueueHistory::Record, status);
            }
            return Ok(());
        }
        if let Some(id) = self.store.queue_id_at(0)? {
            return self.start_with_status(id, QueueHistory::Record, status);
        }
        Ok(())
    }
    pub(in crate::core) fn seek(&mut self, seconds: f64) -> Result<()> {
        audio::validate_seek_position(seconds)?;
        if self.state.playback.status == PlaybackStatus::Stopped {
            bail!("Nothing is playing");
        }
        let end = self
            .transport
            .session
            .as_ref()
            .and_then(|playback| playback.loaded_track.cue.as_ref())
            .and_then(|cue| cue.end_seconds().map(|end| end - cue.start_seconds()));
        let seconds = end.map_or(seconds, |end| seconds.min(end));
        let revision = self.state.playback.seek_revision.wrapping_add(1);
        // This can reject a physically ended traversal even while its Ended
        // event is still queued here. Publish intent only after admission.
        self.engine.send(AudioCommand::Seek { seconds, revision })?;
        // A decoder failure caused by this admitted manual seek belongs to
        // this occurrence, not to the earlier natural navigation search.
        self.transport.autoplay = None;
        self.state.playback.position = seconds;
        self.state.playback.seek_revision = revision;
        self.transport.progress_seek_revision = revision;
        Ok(())
    }

    pub(in crate::core) fn replace_history(
        &mut self,
        history: Vec<rivu_protocol::model::HistoryEntry>,
    ) {
        self.state.library.history = Arc::new(history);
        self.transport.history_pending = false;
    }

    fn project_playback_stats(&mut self, track_id: i64, stats: crate::store::TrackStats) {
        self.update_track_stats(track_id, stats)
            .expect("Playback statistics projection only patches in-memory state");
    }

    fn refresh_playback_history(&mut self) {
        if self.transport.history_pending {
            if let Some(attempted) = &mut self.projections.request_attempts {
                attempted.history = true;
            }
            match self.store.history(MAX_QUERY_ROWS) {
                Ok(history) => self.replace_history(history),
                Err(error) => {
                    self.state.system.last_error = Some(format!(
                        "Playback activity committed, but refreshing history failed; previous history retained: {error:#}"
                    ));
                }
            }
        }
    }

    /// Replay only retained producer facts, never timed-out user commands.
    pub(in crate::core) fn flush_deferred_playback(&mut self) -> Result<()> {
        if self.database_maintenance_active() {
            return Ok(());
        }
        let threshold = self.preferences.settings().play_count_threshold_percent;
        let pending = std::mem::take(&mut self.transport.write_deferred)
            || self.transport.retired.is_some()
            || self.transport.session.as_ref().is_some_and(|session| {
                session.activity_pending || session.count_pending(threshold)
            });
        let result = if pending {
            debug_assert!(self.transport.retired.is_none() || self.transport.session.is_none());
            let session = self
                .transport
                .retired
                .as_mut()
                .map(|retired| &mut retired.stats)
                .or(self.transport.session.as_mut());
            if let Some(session) = session {
                let track_id = session.loaded_track.id;
                // Independent writes both run before publication. A committed
                // count is already fenced even if activity or projection fails.
                let count = persist_session_count(&self.store, session, threshold);
                let activity = persist_session_activity(&self.store, session);
                let bitrate = persist_session_bitrate(&self.store, session, false);
                let count = count.map(|stats| {
                    if let Some(stats) = stats {
                        self.project_playback_stats(track_id, stats);
                    }
                });
                let activity = activity.map(|stats| {
                    if let Some(stats) = stats {
                        self.transport.history_pending |= stats.changed;
                        self.project_playback_stats(track_id, stats);
                    }
                });
                if let Some((bitrate, exact)) = bitrate {
                    self.publish_playback_bitrate(track_id, bitrate, exact);
                }
                count.and(activity)
            } else {
                Ok(())
            }
        } else {
            Ok(())
        };
        if result.is_ok()
            && self
                .transport
                .retired
                .as_ref()
                .is_some_and(|retired| retired.pending_close.is_none())
        {
            self.transport.retired = None;
        }
        if result.is_err() {
            self.transport.pending_navigation = None;
        }
        let result = result.and_then(|()| {
            if let Some(status) = self.transport.pending_navigation.take()
                && !self.state.system.shutting_down
            {
                if self.transport.autoplay.is_some() {
                    if self.preferences.live().repeat == RepeatMode::All {
                        self.continue_autoplay(status)?;
                    } else {
                        self.stop()?;
                    }
                } else {
                    self.advance_with_status(true, status)?;
                }
            }
            Ok(())
        });
        // Publish view failures after navigation, whose successful Load clears
        // prior errors. Projection recovery never changes the producer outcome.
        self.refresh_playback_history();
        result
    }
}
