//! Metadata identity, persisted availability, drafts, and independent read/save progress.
//!
//! Playback snapshots seed a follow identity, but only an accepted read can restore
//! presence after a row was unavailable. Retrying or acknowledging a save cannot
//! erase unavailability or cancel a newer read. Identity epochs invalidate callbacks
//! after leaving and reopening the same track or discarding it.

use rivu_format::decoder_hint::DecoderHint;
use rivu_protocol::model::{Command, Track};
use std::sync::Arc;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::gui) struct MetadataValues {
    pub(in crate::gui) title: String,
    pub(in crate::gui) artist: String,
    pub(in crate::gui) album: String,
    pub(in crate::gui) decoder_hint: DecoderHint,
}

impl MetadataValues {
    fn from_track(track: &Track) -> Self {
        Self {
            title: track.title.clone(),
            artist: track.artist.clone(),
            album: track.album.clone(),
            decoder_hint: track.decoder_hint,
        }
    }

    pub(in crate::gui) fn as_draft(&self) -> MetadataDraft<'_> {
        MetadataDraft {
            title: &self.title,
            artist: &self.artist,
            album: &self.album,
            decoder_hint: self.decoder_hint,
        }
    }
}

#[derive(Clone, Copy)]
pub(in crate::gui) struct MetadataDraft<'a> {
    pub(in crate::gui) title: &'a str,
    pub(in crate::gui) artist: &'a str,
    pub(in crate::gui) album: &'a str,
    pub(in crate::gui) decoder_hint: DecoderHint,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::gui) struct EditorIdentity {
    epoch: u64,
    track_id: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::gui) struct LoadToken {
    epoch: u64,
    request: u64,
    track_id: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::gui) struct SaveToken {
    epoch: u64,
    request: u64,
    pub(in crate::gui) track_id: i64,
}

/// Unavailable details may remain cached solely to preserve an active draft.
#[derive(Debug, Default, PartialEq)]
pub(in crate::gui) enum MetadataAvailability {
    #[default]
    Unknown,
    Present(Arc<Track>),
    Unavailable(Option<Arc<Track>>),
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(in crate::gui) enum MetadataReadState {
    #[default]
    Idle,
    Pending(LoadToken),
    Failed(String),
    WrongTrack,
}

#[derive(Default)]
pub(in crate::gui) struct MetadataSession {
    selected: Option<i64>,
    identity: Option<i64>,
    availability: MetadataAvailability,
    baseline: Option<MetadataValues>,
    read_state: MetadataReadState,
    epoch: u64,
    request_serial: u64,
    saving: Option<SaveToken>,
}

impl MetadataSession {
    pub(in crate::gui) fn selected(&self) -> Option<i64> {
        self.selected
    }

    pub(in crate::gui) fn identity(&self) -> Option<i64> {
        self.identity
    }

    pub(in crate::gui) fn editor_identity(&self) -> Option<EditorIdentity> {
        self.identity.map(|track_id| EditorIdentity {
            epoch: self.epoch,
            track_id,
        })
    }

    pub(in crate::gui) fn track(&self) -> Option<&Track> {
        match &self.availability {
            MetadataAvailability::Present(track) => Some(track),
            MetadataAvailability::Unavailable(track) => track.as_deref(),
            MetadataAvailability::Unknown => None,
        }
    }

    /// Share accepted details with presentation consumers without copying per frame.
    pub(in crate::gui) fn track_snapshot(&self) -> Option<Arc<Track>> {
        match &self.availability {
            MetadataAvailability::Present(track) => Some(Arc::clone(track)),
            MetadataAvailability::Unavailable(track) => track.clone(),
            MetadataAvailability::Unknown => None,
        }
    }

    pub(in crate::gui) fn availability(&self) -> &MetadataAvailability {
        &self.availability
    }

    pub(in crate::gui) fn read_state(&self) -> &MetadataReadState {
        &self.read_state
    }

    /// Cached details keep an unavailable draft editable, but never savable.
    pub(in crate::gui) fn editable(&self) -> bool {
        self.track().is_some()
    }

    pub(in crate::gui) fn saving(&self) -> bool {
        self.saving.is_some()
    }

    pub(in crate::gui) fn dirty(&self, draft: MetadataDraft<'_>) -> bool {
        self.baseline.as_ref().is_some_and(|baseline| {
            baseline.title != draft.title
                || baseline.artist != draft.artist
                || baseline.album != draft.album
                || baseline.decoder_hint != draft.decoder_hint
        })
    }

    pub(in crate::gui) fn can_save(&self, draft: MetadataDraft<'_>) -> bool {
        matches!(self.availability, MetadataAvailability::Present(_))
            && !self.saving()
            && self.dirty(draft)
    }

    pub(in crate::gui) fn protected(&self, draft: MetadataDraft<'_>) -> bool {
        self.saving() || self.dirty(draft)
    }

    /// Reconcile a clean editor with accepted details without changing read/presence state.
    pub(in crate::gui) fn reconcile_clean(
        &mut self,
        draft: MetadataDraft<'_>,
    ) -> Option<MetadataValues> {
        if self.protected(draft) {
            return None;
        }
        let MetadataAvailability::Present(track) = &self.availability else {
            return None;
        };
        if !self.dirty(MetadataDraft {
            title: &track.title,
            artist: &track.artist,
            album: &track.album,
            decoder_hint: track.decoder_hint,
        }) {
            return None;
        }
        let values = MetadataValues::from_track(track);
        self.baseline = Some(values.clone());
        Some(values)
    }

    pub(in crate::gui) fn needs_confirmation(
        &self,
        target: Option<i64>,
        draft: MetadataDraft<'_>,
    ) -> bool {
        self.protected(draft) && (target.is_none() || target != self.identity)
    }

    fn change_identity(&mut self, id: Option<i64>) {
        self.identity = id;
        self.epoch = self.epoch.wrapping_add(1);
        self.availability = MetadataAvailability::Unknown;
        self.baseline = None;
        self.saving = None;
        self.read_state = MetadataReadState::Idle;
    }

    /// Returns whether inputs must be cleared before this selection loads.
    pub(in crate::gui) fn select(&mut self, id: i64) -> bool {
        let changed = self.identity != Some(id);
        if changed {
            self.change_identity(Some(id));
        }
        self.selected = Some(id);
        changed
    }

    pub(in crate::gui) fn clear_selection(&mut self) {
        self.selected = None;
        self.change_identity(None);
    }

    /// Discard invalidates old edit callbacks even when the selected track stays put.
    pub(in crate::gui) fn discard_draft(&mut self) {
        self.change_identity(self.identity);
    }

    pub(in crate::gui) fn begin_load(&mut self, id: i64) -> LoadToken {
        debug_assert_eq!(self.identity, Some(id));
        self.request_serial = self.request_serial.wrapping_add(1);
        let token = LoadToken {
            epoch: self.epoch,
            request: self.request_serial,
            track_id: id,
        };
        self.read_state = MetadataReadState::Pending(token);
        token
    }

    pub(in crate::gui) fn accepts_load(&self, token: LoadToken) -> bool {
        self.read_state == MetadataReadState::Pending(token)
            && self.epoch == token.epoch
            && self.identity == Some(token.track_id)
    }

    pub(in crate::gui) fn fail_load(&mut self, token: LoadToken, error: String) -> bool {
        if !self.accepts_load(token) {
            return false;
        }
        self.read_state = MetadataReadState::Failed(error);
        true
    }

    /// `Some(values)` replaces widget text. Dirty drafts keep their old baseline.
    pub(in crate::gui) fn accept_load(
        &mut self,
        token: LoadToken,
        track: Option<Track>,
        draft: MetadataDraft<'_>,
    ) -> Option<MetadataValues> {
        if !self.accepts_load(token) {
            return None;
        }
        if track
            .as_ref()
            .is_some_and(|track| track.id != token.track_id)
        {
            self.read_state = MetadataReadState::WrongTrack;
            return None;
        }
        self.read_state = MetadataReadState::Idle;
        self.accept_track(track, draft)
    }

    fn accept_track(
        &mut self,
        track: Option<Track>,
        draft: MetadataDraft<'_>,
    ) -> Option<MetadataValues> {
        let Some(track) = track else {
            // A deleted row must not erase the only in-memory draft or its baseline.
            let retain = self.protected(draft);
            let cached = if retain {
                match std::mem::take(&mut self.availability) {
                    MetadataAvailability::Present(track) => Some(track),
                    MetadataAvailability::Unavailable(track) => track,
                    MetadataAvailability::Unknown => None,
                }
            } else {
                self.baseline = None;
                None
            };
            self.availability = MetadataAvailability::Unavailable(cached);
            return (!retain).then(MetadataValues::default);
        };
        let replace = self.baseline.is_none() || (!self.dirty(draft) && !self.saving());
        let values = replace.then(|| MetadataValues::from_track(&track));
        if let Some(values) = values.as_ref() {
            self.baseline = Some(values.clone());
        }
        self.availability = MetadataAvailability::Present(Arc::new(track));
        values
    }

    pub(in crate::gui) fn sync_playback(
        &mut self,
        track: Option<&Track>,
        draft: MetadataDraft<'_>,
    ) -> Option<MetadataValues> {
        if self.selected.is_some() {
            return None;
        }
        let id = track.map(|track| track.id);
        if self.identity != id {
            if self.protected(draft) {
                return None;
            }
            self.change_identity(id);
            if track.is_none() {
                return Some(MetadataValues::default());
            }
            let empty = MetadataValues::default();
            return self.accept_track(track.cloned(), empty.as_draft());
        }
        // Playback carries cached details, not an authoritative row lookup. A stale
        // snapshot must not undo an accepted read that established unavailability.
        if matches!(self.availability, MetadataAvailability::Unavailable(_)) {
            return None;
        }
        if matches!(self.availability, MetadataAvailability::Unknown) && self.baseline.is_some() {
            return None;
        }
        if self.track() == track {
            return None;
        }
        self.accept_track(track.cloned(), draft)
    }

    pub(in crate::gui) fn begin_save(
        &mut self,
        draft: MetadataDraft<'_>,
    ) -> Option<(SaveToken, Command)> {
        if !self.can_save(draft) {
            return None;
        }
        let track_id = self.identity?;
        let baseline = self.baseline.as_ref()?;
        let command = Command::EditTrack {
            track_id,
            title: (baseline.title != draft.title).then(|| draft.title.to_owned()),
            artist: (baseline.artist != draft.artist).then(|| draft.artist.to_owned()),
            album: (baseline.album != draft.album).then(|| draft.album.to_owned()),
            decoder_hint: (baseline.decoder_hint != draft.decoder_hint)
                .then_some(draft.decoder_hint),
        };
        // Issuance is shared, acceptance is not: a save cannot supersede a read.
        self.request_serial = self.request_serial.wrapping_add(1);
        let token = SaveToken {
            epoch: self.epoch,
            request: self.request_serial,
            track_id,
        };
        self.saving = Some(token);
        Some((token, command))
    }

    /// Capture the committed source before an acknowledgement can outlive this editor.
    pub(in crate::gui) fn submitted_hint_change(&self, command: &Command) -> Option<Track> {
        let Command::EditTrack {
            decoder_hint: Some(hint),
            ..
        } = command
        else {
            return None;
        };
        let track = self.track()?;
        (track.decoder_hint != *hint).then(|| {
            let mut track = track.clone();
            track.decoder_hint = *hint;
            track
        })
    }

    /// Confirm the submitted baseline; only reads may replace accepted track data.
    pub(in crate::gui) fn complete_save(
        &mut self,
        token: SaveToken,
        values: MetadataValues,
        ok: bool,
    ) -> bool {
        if self.saving != Some(token)
            || self.epoch != token.epoch
            || self.identity != Some(token.track_id)
        {
            return false;
        }
        self.saving = None;
        if ok {
            self.baseline = Some(values);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn bitrate_reload_preserves_shared_snapshot_draft_baseline_and_save() {
        let mut session = MetadataSession::default();
        let baseline = load(&mut session, 1, "original");
        let draft = values("local edit");
        let (save, _) = session.begin_save(draft.as_draft()).unwrap();
        let original = session.track_snapshot().unwrap();
        let token = session.begin_load(1);
        let mut refreshed = track(1, "original");
        refreshed.bitrate_bps = Some(192_000);
        refreshed.bitrate_exact = true;
        session.accept_load(token, Some(refreshed), draft.as_draft());
        assert_eq!(session.track().unwrap().bitrate_bps, Some(192_000));
        assert!(session.track().unwrap().bitrate_exact);
        assert_eq!(original.bitrate_bps, None);
        assert_eq!(session.baseline, Some(baseline));
        assert!(session.dirty(draft.as_draft()));
        assert!(session.complete_save(save, draft, true));
    }

    #[test]
    fn bitrate_reload_keeps_identity_and_draft() {
        let mut session = MetadataSession::default();
        load(&mut session, 1, "original");
        let identity = session.editor_identity();
        let draft = values("local edit");
        assert_eq!(session.editor_identity(), identity);
        assert!(session.track_snapshot().is_some());
        assert!(session.dirty(draft.as_draft()));
        session.selected = None;
        assert!(
            session
                .sync_playback(Some(&track(1, "stale")), draft.as_draft())
                .is_none()
        );
        let token = session.begin_load(1);
        let mut refreshed = track(1, "remote");
        refreshed.bitrate_bps = Some(192_000);
        refreshed.bitrate_exact = true;
        assert!(
            session
                .accept_load(token, Some(refreshed), draft.as_draft())
                .is_none()
        );
        assert_eq!(session.track().unwrap().bitrate_bps, Some(192_000));
        assert_eq!(session.baseline, Some(values("original")));
        assert!(session.dirty(draft.as_draft()));
    }

    #[test]
    fn decoder_hint_draft_refresh_and_save_lifecycle() {
        let mut session = MetadataSession::default();
        let baseline = load(&mut session, 1, "original");
        let submitted = MetadataValues {
            decoder_hint: DecoderHint::Ffmpeg,
            ..baseline.clone()
        };
        assert!(session.dirty(submitted.as_draft()));
        assert!(!session.dirty(baseline.as_draft()));
        assert!(session.begin_save(baseline.as_draft()).is_none());
        assert_eq!(session.track().unwrap().decoder_hint, DecoderHint::Auto);

        let refresh = session.begin_load(1);
        let mut remote = track(1, "remote title");
        remote.decoder_hint = DecoderHint::Native;
        assert!(
            session
                .accept_load(refresh, Some(remote), submitted.as_draft())
                .is_none()
        );
        assert!(session.dirty(submitted.as_draft()));
        assert!(!session.dirty(baseline.as_draft()));
        let (failed, command) = session.begin_save(submitted.as_draft()).unwrap();
        assert!(matches!(
            command,
            Command::EditTrack {
                title: None,
                artist: None,
                album: None,
                decoder_hint: Some(DecoderHint::Ffmpeg),
                ..
            }
        ));
        assert_eq!(session.track().unwrap().decoder_hint, DecoderHint::Native);
        assert!(session.complete_save(failed, submitted.clone(), false));
        assert!(session.dirty(submitted.as_draft()));
        assert_eq!(session.track().unwrap().decoder_hint, DecoderHint::Native);

        let (saved, _) = session.begin_save(submitted.as_draft()).unwrap();
        let edited_during_save = MetadataValues {
            title: "new title".into(),
            decoder_hint: DecoderHint::Auto,
            ..submitted.clone()
        };
        assert!(session.complete_save(saved, submitted.clone(), true));
        assert_eq!(session.track().unwrap().title, "remote title");
        assert!(!session.dirty(submitted.as_draft()));
        assert!(session.dirty(edited_during_save.as_draft()));
        let (pending, command) = session.begin_save(edited_during_save.as_draft()).unwrap();
        assert!(matches!(&command, Command::EditTrack {
            title: Some(title), artist: None, album: None,
            decoder_hint: Some(DecoderHint::Auto), ..
        } if title == "new title"));
        let switched = load(&mut session, 2, "other");
        assert_eq!(switched.decoder_hint, DecoderHint::Auto);
        assert!(!session.dirty(switched.as_draft()));
        assert!(!session.complete_save(pending, edited_during_save.clone(), true));
        load(&mut session, 1, "reopened");
        assert!(!session.complete_save(pending, edited_during_save, true));
        assert_eq!(session.track().unwrap().decoder_hint, DecoderHint::Auto);
        let refresh = session.begin_load(1);
        let mut remote = track(1, "reopened");
        remote.decoder_hint = DecoderHint::Native;
        let clean = session
            .accept_load(refresh, Some(remote), values("reopened").as_draft())
            .unwrap();
        assert_eq!(clean.decoder_hint, DecoderHint::Native);
        assert!(!session.dirty(clean.as_draft()));
        session.clear_selection();
        let playing = session
            .sync_playback(Some(&track(2, "playing")), clean.as_draft())
            .unwrap();
        assert_eq!(playing.decoder_hint, DecoderHint::Auto);
        assert!(!session.dirty(playing.as_draft()));
    }

    use super::*;
    use crate::gui::visualization::full_waveform::FullWaveformRequest;

    fn track(id: i64, title: &str) -> Track {
        Track {
            id,
            title: title.into(),
            path: Default::default(),
            fingerprint: None,
            cue: None,
            artist: String::new(),
            album: String::new(),
            duration: None,
            codec: String::new(),
            channels: 0,
            sample_rate: 0,
            bitrate_bps: None,
            bitrate_exact: false,
            track_number: None,
            disc_number: None,
            bits_per_sample: None,
            release_date: None,
            favorite: false,
            decoder_hint: Default::default(),
            missing: false,
            play_count: 0,
            last_played: None,
        }
    }
    fn values(title: &str) -> MetadataValues {
        MetadataValues {
            title: title.into(),
            ..Default::default()
        }
    }
    fn load(session: &mut MetadataSession, id: i64, title: &str) -> MetadataValues {
        session.select(id);
        let token = session.begin_load(id);
        session
            .accept_load(token, Some(track(id, title)), values("").as_draft())
            .unwrap()
    }

    #[test]
    fn changed_hint_ack_keeps_original_source_after_editor_switch() {
        let mut session = MetadataSession::default();
        let baseline = load(&mut session, 1, "original");
        let mut original = track(1, "original");
        original.path = "album.flac".into();
        original.cue = Some(rivu_protocol::model::CueSegment {
            sheet: "album.cue".into(),
            number: 1,
            start_frame: 75,
            end_frame: Some(750),
        });
        let token = session.begin_load(1);
        session.accept_load(token, Some(original.clone()), baseline.as_draft());
        let submitted = MetadataValues {
            decoder_hint: DecoderHint::Ffmpeg,
            ..baseline
        };
        let (save, command) = session.begin_save(submitted.as_draft()).unwrap();
        let committed = session.submitted_hint_change(&command).unwrap();
        assert_eq!(committed.path, original.path);
        assert_eq!(committed.cue, original.cue);
        assert_eq!(committed.decoder_hint, DecoderHint::Ffmpeg);
        let mut request = FullWaveformRequest::default();
        let generation = request.begin(&original);

        load(&mut session, 2, "other");
        assert!(!session.complete_save(save, submitted, true));
        request.saved_hint_changed(&committed);
        assert!(!request.accepts(generation, Some(&original)));
        assert_eq!(session.track().unwrap().id, 2);
        assert_eq!(session.track().unwrap().decoder_hint, DecoderHint::Auto);
    }

    #[test]
    fn tag_save_and_already_committed_hint_do_not_invalidate_scans() {
        let mut session = MetadataSession::default();
        let baseline = load(&mut session, 1, "original");
        let (save, command) = session.begin_save(values("new title").as_draft()).unwrap();
        assert!(session.submitted_hint_change(&command).is_none());
        assert!(session.complete_save(save, values("new title"), false));

        let submitted = MetadataValues {
            decoder_hint: DecoderHint::Native,
            ..baseline
        };
        let token = session.begin_load(1);
        let mut remote = track(1, "original");
        remote.decoder_hint = DecoderHint::Native;
        session.accept_load(token, Some(remote), submitted.as_draft());
        let (_, command) = session.begin_save(submitted.as_draft()).unwrap();
        assert!(matches!(
            &command,
            Command::EditTrack {
                decoder_hint: Some(DecoderHint::Native),
                ..
            }
        ));
        assert!(session.submitted_hint_change(&command).is_none());
    }

    #[test]
    fn explicit_waveform_queries_use_latest_request_and_allow_fresh_saved_hint() {
        let original = track(1, "original");
        let mut request = FullWaveformRequest::default();
        let first = request.begin(&original);
        let latest = request.begin(&original);
        let mut committed = original.clone();
        committed.decoder_hint = DecoderHint::Ffmpeg;
        committed.title = "updated tags".into();
        request.retain_target(Some(&original), true);
        assert!(!request.accepts(first, Some(&committed)));
        assert!(request.accepts(latest, Some(&committed)));
        assert!(!request.accepts(latest, Some(&track(2, "other"))));
        committed.path = "moved.flac".into();
        assert!(!request.accepts(latest, Some(&committed)));
    }

    #[test]
    fn waveform_query_timeout_and_replacement_keep_physical_reservation() {
        let original = track(1, "original");
        let latest = track(2, "latest");
        let mut request = FullWaveformRequest::default();
        let first = request.begin(&original);
        assert_eq!(request.reserve(), Some((first, 1)));
        // Logical timeout retires publication, not the submitted query slot.
        request.invalidate();
        assert!(!request.accepts(first, Some(&original)));
        assert!(request.reserve().is_none());
        let next = request.begin(&latest);
        assert!(request.reserve().is_none());
        request.retire(first);
        assert_eq!(request.reserve(), Some((next, 2)));
        request.retire(first);
        assert!(request.reserve().is_none());
        assert!(request.accepts(next, Some(&latest)));
    }

    #[test]
    fn explicit_waveform_queries_do_not_revive_after_target_or_visualization_enabled_changes() {
        let original = track(1, "original");
        let other = track(2, "other");
        let mut request = FullWaveformRequest::default();
        for (target, visualization_enabled) in
            [(Some(&other), true), (None, true), (Some(&original), false)]
        {
            let generation = request.begin(&original);
            request.retain_target(target, visualization_enabled);
            request.retain_target(Some(&original), true);
            assert!(!request.accepts(generation, Some(&original)));
        }
    }

    #[test]
    fn saved_hint_invalidates_only_the_matching_path_and_cue_range() {
        let mut original = track(1, "original");
        original.path = "album.flac".into();
        original.cue = Some(rivu_protocol::model::CueSegment {
            sheet: "album.cue".into(),
            number: 1,
            start_frame: 75,
            end_frame: Some(750),
        });
        let mut request = FullWaveformRequest::default();
        let generation = request.begin(&original);
        let mut committed = original.clone();
        committed.path = "other.flac".into();
        request.saved_hint_changed(&committed);
        assert!(request.accepts(generation, Some(&original)));
        committed.path.clone_from(&original.path);
        committed.cue.as_mut().unwrap().end_frame = Some(1500);
        request.saved_hint_changed(&committed);
        assert!(request.accepts(generation, Some(&original)));
        assert!(!request.accepts(generation, Some(&committed)));
        committed.cue.clone_from(&original.cue);
        committed.id = 99;
        committed.decoder_hint = DecoderHint::Native;
        request.saved_hint_changed(&committed);
        assert!(!request.accepts(generation, Some(&original)));
    }

    #[test]
    fn dirty_refresh_keeps_baseline_even_when_remote_matches_the_draft() {
        let mut session = MetadataSession::default();
        load(&mut session, 1, "original");
        let draft = values("edited");
        let token = session.begin_load(1);
        assert!(
            session
                .accept_load(token, Some(track(1, "edited")), draft.as_draft())
                .is_none()
        );
        assert!(session.dirty(draft.as_draft()));
        let token = session.begin_load(1);
        assert!(
            session
                .accept_load(token, Some(track(1, "new remote")), draft.as_draft())
                .is_none()
        );
        assert!(!session.dirty(values("original").as_draft()));
        assert_eq!(session.track().unwrap().title, "new remote");
    }

    #[test]
    fn switched_identity_rejects_old_load_and_save_even_after_switching_back() {
        let mut session = MetadataSession::default();
        load(&mut session, 1, "one");
        let old_load = session.begin_load(1);
        let (save, _) = session.begin_save(values("edit").as_draft()).unwrap();
        load(&mut session, 2, "two");
        load(&mut session, 1, "fresh");
        assert!(!session.accepts_load(old_load));
        assert!(!session.complete_save(save, values("edit"), true));
        assert_eq!(session.track().unwrap().title, "fresh");
    }

    #[test]
    fn save_patches_baseline_not_refreshed_track_and_preserves_untouched_fields() {
        let mut session = MetadataSession::default();
        load(&mut session, 1, "original");
        let submitted = values("saved title");
        let (token, command) = session.begin_save(submitted.as_draft()).unwrap();
        let Command::EditTrack {
            title,
            artist,
            album,
            ..
        } = command
        else {
            panic!("expected metadata patch");
        };
        assert_eq!(title.as_deref(), Some("saved title"));
        assert_eq!(artist, None);
        assert_eq!(album, None);
        let load_token = session.begin_load(1);
        let mut remote = track(1, "newer remote title");
        remote.artist = "External artist".into();
        let newer_draft = values("typed while saving");
        assert!(
            session
                .accept_load(load_token, Some(remote), newer_draft.as_draft())
                .is_none()
        );
        assert!(session.complete_save(token, submitted.clone(), true));
        assert!(!session.dirty(submitted.as_draft()));
        assert_eq!(session.track().unwrap().artist, "External artist");
        assert_eq!(session.track().unwrap().title, "newer remote title");
        assert!(session.dirty(newer_draft.as_draft()));
        let (_, command) = session.begin_save(newer_draft.as_draft()).unwrap();
        let Command::EditTrack {
            title,
            artist,
            album,
            ..
        } = command
        else {
            panic!("expected metadata patch");
        };
        assert_eq!(title.as_deref(), Some("typed while saving"));
        assert_eq!(artist, None);
        assert_eq!(album, None);
    }

    #[test]
    fn explicitly_cleared_field_is_included_but_clean_draft_cannot_save() {
        let mut session = MetadataSession::default();
        let baseline = load(&mut session, 1, "original");
        assert!(session.begin_save(baseline.as_draft()).is_none());
        let (_, command) = session.begin_save(values("").as_draft()).unwrap();
        let Command::EditTrack {
            title,
            artist,
            album,
            ..
        } = command
        else {
            panic!("expected metadata patch");
        };
        assert_eq!(title.as_deref(), Some(""));
        assert_eq!(artist, None);
        assert_eq!(album, None);
    }

    #[test]
    fn different_field_typed_during_save_is_the_only_next_patch() {
        let mut session = MetadataSession::default();
        load(&mut session, 1, "original");
        let submitted = values("saved title");
        let (token, _) = session.begin_save(submitted.as_draft()).unwrap();
        let newer_draft = MetadataValues {
            artist: "New artist typed while saving".into(),
            ..submitted.clone()
        };
        assert!(session.complete_save(token, submitted, true));
        assert!(session.dirty(newer_draft.as_draft()));
        let (_, command) = session.begin_save(newer_draft.as_draft()).unwrap();
        let Command::EditTrack {
            title,
            artist,
            album,
            ..
        } = command
        else {
            panic!("expected metadata patch");
        };
        assert_eq!(title, None);
        assert_eq!(artist.as_deref(), Some("New artist typed while saving"));
        assert_eq!(album, None);
    }

    #[test]
    fn save_ack_advances_only_submitted_baseline_and_preserves_new_edits() {
        let mut session = MetadataSession::default();
        load(&mut session, 1, "one");
        let submitted = values("saved");
        let (token, _) = session.begin_save(submitted.as_draft()).unwrap();
        let pending_load = session.begin_load(1);
        assert!(session.complete_save(token, submitted.clone(), true));
        assert!(session.accepts_load(pending_load));
        assert!(!session.dirty(submitted.as_draft()));
        assert!(session.dirty(values("typed while saving").as_draft()));
        let (token, _) = session.begin_save(values("failed").as_draft()).unwrap();
        assert!(session.complete_save(token, values("failed"), false));
        assert!(session.dirty(values("failed").as_draft()));
    }

    #[test]
    fn unloaded_clean_unavailable_and_discarded_sessions_cannot_save_old_drafts() {
        let mut session = MetadataSession::default();
        session.select(1);
        assert!(session.begin_save(values("premature").as_draft()).is_none());
        load(&mut session, 1, "one");
        let token = session.begin_load(1);
        assert_eq!(
            session.accept_load(token, None, values("one").as_draft()),
            Some(values(""))
        );
        assert!(session.track().is_none());
        assert!(session.begin_save(values("dirty").as_draft()).is_none());
        session.clear_selection();
        assert_eq!(
            session.sync_playback(Some(&track(2, "playing")), values("").as_draft()),
            Some(values("playing"))
        );
        assert_eq!(session.selected(), None);
    }

    #[test]
    fn playback_advance_and_stop_preserve_failed_draft_until_successful_retry() {
        let mut session = MetadataSession::default();
        let one = track(1, "one");
        let two = track(2, "two");
        let original = session
            .sync_playback(Some(&one), values("").as_draft())
            .unwrap();
        let draft = values("edited one");
        assert!(
            session
                .sync_playback(Some(&two), draft.as_draft())
                .is_none()
        );
        assert!(session.sync_playback(None, draft.as_draft()).is_none());
        assert_eq!(session.identity(), Some(1));
        assert_eq!(session.track().unwrap().id, 1);
        let (failed, command) = session.begin_save(draft.as_draft()).unwrap();
        assert!(matches!(command, Command::EditTrack { track_id: 1, .. }));
        // Saving is protected even if the widgets are reverted to the old baseline.
        assert!(
            session
                .sync_playback(Some(&two), original.as_draft())
                .is_none()
        );
        assert!(session.complete_save(failed, draft.clone(), false));
        assert!(session.dirty(draft.as_draft()));
        assert!(
            session
                .sync_playback(Some(&two), draft.as_draft())
                .is_none()
        );
        let (retry, _) = session.begin_save(draft.as_draft()).unwrap();
        assert!(session.complete_save(retry, draft.clone(), true));
        assert_eq!(
            session.sync_playback(Some(&two), draft.as_draft()),
            Some(values("two"))
        );
        assert_eq!(session.identity(), Some(2));
    }

    #[test]
    fn playback_does_not_resume_after_save_when_newer_edits_remain() {
        let mut session = MetadataSession::default();
        session.sync_playback(Some(&track(1, "one")), values("").as_draft());
        let submitted = values("submitted");
        let (save, _) = session.begin_save(submitted.as_draft()).unwrap();
        let newer = values("typed while saving");
        assert!(
            session
                .sync_playback(Some(&track(2, "two")), newer.as_draft())
                .is_none()
        );
        assert!(session.complete_save(save, submitted, true));
        assert!(
            session
                .sync_playback(Some(&track(3, "latest")), newer.as_draft())
                .is_none()
        );
        assert_eq!(session.identity(), Some(1));
        let (save, command) = session.begin_save(newer.as_draft()).unwrap();
        assert!(matches!(command, Command::EditTrack { track_id: 1, .. }));
        assert!(session.complete_save(save, newer.clone(), true));
        assert_eq!(
            session.sync_playback(Some(&track(3, "latest")), newer.as_draft()),
            Some(values("latest"))
        );
    }

    #[test]
    fn manual_target_and_clear_guards_keep_original_identity_until_resolution() {
        let mut session = MetadataSession::default();
        let baseline = load(&mut session, 1, "one");
        assert!(!session.needs_confirmation(Some(2), baseline.as_draft()));
        assert!(!session.needs_confirmation(None, baseline.as_draft()));
        let draft = values("edited");
        assert!(!session.needs_confirmation(Some(1), draft.as_draft()));
        assert!(session.needs_confirmation(Some(2), draft.as_draft()));
        assert!(session.needs_confirmation(None, draft.as_draft()));
        // Declining the deferred action leaves both explicit identity and draft intact.
        assert_eq!(session.selected(), Some(1));
        assert_eq!(session.identity(), Some(1));
        assert!(session.dirty(draft.as_draft()));
        let (save, _) = session.begin_save(draft.as_draft()).unwrap();
        assert!(session.needs_confirmation(Some(2), baseline.as_draft()));
        assert!(session.needs_confirmation(None, baseline.as_draft()));
        session.clear_selection();
        load(&mut session, 2, "two");
        assert!(!session.complete_save(save, draft, true));
        assert_eq!(session.selected(), Some(2));
    }

    #[test]
    fn unavailable_refresh_retains_dirty_and_saving_identity_without_writing_deleted_rows() {
        let mut session = MetadataSession::default();
        load(&mut session, 1, "one");
        let draft = values("draft");
        let token = session.begin_load(1);
        assert!(session.accept_load(token, None, draft.as_draft()).is_none());
        assert!(matches!(
            session.availability(),
            MetadataAvailability::Unavailable(_)
        ));
        assert_eq!(session.identity(), Some(1));
        assert_eq!(session.track().unwrap().title, "one");
        assert!(session.dirty(draft.as_draft()));
        assert!(session.begin_save(draft.as_draft()).is_none());
        let retry = session.begin_load(1);
        assert!(
            session
                .accept_load(retry, Some(track(1, "remote")), draft.as_draft())
                .is_none()
        );
        let (save, _) = session.begin_save(draft.as_draft()).unwrap();
        let token = session.begin_load(1);
        assert!(session.accept_load(token, None, draft.as_draft()).is_none());
        assert!(session.saving());
        assert!(session.complete_save(save, draft.clone(), false));
        assert!(session.dirty(draft.as_draft()));
        assert!(matches!(
            session.availability(),
            MetadataAvailability::Unavailable(_)
        ));
    }

    #[test]
    fn successful_save_preserves_later_unavailability_until_a_row_is_loaded() {
        let mut session = MetadataSession::default();
        load(&mut session, 1, "one");
        let submitted = values("submitted");
        let (save, _) = session.begin_save(submitted.as_draft()).unwrap();
        let newer = values("typed while saving");
        let missing = session.begin_load(1);
        assert!(
            session
                .accept_load(missing, None, newer.as_draft())
                .is_none()
        );
        assert!(matches!(
            session.availability(),
            MetadataAvailability::Unavailable(_)
        ));

        assert!(session.complete_save(save, submitted.clone(), true));
        assert!(!session.saving());
        assert!(matches!(
            session.availability(),
            MetadataAvailability::Unavailable(_)
        ));
        assert_eq!(session.identity(), Some(1));
        assert!(!session.dirty(submitted.as_draft()));
        assert!(session.dirty(newer.as_draft()));
        assert!(session.begin_save(newer.as_draft()).is_none());

        let recovered = session.begin_load(1);
        assert!(
            session
                .accept_load(recovered, Some(track(1, "remote")), newer.as_draft())
                .is_none()
        );
        assert!(matches!(
            session.availability(),
            MetadataAvailability::Present(_)
        ));
        assert_eq!(session.read_state(), &MetadataReadState::Idle);
        assert!(session.dirty(newer.as_draft()));
        let (_, command) = session.begin_save(newer.as_draft()).unwrap();
        assert!(matches!(
            command,
            Command::EditTrack {
                track_id: 1,
                title: Some(title),
                artist: None,
                album: None,
                decoder_hint: None,
            } if title == newer.title
        ));
    }

    #[test]
    fn selection_of_playback_identity_keeps_draft() {
        let mut session = MetadataSession::default();
        session.sync_playback(Some(&track(1, "playing")), values("").as_draft());
        assert!(!session.select(1));
        assert!(session.dirty(values("draft").as_draft()));
    }

    #[test]
    fn selection_loading_failure_unavailable_and_retry_are_identity_owned() {
        let mut session = MetadataSession::default();
        assert_eq!(session.availability(), &MetadataAvailability::Unknown);
        assert_eq!(session.read_state(), &MetadataReadState::Idle);
        assert!(!session.editable());
        load(&mut session, 1, "one");
        let old = session.begin_load(1);
        assert!(session.select(2));
        let pending = session.begin_load(2);
        assert!(matches!(
            session.read_state(),
            MetadataReadState::Pending(_)
        ));
        assert!(session.track().is_none());
        assert!(!session.editable());
        assert!(session.begin_save(values("old draft").as_draft()).is_none());
        assert!(!session.fail_load(old, "late failure".into()));
        assert!(
            session
                .accept_load(old, Some(track(1, "late one")), values("").as_draft())
                .is_none()
        );
        assert!(matches!(
            session.read_state(),
            MetadataReadState::Pending(_)
        ));
        assert!(session.fail_load(pending, "query failed".into()));
        assert_eq!(
            session.read_state(),
            &MetadataReadState::Failed("query failed".into())
        );
        assert!(!session.accepts_load(pending));
        assert!(!session.fail_load(pending, "duplicate failure".into()));
        assert!(!session.editable());
        assert!(session.begin_save(values("old draft").as_draft()).is_none());

        let retry = session.begin_load(2);
        session.accept_load(retry, None, values("").as_draft());
        assert_eq!(session.selected(), Some(2));
        assert!(matches!(
            session.availability(),
            MetadataAvailability::Unavailable(_)
        ));
        assert!(!session.editable());
        assert!(!session.accepts_load(retry));
        let retry = session.begin_load(2);
        let loaded = session
            .accept_load(retry, Some(track(2, "two")), values("").as_draft())
            .unwrap();
        assert_eq!(loaded.title, "two");
        assert!(session.editable());
        assert!(matches!(
            session.availability(),
            MetadataAvailability::Present(_)
        ));
        assert_eq!(session.read_state(), &MetadataReadState::Idle);
        assert!(!session.accepts_load(retry));
        session.clear_selection();
        assert!(!session.fail_load(retry, "after cancellation".into()));
        assert_eq!(session.availability(), &MetadataAvailability::Unknown);
        assert_eq!(session.read_state(), &MetadataReadState::Idle);
        assert!(session.track().is_none());
    }

    #[test]
    fn same_track_refresh_keeps_ready_data_and_dirty_draft_through_failure() {
        let mut session = MetadataSession::default();
        load(&mut session, 1, "one");
        let draft = values("draft");
        assert!(!session.select(1));
        let token = session.begin_load(1);
        assert!(session.editable());
        assert_eq!(session.track().unwrap().title, "one");
        assert!(session.fail_load(token, "refresh failed".into()));
        assert!(session.editable());
        assert!(session.dirty(draft.as_draft()));
        assert_eq!(session.track().unwrap().title, "one");
        let retry = session.begin_load(1);
        assert!(
            session
                .accept_load(retry, Some(track(1, "remote")), draft.as_draft())
                .is_none()
        );
        assert!(matches!(
            session.availability(),
            MetadataAvailability::Present(_)
        ));
        assert_eq!(session.read_state(), &MetadataReadState::Idle);
        assert_eq!(session.track().unwrap().title, "remote");
        assert!(session.dirty(draft.as_draft()));
        let (_, command) = session.begin_save(draft.as_draft()).unwrap();
        assert!(
            matches!(command, Command::EditTrack { title: Some(title), .. } if title == "draft")
        );
    }

    #[test]
    fn mismatched_track_response_completes_as_failure_without_ready_data() {
        let mut session = MetadataSession::default();
        session.select(1);
        let token = session.begin_load(1);
        assert!(
            session
                .accept_load(token, Some(track(2, "wrong")), values("").as_draft())
                .is_none()
        );
        assert_eq!(session.read_state(), &MetadataReadState::WrongTrack);
        assert!(!session.accepts_load(token));
        assert!(session.track().is_none());
        assert!(!session.editable());
    }
    #[test]
    fn snapshot_reload_supersedes_old_metadata_and_completes_without_a_frame() {
        let mut session = MetadataSession::default();
        session.select(1);
        let old = session.begin_load(1);
        let current = session.begin_load(1);
        assert!(!session.accepts_load(old));
        assert!(
            session
                .accept_load(old, Some(track(1, "stale")), values("").as_draft())
                .is_none()
        );
        let loaded = session
            .accept_load(current, Some(track(1, "current")), values("").as_draft())
            .unwrap();
        assert_eq!(loaded.title, "current");
        assert_eq!(session.track().unwrap().title, "current");
        assert!(!session.dirty(loaded.as_draft()));
    }

    #[test]
    fn unavailable_retry_failure_keeps_cached_draft_unsavable_without_any_ack() {
        let mut session = MetadataSession::default();
        load(&mut session, 1, "original");
        let draft = values("unsaved");
        let missing = session.begin_load(1);
        assert!(
            session
                .accept_load(missing, None, draft.as_draft())
                .is_none()
        );
        assert!(session.editable());
        assert!(!session.can_save(draft.as_draft()));

        let retry = session.begin_load(1);
        assert!(matches!(
            session.availability(),
            MetadataAvailability::Unavailable(_)
        ));
        assert!(!session.can_save(draft.as_draft()));
        assert!(session.begin_save(draft.as_draft()).is_none());
        assert!(session.fail_load(retry, "retry failed".into()));
        assert!(matches!(
            session.availability(),
            MetadataAvailability::Unavailable(_)
        ));
        assert_eq!(session.track().unwrap().title, "original");
        assert!(session.dirty(draft.as_draft()));
        assert!(!session.can_save(draft.as_draft()));
        assert!(session.begin_save(draft.as_draft()).is_none());

        let recovered = session.begin_load(1);
        assert!(
            session
                .accept_load(recovered, Some(track(1, "remote")), draft.as_draft())
                .is_none()
        );
        assert!(session.can_save(draft.as_draft()));
    }

    #[test]
    fn pending_read_survives_save_start_and_ack_and_preserves_later_edits() {
        let mut session = MetadataSession::default();
        load(&mut session, 1, "original");
        let pending = session.begin_load(1);
        let submitted = values("submitted");
        let (save, _) = session.begin_save(submitted.as_draft()).unwrap();
        assert!(session.accepts_load(pending));
        let newer = MetadataValues {
            artist: "later artist".into(),
            ..submitted.clone()
        };
        assert!(session.complete_save(save, submitted.clone(), true));
        assert!(session.accepts_load(pending));
        assert!(session.dirty(newer.as_draft()));
        let mut remote = track(1, "submitted");
        remote.album = "external album".into();
        assert!(
            session
                .accept_load(pending, Some(remote), newer.as_draft())
                .is_none()
        );
        assert_eq!(session.track().unwrap().album, "external album");
        assert!(!session.dirty(submitted.as_draft()));
        let (_, patch) = session.begin_save(newer.as_draft()).unwrap();
        assert!(matches!(patch, Command::EditTrack {
            title: None, artist: Some(artist), album: None, decoder_hint: None, ..
        } if artist == "later artist"));
    }

    #[test]
    fn save_ack_and_unavailable_retry_failure_are_independent_in_either_order() {
        for ack_first in [false, true] {
            let mut session = MetadataSession::default();
            load(&mut session, 1, "original");
            let submitted = values("submitted");
            let (save, _) = session.begin_save(submitted.as_draft()).unwrap();
            let newer = values("later edit");
            let missing = session.begin_load(1);
            assert!(
                session
                    .accept_load(missing, None, newer.as_draft())
                    .is_none()
            );
            let retry = session.begin_load(1);
            if ack_first {
                assert!(session.complete_save(save, submitted.clone(), true));
                assert!(session.accepts_load(retry));
                assert!(session.fail_load(retry, "retry failed".into()));
            } else {
                assert!(session.fail_load(retry, "retry failed".into()));
                assert!(session.complete_save(save, submitted.clone(), true));
            }
            assert!(matches!(
                session.availability(),
                MetadataAvailability::Unavailable(_)
            ));
            assert_eq!(
                session.read_state(),
                &MetadataReadState::Failed("retry failed".into())
            );
            assert!(session.editable());
            assert!(!session.dirty(submitted.as_draft()));
            assert!(session.dirty(newer.as_draft()));
            assert!(!session.can_save(newer.as_draft()));
            assert!(session.begin_save(newer.as_draft()).is_none());
        }
    }

    #[test]
    fn failed_save_ack_cannot_complete_a_new_save_retry() {
        let mut session = MetadataSession::default();
        load(&mut session, 1, "original");
        let draft = values("edited");
        let (old, _) = session.begin_save(draft.as_draft()).unwrap();
        assert!(session.complete_save(old, draft.clone(), false));
        let (current, _) = session.begin_save(draft.as_draft()).unwrap();
        assert!(!session.complete_save(old, draft.clone(), true));
        assert!(session.saving());
        assert!(session.dirty(draft.as_draft()));
        assert!(session.complete_save(current, draft.clone(), true));
        assert!(!session.dirty(draft.as_draft()));
    }

    #[test]
    fn discard_same_identity_invalidates_edit_read_and_save_callbacks() {
        let mut session = MetadataSession::default();
        load(&mut session, 1, "original");
        let editor = session.editor_identity();
        let pending = session.begin_load(1);
        let submitted = values("submitted");
        let (save, _) = session.begin_save(submitted.as_draft()).unwrap();
        session.discard_draft();
        assert_eq!(session.selected(), Some(1));
        assert_eq!(session.identity(), Some(1));
        assert_ne!(session.editor_identity(), editor);
        assert!(!session.accepts_load(pending));
        assert!(!session.complete_save(save, submitted, true));
        let fresh = load(&mut session, 1, "reloaded");
        assert_ne!(session.editor_identity(), editor);
        assert_eq!(session.track().unwrap().title, "reloaded");
        assert!(!session.dirty(fresh.as_draft()));
    }

    #[test]
    fn cached_playback_cannot_restore_an_unavailable_follow_draft_after_save_ack() {
        let mut session = MetadataSession::default();
        let playback = track(1, "original");
        session.sync_playback(Some(&playback), values("").as_draft());
        let submitted = values("submitted");
        let (save, _) = session.begin_save(submitted.as_draft()).unwrap();
        let newer = values("later edit");
        let missing = session.begin_load(1);
        assert!(
            session
                .accept_load(missing, None, newer.as_draft())
                .is_none()
        );
        assert!(session.complete_save(save, submitted, true));

        assert!(
            session
                .sync_playback(Some(&playback), newer.as_draft())
                .is_none()
        );
        assert!(matches!(
            session.availability(),
            MetadataAvailability::Unavailable(_)
        ));
        assert!(session.dirty(newer.as_draft()));
        assert!(!session.can_save(newer.as_draft()));

        let retry = session.begin_load(1);
        assert!(
            session
                .sync_playback(Some(&playback), newer.as_draft())
                .is_none()
        );
        assert!(session.accepts_load(retry));
        assert!(session.fail_load(retry, "retry failed".into()));
        assert!(
            session
                .sync_playback(Some(&playback), newer.as_draft())
                .is_none()
        );
        assert_eq!(
            session.read_state(),
            &MetadataReadState::Failed("retry failed".into())
        );
        assert!(!session.can_save(newer.as_draft()));

        let recovered = session.begin_load(1);
        assert!(
            session
                .accept_load(recovered, Some(track(1, "recovered")), newer.as_draft())
                .is_none()
        );
        assert!(session.can_save(newer.as_draft()));
    }

    #[test]
    fn clean_save_reconciles_untouched_fields_without_superseding_reads() {
        for refresh_during_save in [false, true] {
            let mut session = MetadataSession::default();
            let original = load(&mut session, 1, "original");
            let submitted = MetadataValues {
                title: "submitted".into(),
                ..original
            };
            let mut remote = track(1, "submitted");
            remote.artist = "remote artist".into();
            remote.album = "remote album".into();
            remote.decoder_hint = DecoderHint::Native;
            let mut save = None;
            if refresh_during_save {
                save = Some(session.begin_save(submitted.as_draft()).unwrap().0);
            }
            let refresh = session.begin_load(1);
            assert!(
                session
                    .accept_load(refresh, Some(remote), submitted.as_draft())
                    .is_none()
            );
            let save = save.unwrap_or_else(|| session.begin_save(submitted.as_draft()).unwrap().0);
            let pending = session.begin_load(1);
            assert!(session.complete_save(save, submitted.clone(), true));
            let reconciled = session.reconcile_clean(submitted.as_draft()).unwrap();
            assert_eq!(reconciled.title, "submitted");
            assert_eq!(reconciled.artist, "remote artist");
            assert_eq!(reconciled.album, "remote album");
            assert_eq!(reconciled.decoder_hint, DecoderHint::Native);
            assert!(!session.dirty(reconciled.as_draft()));
            assert!(session.accepts_load(pending));
            let cleared = MetadataValues {
                artist: String::new(),
                ..reconciled
            };
            let (_, command) = session.begin_save(cleared.as_draft()).unwrap();
            assert!(matches!(command, Command::EditTrack {
                title: None, artist: Some(artist), album: None, decoder_hint: None, ..
            } if artist.is_empty()));
        }
    }

    #[test]
    fn reconciliation_preserves_saving_and_later_edits() {
        let mut session = MetadataSession::default();
        let original = load(&mut session, 1, "original");
        let submitted = MetadataValues {
            title: "submitted".into(),
            ..original
        };
        let (save, _) = session.begin_save(submitted.as_draft()).unwrap();
        let refresh = session.begin_load(1);
        let mut remote = track(1, "submitted");
        remote.artist = "remote artist".into();
        assert!(
            session
                .accept_load(refresh, Some(remote), submitted.as_draft())
                .is_none()
        );
        assert!(session.reconcile_clean(submitted.as_draft()).is_none());
        let later = MetadataValues {
            title: "later edit".into(),
            ..submitted.clone()
        };
        assert!(session.complete_save(save, submitted, true));
        assert!(session.reconcile_clean(later.as_draft()).is_none());
        assert!(session.dirty(later.as_draft()));
        let (_, command) = session.begin_save(later.as_draft()).unwrap();
        assert!(matches!(command, Command::EditTrack {
            title: Some(title), artist: None, album: None, decoder_hint: None, ..
        } if title == "later edit"));
    }

    #[test]
    fn reconciliation_cannot_restore_unavailable_details_after_save() {
        let mut session = MetadataSession::default();
        let original = load(&mut session, 1, "original");
        let submitted = MetadataValues {
            title: "submitted".into(),
            ..original
        };
        let (save, _) = session.begin_save(submitted.as_draft()).unwrap();
        let missing = session.begin_load(1);
        assert!(
            session
                .accept_load(missing, None, submitted.as_draft())
                .is_none()
        );
        let retry = session.begin_load(1);
        assert!(session.complete_save(save, submitted.clone(), true));
        assert!(session.reconcile_clean(submitted.as_draft()).is_none());
        assert!(session.accepts_load(retry));
        assert!(!session.can_save(values("another edit").as_draft()));
        assert!(matches!(
            session.availability(),
            MetadataAvailability::Unavailable(_)
        ));
    }
}
