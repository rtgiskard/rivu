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
            self.error = Some(self.i18n.text("shell-settings-before-layout").to_owned());
            cx.notify();
            false
        } else {
            true
        }
    }

    fn finish_layout_switch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Layouts only replace the panel tree. Keep the window/editor and its
        // in-flight metadata save alive even when Details is no longer visible.
        self.begin_manual_intent();
        self.layouts_open = false;
        self.layout_action = None;
        self.catalog_open = false;
        self.layout_changed(cx);
        self.focus_layout(window, cx);
        self.sync_analysis(cx);
        cx.notify();
    }

    fn select_layout(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.layout_management_allowed(cx) || name == self.layouts.active_name() {
            return;
        }
        let cancellation = self.interaction.cancel();
        self.apply_interaction_cancellation(cancellation, cx);
        if let Err(error) = self.layouts.select(name) {
            self.error = Some(self.i18n.format(
                "shell-switch-layout-error",
                &[("error", format!("{error:#}").into())],
            ));
            cx.notify();
            return;
        }
        self.layouts
            .current_mut()
            .initialize_audio_visualizers(AudioVisualizerOptions::from(
                self.state.system.config.as_ref(),
            ));
        self.layouts.current_mut().initialize_album_artworks(
            crate::config::AlbumArtworkOptions::from(self.state.system.config.as_ref()),
        );
        self.queue_layout_save();
        self.finish_layout_switch(window, cx);
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
        let name = self.value(Field::LayoutName, cx).trim().to_owned();
        let (result, switched) = match action {
            Action::Duplicate => {
                let cancellation = self.interaction.cancel();
                self.apply_interaction_cancellation(cancellation, cx);
                let result = self.layouts.duplicate(&name);
                (result, true)
            }
            Action::Rename => (self.layouts.rename(&name), false),
            Action::Delete(name) => (self.layouts.remove(&name), false),
        };
        match result {
            Ok(()) => {
                self.queue_layout_save();
                self.layout_action = None;
                if self.error != self.layout_save_error {
                    self.error = None;
                }
                if switched {
                    self.finish_layout_switch(window, cx);
                } else {
                    self.focus_layout(window, cx);
                }
            }
            Err(error) => {
                self.error = Some(self.i18n.format(
                    "shell-manage-layouts-error",
                    &[("error", format!("{error:#}").into())],
                ))
            }
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
                .child(div().flex_1().text_lg().child(self.tr("shell-layouts")))
                .child(components::quiet_icon_button(icon_button(
                    "layouts-close",
                    "",
                    self.tr("shell-close-layouts"),
                    cx,
                    |this, window, cx| {
                        this.layouts_open = false;
                        this.layout_action = None;
                        this.focus_layout(window, cx);
                        cx.notify();
                    },
                ))),
        );
        if let Some(action) = self.layout_action.clone() {
            let (title, submit) = match &action {
                Action::Duplicate => (
                    self.tr("shell-layout-duplicate-title"),
                    self.tr("shell-duplicate"),
                ),
                Action::Rename => (
                    self.tr("shell-layout-rename-title"),
                    self.tr("shell-rename"),
                ),
                Action::Delete(_) => (
                    self.tr("shell-layout-delete-title"),
                    self.tr("shell-delete"),
                ),
            };
            content = content.child(caption(title));
            if let Action::Delete(name) = &action {
                content = content.child(div().text_sm().child(self.i18n.format(
                    "shell-delete-layout-confirm",
                    &[("name", name.as_str().into())],
                )));
            } else {
                content = content.child(form_field(
                    self.tr("shell-name"),
                    self.input(Field::LayoutName),
                ));
            }
            content = content.child(
                row()
                    .child(icon_button(
                        "layout-action-cancel",
                        "",
                        self.tr("shell-cancel-layout-action"),
                        cx,
                        |this, window, cx| {
                            this.layout_action = None;
                            this.focus_layout(window, cx);
                            cx.notify();
                        },
                    ))
                    .child(if matches!(action, Action::Delete(_)) {
                        components::danger_control(button(
                            "layout-action-submit",
                            submit,
                            cx,
                            |this, window, cx| this.finish_layout_action(window, cx),
                        ))
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
                let mut choice = row().group("layout-choice-row").child(
                    menu_item(
                        ("layout-select", index),
                        if current {
                            format!("✓ {name}")
                        } else {
                            name.to_owned()
                        },
                        cx,
                        move |this, window, cx| this.select_layout(&target, window, cx),
                    )
                    .flex_1(),
                );
                if !current {
                    let name = name.to_owned();
                    choice = choice.child(
                        components::quiet_icon_button(icon_button(
                            ("layout-delete", index),
                            "󰆴",
                            self.i18n.format(
                                "shell-delete-layout-hint",
                                &[("name", name.as_str().into())],
                            ),
                            cx,
                            move |this, window, cx| {
                                this.begin_layout_action(Action::Delete(name.clone()), window, cx)
                            },
                        ))
                        .opacity(0.)
                        .group_hover("layout-choice-row", |style| style.opacity(1.))
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            |_, _, cx| {
                                cx.stop_propagation();
                            },
                        ),
                    );
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
                            self.tr("shell-duplicate-layout-as"),
                            cx,
                            |this, window, cx| {
                                this.begin_layout_action(Action::Duplicate, window, cx)
                            },
                        ))
                        .child(icon_button(
                            "layout-rename",
                            "󰏫",
                            self.tr("shell-rename-layout"),
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
            .map(|surface| self.dialog_surface(surface))
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
