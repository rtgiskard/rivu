//! Window-owned explicit actions and their metadata confirmation barrier.
//! Feature owners prepare, validate, and commit their selections; this coordinator
//! owns only cross-feature intent identity and the window's confirmation dialog.
//! Refresh reconciles owners before a frozen target is validated or a draft discarded.

use super::{
    GuiApp, UI_INSET,
    components::{column, icon_button, panel_header, row},
};
use gpui::{AnyElement, Context, prelude::*, px, rgb};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::gui) struct ManualIntentId(u64);

/// Only explicit input advances this identity. Completion supplies data for that
/// input, but cannot create, replace or cancel a newer desired action.
#[derive(Default)]
pub(in crate::gui) struct ManualIntents {
    serial: u64,
    preparing: bool,
    pending: Option<(ManualIntentId, ManualAction)>,
}

impl ManualIntents {
    pub(in crate::gui) fn begin(&mut self) -> ManualIntentId {
        self.serial = self.serial.wrapping_add(1);
        self.pending = None;
        self.preparing = true;
        ManualIntentId(self.serial)
    }

    pub(in crate::gui) fn current(&self, id: ManualIntentId) -> bool {
        self.serial == id.0 && self.preparing
    }

    fn offer(
        &mut self,
        id: ManualIntentId,
        action: ManualAction,
        protected: bool,
    ) -> Option<ManualAction> {
        if !self.current(id) {
            return None;
        }
        self.preparing = false;
        if protected {
            self.pending = Some((id, action));
            None
        } else {
            Some(action)
        }
    }

    fn take_pending(&mut self) -> Option<ManualAction> {
        let (id, action) = self.pending.take()?;
        (self.serial == id.0).then_some(action)
    }

    fn reject(&mut self, id: ManualIntentId) {
        if self.current(id) {
            self.preparing = false;
        }
    }
}

pub(in crate::gui) enum ManualAction {
    Library(super::library::selection::LibraryCommit),
    Playlist(super::playlists::selection::EntryCommit),
    Queue(super::queue::QueueCommit),
    History {
        commit: super::history::HistoryCommit,
        structure_revision: u64,
    },
    Clear,
    ReloadPlayback,
    Close(gpui::AnyWindowHandle),
    Quit,
}

impl GuiApp {
    pub(in crate::gui) fn begin_manual_intent(&mut self, cx: &mut Context<Self>) -> ManualIntentId {
        let id = self.manual_intents.begin();
        self.library
            .update(cx, |library, _| library.cancel_selection());
        self.queue
            .update(cx, |queue, cx| queue.cancel_selection(cx));
        self.playlists
            .update(cx, |playlists, _| playlists.cancel_selection());
        id
    }

    pub(in crate::gui) fn manual_intent_current(&self, id: ManualIntentId) -> bool {
        self.manual_intents.current(id)
    }

    pub(in crate::gui) fn fail_manual_intent(&mut self, id: ManualIntentId) {
        self.manual_intents.reject(id);
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

    fn manual_action_target(&self, action: &ManualAction, cx: &gpui::App) -> Option<i64> {
        match action {
            ManualAction::Library(commit) => self.library.read(cx).library_commit_target(commit),
            ManualAction::Playlist(commit) => Some(commit.target.track_id),
            ManualAction::Queue(commit) => Some(commit.target.track_id),
            ManualAction::History { commit, .. } => Some(commit.track_id),
            _ => None,
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
            self.manual_intents.reject(id);
            self.reject_stale_manual_action(cx);
            return;
        }
        let protected = self
            .metadata
            .read(cx)
            .needs_confirmation(self.manual_action_target(&action, cx), cx);
        if let Some(action) = self.manual_intents.offer(id, action, protected) {
            self.commit_manual_action(action, cx);
        } else {
            cx.notify();
        }
    }

    fn reject_stale_manual_action(&mut self, cx: &mut Context<Self>) {
        self.error = Some(self.i18n.text("gl-metadata-stale-action").into());
        cx.notify();
    }

    pub(super) fn manual_confirmation_open(&self) -> bool {
        self.manual_intents.pending.is_some()
    }

    pub(super) fn cancel_manual_action(&mut self, cx: &mut Context<Self>) {
        self.begin_manual_intent(cx);
        cx.notify();
    }

    pub(super) fn resume_manual_action(&mut self, discard: bool, cx: &mut Context<Self>) {
        let Some(action) = self.manual_intents.take_pending() else {
            return;
        };
        self.refresh(cx);
        // Validate the frozen target before touching the original editor or draft.
        if !self.manual_action_valid(&action, cx) {
            self.reject_stale_manual_action(cx);
            return;
        }
        if discard {
            self.metadata
                .update(cx, |metadata, cx| metadata.discard(cx));
        }
        self.commit_manual_action(action, cx);
        cx.notify();
    }

    fn discard_metadata_and_continue(&mut self, cx: &mut Context<Self>) {
        self.resume_manual_action(true, cx);
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
                    metadata.discard(cx);
                    metadata.clear_selection(cx);
                });
            }
            ManualAction::Close(window) => {
                cx.defer(move |cx| {
                    let _ = window.update(cx, |_, window, _| window.remove_window());
                });
            }
            ManualAction::Quit => self.finish_quit(),
        }
        cx.notify();
    }

    pub(super) fn request_clear_selection(&mut self, cx: &mut Context<Self>) {
        let id = self.begin_manual_intent(cx);
        self.ready_manual_action(id, ManualAction::Clear, cx);
    }

    pub(super) fn render_manual_confirmation(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = crate::gui::theme::palette(cx);
        self.manual_intents.pending.as_ref()?;
        let detail = if self.metadata.read(cx).saving() {
            self.i18n.text("gl-metadata-discard-saving")
        } else {
            self.i18n.text("gl-metadata-discard-note")
        };
        Some(
            crate::gui::components::modal_backdrop("metadata-confirmation-modal")
                .map(|surface| self.shell.scope().surface(surface))
                .child(
                    column()
                        .id("metadata-confirmation-card")
                        .w(gpui::rems(26.25))
                        .max_w(gpui::relative(0.9))
                        .p(px(UI_INSET))
                        .bg(rgb(theme.panel))
                        .border_1()
                        .border_color(rgb(theme.border))
                        .rounded_md()
                        .child(panel_header(
                            theme,
                            self.tr("gl-metadata-discard-title"),
                            detail.to_owned(),
                        ))
                        .child(
                            row()
                                .justify_end()
                                .child(icon_button(
                                    "metadata-confirmation-cancel",
                                    "",
                                    self.tr("gl-cancel"),
                                    cx,
                                    |this, _, cx| this.cancel_manual_action(cx),
                                ))
                                .child(super::components::danger_control(
                                    theme,
                                    icon_button(
                                        "metadata-confirmation-discard",
                                        "󰆴",
                                        self.tr("gl-metadata-discard"),
                                        cx,
                                        |this, _, cx| this.discard_metadata_and_continue(cx),
                                    ),
                                )),
                        ),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{ManualAction, ManualIntents};

    #[test]
    fn late_range_cannot_replace_newer_exit_or_clean_selection() {
        let mut owner = ManualIntents::default();
        let range = owner.begin();
        let quit = owner.begin();
        assert!(owner.offer(quit, ManualAction::Quit, true).is_none());
        assert!(owner.offer(range, ManualAction::Clear, true).is_none());
        assert!(matches!(owner.take_pending(), Some(ManualAction::Quit)));
        let navigation = owner.begin();
        let clean_selection = owner.begin();
        assert!(matches!(
            owner.offer(clean_selection, ManualAction::ReloadPlayback, false),
            Some(ManualAction::ReloadPlayback)
        ));
        assert!(owner.offer(navigation, ManualAction::Clear, true).is_none());
        assert!(owner.take_pending().is_none());
    }

    #[test]
    fn cancel_and_new_input_prevent_old_save_resume() {
        let mut owner = ManualIntents::default();
        let first = owner.begin();
        assert!(owner.offer(first, ManualAction::Clear, true).is_none());
        owner.begin(); // Cancel invalidates intent, not the editor or draft.
        assert!(owner.take_pending().is_none());
        assert!(owner.offer(first, ManualAction::Clear, false).is_none());
        let next = owner.begin();
        assert!(owner.offer(next, ManualAction::Quit, true).is_none());
        assert!(matches!(owner.take_pending(), Some(ManualAction::Quit)));
        assert!(owner.take_pending().is_none());
    }

    #[test]
    fn completion_and_confirmation_can_each_publish_only_once() {
        let mut owner = ManualIntents::default();
        let clean = owner.begin();
        assert!(matches!(
            owner.offer(clean, ManualAction::Clear, false),
            Some(ManualAction::Clear)
        ));
        assert!(!owner.current(clean));
        assert!(owner.offer(clean, ManualAction::Quit, true).is_none());
        assert!(owner.take_pending().is_none());
        let protected = owner.begin();
        assert!(owner.offer(protected, ManualAction::Quit, true).is_none());
        assert!(owner.offer(protected, ManualAction::Clear, true).is_none());
        assert!(matches!(owner.take_pending(), Some(ManualAction::Quit)));
        assert!(owner.offer(protected, ManualAction::Clear, false).is_none());
        assert!(owner.take_pending().is_none());
        let failed = owner.begin();
        owner.reject(failed);
        assert!(owner.offer(failed, ManualAction::Clear, false).is_none());
        assert!(owner.take_pending().is_none());
    }
}
