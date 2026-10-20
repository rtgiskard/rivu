use super::*;
use rivu_ui::{icon_button, row};

fn notification_button(
    theme: &'static rivu_ui::theme::Palette,
    control: gpui::Stateful<gpui::Div>,
) -> gpui::Stateful<gpui::Div> {
    rivu_ui::quiet_icon_button(theme, control)
        .group_hover("control", |style| style.bg(rgb(theme.text).alpha(0.08)))
        .flex_shrink_0()
}

impl GuiApp {
    pub(super) fn footer_notification(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let error = self
            .error
            .as_ref()
            .filter(|error| {
                !self.shell.is_open(Surface::Layouts)
                    || self.workspace.save_error.as_ref() != Some(*error)
            })
            .or(self.state.system.last_error.as_ref());
        let is_error = error.is_some();
        let message: SharedString = match error {
            Some(error) => error.clone().into(),
            None if self.state.system.scanning => self.state.system.scan_message.clone().into(),
            None => return None,
        };
        let theme = rivu_ui::theme::palette(cx);
        let copied_message = message.clone();
        let mut buttons = row()
            .flex_shrink_0()
            .gap_0()
            .ml_4()
            .child(notification_button(
                theme,
                icon_button(
                    if is_error {
                        "copy-error"
                    } else {
                        "copy-scan-message"
                    },
                    "",
                    self.tr("shell-copy-message"),
                    cx,
                    move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(
                            copied_message.to_string(),
                        ));
                    },
                ),
            ));
        if is_error {
            buttons = buttons.child(notification_button(
                theme,
                icon_button(
                    "dismiss-error",
                    "",
                    self.tr("shell-dismiss-message"),
                    cx,
                    |this, _, cx| {
                        this.error = None;
                        this.send(Command::DismissError, cx);
                    },
                ),
            ));
        }
        Some(
            row()
                .flex_initial()
                .min_w_0()
                .p_1()
                .gap_0()
                .text_xs()
                .text_color(rgb(theme.text))
                .when(is_error, |notification| {
                    notification.bg(rgb(theme.error_bg))
                })
                .child(
                    div()
                        .id("footer-notification-text")
                        .flex_initial()
                        .min_w_0()
                        .truncate()
                        .child(message.clone())
                        .tooltip(rivu_ui::text_tooltip(message)),
                )
                .child(buttons)
                .into_any_element(),
        )
    }
}
