use crate::gui::components::{
    StatusKind, caption, column, form_field, icon_button, row, status_message,
};
use crate::gui::*;

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

impl GuiApp {
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
        self.cancel_interaction(window, cx);
        self.panel_menu = None;
        self.set_value(Field::PlaylistName, name, cx);
        self.playlist_editor = Some(PlaylistEditor {
            action,
            pending: false,
            error: None,
        });
        self.input(Field::PlaylistName)
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
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
        self.set_value(Field::PlaylistName, "", cx);
        self.focus_layout(window, cx);
        cx.notify();
    }

    pub(in crate::gui) fn submit_playlist_name(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = self.value(Field::PlaylistName, cx).trim().to_owned();
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
        self.focus_layout(window, cx);
        let handle = self.handle.clone();
        cx.spawn_in(window, async move |this, cx| {
            let ack = cx
                .background_executor()
                .spawn(async move { handle.request_ack(command) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.refresh(cx);
                if ack.ok {
                    this.playlist_editor = None;
                    this.set_value(Field::PlaylistName, "", cx);
                    this.focus_layout(window, cx);
                } else if let Some(editor) = this.playlist_editor.as_mut() {
                    editor.pending = false;
                    editor.error = Some(
                        ack.error
                            .unwrap_or_else(|| this.i18n.text("gl-playlist-save-failed").into()),
                    );
                    this.input(Field::PlaylistName)
                        .read(cx)
                        .focus_handle(cx)
                        .focus(window, cx);
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
        let theme = crate::gui::theme::palette(cx);
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
                .child(caption(theme, self.value(Field::PlaylistName, cx)))
                .child(status_message(
                    theme,
                    self.tr("gl-saving"),
                    StatusKind::Info,
                ));
        } else {
            content = content.child(form_field(
                theme,
                self.tr("gl-name"),
                self.input(Field::PlaylistName),
            ));
            if let Some(error) = &editor.error {
                content = content.child(status_message(theme, error.clone(), StatusKind::Error));
            }
            content = content.child(
                row()
                    .justify_end()
                    .child(icon_button(
                        "playlist-name-cancel",
                        "",
                        self.tr("gl-playlist-name-cancel"),
                        cx,
                        |this, window, cx| this.cancel_playlist_name(window, cx),
                    ))
                    .child(icon_button(
                        "playlist-name-submit",
                        "",
                        submit,
                        cx,
                        |this, window, cx| this.submit_playlist_name(window, cx),
                    )),
            );
        }
        crate::gui::components::modal_backdrop("playlist-name-modal")
            .map(|surface| self.dialog_surface(surface))
            .child(
                column()
                    .id("playlist-name-card")
                    .w(rems(26.25))
                    .max_w(relative(0.9))
                    .max_h(window.viewport_size().height * 0.9)
                    .overflow_y_scroll()
                    .p(px(UI_INSET))
                    .bg(rgb(theme.panel))
                    .border_1()
                    .border_color(rgb(theme.border))
                    .rounded_md()
                    .child(content.flex_shrink_0()),
            )
            .into_any_element()
    }
}
