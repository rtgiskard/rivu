use super::super::*;

#[derive(Clone)]
pub(in crate::gui) enum Action {
    Duplicate,
    Rename,
    Delete(String),
}

impl GuiApp {
    pub(in crate::gui) fn layout_management_allowed(&mut self, cx: &mut Context<Self>) -> bool {
        if self.settings_open {
            self.error = Some("Apply or discard settings before changing layouts.".into());
            cx.notify();
            false
        } else {
            true
        }
    }

    fn reload_layout_gui(&self, cx: &mut Context<Self>) {
        let host = self.host.clone();
        cx.defer(move |cx| {
            if let Some(host) = host.upgrade() {
                host.borrow_mut().reload_window(cx);
            }
        });
    }

    fn select_layout(&mut self, name: &str, cx: &mut Context<Self>) {
        if !self.layout_management_allowed(cx) || name == self.layouts.active_name() {
            return;
        }
        let old = self.layouts.active_name().to_owned();
        let result = self
            .layouts
            .select(name)
            .and_then(|_| self.layouts.save(&self.layout_path));
        if let Err(error) = result {
            self.layouts
                .select(&old)
                .expect("previous layout still exists");
            self.error = Some(format!("Switching layout: {error:#}"));
            cx.notify();
        } else {
            self.reload_layout_gui(cx);
        }
    }

    fn begin_layout_action(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        if !self.layout_management_allowed(cx) {
            return;
        }
        let name = if matches!(action, Action::Rename) {
            self.layouts.active_name().to_owned()
        } else {
            String::new()
        };
        self.set_value(Field::LayoutName, name, cx);
        if !matches!(action, Action::Delete(_)) {
            self.input(Field::LayoutName)
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx);
        }
        self.layout_action = Some(action);
        cx.notify();
    }

    pub(in crate::gui) fn finish_layout_action(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.layout_management_allowed(cx) {
            return;
        }
        let Some(action) = self.layout_action.clone() else {
            return;
        };
        let old = self.layouts.active_name().to_owned();
        let name = self.value(Field::LayoutName, cx).trim().to_owned();
        let (result, reload) = match action {
            Action::Duplicate => {
                let result = self.layouts.duplicate(&name).and_then(|_| {
                    if let Err(error) = self.layouts.save(&self.layout_path) {
                        self.layouts
                            .select(&old)
                            .expect("original layout still exists");
                        self.layouts
                            .remove(&name)
                            .expect("duplicate exists and is not current");
                        Err(error)
                    } else {
                        Ok(())
                    }
                });
                (result, true)
            }
            Action::Rename => {
                let result = self.layouts.rename(&name).and_then(|_| {
                    if let Err(error) = self.layouts.save(&self.layout_path) {
                        self.layouts
                            .rename(&old)
                            .expect("original layout name is available");
                        Err(error)
                    } else {
                        Ok(())
                    }
                });
                (result, false)
            }
            Action::Delete(name) => (self.layouts.remove_saved(&name, &self.layout_path), false),
        };
        match result {
            Ok(()) => {
                self.layout_action = None;
                self.error = None;
                if reload {
                    self.reload_layout_gui(cx);
                } else {
                    self.focus_layout(window, cx);
                }
            }
            Err(error) => self.error = Some(format!("Managing layouts: {error:#}")),
        }
        cx.notify();
    }

    pub(in crate::gui) fn layouts_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut content = column().gap_2().child(
            row()
                .child(div().flex_1().text_lg().child("Layouts"))
                .child(icon_button(
                    "layouts-close",
                    "",
                    "Close layouts",
                    cx,
                    |this, window, cx| {
                        this.layouts_open = false;
                        this.layout_action = None;
                        this.focus_layout(window, cx);
                        cx.notify();
                    },
                )),
        );
        if let Some(action) = self.layout_action.clone() {
            let (title, submit) = match &action {
                Action::Duplicate => ("Duplicate current layout", "Duplicate"),
                Action::Rename => ("Rename current layout", "Rename"),
                Action::Delete(_) => ("Delete saved layout", "Delete"),
            };
            content = content.child(caption(title));
            if let Action::Delete(name) = &action {
                content = content.child(
                    div()
                        .text_sm()
                        .child(format!("Delete “{name}”? This cannot be undone.")),
                );
            } else {
                content = content.child(form_field("Name", self.input(Field::LayoutName)));
            }
            content = content.child(
                row()
                    .child(icon_button(
                        "layout-action-cancel",
                        "",
                        "Cancel layout action",
                        cx,
                        |this, window, cx| {
                            this.layout_action = None;
                            this.focus_layout(window, cx);
                            cx.notify();
                        },
                    ))
                    .child(if matches!(action, Action::Delete(_)) {
                        button("layout-action-submit", submit, cx, |this, window, cx| {
                            this.finish_layout_action(window, cx)
                        })
                    } else {
                        icon_button(
                            "layout-action-submit",
                            "",
                            submit,
                            cx,
                            |this, window, cx| this.finish_layout_action(window, cx),
                        )
                    }),
            );
        } else {
            let mut choices = column().gap_1();
            for (index, name) in self.layouts.names().enumerate() {
                let current = name == self.layouts.active_name();
                let target = name.to_owned();
                let mut choice = row().child(
                    menu_item(
                        ("layout-select", index),
                        if current {
                            format!("✓ {name}")
                        } else {
                            name.to_owned()
                        },
                        cx,
                        move |this, _, cx| this.select_layout(&target, cx),
                    )
                    .flex_1(),
                );
                if !current {
                    let name = name.to_owned();
                    choice = choice.child(icon_button(
                        ("layout-delete", index),
                        "󰆴",
                        format!("Delete layout “{name}”…"),
                        cx,
                        move |this, window, cx| {
                            this.begin_layout_action(Action::Delete(name.clone()), window, cx)
                        },
                    ));
                }
                choices = choices.child(choice);
            }
            content = content
                .child(
                    div()
                        .id("layout-list")
                        .max_h((window.viewport_size().height * 0.5).min(px(360.)))
                        .overflow_y_scroll()
                        .child(choices),
                )
                .child(
                    row()
                        .child(icon_button(
                            "layout-duplicate",
                            "",
                            "Duplicate current layout as…",
                            cx,
                            |this, window, cx| {
                                this.begin_layout_action(Action::Duplicate, window, cx)
                            },
                        ))
                        .child(icon_button(
                            "layout-rename",
                            "󰏫",
                            "Rename current layout…",
                            cx,
                            |this, window, cx| this.begin_layout_action(Action::Rename, window, cx),
                        )),
                );
        }
        if let Some(error) = &self.error {
            content = content.child(
                copyable_message("layout-management-error", error.clone(), cx)
                    .text_color(rgb(ERROR)),
            );
        }
        div()
            .id("layouts-modal")
            .occlude()
            .absolute()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(0x000000).alpha(0.58))
            .child(
                content
                    .w(px(460.))
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
