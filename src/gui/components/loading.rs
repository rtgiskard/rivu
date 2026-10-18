use gpui::{Div, ElementId, SharedString, Stateful, prelude::*};
use rivu_ui::{caption, list_row};
/// Page placeholder presentation; request identity and retry gating remain with the owner.
pub(in crate::gui) fn paged_loading_row<T: 'static>(
    id: impl Into<ElementId>,
    error: Option<&str>,
    show: bool,
    text: impl Fn(&str) -> SharedString,
    cx: &mut gpui::Context<T>,
    retry: impl Fn(&mut T, &mut gpui::Window, &mut gpui::Context<T>) + 'static,
) -> Stateful<Div> {
    let theme = rivu_ui::theme::palette(cx);
    list_row(theme, id, false)
        .child(
            caption(
                theme,
                text(if error.is_some() {
                    "shell-loading-failed"
                } else {
                    "shell-loading"
                }),
            )
            .when(!show, |text| text.invisible()),
        )
        .when_some(
            error.map(|error| SharedString::from(error.to_owned())),
            |row, error| {
                row.tooltip(move |_, cx| {
                    cx.new(|_| rivu_ui::ButtonTooltip {
                        text: error.clone(),
                    })
                    .into()
                })
                .on_click(cx.listener(move |this, _, window, cx| retry(this, window, cx)))
            },
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Context, InputEvent, Render, TestAppContext, Window, div};

    struct LoadingRow {
        error: Option<&'static str>,
        show: bool,
        retries: usize,
    }

    impl Render for LoadingRow {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(
                paged_loading_row(
                    ("loading-row", 0usize),
                    self.error,
                    self.show,
                    |key| match key {
                        "shell-loading-failed" => "Loading failed".into(),
                        "shell-loading" => "Loading".into(),
                        _ => unreachable!(),
                    },
                    cx,
                    |this, _, cx| {
                        this.retries += 1;
                        cx.notify();
                    },
                )
                .w(gpui::px(240.))
                .h(gpui::rems(rivu_ui::LIST_ROW_REMS)),
            )
        }
    }

    #[gpui::test]
    fn placeholder_retry_listener_tracks_rendered_error(cx: &mut TestAppContext) {
        cx.update(|cx| {
            rivu_ui::theme::set_palette(crate::theme::Theme::default().palette(), cx);
            rivu_ui::theme::set_typography(
                gpui::font("sans-serif"),
                crate::gui::appearance::fonts::SYMBOLS_FAMILY,
                cx,
            );
        });
        let window = cx.add_window(|_, _| LoadingRow {
            error: None,
            show: false,
            retries: 0,
        });
        let row = window.root(cx).unwrap();
        let handle = window.into();
        for (error, show, retries) in [
            (None, false, 0),
            (None, true, 0),
            (Some("Page failed"), true, 1),
            (None, false, 1),
            (None, true, 1),
            (Some("Another failure"), true, 2),
        ] {
            row.update(cx, |row, cx| {
                row.error = error;
                row.show = show;
                cx.notify();
            });
            cx.run_until_parked();
            cx.update_window(handle, |_, window, cx| {
                window.draw(cx).clear(cx);
                let position = gpui::point(gpui::px(12.), gpui::px(12.));
                window.dispatch_event(
                    gpui::MouseDownEvent {
                        button: gpui::MouseButton::Left,
                        position,
                        ..Default::default()
                    }
                    .to_platform_input(),
                    cx,
                );
                window.dispatch_event(
                    gpui::MouseUpEvent {
                        button: gpui::MouseButton::Left,
                        position,
                        ..Default::default()
                    }
                    .to_platform_input(),
                    cx,
                );
            })
            .unwrap();
            assert_eq!(row.read_with(cx, |row, _| row.retries), retries);
        }
    }
}
