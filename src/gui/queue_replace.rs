//! Confirmation of commands that replace the playback queue, not playlist management.
use super::components::{column, danger_control, icon_button, panel_header, row};
use super::*;

#[derive(Clone)]
pub(super) enum QueueReplacement {
    Playlist { id: i64, name: String },
    Album(crate::model::AlbumKey),
}

impl GuiApp {
    pub(super) fn request_album_play(
        &mut self,
        key: crate::model::AlbumKey,
        cx: &mut Context<Self>,
    ) {
        self.request_queue_replace(QueueReplacement::Album(key), cx);
    }

    pub(super) fn request_queue_replace(
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

    pub(super) fn dismiss_queue_replace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.queue_replace = None;
        self.focus_layout(window, cx);
        cx.notify();
    }

    pub(super) fn queue_replace_overlay(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(replacement) = &self.queue_replace else {
            return div().into_any_element();
        };
        let theme = crate::gui::theme::palette(cx);
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
        crate::gui::components::modal_backdrop("queue-replace-modal")
            .map(|surface| self.dialog_surface(surface))
            .child(
                column()
                    .id("queue-replace-card")
                    .w(gpui::rems(26.25))
                    .max_w(gpui::relative(0.9))
                    .max_h(window.viewport_size().height * 0.9)
                    .overflow_y_scroll()
                    .p(px(UI_INSET))
                    .bg(rgb(theme.panel))
                    .border_1()
                    .border_color(rgb(theme.border))
                    .rounded_md()
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
