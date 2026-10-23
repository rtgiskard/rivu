use super::{PlaylistEvent, Playlists};
use crate::model::Command;
use gpui::{AnyElement, Context, Focusable, Window, div, prelude::*, rems};
use rivu_ui::{
    FormField, IconButton, IconButtonDensity, IconButtonStyle, StatusKind, caption, column, row,
    status_message,
};

pub(in crate::gui) enum PlaylistNameAction {
    Create { track_ids: Vec<i64> },
    Rename { playlist_id: i64 },
    SaveQueue,
}

pub(in crate::gui) struct PlaylistEditor {
    action: PlaylistNameAction,
    pending: bool,
    pub(in crate::gui) error: Option<String>,
}

impl Playlists {
    pub(in crate::gui) fn begin_playlist_name(
        &mut self,
        action: PlaylistNameAction,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.playlist_editor.is_some() {
            return;
        }
        cx.emit(PlaylistEvent::CancelInteraction);
        cx.emit(PlaylistEvent::ClearMenus);
        self.name_input
            .update(cx, |input, cx| input.set_text(name, cx));
        self.playlist_editor = Some(PlaylistEditor {
            action,
            pending: false,
            error: None,
        });
        self.name_input.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    pub(in crate::gui) fn cancel_playlist_name(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .playlist_editor
            .as_ref()
            .is_some_and(|editor| editor.pending)
        {
            return;
        }
        self.playlist_editor = None;
        self.name_input
            .update(cx, |input, cx| input.set_text("", cx));
        self.focus_layout(window, cx);
        cx.notify();
    }

    pub(in crate::gui) fn submit_playlist_name(&mut self, cx: &mut Context<Self>) {
        let name = self.name_input.read(cx).text().trim().to_owned();
        let Some(editor) = self.playlist_editor.as_mut() else {
            return;
        };
        if editor.pending {
            return;
        }
        if name.is_empty() {
            editor.error = Some(self.i18n.text("gl-playlist-name-required").into());
            cx.notify();
            return;
        }
        let command = match &editor.action {
            PlaylistNameAction::Create { track_ids } if track_ids.is_empty() => {
                Command::CreatePlaylist { name }
            }
            PlaylistNameAction::Create { track_ids } => Command::CreatePlaylistWithTracks {
                name,
                track_ids: track_ids.clone(),
            },
            PlaylistNameAction::Rename { playlist_id } => Command::RenamePlaylist {
                playlist_id: *playlist_id,
                name,
            },
            PlaylistNameAction::SaveQueue => Command::CreatePlaylistFromQueue { name },
        };
        editor.pending = true;
        editor.error = None;
        cx.emit(PlaylistEvent::Focus(None));
        let handle = self.context.handle().clone();
        cx.spawn(async move |this, cx| {
            let ack = cx
                .background_executor()
                .spawn(async move { handle.request_ack(command) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.refresh(cx);
                if ack.is_success() {
                    this.playlist_editor = None;
                    this.name_input
                        .update(cx, |input, cx| input.set_text("", cx));
                    cx.emit(PlaylistEvent::Focus(None));
                } else if let Some(editor) = this.playlist_editor.as_mut() {
                    editor.pending = false;
                    editor.error = Some(
                        ack.error
                            .unwrap_or_else(|| this.i18n.text("gl-playlist-save-failed").into()),
                    );
                    cx.emit(PlaylistEvent::FocusName);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(in crate::gui) fn playlist_name_dialog(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = rivu_ui::theme::palette(cx);
        let editor = self.playlist_editor.as_ref().expect("playlist editor open");
        let (title, submit) = match editor.action {
            PlaylistNameAction::Create { .. } => (
                self.tr("gl-playlist-new-title"),
                self.tr("gl-playlist-create"),
            ),
            PlaylistNameAction::Rename { .. } => (
                self.tr("gl-playlist-rename-title"),
                self.tr("gl-playlist-save"),
            ),
            PlaylistNameAction::SaveQueue => (
                self.tr("gl-playlist-save-queue-title"),
                self.tr("gl-playlist-save"),
            ),
        };
        let mut content = column().gap_3().child(div().text_lg().child(title));
        if let PlaylistNameAction::Create { track_ids } = &editor.action
            && !track_ids.is_empty()
        {
            content = content.child(caption(
                theme,
                self.i18n
                    .format("gl-tracks-selected", &[("count", track_ids.len().into())]),
            ));
        }
        if editor.pending {
            content = content
                .child(caption(theme, self.name_input.read(cx).text().to_owned()))
                .child(status_message(
                    theme,
                    self.tr("gl-saving"),
                    StatusKind::Info,
                ));
        } else {
            content = content.child(
                FormField::new(theme, self.tr("gl-name"), self.name_input.clone()).into_div(),
            );
            if let Some(error) = &editor.error {
                content = content.child(status_message(theme, error.clone(), StatusKind::Error));
            }
            content = content.child(
                row()
                    .justify_end()
                    .child(
                        IconButton::new(
                            "playlist-name-cancel",
                            "",
                            self.tr("gl-playlist-name-cancel"),
                        )
                        .build(
                            IconButtonStyle::Surface,
                            IconButtonDensity::Regular,
                            cx,
                            |this, window, cx| this.cancel_playlist_name(window, cx),
                        ),
                    )
                    .child(IconButton::new("playlist-name-submit", "", submit).build(
                        IconButtonStyle::Surface,
                        IconButtonDensity::Regular,
                        cx,
                        |this, _, cx| this.submit_playlist_name(cx),
                    )),
            );
        }
        rivu_ui::Dialog::backdrop("playlist-name-modal")
            .map(|surface| self.dialog_surface(surface))
            .child(
                rivu_ui::Dialog::scroll_card(theme, "playlist-name-card", window)
                    .w(rems(26.25))
                    .child(content.flex_shrink_0()),
            )
            .into_any_element()
    }
}
