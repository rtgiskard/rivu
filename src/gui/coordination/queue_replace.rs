//! Confirmation of commands that replace the playback queue, not playlist management.
use crate::gui::*;
use rivu_ui::{danger_control, icon_button, panel_header, row};

#[derive(Clone)]
pub(in crate::gui) enum QueueReplacement {
    Playlist { id: i64, name: String },
    Album(crate::model::AlbumKey),
}

impl GuiApp {
    pub(in crate::gui) fn request_album_play(
        &mut self,
        key: crate::model::AlbumKey,
        cx: &mut Context<Self>,
    ) {
        self.request_queue_replace(QueueReplacement::Album(key), cx);
    }

    pub(in crate::gui) fn request_queue_replace(
        &mut self,
        replacement: QueueReplacement,
        cx: &mut Context<Self>,
    ) {
        self.refresh(cx);
        self.playlists
            .update(cx, |playlists, cx| playlists.clear_action(cx));
        if self.state.system.config.confirm_queue_replace && self.state.queue.total > 0 {
            self.queue_replace = Some(replacement);
            cx.notify();
        } else {
            self.send(replacement.command(), cx);
        }
    }

    pub(in crate::gui) fn dismiss_queue_replace(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.queue_replace = None;
        self.focus_layout(window, cx);
        cx.notify();
    }

    pub(in crate::gui) fn queue_replace_overlay(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(replacement) = &self.queue_replace else {
            return div().into_any_element();
        };
        let theme = rivu_ui::theme::palette(cx);
        let (name, title_key) = match replacement {
            QueueReplacement::Playlist { name, .. } => (name, "gl-confirm-play-playlist"),
            QueueReplacement::Album(key) => (&key.title, "gl-confirm-play-album"),
        };
        let title = self
            .i18n
            .format(title_key, &[("name", name.as_str().into())]);
        let detail = self.i18n.format(
            "gl-replace-queue-confirm",
            &[("count", self.state.queue.total.into())],
        );
        let command = replacement.command();
        rivu_ui::Dialog::backdrop("queue-replace-modal")
            .map(|surface| self.dialog_surface(surface))
            .child(
                rivu_ui::Dialog::scroll_card(theme, "queue-replace-card", window)
                    .w(gpui::rems(26.25))
                    .child(panel_header(theme, title, detail).flex_shrink_0())
                    .child(
                        row()
                            .flex_shrink_0()
                            .justify_end()
                            .child(icon_button(
                                "queue-replace-cancel",
                                "",
                                self.tr("gl-cancel"),
                                cx,
                                |this, window, cx| this.dismiss_queue_replace(window, cx),
                            ))
                            .child(danger_control(
                                theme,
                                icon_button(
                                    "queue-replace-confirm",
                                    "",
                                    self.tr("gl-replace-playlist"),
                                    cx,
                                    move |this, window, cx| {
                                        this.dismiss_queue_replace(window, cx);
                                        this.send(command.clone(), cx);
                                    },
                                ),
                            )),
                    ),
            )
            .into_any_element()
    }
}

impl QueueReplacement {
    fn command(&self) -> Command {
        match self {
            Self::Playlist { id, .. } => Command::PlayPlaylist { playlist_id: *id },
            Self::Album(key) => Command::PlayAlbum { key: key.clone() },
        }
    }
}
