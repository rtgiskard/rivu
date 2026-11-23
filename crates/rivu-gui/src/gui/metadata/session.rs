//! Track details identity and independent read/write progress.
//! Unsubmitted inline text belongs to the view, never to selection arbitration.
//! Epochs reject late reads and acknowledgements after leaving and reopening a track.

use super::source::{SourceAction, SourceTags, SourceToken};
use rivu_format::decoder_hint::DecoderHint;
use rivu_format::tags::TagValues;
use rivu_presentation::refresh::RefreshDeadline;
use rivu_protocol::model::{Command, SourceTagRecovery, Track};
use std::{collections::HashMap, sync::Arc, time::Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Field {
    Title,
    Artist,
    Album,
    ReleaseDate,
    PlayCount,
    DecoderHint,
    ReplayGainTrackGain,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum SavedValue {
    Title(String),
    Artist(String),
    Album(String),
    ReleaseDate(String),
    PlayCount(u64),
    DecoderHint(DecoderHint),
    ReplayGain(Option<f64>),
}

impl SavedValue {
    pub(super) fn field(&self) -> Field {
        match self {
            Self::Title(_) => Field::Title,
            Self::Artist(_) => Field::Artist,
            Self::Album(_) => Field::Album,
            Self::ReleaseDate(_) => Field::ReleaseDate,
            Self::PlayCount(_) => Field::PlayCount,
            Self::DecoderHint(_) => Field::DecoderHint,
            Self::ReplayGain(_) => Field::ReplayGainTrackGain,
        }
    }

    fn changed(&self, track: &Track) -> bool {
        match self {
            Self::Title(value) => *value != track.title,
            Self::Artist(value) => *value != track.artist,
            Self::Album(value) => *value != track.album,
            Self::ReleaseDate(value) => value != track.release_date.as_deref().unwrap_or_default(),
            Self::PlayCount(value) => *value != track.play_count,
            Self::DecoderHint(value) => *value != track.decoder_hint,
            Self::ReplayGain(value) => *value != track.replaygain_track_gain,
        }
    }

    fn command(&self, track_id: i64) -> Command {
        if let Self::ReplayGain(value) = self {
            return Command::SetReplayGain {
                track_id,
                value: *value,
            };
        }
        if let Self::PlayCount(play_count) = self {
            return Command::SetPlayCount {
                track_id,
                play_count: *play_count,
            };
        }
        Command::EditTrack {
            track_id,
            title: if let Self::Title(value) = self {
                Some(value.clone())
            } else {
                None
            },
            artist: if let Self::Artist(value) = self {
                Some(value.clone())
            } else {
                None
            },
            album: if let Self::Album(value) = self {
                Some(value.clone())
            } else {
                None
            },
            decoder_hint: if let Self::DecoderHint(value) = self {
                Some(*value)
            } else {
                None
            },
            release_date: if let Self::ReleaseDate(value) = self {
                Some(value.clone())
            } else {
                None
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::gui) struct EditorIdentity {
    epoch: u64,
    pub(super) track_id: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::gui) struct LoadToken {
    epoch: u64,
    request: u64,
    track_id: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SaveToken {
    epoch: u64,
    request: u64,
    pub(super) track_id: i64,
    pub(super) field: Field,
    pub(super) overlay_serial: u64,
}

#[derive(Debug, Default, PartialEq)]
pub(in crate::gui) enum MetadataAvailability {
    #[default]
    Unknown,
    Present(Arc<Track>),
    Unavailable,
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
    read_state: MetadataReadState,
    retention: RefreshDeadline,
    // The same-identity source remains owned after its details stop being displayed.
    details_expired: bool,
    epoch: u64,
    request_serial: u64,
    database_serial: u64,
    // The command committed, but playback may still expose its old tag baseline.
    playback_tag_commit: Option<(i64, TagValues)>,
    // A new commit can block the same baseline while a confirmation is in flight.
    playback_tag_commit_serial: u64,
    saves: HashMap<Field, SaveToken>,
    pub(super) source: SourceTags,
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
    pub(super) fn display_track_at(&self, now: Instant) -> Option<&Track> {
        if self.details_expired || self.retention.expired(now) {
            return None;
        }
        self.source_track()
    }

    pub(super) fn source_track(&self) -> Option<&Track> {
        match &self.availability {
            MetadataAvailability::Present(track) => Some(track),
            _ => None,
        }
    }
    pub(super) fn authoritative_track(&self) -> Option<&Track> {
        if self.details_expired
            || self.retention.deadline().is_some()
            || (self.selected.is_none() && self.playback_tag_commit.is_some())
        {
            return None;
        }
        self.source_track()
    }
    pub(in crate::gui) fn track_snapshot(&self) -> Option<Arc<Track>> {
        match &self.availability {
            MetadataAvailability::Present(track) => Some(Arc::clone(track)),
            _ => None,
        }
    }
    pub(in crate::gui) fn availability(&self, now: Instant) -> &MetadataAvailability {
        if matches!(self.availability, MetadataAvailability::Present(_))
            && self.display_track_at(now).is_none()
        {
            &MetadataAvailability::Unknown
        } else {
            &self.availability
        }
    }
    pub(in crate::gui) fn read_state(&self) -> &MetadataReadState {
        &self.read_state
    }
    pub(in crate::gui) fn editable(&self) -> bool {
        self.authoritative_track().is_some()
    }
    pub(super) fn saving(&self, field: Field) -> bool {
        self.saves.contains_key(&field)
    }

    pub(super) fn any_saving(&self) -> bool {
        !self.saves.is_empty()
    }

    pub(super) fn invalidate_at(&mut self, now: Instant) {
        if !self.details_expired && matches!(self.availability, MetadataAvailability::Present(_)) {
            self.retention.begin(now);
        }
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.retention.deadline()
    }

    pub(super) fn expire(&mut self, now: Instant) -> bool {
        if !self.retention.expire(now) {
            return false;
        }
        let changed =
            !self.details_expired && matches!(self.availability, MetadataAvailability::Present(_));
        self.details_expired = true;
        changed
    }

    fn advance_database_serial(&mut self, next: Option<&Track>) {
        let unchanged = match (self.source_track(), next) {
            (Some(previous), Some(next)) => previous.tag_view() == next.tag_view(),
            (None, None) => true,
            _ => false,
        };
        if !unchanged {
            self.database_serial = self.database_serial.wrapping_add(1);
        }
    }

    pub(super) fn observe_source_recovery(
        &mut self,
        recovery: Option<&Arc<SourceTagRecovery>>,
    ) -> bool {
        self.source.observe_recovery(self.identity, recovery)
    }

    pub(super) fn begin_source(&mut self, action: SourceAction) -> Option<(SourceToken, Command)> {
        if self.any_saving() {
            return None;
        }
        let identity = self.editor_identity()?;
        let track = self.authoritative_track()?;
        let track_id = track.id;
        if !self.source.allows(track_id, action) {
            return None;
        }
        let action = if action == SourceAction::Sync && self.source.recovery(track_id).is_some() {
            SourceAction::RetrySync
        } else {
            action
        };
        let command = match action {
            SourceAction::Load(encoding) => Command::LoadSourceTags { track_id, encoding },
            SourceAction::RetrySync => Command::RetrySourceTagSync {
                track_id,
                tags: self.source.recovery(track_id)?.tags.clone(),
            },
            SourceAction::Sync | SourceAction::Save => {
                let tags = self.source.values(track);
                let expected = Box::new(track.tag_view().into_owned());
                match action {
                    SourceAction::Sync => Command::SyncTrackTags {
                        track_id,
                        tags,
                        expected,
                    },
                    SourceAction::Save => Command::SaveSourceTags {
                        track_id,
                        tags,
                        expected,
                    },
                    SourceAction::Load(_) | SourceAction::RetrySync => unreachable!(),
                }
            }
        };
        let token = self.source.begin(identity, action, self.database_serial)?;
        Some((token, command))
    }

    /// Rebase explicit selection tag fields, never Core's playback projection.
    pub(super) fn commit_tags(&mut self, token: SourceToken, tags: &TagValues) {
        if self.selected.is_none() || self.database_serial != token.database_serial {
            return;
        }
        if let MetadataAvailability::Present(track) = &mut self.availability {
            if track.tag_view() == tags.as_view() {
                return;
            }
            self.database_serial = self.database_serial.wrapping_add(1);
            let track = Arc::make_mut(track);
            track.title.clone_from(&tags.title);
            track.artist.clone_from(&tags.artist);
            track.album.clone_from(&tags.album);
            track.release_date.clone_from(&tags.release_date);
            track.track_number = tags.track_number;
            track.disc_number = tags.disc_number;
            track.replaygain_track_gain = tags.replaygain_track_gain;
        }
    }

    fn change_identity(&mut self, id: Option<i64>) {
        self.identity = id;
        self.epoch = self.epoch.wrapping_add(1);
        self.availability = MetadataAvailability::Unknown;
        self.retention.clear();
        self.details_expired = false;
        self.saves.clear();
        self.source.change_selection();
        self.read_state = MetadataReadState::Idle;
    }

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

    pub(in crate::gui) fn begin_load(&mut self, id: i64) -> LoadToken {
        debug_assert_eq!(self.selected, Some(id));
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
            && self.selected == Some(token.track_id)
    }

    pub(in crate::gui) fn retire_load(&mut self, token: LoadToken) -> bool {
        if !self.accepts_load(token) {
            return false;
        }
        self.read_state = MetadataReadState::Idle;
        true
    }

    pub(in crate::gui) fn fail_load(&mut self, token: LoadToken, error: String) -> bool {
        if !self.accepts_load(token) {
            return false;
        }
        self.read_state = MetadataReadState::Failed(error);
        true
    }

    pub(in crate::gui) fn accept_load(&mut self, token: LoadToken, track: Option<Track>) -> bool {
        if !self.accepts_load(token) {
            return false;
        }
        if track
            .as_ref()
            .is_some_and(|track| track.id != token.track_id)
        {
            self.read_state = MetadataReadState::WrongTrack;
            return false;
        }
        self.read_state = MetadataReadState::Idle;
        self.retention.clear();
        self.details_expired = false;
        self.advance_database_serial(track.as_ref());
        if !matches!((&self.availability, &track),
            (MetadataAvailability::Present(previous), Some(next)) if previous.as_ref() == next)
        {
            self.availability = track.map_or(MetadataAvailability::Unavailable, |track| {
                MetadataAvailability::Present(Arc::new(track))
            });
        }
        true
    }

    /// Track only the current playback projection. Explicit selection obtains
    /// its own database baseline through the existing selected-track read.
    pub(super) fn observe_tag_commit(
        &mut self,
        track_id: i64,
        tags: &TagValues,
        refresh_failed: bool,
        playback: Option<&Track>,
    ) -> bool {
        let was_waiting = self.playback_tag_commit.is_some();
        if !refresh_failed && !was_waiting {
            return false;
        }
        let Some(playback) = playback.filter(|track| track.id == track_id) else {
            return false;
        };
        // A later successful write may supersede the one we were waiting for.
        let playback_tags = playback.tag_view();
        let waiting = playback_tags != tags.as_view();
        self.playback_tag_commit = waiting.then(|| (track_id, playback_tags.into_owned()));
        if waiting {
            self.playback_tag_commit_serial = self.playback_tag_commit_serial.wrapping_add(1);
        }
        waiting != was_waiting
    }

    pub(super) fn playback_tag_commit_serial(&self) -> Option<u64> {
        self.playback_tag_commit
            .as_ref()
            .map(|_| self.playback_tag_commit_serial)
    }

    pub(super) fn confirm_playback_tags(&mut self, serial: u64, track: &Track) -> bool {
        if self.playback_tag_commit_serial == serial
            && self
                .playback_tag_commit
                .as_ref()
                .is_some_and(|(id, stale)| *id == track.id && track.tag_view() == stale.as_view())
        {
            self.playback_tag_commit = None;
            return true;
        }
        false
    }

    pub(super) fn sync_playback(&mut self, track: Option<&Arc<Track>>) {
        if self
            .playback_tag_commit
            .as_ref()
            .is_some_and(|(id, stale)| {
                track.is_none_or(|track| track.id != *id || track.tag_view() != stale.as_view())
            })
        {
            self.playback_tag_commit = None;
        }
        if self.selected.is_some() {
            return;
        }
        let id = track.map(|track| track.id);
        if self.identity != id {
            self.change_identity(id);
        }
        self.advance_database_serial(track.map(Arc::as_ref));
        self.retention.clear();
        self.details_expired = false;
        self.availability = track.map_or(MetadataAvailability::Unknown, |track| {
            MetadataAvailability::Present(Arc::clone(track))
        });
    }

    pub(super) fn begin_save(&mut self, value: &SavedValue) -> Option<(SaveToken, Command)> {
        let field = value.field();
        let track = self.authoritative_track()?;
        if self.source.busy() || self.saving(field) || !value.changed(track) {
            return None;
        }
        let track_id = track.id;
        self.request_serial = self.request_serial.wrapping_add(1);
        let token = SaveToken {
            epoch: self.epoch,
            request: self.request_serial,
            track_id,
            field,
            overlay_serial: self.source.generation(),
        };
        self.saves.insert(field, token);
        Some((token, value.command(track_id)))
    }

    pub(super) fn complete_save(&mut self, token: SaveToken) -> bool {
        if self.epoch != token.epoch
            || self.identity != Some(token.track_id)
            || self.saves.get(&token.field) != Some(&token)
        {
            return false;
        }
        self.saves.remove(&token.field);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::metadata::tests::track;

    fn loaded() -> MetadataSession {
        let mut session = MetadataSession::default();
        session.select(1);
        let token = session.begin_load(1);
        assert!(session.accept_load(token, Some(track(1, "original"))));
        session
    }

    #[test]
    fn saves_are_field_scoped_and_hint_is_independent() {
        let mut session = loaded();
        let (title, command) = session
            .begin_save(&SavedValue::Title("new".into()))
            .unwrap();
        assert!(matches!(
            command,
            Command::EditTrack {
                title: Some(_),
                artist: None,
                album: None,
                decoder_hint: None,
                ..
            }
        ));
        assert!(
            session
                .begin_save(&SavedValue::Title("later".into()))
                .is_none()
        );
        let (hint, _) = session
            .begin_save(&SavedValue::DecoderHint(DecoderHint::Native))
            .unwrap();
        assert!(session.complete_save(hint));
        assert!(session.saving(Field::Title));
        assert!(session.complete_save(title));
        assert!(!session.complete_save(title));
    }

    #[test]
    fn selection_change_invalidates_writes_even_after_returning() {
        let mut session = loaded();
        let (save, _) = session.begin_save(&SavedValue::PlayCount(42)).unwrap();
        session.select(2);
        session.select(1);
        let load = session.begin_load(1);
        assert!(session.accept_load(load, Some(track(1, "returned"))));
        assert!(!session.complete_save(save));
        assert_eq!(
            session.display_track_at(Instant::now()).unwrap().title,
            "returned"
        );
    }

    #[test]
    fn unchanged_fields_do_not_write_and_count_uses_dedicated_command() {
        let mut session = loaded();
        assert!(
            session
                .begin_save(&SavedValue::Title("original".into()))
                .is_none()
        );
        assert!(session.begin_save(&SavedValue::PlayCount(0)).is_none());
        let (_, command) = session.begin_save(&SavedValue::PlayCount(42)).unwrap();
        assert!(matches!(
            command,
            Command::SetPlayCount {
                track_id: 1,
                play_count: 42
            }
        ));
    }

    #[test]
    fn read_and_save_tokens_do_not_supersede_each_other() {
        let mut session = loaded();
        let read = session.begin_load(1);
        let (save, _) = session
            .begin_save(&SavedValue::Artist("new".into()))
            .unwrap();
        assert!(session.accepts_load(read));
        assert!(session.accept_load(read, Some(track(1, "refreshed"))));
        assert!(session.complete_save(save));
        assert_eq!(
            session.display_track_at(Instant::now()).unwrap().title,
            "refreshed"
        );
    }

    #[test]
    fn unavailable_explicit_row_cannot_be_restored_by_playback_or_ack() {
        let mut session = loaded();
        let (save, _) = session
            .begin_save(&SavedValue::Title("new".into()))
            .unwrap();
        let read = session.begin_load(1);
        assert!(session.accept_load(read, None));
        session.sync_playback(Some(&Arc::new(track(1, "stale"))));
        assert!(session.complete_save(save));
        assert!(session.display_track_at(Instant::now()).is_none());
    }
    #[test]
    fn stale_details_expire_once_without_authorizing_writes_or_losing_source_drafts() {
        use std::time::Duration;

        let mut session = loaded();
        let start = Instant::now();
        let (save, _) = session.begin_save(&SavedValue::PlayCount(7)).unwrap();
        let staged = session
            .display_track_at(Instant::now())
            .unwrap()
            .tag_view()
            .into_owned();
        session.source.stage(staged.clone());
        session.invalidate_at(start);
        let deadline = session.deadline();
        assert!(!session.editable());
        assert!(
            session
                .begin_save(&SavedValue::Title("stale".into()))
                .is_none()
        );
        assert!(session.begin_source(SourceAction::Sync).is_none());
        session.invalidate_at(start + Duration::from_millis(150));
        assert_eq!(session.deadline(), deadline);
        assert!(
            session
                .display_track_at(start + Duration::from_millis(199))
                .is_some()
        );
        assert!(
            session
                .display_track_at(start + rivu_presentation::refresh::REFRESH_RETENTION)
                .is_none()
        );
        assert!(session.expire(start + rivu_presentation::refresh::REFRESH_RETENTION));
        assert!(!session.expire(start + Duration::from_secs(1)));
        assert!(session.complete_save(save));
        let read = session.begin_load(1);
        assert!(session.accept_load(read, Some(track(1, "fresh"))));
        assert!(session.editable());
        assert_eq!(
            session
                .source
                .values(session.display_track_at(Instant::now()).unwrap()),
            staged
        );
        session.invalidate_at(start + Duration::from_secs(1));
        session.select(2);
        assert!(session.display_track_at(Instant::now()).is_none());
        assert!(session.deadline().is_none());
    }

    #[test]
    fn equal_fresh_details_keep_the_display_allocation_and_restore_authority() {
        let mut session = loaded();
        let previous = session.track_snapshot().unwrap();
        session.invalidate_at(Instant::now());
        let read = session.begin_load(1);
        assert!(session.accept_load(read, Some(track(1, "original"))));
        assert!(session.editable());
        assert!(session.deadline().is_none());
        assert!(Arc::ptr_eq(&previous, &session.track_snapshot().unwrap()));
    }
}
