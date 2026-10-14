use super::*;

pub(super) enum PlaylistNameAction {
    Create { track_ids: Vec<i64> },
    Rename { playlist_id: i64 },
}

pub(super) struct PlaylistEditor {
    action: PlaylistNameAction,
    pending: bool,
    pub(super) error: Option<String>,
}

impl GuiApp {
    pub(super) fn begin_playlist_name(
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

    pub(super) fn cancel_playlist_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    pub(super) fn submit_playlist_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.value(Field::PlaylistName, cx).trim().to_owned();
        let Some(editor) = self.playlist_editor.as_mut() else {
            return;
        };
        if editor.pending {
            return;
        }
        if name.is_empty() {
            editor.error = Some("Enter a playlist name.".into());
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
                            .unwrap_or_else(|| "Could not save playlist.".into()),
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

    pub(super) fn playlist_name_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let editor = self.playlist_editor.as_ref().expect("playlist editor open");
        let (title, submit) = match editor.action {
            PlaylistNameAction::Create { .. } => ("New playlist", "Create"),
            PlaylistNameAction::Rename { .. } => ("Rename playlist", "Save"),
        };
        let mut content = column().gap_3().child(div().text_lg().child(title));
        if let PlaylistNameAction::Create { track_ids } = &editor.action
            && !track_ids.is_empty()
        {
            content = content.child(caption(format!("{} selected tracks", track_ids.len())));
        }
        if editor.pending {
            content = content
                .child(caption(self.value(Field::PlaylistName, cx)))
                .child(status_message("Saving…", StatusKind::Info));
        } else {
            content = content.child(form_field("Name", self.input(Field::PlaylistName)));
            if let Some(error) = &editor.error {
                content = content.child(status_message(error.clone(), StatusKind::Error));
            }
            content = content.child(
                row()
                    .justify_end()
                    .child(icon_button(
                        "playlist-name-cancel",
                        "",
                        "Cancel playlist name editing",
                        cx,
                        |this, window, cx| this.cancel_playlist_name(window, cx),
                    ))
                    .child(icon_button(
                        "playlist-name-submit",
                        "",
                        format!("{submit} playlist"),
                        cx,
                        |this, window, cx| this.submit_playlist_name(window, cx),
                    )),
            );
        }
        div()
            .id("playlist-name-modal")
            .occlude()
            .absolute()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(0x000000).alpha(0.58))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.stop_propagation()),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|_, _, _, cx| cx.stop_propagation()),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|_, _, _, cx| cx.stop_propagation()),
            )
            .on_scroll_wheel(cx.listener(|_, _, _, cx| cx.stop_propagation()))
            .child(
                content
                    .w(px(420.))
                    .max_w(relative(0.9))
                    .p(px(UI_INSET))
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .rounded_md(),
            )
            .into_any_element()
    }
}
