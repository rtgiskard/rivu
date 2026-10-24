use super::*;
use rivu_ui::{IconButton, IconButtonDensity, IconButtonStyle, row};

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
            None if self.state.system.scanning
                && self.state.system.scan_owner
                    != Some(crate::projection::MediaTaskOwner::Library) =>
            {
                self.state.system.scan_message.clone().into()
            }
            None => return None,
        };
        let theme = rivu_ui::theme::palette(cx);
        let copied_message = message.clone();
        let mut buttons = row().flex_shrink_0().gap_0().ml_4().child(
            IconButton::new(
                if is_error {
                    "copy-error"
                } else {
                    "copy-scan-message"
                },
                "",
                self.tr("shell-copy-message"),
            )
            .build_with(
                IconButtonStyle::Quiet,
                IconButtonDensity::Regular,
                cx,
                move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copied_message.to_string()));
                },
                |control| {
                    control.group_hover("control", |style| style.bg(rgb(theme.text).alpha(0.08)))
                },
            ),
        );
        if is_error {
            buttons = buttons.child(
                IconButton::new("dismiss-error", "", self.tr("shell-dismiss-message")).build_with(
                    IconButtonStyle::Quiet,
                    IconButtonDensity::Regular,
                    cx,
                    |this, _, cx| {
                        this.error = None;
                        this.send(Command::DismissError, cx);
                    },
                    |control| {
                        control
                            .group_hover("control", |style| style.bg(rgb(theme.text).alpha(0.08)))
                    },
                ),
            );
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
