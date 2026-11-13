//! Track details identity and independent read/write progress.
//! Unsubmitted inline text belongs to the view, never to selection arbitration.
//! Epochs reject late reads and acknowledgements after leaving and reopening a track.

use rivu_format::decoder_hint::DecoderHint;
use rivu_protocol::model::{Command, Track};
use std::{collections::HashMap, sync::Arc};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Field {
    Title,
    Artist,
    Album,
    ReleaseDate,
    PlayCount,
    DecoderHint,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SavedValue {
    Title(String),
    Artist(String),
    Album(String),
    ReleaseDate(String),
    PlayCount(u64),
    DecoderHint(DecoderHint),
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
        }
    }

    fn command(&self, track_id: i64) -> Command {
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
    track_id: i64,
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
    field: Field,
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
    epoch: u64,
    request_serial: u64,
    saves: HashMap<Field, SaveToken>,
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
            _ => None,
        }
    }
    pub(in crate::gui) fn track_snapshot(&self) -> Option<Arc<Track>> {
        match &self.availability {
            MetadataAvailability::Present(track) => Some(Arc::clone(track)),
            _ => None,
        }
    }
    pub(in crate::gui) fn availability(&self) -> &MetadataAvailability {
        &self.availability
    }
    pub(in crate::gui) fn read_state(&self) -> &MetadataReadState {
        &self.read_state
    }
    pub(in crate::gui) fn editable(&self) -> bool {
        self.track().is_some()
    }
    pub(super) fn saving(&self, field: Field) -> bool {
        self.saves.contains_key(&field)
    }

    fn change_identity(&mut self, id: Option<i64>) {
        self.identity = id;
        self.epoch = self.epoch.wrapping_add(1);
        self.availability = MetadataAvailability::Unknown;
        self.saves.clear();
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
        self.availability = track.map_or(MetadataAvailability::Unavailable, |track| {
            MetadataAvailability::Present(Arc::new(track))
        });
        true
    }

    pub(in crate::gui) fn sync_playback(&mut self, track: Option<&Track>) -> bool {
        if self.selected.is_some() {
            return false;
        }
        let id = track.map(|track| track.id);
        if self.identity != id {
            self.change_identity(id);
        } else if matches!(self.availability, MetadataAvailability::Unavailable)
            || self.track() == track
        {
            return false;
        }
        self.availability = track.map_or(MetadataAvailability::Unknown, |track| {
            MetadataAvailability::Present(Arc::new(track.clone()))
        });
        true
    }

    pub(super) fn begin_save(&mut self, value: &SavedValue) -> Option<(SaveToken, Command)> {
        let field = value.field();
        let track = self.track()?;
        if self.saving(field) || !value.changed(track) {
            return None;
        }
        let track_id = track.id;
        self.request_serial = self.request_serial.wrapping_add(1);
        let token = SaveToken {
            epoch: self.epoch,
            request: self.request_serial,
            track_id,
            field,
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
        assert_eq!(session.track().unwrap().title, "returned");
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
        let (_, command) = session
            .begin_save(&SavedValue::PlayCount(u64::MAX))
            .unwrap();
        assert!(matches!(
            command,
            Command::SetPlayCount {
                track_id: 1,
                play_count: u64::MAX
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
        assert_eq!(session.track().unwrap().title, "refreshed");
    }

    #[test]
    fn unavailable_row_cannot_be_restored_by_playback_or_ack() {
        let mut session = loaded();
        session.selected = None;
        let (save, _) = session
            .begin_save(&SavedValue::Title("new".into()))
            .unwrap();
        let read = session.begin_load(1);
        assert!(session.accept_load(read, None));
        assert!(!session.sync_playback(Some(&track(1, "stale"))));
        assert!(session.complete_save(save));
        assert!(session.track().is_none());
    }
}
