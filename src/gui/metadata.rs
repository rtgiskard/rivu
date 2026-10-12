//! One metadata edit session follows either an explicit selection or playback.
//!
//! Input entities remain window-owned and are the only draft storage. A loaded
//! track supplies display details; its refresh must not move a dirty edit's
//! baseline. Identity epochs reject responses and save acknowledgements from a
//! previous selection, including a switch away and back to the same track.
//! Playlist container/membership changes do not change this track identity;
//! they must not cancel the editor or its in-flight save.

use crate::model::Track;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct MetadataValues {
    pub(super) title: String,
    pub(super) artist: String,
    pub(super) album: String,
}

impl MetadataValues {
    fn from_track(track: &Track) -> Self {
        Self {
            title: track.title.clone(),
            artist: track.artist.clone(),
            album: track.album.clone(),
        }
    }

    pub(super) fn as_draft(&self) -> MetadataDraft<'_> {
        MetadataDraft {
            title: &self.title,
            artist: &self.artist,
            album: &self.album,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct MetadataDraft<'a> {
    pub(super) title: &'a str,
    pub(super) artist: &'a str,
    pub(super) album: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct LoadToken {
    epoch: u64,
    request: u64,
    track_id: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SaveToken {
    epoch: u64,
    request: u64,
    pub(super) track_id: i64,
}

#[derive(Default)]
pub(super) struct MetadataSession {
    selected: Option<i64>,
    identity: Option<i64>,
    track: Option<Track>,
    baseline: Option<MetadataValues>,
    pending_edits: u8,
    epoch: u64,
    request: u64,
    saving: Option<SaveToken>,
}

impl MetadataSession {
    pub(super) fn selected(&self) -> Option<i64> {
        self.selected
    }
    pub(super) fn track(&self) -> Option<&Track> {
        self.track.as_ref()
    }
    pub(super) fn saving(&self) -> bool {
        self.saving.is_some()
    }

    pub(super) fn dirty(&self, draft: MetadataDraft<'_>) -> bool {
        self.baseline.as_ref().is_some_and(|baseline| {
            baseline.title != draft.title
                || baseline.artist != draft.artist
                || baseline.album != draft.album
        })
    }

    pub(super) fn edited_before_load(&mut self, field: Field) {
        if self.baseline.is_none() && self.identity.is_some() {
            self.pending_edits |= match field {
                Field::Title => 1,
                Field::Artist => 2,
                Field::Album => 4,
                _ => return,
            };
        }
    }

    fn change_identity(&mut self, id: Option<i64>) {
        self.identity = id;
        self.epoch = self.epoch.wrapping_add(1);
        self.request = self.request.wrapping_add(1);
        self.track = None;
        self.baseline = None;
        self.saving = None;
        self.pending_edits = 0;
    }

    /// Returns whether inputs must be cleared before this selection loads.
    pub(super) fn select(&mut self, id: i64) -> bool {
        let changed = self.identity != Some(id);
        if changed {
            self.change_identity(Some(id));
        }
        self.selected = Some(id);
        changed
    }

    pub(super) fn clear_selection(&mut self) {
        self.selected = None;
        self.change_identity(None);
    }

    pub(super) fn begin_load(&mut self, id: i64) -> LoadToken {
        debug_assert_eq!(self.identity, Some(id));
        self.request = self.request.wrapping_add(1);
        LoadToken {
            epoch: self.epoch,
            request: self.request,
            track_id: id,
        }
    }

    pub(super) fn accepts_load(&self, token: LoadToken) -> bool {
        self.epoch == token.epoch
            && self.request == token.request
            && self.identity == Some(token.track_id)
    }

    /// `Some(values)` replaces widget text. Dirty drafts keep their old baseline.
    pub(super) fn accept_load(
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
            return None;
        }
        self.accept_track(track, draft)
    }

    fn accept_track(
        &mut self,
        track: Option<Track>,
        draft: MetadataDraft<'_>,
    ) -> Option<MetadataValues> {
        let Some(track) = track else {
            self.change_identity(None);
            return Some(MetadataValues::default());
        };
        let first_load = self.baseline.is_none();
        let replace = first_load || (!self.dirty(draft) && !self.saving());
        let mut values = replace.then(|| MetadataValues::from_track(&track));
        if let Some(values) = values.as_ref() {
            self.baseline = Some(values.clone());
        }
        // A formerly focused widget can receive input while the new query loads.
        // Preserve every user-touched field, including explicitly cleared text.
        if first_load {
            let values = values.as_mut().expect("first load supplies a baseline");
            if self.pending_edits & 1 != 0 {
                values.title = draft.title.to_owned();
            }
            if self.pending_edits & 2 != 0 {
                values.artist = draft.artist.to_owned();
            }
            if self.pending_edits & 4 != 0 {
                values.album = draft.album.to_owned();
            }
            self.pending_edits = 0;
        }
        self.track = Some(track);
        values
    }

    pub(super) fn sync_playback(
        &mut self,
        track: Option<&Track>,
        draft: MetadataDraft<'_>,
    ) -> Option<MetadataValues> {
        if self.selected.is_some() {
            return None;
        }
        let id = track.map(|track| track.id);
        if self.identity != id {
            self.change_identity(id);
            if track.is_none() {
                return Some(MetadataValues::default());
            }
            let empty = MetadataValues::default();
            return self.accept_track(track.cloned(), empty.as_draft());
        }
        if self.track.as_ref() == track {
            return None;
        }
        self.accept_track(track.cloned(), draft)
    }

    pub(super) fn begin_save(&mut self, draft: MetadataDraft<'_>) -> Option<(SaveToken, Command)> {
        let track_id = self.track.as_ref()?.id;
        if self.saving() || !self.dirty(draft) {
            return None;
        }
        let baseline = self.baseline.as_ref()?;
        let command = Command::EditTrack {
            track_id,
            title: (baseline.title != draft.title).then(|| draft.title.to_owned()),
            artist: (baseline.artist != draft.artist).then(|| draft.artist.to_owned()),
            album: (baseline.album != draft.album).then(|| draft.album.to_owned()),
        };
        let token = SaveToken {
            epoch: self.epoch,
            request: self.request.wrapping_add(1),
            track_id,
        };
        self.saving = Some(token);
        Some((token, command))
    }

    /// Never writes widgets: edits typed while saving remain the active draft.
    pub(super) fn complete_save(
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
            self.request = self.request.wrapping_add(1);
            if let (Some(track), Some(baseline)) = (self.track.as_mut(), self.baseline.as_ref()) {
                // Refreshes may have updated other fields while this save was in flight.
                // Only the submitted changes are acknowledged; untouched draft fields
                // keep their baseline so a later save cannot turn them into overrides.
                if values.title != baseline.title {
                    track.title.clone_from(&values.title);
                }
                if values.artist != baseline.artist {
                    track.artist.clone_from(&values.artist);
                }
                if values.album != baseline.album {
                    track.album.clone_from(&values.album);
                }
            }
            self.baseline = Some(values);
        }
        true
    }
}

use super::{Field, GuiApp};
use crate::{
    model::{Command, Query},
    response::ViewResponse,
};
use gpui::{App, Context};

impl GuiApp {
    pub(super) fn metadata_dirty(&self, cx: &App) -> bool {
        self.metadata.dirty(MetadataDraft {
            title: self.inputs[&Field::Title].read(cx).text(),
            artist: self.inputs[&Field::Artist].read(cx).text(),
            album: self.inputs[&Field::Album].read(cx).text(),
        })
    }

    fn set_metadata_values(&mut self, values: MetadataValues, cx: &mut Context<Self>) {
        self.set_value(Field::Title, values.title, cx);
        self.set_value(Field::Artist, values.artist, cx);
        self.set_value(Field::Album, values.album, cx);
    }

    pub(super) fn request_track(&mut self, id: i64, cx: &mut Context<Self>) {
        if self.metadata.select(id) {
            self.set_metadata_values(MetadataValues::default(), cx);
        }
        let token = self.metadata.begin_load(id);
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            // Retain the existing quiet interval without querying superseded selections.
            cx.background_executor()
                .timer(std::time::Duration::from_millis(80))
                .await;
            if !this
                .update(cx, |this, _| this.metadata.accepts_load(token))
                .unwrap_or(false)
            {
                return;
            }
            let response = cx
                .background_executor()
                .spawn(async move { handle.query(Query::Track { track_id: id }) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !this.metadata.accepts_load(token) {
                    return;
                }
                let result = match response.result {
                    Ok(result) => result,
                    Err(error) => {
                        this.error = Some(error.to_string());
                        cx.notify();
                        return;
                    }
                };
                if response.revisions.library != this.state.library.revision {
                    this.request_track(id, cx);
                    return;
                }
                if let ViewResponse::Track(track) = result {
                    let draft = MetadataDraft {
                        title: this.inputs[&Field::Title].read(cx).text(),
                        artist: this.inputs[&Field::Artist].read(cx).text(),
                        album: this.inputs[&Field::Album].read(cx).text(),
                    };
                    if let Some(values) = this.metadata.accept_load(token, track, draft) {
                        this.set_metadata_values(values, cx);
                    }
                    this.sync_waveform(cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn clear_metadata_selection(&mut self, cx: &mut Context<Self>) {
        self.metadata.clear_selection();
        self.set_metadata_values(MetadataValues::default(), cx);
        self.sync_metadata_default(cx);
        self.sync_waveform(cx);
    }

    pub(super) fn sync_metadata_default(&mut self, cx: &mut Context<Self>) {
        let draft = MetadataDraft {
            title: self.inputs[&Field::Title].read(cx).text(),
            artist: self.inputs[&Field::Artist].read(cx).text(),
            album: self.inputs[&Field::Album].read(cx).text(),
        };
        if let Some(values) = self
            .metadata
            .sync_playback(self.state.current_track(), draft)
        {
            self.set_metadata_values(values, cx);
        }
    }

    pub(super) fn save_metadata(&mut self, cx: &mut Context<Self>) {
        let values = MetadataValues {
            title: self.value(Field::Title, cx),
            artist: self.value(Field::Artist, cx),
            album: self.value(Field::Album, cx),
        };
        let Some((token, command)) = self.metadata.begin_save(values.as_draft()) else {
            return;
        };
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            let ack = cx
                .background_executor()
                .spawn(async move { handle.request_ack(command) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !this.metadata.complete_save(token, values, ack.ok) {
                    return;
                }
                if ack.ok {
                    if let Some(id) = this.metadata.selected() {
                        this.request_track(id, cx);
                    }
                } else {
                    this.error = Some(
                        ack.error
                            .unwrap_or_else(|| "Could not save metadata".into()),
                    );
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            track_number: None,
            disc_number: None,
            bits_per_sample: None,
            release_date: None,
            favorite: false,
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
        let mut remote = track(1, "saved title");
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
        assert!(!session.accepts_load(pending_load));
        assert!(!session.dirty(submitted.as_draft()));
        assert!(session.dirty(values("typed while saving").as_draft()));
        let (token, _) = session.begin_save(values("failed").as_draft()).unwrap();
        assert!(session.complete_save(token, values("failed"), false));
        assert!(session.dirty(values("failed").as_draft()));
    }

    #[test]
    fn unloaded_removed_and_cancelled_sessions_cannot_save_old_drafts() {
        let mut session = MetadataSession::default();
        session.select(1);
        assert!(session.begin_save(values("premature").as_draft()).is_none());
        load(&mut session, 1, "one");
        let token = session.begin_load(1);
        assert_eq!(
            session.accept_load(token, None, values("dirty").as_draft()),
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
    fn selection_of_playback_identity_keeps_draft_and_pending_load_preserves_typed_fields() {
        let mut session = MetadataSession::default();
        session.sync_playback(Some(&track(1, "playing")), values("").as_draft());
        assert!(!session.select(1));
        assert!(session.dirty(values("draft").as_draft()));
        session.select(2);
        let token = session.begin_load(2);
        session.edited_before_load(Field::Title);
        let mut remote = track(2, "loaded");
        remote.artist = "remote artist".into();
        let loaded = session
            .accept_load(
                token,
                Some(remote),
                values("typed while loading").as_draft(),
            )
            .unwrap();
        assert_eq!(loaded.title, "typed while loading");
        assert_eq!(loaded.artist, "remote artist");
        assert!(session.dirty(loaded.as_draft()));
    }

    #[test]
    fn pending_user_clear_survives_load_but_switching_track_discards_pending_edits() {
        let mut session = MetadataSession::default();
        session.select(1);
        session.edited_before_load(Field::Title);
        let token = session.begin_load(1);
        let loaded = session
            .accept_load(token, Some(track(1, "remote")), values("").as_draft())
            .unwrap();
        assert_eq!(loaded.title, "");
        assert!(session.dirty(loaded.as_draft()));
        session.select(2);
        let token = session.begin_load(2);
        let loaded = session
            .accept_load(token, Some(track(2, "next")), values("").as_draft())
            .unwrap();
        assert_eq!(loaded.title, "next");
        assert!(!session.dirty(loaded.as_draft()));
    }
}
