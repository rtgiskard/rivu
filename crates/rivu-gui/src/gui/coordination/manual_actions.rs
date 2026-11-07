//! Window-owned newest explicit action identity.
//! Features prepare and validate their targets before committing selections.

use crate::gui::WindowView;
use gpui::Context;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::gui) struct ManualIntentId(u64);

/// Only explicit input advances this identity. Completion supplies data for that
/// input, but cannot create, replace or cancel a newer desired action.
#[derive(Default)]
pub(in crate::gui) struct ManualIntents {
    serial: u64,
    preparing: bool,
}

impl ManualIntents {
    pub(in crate::gui) fn begin(&mut self) -> ManualIntentId {
        self.serial = self.serial.wrapping_add(1);
        self.preparing = true;
        ManualIntentId(self.serial)
    }

    pub(in crate::gui) fn current(&self, id: ManualIntentId) -> bool {
        self.serial == id.0 && self.preparing
    }

    fn offer(&mut self, id: ManualIntentId, action: ManualAction) -> Option<ManualAction> {
        if !self.current(id) {
            return None;
        }
        self.preparing = false;
        Some(action)
    }

    fn settle(&mut self, id: ManualIntentId) -> bool {
        if self.serial != id.0 || !self.preparing {
            return false;
        }
        self.preparing = false;
        true
    }

    pub(in crate::gui) fn allows_follow(&self) -> bool {
        !self.preparing
    }
}

pub(in crate::gui) enum ManualAction {
    Library(crate::gui::library::selection::LibraryCommit),
    Playlist(crate::gui::playlists::EntryCommit),
    Queue(crate::gui::queue::QueueCommit),
    History {
        commit: crate::gui::history::HistoryCommit,
        structure_revision: u64,
    },
    Clear,
    ReloadPlayback,
    Close(gpui::AnyWindowHandle),
    Quit,
}

impl WindowView {
    pub(in crate::gui) fn begin_manual_intent(&mut self, cx: &mut Context<Self>) -> ManualIntentId {
        let id = self.manual_intents.begin();
        self.library
            .update(cx, |library, cx| library.cancel_selection(cx));
        self.queue
            .update(cx, |queue, cx| queue.cancel_selection(cx));
        self.playlists
            .update(cx, |playlists, cx| playlists.cancel_selection(cx));
        self.sync_transfer_state(cx);
        id
    }

    pub(in crate::gui) fn invalidate_manual_intent(&mut self, cx: &mut Context<Self>) {
        let id = self.begin_manual_intent(cx);
        self.settle_manual_intent(id, cx);
    }

    pub(in crate::gui) fn manual_intent_current(&self, id: ManualIntentId) -> bool {
        self.manual_intents.current(id)
    }

    pub(in crate::gui) fn fail_manual_intent(
        &mut self,
        id: ManualIntentId,
        cx: &mut Context<Self>,
    ) {
        if self.manual_intent_current(id) {
            self.settle_manual_intent(id, cx);
        }
    }

    pub(in crate::gui) fn settle_manual_intent(
        &mut self,
        id: ManualIntentId,
        cx: &mut Context<Self>,
    ) {
        if self.manual_intents.settle(id) {
            self.reconcile_manual_follow(cx);
        }
    }

    pub(in crate::gui) fn reconcile_manual_follow(&mut self, cx: &mut Context<Self>) {
        // Preparing a selection holds playback follow until the intent settles.
        if self.manual_intents.allows_follow() {
            self.metadata
                .update(cx, |metadata, cx| metadata.resume_follow(cx));
        }
    }

    fn manual_action_valid(&self, action: &ManualAction, cx: &gpui::App) -> bool {
        match action {
            ManualAction::Library(commit) => self.library.read(cx).library_commit_valid(commit),
            ManualAction::Playlist(commit) => self.playlists.read(cx).entry_commit_valid(commit),
            ManualAction::Queue(commit) => self.queue.read(cx).queue_commit_valid(commit),
            ManualAction::History {
                commit,
                structure_revision,
            } => self
                .history
                .read(cx)
                .commit_valid(commit, *structure_revision),
            _ => true,
        }
    }

    pub(in crate::gui) fn ready_manual_action(
        &mut self,
        id: ManualIntentId,
        action: ManualAction,
        cx: &mut Context<Self>,
    ) {
        self.refresh(cx);
        if !self.manual_intent_current(id) {
            return;
        }
        if !self.manual_action_valid(&action, cx) {
            self.fail_manual_intent(id, cx);
            self.reject_stale_manual_action(cx);
            return;
        }
        if let Some(action) = self.manual_intents.offer(id, action) {
            self.commit_manual_action(action, cx);
        }
    }

    fn reject_stale_manual_action(&mut self, cx: &mut Context<Self>) {
        self.error = Some(self.i18n.text("gl-selection-stale-action").into());
        cx.notify();
    }

    fn commit_manual_action(&mut self, action: ManualAction, cx: &mut Context<Self>) {
        match action {
            ManualAction::Library(commit) => self.library.update(cx, |library, cx| {
                library.commit_library_selection(commit, cx);
            }),
            ManualAction::Playlist(commit) => self.playlists.update(cx, |playlists, cx| {
                playlists.commit_entry_selection(commit, cx);
            }),
            ManualAction::Queue(commit) => self.queue.update(cx, |queue, cx| {
                queue.commit_queue_selection(commit, cx);
            }),
            ManualAction::History { commit, .. } => self.history.update(cx, |history, cx| {
                history.commit_selection(commit, cx);
            }),
            ManualAction::Clear => {
                self.library
                    .update(cx, |library, cx| library.clear_selection(cx));
                self.queue.update(cx, |queue, cx| queue.clear_selection(cx));
                self.playlists
                    .update(cx, |playlists, cx| playlists.clear_selection(cx));
                self.history
                    .update(cx, |history, cx| history.clear_selection(cx));
                self.metadata
                    .update(cx, |metadata, cx| metadata.clear_selection(cx));
            }
            ManualAction::ReloadPlayback => {
                self.metadata.update(cx, |metadata, cx| {
                    metadata.clear_selection(cx);
                });
            }
            ManualAction::Close(window) => {
                self.content_resolution.cancel();
                cx.defer(move |cx| {
                    let _ = window.update(cx, |_, window, _| {
                        window.remove_window();
                    });
                });
            }
            ManualAction::Quit => {
                self.content_resolution.cancel();
                self.finish_quit();
            }
        }
        cx.notify();
    }

    pub(in crate::gui) fn request_clear_selection(&mut self, cx: &mut Context<Self>) {
        let id = self.begin_manual_intent(cx);
        self.ready_manual_action(id, ManualAction::Clear, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::{ManualAction, ManualIntents};

    #[test]
    fn late_completion_cannot_replace_newer_action() {
        let mut owner = ManualIntents::default();
        let old = owner.begin();
        let current = owner.begin();
        assert!(owner.offer(old, ManualAction::Clear).is_none());
        assert!(owner.current(current));
        assert!(matches!(
            owner.offer(current, ManualAction::Quit),
            Some(ManualAction::Quit)
        ));
        assert!(owner.offer(current, ManualAction::Clear).is_none());
        assert!(owner.allows_follow());
    }

    #[test]
    fn settlement_releases_preparation_once_without_settling_newer_intent() {
        let mut owner = ManualIntents::default();
        let old = owner.begin();
        let current = owner.begin();
        assert!(!owner.settle(old));
        assert!(!owner.allows_follow());
        assert!(owner.settle(current));
        assert!(!owner.settle(current));
        assert!(owner.allows_follow());
        assert!(owner.offer(current, ManualAction::Clear).is_none());
    }
}
