use crate::{control::activate_control, overlay::text_tooltip};
use gpui::{Context, Div, ElementId, Role, SharedString, Stateful, Window, div, prelude::*, rgb};

pub const POPOVER_MAX_REMS: f32 = 15.0;

const MENU_TEXT_REMS: f32 = 0.875;
const MENU_MIN_HEIGHT_REMS: f32 = 1.5;
const MENU_VERTICAL_PADDING_REMS: f32 = 0.125;

/// Match the natural height of action rows, including unscaled borders.
pub fn menu_row_height(window: &Window, cx: &gpui::App) -> gpui::Pixels {
    let rem = window.rem_size();
    let text = gpui::rems(MENU_TEXT_REMS * crate::theme::font_size_scale(cx));
    (rem * MENU_MIN_HEIGHT_REMS).max(
        gpui::phi().to_pixels(text.into(), rem).round()
            + rem * (2. * MENU_VERTICAL_PADDING_REMS)
            + gpui::px(2.),
    )
}

/// Menu item with shared application event handling.
pub fn menu_item<T: 'static>(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    cx: &mut Context<T>,
    action: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
) -> Stateful<Div> {
    let theme = crate::theme::palette(cx);
    let label = label.into();
    activate_control(
        menu_item_style(theme, id, label.clone(), label, cx),
        cx,
        action,
    )
}

pub fn menu_separator(theme: &'static crate::theme::Palette) -> Div {
    div().h(gpui::px(1.)).mx_2().my_1().bg(rgb(theme.border))
}

pub fn menu_title(theme: &'static crate::theme::Palette, title: impl Into<SharedString>) -> Div {
    let title = title.into();
    div()
        .px_2()
        .py_1()
        .text_xs()
        .text_color(rgb(theme.muted))
        .min_w_0()
        .truncate()
        .child(title)
}

pub fn menu_back<T: 'static>(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    cx: &mut Context<T>,
    action: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
) -> Stateful<Div> {
    let label = label.into();
    let theme = crate::theme::palette(cx);
    activate_control(menu_item_style(theme, id, label, "", cx), cx, action)
        .font_family(crate::theme::icon_family(cx))
}

/// Destructive menu rows use text, not a button border, to identify the action.
pub fn danger_menu_item(
    theme: &'static crate::theme::Palette,
    control: Stateful<Div>,
) -> Stateful<Div> {
    control.text_color(rgb(theme.error))
}

/// Custom menu content must supply its actual accessible text, never a glyph or debug name.
pub fn menu_item_style(
    theme: &'static crate::theme::Palette,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    content: impl IntoElement,
    cx: &gpui::App,
) -> Stateful<Div> {
    let label = label.into();
    div()
        .id(id)
        .role(Role::MenuItem)
        .aria_label(label.clone())
        .tab_index(0)
        .focus(|style| style.bg(rgb(theme.selected)))
        .w_full()
        .min_h(gpui::rems(MENU_MIN_HEIGHT_REMS))
        .flex_shrink_0()
        .px_2()
        .py(gpui::rems(MENU_VERTICAL_PADDING_REMS))
        .flex()
        .gap_2()
        .items_center()
        .font_size_scale(1.)
        .text_size(gpui::rems(
            MENU_TEXT_REMS * crate::theme::font_size_scale(cx),
        ))
        .line_height(gpui::phi())
        .cursor_pointer()
        .border_1()
        .border_color(gpui::transparent_black())
        .bg(rgb(theme.panel).alpha(0.0))
        .rounded_sm()
        .hover(|style| style.bg(rgb(theme.highlight)))
        .child(
            div()
                .id(label.clone())
                .flex_1()
                .min_w_0()
                .truncate()
                .tooltip(text_tooltip(label))
                .child(content),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Bounds, Pixels, Render, TestAppContext, TextLayout};
    use std::{cell::Cell, rc::Rc};

    #[derive(Default)]
    struct MenuRows {
        bounds: Rc<Cell<[Bounds<Pixels>; 4]>>,
        text: TextLayout,
    }

    impl Render for MenuRows {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let theme = crate::theme::palette(cx);
            let height = menu_row_height(window, cx);
            let text = gpui::StyledText::new("A playlist with a long name");
            self.text = text.layout().clone();
            let bounds = self.bounds.clone();
            crate::column()
                .w(gpui::px(240.))
                .font(crate::theme::font(cx).clone())
                .font_size_scale(crate::theme::font_size_scale(cx))
                .child(menu_item("new-playlist", "New playlist", cx, |_, _, _| {}))
                .child(menu_back("back", "Back", cx, |_, _, _| {}))
                .child(menu_item_style(theme, "target", "Playlist", text, cx).h(height))
                .child(crate::loading::paged_loading_row(
                    "failed-page",
                    Some("Full diagnostic"),
                    true,
                    crate::loading::LoadingRowLabels {
                        message: "Failed".into(),
                        retry: "Retry".into(),
                        copy_error: "Copy".into(),
                    },
                    height,
                    cx,
                    |_, _, _| {},
                ))
                .on_children_prepainted(move |children, _, _| {
                    bounds.set(children.try_into().unwrap());
                })
        }
    }

    #[test]
    fn menu_candidates_back_and_recovery_fit_the_same_scaled_row() {
        let mut cx = TestAppContext::single();
        cx.update(crate::theme::init_test);
        let window = cx.add_window(|_, _| MenuRows::default());
        let root = window.root(&mut cx).unwrap();
        let handle = window.into();
        for rem in [12., 16., 24., 32.] {
            for size in [8, 12, 18] {
                cx.update(|cx| {
                    crate::theme::set_typography(gpui::font("sans-serif"), "sans-serif", size, cx);
                });
                cx.run_until_parked();
                cx.update_window(handle, |_, window, cx| {
                    window.set_rem_size(gpui::px(rem));
                    window.draw(cx).clear(cx);
                })
                .unwrap();
                root.read_with(&cx, |rows, _| {
                    let bounds = rows.bounds.get();
                    for row in &bounds[1..] {
                        assert_eq!(
                            row.size.height, bounds[0].size.height,
                            "rem={rem}, size={size}"
                        );
                    }
                    assert!(rows.text.bounds().top() >= bounds[2].top());
                    assert!(rows.text.bounds().bottom() <= bounds[2].bottom());
                });
            }
        }
    }
}
